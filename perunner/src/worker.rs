use std::thread::{spawn,JoinHandle};
use crossbeam::channel::Receiver;

use crate::cloudhypervisor::{CloudHypervisor,CloudHypervisorConfig};

pub struct WorkerInput {
    pub id: u64,
    pub ch_config: CloudHypervisorConfig,
    pub io_file: PathBuf,
}

pub struct WorkerOutput {
    pub id: u64,
    pub io_file: PathBuf,
}

// struct WorkerPool {
//
// }

fn worker_run(input: WorkerInput) -> WorkerOutput {
    todo!();
}

fn worker_task(cores: u64,
               input:  &Receiver<WorkerInput>,
               output: &Receiver<WorkerOutput>,
               cancel: &Receiver<()>
               )
    -> JoinHandle<()> {
    spawn(|| {
        loop {
            select! {
                recv(cancel) -> msg => { break; },
                recv(q)      -> msg => {
                    worker_run(msg)
                }
            }
        }
    })
}
