use std::sync::{Arc, RwLock};

use log::error;
use vhost::vhost_user::{Listener, VhostUserProtocolFeatures};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;

// NOTES:
// https://github.com/rust-vmm/vhost/blob/main/vhost-user-backend/README.md
//

struct VhostUserService {
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
}

impl VhostUserBackendMut for VhostUserService {
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        1
    }
    fn max_queue_size(&self) -> usize {
        1
    }
    fn features(&self) -> u64 {
        0
    }
    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
    }
    fn set_event_idx(&mut self, _enabled: bool) {}

    fn update_memory(&mut self, _mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        Ok(())
    }
    fn handle_event(
        &mut self,
        device_event: u16,
        evset: EventSet,
        vrings: &[VringRwLock<GuestMemoryAtomic<GuestMemoryMmap>>],
        thread_id: usize,
    ) -> std::io::Result<()> {
        todo!()
        //let mut used_any = false;
        //let mem = match &self.mem {
        //    Some(m) => m.memory(),
        //    None => return Err(Error::NoMemoryConfigured),
        //};
        //
        //let mut vring_state = vring.get_mut();
        //
        //while let Some(avail_desc) = vring_state
        //    .get_queue_mut()
        //    .iter()
        //    .map_err(|_| Error::IterateQueue)?
        //    .next()
        //{
        //    // Process the request...
        //
        //    if self.event_idx {
        //        if vring_state.add_used(head_index, 0).is_err() {
        //            warn!("Couldn't return used descriptors to the ring");
        //        }
        //
        //        match vring_state.needs_notification() {
        //            Err(_) => {
        //                warn!("Couldn't check if queue needs to be notified");
        //                vring_state.signal_used_queue().unwrap();
        //            }
        //            Ok(needs_notification) => {
        //                if needs_notification {
        //                    vring_state.signal_used_queue().unwrap();
        //                }
        //            }
        //        }
        //    } else {
        //        if vring_state.add_used(head_index, 0).is_err() {
        //            warn!("Couldn't return used descriptors to the ring");
        //        }
        //        vring_state.signal_used_queue().unwrap();
        //    }
        //}
        //
        //Ok(used_any)
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let socket = args.get(1).expect("give me a socket path");

    let mem = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let backend = Arc::new(RwLock::new(VhostUserService { mem: mem.clone() }));

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
