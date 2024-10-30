//use std::io;
use std::thread;
use std::thread::{spawn,JoinHandle};
use crossbeam::channel as channel;
use crossbeam::channel::{Receiver,Sender,TrySendError};
use std::time::Duration;
use waitid_timeout::{WaitIdDataOvertime,Siginfo};
use std::path::PathBuf;

use nix;
use nix::sched::{sched_getaffinity,sched_setaffinity,CpuSet};
use tempfile::NamedTempFile;

use crate::cloudhypervisor;
use crate::cloudhypervisor::{CloudHypervisor,CloudHypervisorConfig,CloudHypervisorPostMortem,CloudHypervisorLogs};
use peinit;

type JoinHandleT = JoinHandle<()>;

pub struct Input {
    pub id: u64,
    pub ch_config: CloudHypervisorConfig,
    pub pe_config: peinit::Config,
    pub rootfs: PathBuf,
    pub io_file: NamedTempFile,
    pub ch_timeout: Duration,
}

pub struct Output {
    pub id: u64,
    pub io_file: NamedTempFile,
    pub ch_logs: CloudHypervisorLogs,
}

pub type OutputResult = Result<Output, CloudHypervisorPostMortem>;

pub struct Pool {
    sender: Sender<Input>,
    receiver: Receiver<OutputResult>,
    handles: Vec<JoinHandleT>,
}

pub struct PoolShuttingDown {
    receiver: Receiver<OutputResult>,
    handles: Vec<JoinHandleT>,
}

impl Pool {
    pub fn new(cores: &[CpuSet]) -> Self {
        let (i_s, i_r) = channel::bounded::<Input>(cores.len() * 2);
        let (o_s, o_r) = channel::bounded::<OutputResult>(cores.len() * 2);
        let handles: Vec<_> = cores
            .iter()
            .enumerate()
            .map(|(i, c)| spawn_worker(i, *c, i_r.clone(), o_s.clone()))
            .collect();
        Self {
            sender: i_s,
            receiver: o_r,
            handles: handles,
        }
    }

    pub fn sender(&mut self) -> &Sender<Input> { &self.sender }
    pub fn receiver(&mut self) -> &Receiver<OutputResult> { &self.receiver }

    pub fn close_sender(self) -> PoolShuttingDown {
        PoolShuttingDown { receiver: self.receiver, handles: self.handles, }
    }
}

impl PoolShuttingDown {
    pub fn shutdown(self) -> Vec<thread::Result<()>> {
        // do we need to do anything with receiver?
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
    let mut ret = Vec::with_capacity(n_workers);
    for i in 0..n_workers {
        let mut c = CpuSet::new();
        for j in 0..n_cores_per_worker {
            let k = core_offset + i*n_cores_per_worker + j;
            if !all.is_set(k)? { return nix::Result::Err(nix::errno::Errno::ENAVAIL); }
            c.set(k)?;
        }
        ret.push(c);
    }
    Ok(ret)
}

// TODO another idea is to preboot a task and then wait for the input, but if we want to support
// choosing kernel version, then doesn't really work
// a bit ugly since we can't easily use ? to munge the errors
pub fn run(input: Input) -> OutputResult {
    let mut ch = {
        match CloudHypervisor::start(input.ch_config) {
            Ok(ch) => ch,
            Err(e) => { return Err(e.into()); }
        }
    };
    // order of calls is important here
    match ch.add_pmem_ro(input.rootfs) {
        Ok(_) => { },
        Err(e) => { return Err(ch.postmortem(e)); }
    }
    match ch.add_pmem_rw(&input.io_file) {
        Ok(_) => { },
        Err(e) => { return Err(ch.postmortem(e)); }
    }
    match ch.wait_timeout_or_kill(input.ch_timeout).map_err(|_| cloudhypervisor::Error::Wait) {
        Ok(WaitIdDataOvertime::NotExited) => {
            // TODO this is real bad
        },
        Ok(WaitIdDataOvertime::Exited{siginfo, ..}) => {
            let info: Siginfo = (&siginfo).into();
            if info != Siginfo::Exited(0) {
                return Err(ch.postmortem(cloudhypervisor::Error::BadExit));
            }
        },
        Ok(WaitIdDataOvertime::ExitedOvertime{..}) => {
            return Err(ch.postmortem(cloudhypervisor::Error::Overtime));
        },
        Err(e) => { return Err(ch.postmortem(e)); }
    }
    Ok(Output{
        id: input.id,
        io_file: input.io_file,
        ch_logs: ch.into_logs(),
    })
}

fn spawn_worker(id: usize,
                cpuset: CpuSet,
                input:  Receiver<Input>,
                output: Sender<OutputResult>,
               )
    -> JoinHandleT {
    spawn(move || {
        println!("starting worker {id}");
        sched_setaffinity(nix::unistd::Pid::from_raw(0), &cpuset).unwrap();
        for msg in input.iter() {
            match output.send(run(msg)) {
                Ok(_) => { },
                Err(_) => {
                    // output got disconnected somehow
                    println!("worker {id} got disconnected");
                    return;
                },
            }
        }
        println!("worker {id} shutting down");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuset_good() {
        // TODO bad test only works on this machine
        const N: usize = 32;
        let mut s = ['1'; N];
        let set = cpuset(2, 15, 2).unwrap();
        //println!("{:?}", set);
        for x in set {
            let s: String = (0..N).map(|i| if x.is_set(i).unwrap() { '1' } else { '_' }).collect();
            println!("{:?}", s);
        }
    }

    #[test]
    fn test_cpuset_bad() {
        let _ = cpuset(1, 1, 2).unwrap_err();  // odd offset
        let _ = cpuset(0, 1, 1).unwrap_err();  // odd cores per worker
        let _ = cpuset(2, 16, 2).unwrap_err(); // too many cores
    }
}
