//use std::io;
use std::thread;
use std::thread::{spawn,JoinHandle};
use crossbeam::channel as channel;
use crossbeam::channel::{Receiver,Sender};
use std::time::Duration;
use waitid_timeout::{WaitIdDataOvertime};
use std::path::PathBuf;

use nix;
use nix::sched::{sched_getaffinity,sched_setaffinity,CpuSet};

use crate::cloudhypervisor;
use crate::cloudhypervisor::{CloudHypervisor,CloudHypervisorConfig};

use peinit;

type JoinHandleT = JoinHandle<()>;

pub struct Input {
    pub id: u64,
    pub ch_config: CloudHypervisorConfig,
    pub pe_config: peinit::Config,
    pub rootfs: PathBuf,
    pub io_file: PathBuf,
    pub ch_timeout: Duration,
}

pub struct Output {
    pub id: u64,
    pub io_file: PathBuf,
}

type OutputResult = Result<Output, cloudhypervisor::Error>;

struct Pool {
    sender: Sender<Input>,
    receiver: Receiver<OutputResult>,
    handles: Vec<JoinHandleT>,
}

impl Pool {
    fn new(cores: &[CpuSet]) -> Self {
        let (i_s, i_r) = channel::bounded::<Input>(cores.len() * 2);
        let (o_s, o_r) = channel::bounded::<OutputResult>(cores.len() * 2);
        let handles: Vec<_> = cores.iter()
            .map(|c| spawn_worker(*c, i_r.clone(), o_s.clone())).collect();
        Self {
            sender: i_s,
            receiver: o_r,
            handles: handles,
        }
    }

    fn shutdown(self) -> Vec<thread::Result<()>> {
        drop(self.sender);
        self.handles.into_iter().map(|h| h.join()).collect()
    }
}

pub fn cpuset(core_offset: usize,
              n_workers: usize,
              n_cores_per_worker: usize)
    -> nix::Result<Vec<CpuSet>> {
    // restrict to even offset and even cores per worker to keep workers
    // on separate physical cores
    if core_offset % 2 == 1 { return nix::Result::Err(nix::errno::Errno::EINVAL); }
    if n_cores_per_worker % 2 == 1 { return nix::Result::Err(nix::errno::Errno::EINVAL); }
    let all = sched_getaffinity(nix::unistd::Pid::from_raw(0))?; // pid 0 means us
    let total_cores = n_workers * n_cores_per_worker;
    for i in (core_offset-1)..total_cores {
        if !all.is_set(i)? { return nix::Result::Err(nix::errno::Errno::ENAVAIL); }
    }
    let mut ret = vec![];
    for i in 0..n_workers {
        let mut c = CpuSet::new();
        for j in 0..n_cores_per_worker {
            c.set(core_offset + i*n_workers + j)?;
        }
        ret.push(c);
    }
    Ok(ret)
}

// so I was thinking we would always have a hot task running but then we can't even start without
// which if we allow per request then we have to wait for the startup anyways and we could just put
// this all in the config...
pub fn run(input: Input) -> OutputResult {
    let mut ch = CloudHypervisor::start(input.ch_config)?;
    // order of calls is important here
    ch.add_pmem_ro(input.rootfs)?;
    ch.add_pmem_rw(&input.io_file)?;
    match ch.wait_timeout_or_kill(input.ch_timeout).map_err(|_| cloudhypervisor::Error::Wait)? {
        WaitIdDataOvertime::NotExited => {
            // TODO this is real bad
        },
        WaitIdDataOvertime::Exited{..} => { },
        WaitIdDataOvertime::ExitedOvertime{..} => {
            return Err(cloudhypervisor::Error::Overtime);
        },
    }
    Ok(Output{
        id: input.id,
        io_file: input.io_file,
    })
}

fn spawn_worker(cpuset: CpuSet,
                input:  Receiver<Input>,
                output: Sender<OutputResult>,
               )
    -> JoinHandleT {
    spawn(move || {
        sched_setaffinity(nix::unistd::Pid::from_raw(0), &cpuset).unwrap();
        for msg in input.iter() {
            match output.send(run(msg)) {
                Ok(_) => { },
                Err(_) => {
                    // output got disconnected somehow
                    return;
                },
            }
        }
    })
}
