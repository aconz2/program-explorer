use std::sync::{Arc, RwLock};

use log::{debug, error, info, warn};
use vhost::vhost_user::message::VHOST_USER_CONFIG_OFFSET;
use vhost::vhost_user::{Listener, VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_bindings::virtio_blk::{VIRTIO_BLK_S_OK, VIRTIO_BLK_T_IN};
use virtio_bindings::virtio_blk::{
    virtio_blk_config as VirtioBlockConfig, virtio_blk_outhdr as VirtioBlockHeader,
};
use virtio_queue::QueueT;
use vm_memory::{
    ByteValued, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap,
    bitmap::Bitmap,
};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

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
    EventNotEpollIn,
    NoHead,
    NeedRead,
    NeedWrite,
    NoStatus,
    Mem,
    NotARead,
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

struct VhostUserService {
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
    config: VirtioBlockConfig,
    exit_evt: EventFd,
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

impl VhostUserBackendMut for VhostUserService {
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        1
    }

    fn max_queue_size(&self) -> usize {
        1024
    }

    fn features(&self) -> u64 {
        use virtio_bindings::virtio_blk::*;
        use virtio_bindings::virtio_config::*;
        use virtio_bindings::virtio_ring::*;

        (1 << VIRTIO_BLK_F_SEG_MAX)
            | (1 << VIRTIO_BLK_F_SIZE_MAX)
            | (1 << VIRTIO_BLK_F_BLK_SIZE)
            //| (1 << VIRTIO_BLK_F_TOPOLOGY)
            | (1 << VIRTIO_BLK_F_RO)
            | (1 << VIRTIO_F_VERSION_1)
            //| (1 << VIRTIO_RING_F_EVENT_IDX)
            // this is VHOST_USER_F_PROTOCOL_FEATURES
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        // this is VHOST_USER_PROTOCOL_F_CONFIG
        VhostUserProtocolFeatures::CONFIG
    }

    fn update_memory(&mut self, _mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        debug!("update memory");
        Ok(())
    }

    fn set_event_idx(&mut self, event_idx: bool) {
        if event_idx {
            //panic!("unsupported");
        }
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: EventSet,
        vrings: &[VringRwLock<GuestMemoryAtomic<GuestMemoryMmap>>],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        debug!("handle_event");
        // TODO I think we should be responding with error instead of returning early with error
        if evset != EventSet::IN {
            return Err(Error::EventNotEpollIn.into());
        }

        // vhost-user-backend/src/event_loop.rs
        // our caller has already checked that device_event is a valid index into vrings
        let mut vring = vrings[device_event as usize].get_mut();

        // this is almost equivalent, but makes it so there's a double mut borrow at the end when
        // we do vring.add_used
        // for mut chain in vring.get_queue().iter(self.mem.memory()).unwrap() {
        while let Some(mut chain) = vring
            .get_queue_mut()
            .pop_descriptor_chain(self.mem.memory())
        {
            // the chain looks like
            // header : readble
            // Out/In : readable for writes, writable for reads
            // status : writeable
            //debug!("{:?}", chain);
            for (i, x) in chain.clone().enumerate() {
                debug!("{i} {x:?}");
            }
            let head_desc = chain
                .next()
                .ok_or(Error::NoHead)
                .inspect_err(|_| error!("no head"))?;

            if head_desc.is_write_only() {
                error!("head not readable");
                return Err(Error::NeedRead.into());
            }

            if false {
                use vm_memory::GuestMemory;
                eprintln!("header raw data");
                let mut buf = vec![0; head_desc.len() as usize];
                let _ = chain.memory().get_slice(head_desc.addr(), head_desc.len() as usize).unwrap().copy_to(&mut buf);
                for byte in buf {
                    eprint!("{:x}", byte);
                }
                eprintln!();
            }

            let header =
                read_virtio_blk_outhdr(chain.memory(), head_desc.addr())
                .inspect_err(|e| error!("read head {e}"))
                .map_err(|_| Error::Mem)?;

            debug!("header {:?}", header);

            if header.type_ != VIRTIO_BLK_T_IN {
                error!("got a header not expecting {}", header.type_);
                return Err(Error::NotARead.into());
            }
            debug!("sector read starting at {}", header.sector);
            // TODO VIRTIO_BLK_T_GET_ID

            let mut requests = vec![];
            let mut status_desc = None;
            while let Some(desc) = chain.next() {
                // we only serve reads which must be writeable by us
                if !desc.is_write_only() {
                    return Err(Error::NeedWrite.into());
                }
                if desc.has_next() {
                    requests.push(desc);
                } else {
                    status_desc = Some(desc);
                }
            }
            let status_desc = status_desc.ok_or(Error::NoStatus)?;

            if status_desc.len() < 1 {
                return Err(Error::StatusDescTooSmall.into());
            }
            //debug!(
            //    "head {:?} status {:?} requests {:?}",
            //    head_desc, status_desc, requests
            //);
            // TODO write the actual response data
            let mut total_len = 0;
            for desc in requests {
                let _addr = desc.addr();
                let len = desc.len();
                debug!("read {:?} {}", _addr, len);
                if true {
                    use vm_memory::GuestMemory;
                    let buf = vec![42u8; len as usize];
                    chain.memory()
                        .get_slice(_addr, len as usize)
                        .unwrap()
                        .copy_from(&buf);
                }
                total_len += len;
            }

            // the linux kernel doesn't seem to actually care about the len written in the used
            // descriptor

            chain
                .memory()
                .write_obj(VIRTIO_BLK_S_OK as u8, status_desc.addr())
                .unwrap();

            // only have to return the head descriptor to the used ring, and for reads we write the
            // total amount of data written (written by us, read by them)
            vring
                .add_used(chain.head_index(), total_len)
                .unwrap();
        }

        // needs_notification takes care of checking the proper condition when event_idx is
        // enabled
        debug!("vring event_idx_enabled {}", vring.get_queue().event_idx_enabled());
        if vring.needs_notification().unwrap() {
            debug!("needs_notification? true");
            vring.signal_used_queue().unwrap();
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
        warn!("set_config called, ignoring");
        Ok(())
    }

    fn queues_per_thread(&self) -> Vec<u64> {
        debug!("queues_per_thread");
        vec![1]
    }

    fn exit_event(&self, _thread_index: usize) -> Option<EventFd> {
        debug!("exit_event");
        self.exit_evt.try_clone().ok()
    }
}

fn main() {
    env_logger::init();
    let args: Vec<_> = std::env::args().collect();
    let socket = args.get(1).expect("give me a socket path");

    let fake_size = 8388608; // size of busybox.erofs
    // TODO can't get any block size besides 512 to work with or without F_TOPOLOGY
    let block_size: u32 = 512 * 1;
    assert!(fake_size % block_size == 0);
    let num_blocks = (fake_size as u64) / (block_size as u64);
    let physical_block_exp = block_size.ilog2();
    assert!(1 << physical_block_exp == block_size);

    let mem = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let backend = Arc::new(RwLock::new(VhostUserService {
        mem: mem.clone(),
        exit_evt: EventFd::new(EFD_NONBLOCK).unwrap(),
        config: VirtioBlockConfig {
            capacity: num_blocks, // number of sectors in 512-byte sectors,
            blk_size: block_size,   // block size if VIRTIO_BLK_F_BLK_SIZE
            size_max: 65536, // maximum segment size if VIRTIO_BLK_F_SIZE_MAX
            seg_max: 1,      // The maximum number of segments (if VIRTIO_BLK_F_SEG_MAX)
            num_queues: 1,   // number of vqs, only available when VIRTIO_BLK_F_MQ is set
            alignment_offset: 0,
            physical_block_exp: physical_block_exp.try_into().unwrap(),
            min_io_size: 1,
            opt_io_size: 1,
            ..Default::default()
        },
    }));
    info!("listening on {}", socket);

    let unlink = true;
    let listener = Listener::new(socket, unlink).unwrap();

    let name = "virtio-user-block-s3";
    let mut daemon = VhostUserDaemon::new(name.to_string(), backend, mem).unwrap();

    if let Err(e) = daemon.start(listener) {
        error!("Failed to start daemon: {:?}\n", e);
        std::process::exit(1);
    }

    if let Err(e) = daemon.wait() {
        error!("Error from the main thread: {:?}", e);
    }
}
