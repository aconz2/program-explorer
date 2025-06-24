use std::ops::Deref;
use std::sync::{Arc, RwLock, RwLockWriteGuard};

use log::{error, info, trace, warn};
use smallvec::{SmallVec, smallvec};
use vhost::vhost_user::message::VHOST_USER_CONFIG_OFFSET;
use vhost::vhost_user::{Listener, VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringState, VringT};
use virtio_bindings::virtio_blk::{
    VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP, VIRTIO_BLK_T_IN,
};
use virtio_bindings::virtio_blk::{
    virtio_blk_config as VirtioBlockConfig, virtio_blk_outhdr as VirtioBlockHeader,
};
use virtio_queue::{DescriptorChain, QueueT, desc::split::Descriptor};
use vm_memory::{
    ByteValued, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap,
    bitmap::Bitmap,
};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

const QUEUE_SIZE: usize = 1024;
// max len of iovec (essentially), governs size of smallvec
const SEG_MAX: usize = 16;

// NOTES:
// Resources:
// - https://github.com/rust-vmm/vhost/blob/main/vhost-user-backend/README.md
// - virtio_blk_config docs in include/uapi/linux/virtio_blk.h
// - spec: https://docs.oasis-open.org/virtio/virtio/v1.1/virtio-v1.1.html
// - how split virtqueue works https://www.redhat.com/en/blog/virtqueues-and-virtio-ring-how-data-travels
//
// kernel boot message like this shows the size in number of 512-byte sectors, along with the the
// logical block size in MB and Mib (see virtblk_update_capacity)
// [    0.104254] virtio_blk virtio1: [vda] 16384 512-byte logical blocks (8.39 MB/8.00 MiB)
//
// features start with VIRTIO_BLK_F where F is for feature. If enabled, they have a corresponding
// part of the virtio_blk_config struct that will be read to get the value
// - SEG_MAX: maximum number of read descriptors in a single chain we're willing to accept
// - SIZE_MAX: maximum size of a single read descriptor
// - BLK_SIZE: sets the logical block size of the device, where reads should be aligned to the
//   block size (I think) and in multiples of the block size
// - TOPOLOGY: sets physical_block_exp, alignment_offset, min_io_size and opt_io_size
//   I think alignment_offset can always be 0 for us, seems to be when the first sector was at some
//   weird sector start not a multiple of the block size.
//   1 << physical_block_exp == physical_block_size and I think we can always set to == logical block size
//   {min,opt}_io_size are in number of logical blocks
// setting size_max to 2048 results in a kernel error in blk_validate_limits...

// these are wrapper types over
//  struct virtio_blk_config
//  struct virtio_blk_outhdr
// so that they can be used with read_obj/write_obj
#[derive(Copy, Clone)]
#[allow(dead_code)]
struct VirtioBlockConfigWriter(VirtioBlockConfig);
unsafe impl ByteValued for VirtioBlockConfigWriter {}

// struct virtio_blk_outhdr from the kernel is a bit confusingly named, I think out refers to out
// of driver and to the device, since the in_hdr has the status which goes into the driver from the
// device. see virtblk_setup_cmd for how it is used
//   type: le32
//   io_priority: le32
//   sector: le64
// so I'm using it anyways
// see drivers/block/virtio_blk.c struct virtblk_req to see how it's used
// the spec definition for the req is https://docs.oasis-open.org/virtio/virtio/v1.1/virtio-v1.1.html#x1-2500006
// struct virtio_blk_req {
//   le32 type;     <-------------------------
//   le32 reserved;        header descriptor |
//   le64 sector;   <-------------------------
//   u8 data[];
//   u8 status;     <---- status descriptor
// };
// every read request has a chain with at least 3 descriptors, the head, the reads, and the status
#[derive(Copy, Clone)]
#[allow(dead_code)]
struct VirtioBlockHeaderReader(VirtioBlockHeader);
unsafe impl ByteValued for VirtioBlockHeaderReader {}

#[derive(Debug, thiserror::Error)]
enum Error {
    NoHead,
    NeedRead,
    NeedWrite,
    NoStatus,
    Mem,
    StatusDescTooSmall,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::other(e)
    }
}

#[derive(Default, Debug)]
struct Metrics {
    reads: usize,
    segments: usize,
    notifications_skipped: usize,
}

