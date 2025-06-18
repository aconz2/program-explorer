use std::ops::Deref;
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

// these are wrapper types over
//  struct virtio_blk_outhdr
//  struct virtio_blk_config
// so that they can be used with read_obj
#[derive(Copy, Clone)]
#[allow(dead_code)]
struct VirtioBlockConfigWriter(VirtioBlockConfig);
unsafe impl ByteValued for VirtioBlockConfigWriter {}

#[derive(Copy, Clone)]
#[allow(dead_code)]
struct VirtioBlockHeaderReader(VirtioBlockHeader);
unsafe impl ByteValued for VirtioBlockHeaderReader {}

// NOTES:
// https://github.com/rust-vmm/vhost/blob/main/vhost-user-backend/README.md
// virtio_blk_config docs in include/uapi/linux/virtio_blk.h
//
#[derive(Debug, thiserror::Error)]
enum Error {
    EventNotEpollIn,
    UnknownEvent,
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
        debug!("num_queues");
        1
    }

    fn max_queue_size(&self) -> usize {
        debug!("max_queue_size");
        1024
    }

    fn features(&self) -> u64 {
        debug!("features");
        use virtio_bindings::virtio_blk::*;
        use virtio_bindings::virtio_config::*;

        (1 << VIRTIO_BLK_F_SEG_MAX)
            | (1 << VIRTIO_BLK_F_BLK_SIZE)
            | (1 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_BLK_F_RO)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        debug!("protocol_features");
        VhostUserProtocolFeatures::CONFIG | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
    }

    fn update_memory(&mut self, _mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        debug!("update memory");
        Ok(())
    }

    fn set_event_idx(&mut self, event_idx: bool) {
        if event_idx {
            panic!("unsupported");
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

        let mut counter = 0;

        debug!("vring event_idx_enabled {}", vring.get_queue().event_idx_enabled());

        for _ in 0..10 {
        while let Some(mut chain) = vring
            .get_queue_mut()
            .pop_descriptor_chain(self.mem.memory())
        {
            // the chain looks like
            // header : readble
            // Out/In : readable for writes, writable for reads
            // status : writeable
            //debug!("{:?}", chain);
            debug!("event {counter}");
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

            // TODO is virtio_blk_outhdr the right thing to be reading here?
            let header =
                read_virtio_blk_outhdr(chain.memory(), head_desc.addr())
                .inspect_err(|e| error!("read head {e}"))
                .map_err(|_| Error::Mem)?;

            debug!("header {:?}", header);

            if header.type_ != VIRTIO_BLK_T_IN {
                error!("got a header not expecting {}", header.type_);
                return Err(Error::NotARead.into());
            }
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
                total_len += len;
            }

            debug!("responding with total len {}", total_len);
            chain
                .memory()
                .write_obj(VIRTIO_BLK_S_OK as u8, status_desc.addr())
                .unwrap();

            // only have to return the head descriptor to the used ring, and for reads we write the
            // total amount of data written (written by us, read by them)
            vring
                .get_queue_mut()
                .add_used(chain.memory(), chain.head_index(), total_len)
                .unwrap();

            // TODO what is event_idx and do we need it?
            vring.signal_used_queue().unwrap();
            counter += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        }
        debug!("exit handle_event");

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
        debug!("get_config");
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

    let mem = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let backend = Arc::new(RwLock::new(VhostUserService {
        mem: mem.clone(),
        exit_evt: EventFd::new(EFD_NONBLOCK).unwrap(),
        config: VirtioBlockConfig {
            capacity: 1,     // number of sectors in 512-byte sectors,
            blk_size: 512,   // block size if VIRTIO_BLK_F_BLK_SIZE
            size_max: 65535, // maximum segment size if VIRTIO_BLK_F_SIZE_MAX
            seg_max: 2,      // The maximum number of segments (if VIRTIO_BLK_F_SEG_MAX)
            num_queues: 1,   // number of vqs, only available when VIRTIO_BLK_F_MQ is set
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