struct VhostUserService {
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
    config: VirtioBlockConfig,
    exit_evt: EventFd,
    metrics: Metrics,
    #[cfg(feature = "event_idx")]
    event_idx: bool,
}

fn read_virtio_blk_outhdr<B: Bitmap + 'static>(
    mem: &vm_memory::GuestMemoryMmap<B>,
    addr: GuestAddress,
) -> Result<VirtioBlockHeader, Error> {
    Ok(mem
        .read_obj::<VirtioBlockHeaderReader>(addr)
        .map_err(|_| Error::Mem)?
        .0)
}

struct ProcessItemResponse {
    status: u8,
    status_addr: GuestAddress,
    len: u32,
}

impl ProcessItemResponse {
    fn ok(len: u32, status_desc: &Descriptor) -> Self {
        ProcessItemResponse {
            status: VIRTIO_BLK_S_OK as u8,
            len,
            status_addr: status_desc.addr(),
        }
    }
    fn unsupp(status_desc: &Descriptor) -> Self {
        ProcessItemResponse {
            status: VIRTIO_BLK_S_UNSUPP as u8,
            len: 1,
            status_addr: status_desc.addr(),
        }
    }
}

impl VhostUserService {
    fn process_queue(
        &mut self,
        vring: &mut RwLockWriteGuard<'_, VringState>,
    ) -> Result<bool, Error> {
        let mut used_any = false;
        while let Some(mut chain) = vring
            .get_queue_mut()
            .pop_descriptor_chain(self.mem.memory())
        {
            let len = match self.process_item(&mut chain) {
                Ok(ProcessItemResponse {
                    status,
                    len,
                    status_addr,
                }) => {
                    chain.memory().write_obj(status, status_addr).unwrap();
                    len
                }
                Err(e) => {
                    error!("error process_item {e}");
                    1
                }
            };

            // only have to return the head descriptor to the used ring, and for reads we write the
            // total amount of data written (written by us, read by them)
            used_any = true;
            vring.add_used(chain.head_index(), len).unwrap();
        }

        Ok(used_any)
    }

    fn process_item<M>(
        &mut self,
        chain: &mut DescriptorChain<M>,
    ) -> Result<ProcessItemResponse, Error>
    where
        M: Deref<Target = GuestMemoryMmap<()>>,
    {
        // this is almost equivalent, but makes it so there's a double mut borrow at the end when
        // we do vring.add_used
        // for mut chain in vring.get_queue().iter(self.mem.memory()).unwrap() {
        // the chain looks like
        // header : readble
        // Out/In : readable for writes, writable for reads
        // status : writeable
        //debug!("{:?}", chain);
        //for (i, x) in chain.clone().enumerate() {
        //    debug!("{i} {x:?}");
        //}
        let head_desc = chain
            .next()
            .ok_or(Error::NoHead)
            .inspect_err(|_| error!("no head"))?;

        if head_desc.is_write_only() {
            error!("head not readable");
            return Err(Error::NeedRead);
        }

        let header = read_virtio_blk_outhdr(chain.memory(), head_desc.addr())
            .inspect_err(|e| error!("read head {e}"))
            .map_err(|_| Error::Mem)?;

        trace!("header {:?}", header);

        // sector is in 512 byte offset (regardless of block size)
        trace!("sector read starting at {}", header.sector);

        let mut requests: SmallVec<[_; SEG_MAX]> = smallvec![];
        let mut status_desc = None;
        while let Some(desc) = chain.next() {
            // we only serve reads which must be writeable by us
            if !desc.is_write_only() {
                return Err(Error::NeedWrite);
            }
            if desc.has_next() {
                requests.push(desc);
            } else {
                status_desc = Some(desc);
            }
        }
        let status_desc = status_desc.ok_or(Error::NoStatus)?;

        // TODO VIRTIO_BLK_T_GET_ID

        // we check this after trying to get the status_desc
        if header.type_ != VIRTIO_BLK_T_IN {
            error!("got a header not expecting {}", header.type_);
            return Ok(ProcessItemResponse::unsupp(&status_desc));
        }

        if status_desc.len() < 1 {
            return Err(Error::StatusDescTooSmall);
        }

        // so my current thoughts on how to service the actual read are:
        // calculate the blocks required to satisfy the read. Blocks will be likely 1-4MB or so?
        // fast path for a single block is to try openat(cache_dir, {id}_{block}), if it succeeds
        // then we can do the read and we're done
        // we get the cache_dir fd from the cache service which we'll connect to via unix sock
        // stream socket with an initial hello message
        // if we don't get fast path then we send a message to the cache service with our image id,
        // sector, and total length. The cache service then fetches any blocks it needs to from
        // object storage and saves them to files {id}_{block}, then it sendfile's the data over
        // the socket where we have done a readv(socket, iov) to fill in the data
        //
        // having another process we have to relay to isn't amazing, but managing a bounded size
        // cache between processes is a bit hard if they are "independent" especially around
        // managing any kind of lru. And I think the fast path is a decent compromise, and reading
        // directly into the guest memory is better than getting back the fd's and then doing it.
        // This would be a blocking communication over the socket, which for single core guest
        // isn't a problem. A main hurdle in all of this is the epoll-first nature of using this
        // trait which I'm not sure how to integrate with async. That might open more possibility
        // to doing the object retreival in process and concurrently, but tbd how to best do that.
        //
        // what I don't have a clear vision of right now is what kind of id to use for these
        // images, since it is kind of a manifest digest as id vs content address of erofs image
        // id, but if we use the latter then we have to store that mapping somewhere (object tag?).
        // And the cache service has to know what bucket url to use and will it take that as config
        // or be oblivious and we provide that somehow. Does it scan the bucket periodically to get
        // the list of pre-baked images it will service and relay that to the server-worker so it
        // knows whether to spawn with --pmem or --disk vhost_user=on, or should the
        // peimage-service know about this and respond with either one? Should we be using object
        // storage for all of the images or intelligently for some of them? Initially my plan was
        // to support it for the compiler explorer ones which are a) large b) not rapidly changing
        //
        // One aspect of object storage when used in a caching manner is that we could get
        // concurrent usage and deletion, so if we do that eventually we have to use versioning so
        // that if we start a container running that expects to have an image available on object
        // storage, it needs to get the version of that object when it starts because otherwise it
        // might get deleted. And then a bucket with NoncurrentVersionExpiration will take care of
        // fully deleting old objects (assuming max container runtime < NoncurrentDays)

        // TODO write the actual response data
        let mut total_len = 0;
        for desc in &requests {
            let _addr = desc.addr();
            let len = desc.len();
            //debug!("read {:?} {}", _addr, len);
            if true {
                use vm_memory::GuestMemory;
                let buf = vec![42u8; len as usize];
                chain
                    .memory()
                    .get_slice(_addr, len as usize)
                    .unwrap()
                    .copy_from(&buf);
            }
            total_len += len;
        }

        // the linux kernel doesn't seem to actually care about the len written in the used
        // descriptor
        self.metrics.reads += 1;
        self.metrics.segments += requests.len();

        Ok(ProcessItemResponse::ok(total_len, &status_desc))
    }
}

impl VhostUserBackendMut for VhostUserService {
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        1
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        use virtio_bindings::virtio_blk::*;
        use virtio_bindings::virtio_config::*;

        #[cfg(feature = "event_idx")]
        let enable_event_idx = 1 << virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;

        #[cfg(not(feature = "event_idx"))]
        let enable_event_idx = 0;

        (1 << VIRTIO_BLK_F_SEG_MAX)
            | (1 << VIRTIO_BLK_F_SIZE_MAX)
            | (1 << VIRTIO_BLK_F_BLK_SIZE)
            | (1 << VIRTIO_BLK_F_TOPOLOGY)
            | (1 << VIRTIO_BLK_F_RO)
            | (1 << VIRTIO_F_VERSION_1)
            | enable_event_idx
            // this is VHOST_USER_F_PROTOCOL_FEATURES
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        // this is VHOST_USER_PROTOCOL_F_CONFIG
        VhostUserProtocolFeatures::CONFIG
    }

    fn update_memory(&mut self, _mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        Ok(())
    }

    #[cfg(feature = "event_idx")]
    fn set_event_idx(&mut self, event_idx: bool) {
        trace!("event_idx enabled? {}", event_idx);
        self.event_idx = event_idx;
    }

    #[cfg(not(feature = "event_idx"))]
    fn set_event_idx(&mut self, event_idx: bool) {
        // should never happen
        if event_idx {
            error!("event_idx unsupported");
        }
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: EventSet,
        vrings: &[VringRwLock<GuestMemoryAtomic<GuestMemoryMmap>>],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        // TODO returning Err in here will cause the whole process to get torn down
        //
        // TODO same thing here, if we add things to the epoll handler then we could get different
        // event types
        // I'm not sure this condition can ever happen
        if evset != EventSet::IN {
            warn!("handle_event called for non IN event");
            return Ok(());
        }

        // TODO this is only true because we have not registered any other events with the epoll
        // handler, if/when we do to actually service the reads in an async fashion, we can check
        // that here
        // vhost-user-backend/src/event_loop.rs
        // our caller has already checked that device_event is a valid index into vrings
        let mut vring = vrings[device_event as usize].get_mut();

        trace!(
            "vring event_idx_enabled {}",
            vring.get_queue().event_idx_enabled()
        );

        #[cfg(feature = "event_idx")]
        let event_idx = self.event_idx;

        #[cfg(not(feature = "event_idx"))]
        let event_idx = false;

        // the event_idx thing is kinda crazy, every impl I can find has something to this effect
        if event_idx {
            loop {
                vring
                    .get_queue_mut()
                    .enable_notification(self.mem.memory().deref())
                    .unwrap();

                if self
                    .process_queue(&mut vring)
                    .inspect_err(|e| error!("error while processing queue {e}"))
                    .unwrap_or(false)
                {
                    if vring.needs_notification().unwrap() {
                        trace!("needs_notification? true");
                        vring.signal_used_queue().unwrap();
                    } else {
                        self.metrics.notifications_skipped += 1;
                    }
                } else {
                    break;
                }
            }
        } else {
            if self
                .process_queue(&mut vring)
                .inspect_err(|e| error!("error while processing queue {e}"))
                .unwrap_or(false)
            {
                if vring.needs_notification().unwrap() {
                    trace!("needs_notification? true");
                    vring.signal_used_queue().unwrap();
                }
            }
        }

        Ok(())
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        if offset != VHOST_USER_CONFIG_OFFSET {
            return vec![];
        }
        // depending on what features are available, caller may not be asking for the whole
        // struct. If we return something with the unexpected size, the reply message is
        // essentially an error
        VirtioBlockConfigWriter(self.config)
            .as_slice()
            .get(..size as usize)
            .unwrap_or(&[])
            .to_vec()
    }

    fn set_config(&mut self, _offset: u32, _buf: &[u8]) -> std::io::Result<()> {
        // TODO when is this called and do we have to support it?
        warn!("set_config called, ignoring");
        Ok(())
    }

    fn queues_per_thread(&self) -> Vec<u64> {
        vec![1]
    }

    fn exit_event(&self, _thread_index: usize) -> Option<EventFd> {
        self.exit_evt.try_clone().ok()
    }
}

fn main() {
    env_logger::init();
    let args: Vec<_> = std::env::args().collect();
    let socket = args.get(1).expect("give me a socket path");

    let fake_size = 8388608; // size of busybox.erofs
    let block_size: u32 = 512 * 8;
    assert!(fake_size % 512 == 0);
    let num_sectors = (fake_size as u64) / 512;
    let physical_block_exp = block_size.ilog2();
    assert!(1 << physical_block_exp == block_size);

    let mem = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let backend = Arc::new(RwLock::new(VhostUserService {
        mem: mem.clone(),
        exit_evt: EventFd::new(EFD_NONBLOCK).unwrap(),
        config: VirtioBlockConfig {
            capacity: num_sectors,
            blk_size: block_size,
            size_max: 65536,
            seg_max: SEG_MAX.try_into().unwrap(),
            num_queues: 1,
            alignment_offset: 0,
            physical_block_exp: physical_block_exp.try_into().unwrap(),
            min_io_size: 1,
            opt_io_size: 1,
            ..Default::default()
        },
        metrics: Metrics::default(),
        #[cfg(feature = "event_idx")]
        event_idx: false,
    }));
    info!("listening on {}", socket);

    let unlink = true;
    let listener = Listener::new(socket, unlink).unwrap();

    let name = "pevub";
    let mut daemon = VhostUserDaemon::new(name.to_string(), backend.clone(), mem).unwrap();

    if let Err(e) = daemon.start(listener) {
        error!("Failed to start daemon: {:?}\n", e);
        std::process::exit(1);
    }

    if let Err(e) = daemon.wait() {
        error!("Error from the main thread: {:?}", e);
    }
    info!("metrics {:?}", backend.read().unwrap().metrics);
}
