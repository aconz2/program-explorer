use crossbeam::channel;
use crossbeam::channel::{Receiver, Sender};
use std::os::fd::{OwnedFd, AsRawFd, FromRawFd};
use std::thread;
use std::thread::{spawn, JoinHandle};
use std::time::Duration;
use waitid_timeout::{Siginfo, WaitIdDataOvertime};

use log::trace;
use nix;
use nix::sched::{sched_getaffinity, sched_setaffinity, CpuSet};

use crate::cloudhypervisor;
use crate::cloudhypervisor::{
    CloudHypervisor, CloudHypervisorConfig, CloudHypervisorLogs, CloudHypervisorPmem,
    CloudHypervisorPmemMode, CloudHypervisorPostMortem,
    PathBufOrOwnedFd,
};
use crate::iofile::IoFile;

type JoinHandleT = JoinHandle<()>;

pub struct Input {
    pub id: u64,
    pub ch_config: CloudHypervisorConfig,
    pub image: PathBufOrOwnedFd,
    pub io_file: IoFile,
    pub ch_timeout: Duration,
}

pub struct Output {
    pub id: u64,
    pub io_file: IoFile,
    pub ch_logs: CloudHypervisorLogs,
}

pub type OutputResult = Result<Output, CloudHypervisorPostMortem>;

pub struct Pool {
    sender: Sender<Input>,
    receiver: Receiver<OutputResult>,
    #[allow(dead_code)]
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

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn sender(&mut self) -> &Sender<Input> {
        &self.sender
    }
    pub fn receiver(&mut self) -> &Receiver<OutputResult> {
        &self.receiver
    }

    pub fn close_sender(self) -> PoolShuttingDown {
        PoolShuttingDown {
            receiver: self.receiver,
            handles: self.handles,
        }
    }
}

impl PoolShuttingDown {
    pub fn receiver(&mut self) -> &Receiver<OutputResult> {
        &self.receiver
    }
    pub fn shutdown(self) -> Vec<thread::Result<()>> {
        // do we need to do anything with receiver?
        self.handles.into_iter().map(|h| h.join()).collect()
    }
}

fn spawn_worker(
    id: usize,
    cpuset: CpuSet,
    input: Receiver<Input>,
    output: Sender<OutputResult>,
) -> JoinHandleT {
    spawn(move || {
        trace!("starting worker {id}");
        sched_setaffinity(nix::unistd::Pid::from_raw(0), &cpuset).unwrap();
        for msg in input.iter() {
            match output.send(run(msg)) {
                Ok(_) => {}
                Err(_) => {
                    // output got disconnected somehow
                    trace!("worker {id} got disconnected");
                    return;
                }
            }
        }
        trace!("worker {id} shutting down");
    })
}

// a bit ugly since we can't easily use ? to munge the errors
pub fn run(input: Input) -> OutputResult {
    let pmems = CloudHypervisorPmem::Two([
        (input.image, CloudHypervisorPmemMode::ReadOnly),
        (
            // child process is scoped to this function, we keep input.io_file alive
            PathBufOrOwnedFd::Fd(unsafe{ OwnedFd::from_raw_fd(input.io_file.as_raw_fd()) }),
            CloudHypervisorPmemMode::ReadWrite,
        ),
    ]);
    let mut ch = {
        match CloudHypervisor::start(input.ch_config, Some(pmems)) {
            Ok(ch) => ch,
            Err(e) => {
                return Err(e.into());
            }
        }
    };
    match ch
        .wait_timeout_or_kill(input.ch_timeout)
        .map_err(|_| cloudhypervisor::Error::Wait)
    {
        Ok(WaitIdDataOvertime::NotExited) => {
            panic!("ch not exited");
            // TODO this is real bad
        }
        Ok(WaitIdDataOvertime::Exited { siginfo, .. }) => {
            let info: Siginfo = (&siginfo).into();
            if info != Siginfo::Exited(0) {
                return Err(ch.postmortem(cloudhypervisor::Error::BadExit));
            }
        }
        Ok(WaitIdDataOvertime::ExitedOvertime { .. }) => {
            return Err(ch.postmortem(cloudhypervisor::Error::Overtime));
        }
        Err(e) => {
            return Err(ch.postmortem(e));
        }
    }
    Ok(Output {
        id: input.id,
        io_file: input.io_file,
        ch_logs: ch.into_logs(),
    })
}

pub fn cpuset_all_ht() -> nix::Result<Vec<CpuSet>> {
    let all = sched_getaffinity(nix::unistd::Pid::from_raw(0))?; // pid 0 means us
    let mut ret = vec![];
    let mut i = 0;
    let count = CpuSet::count();
    loop {
        if i > count {
            break;
        }
        if all.is_set(i).unwrap_or(false) && all.is_set(i + 1).unwrap_or(false) {
            let mut c = CpuSet::new();
            c.set(i)?;
            c.set(i + 1)?;
            ret.push(c);
        }
        i += 2;
    }
    Ok(ret)
}

pub fn cpuset(
    core_offset: usize,
    n_workers: usize,
    n_cores_per_worker: usize,
) -> nix::Result<Vec<CpuSet>> {
    // restrict to even offset and even cores per worker to keep workers
    // on separate physical cores
    if core_offset % 2 == 1 {
        return nix::Result::Err(nix::errno::Errno::EINVAL);
    }
    if n_cores_per_worker % 2 == 1 {
        return nix::Result::Err(nix::errno::Errno::EINVAL);
    }
    let all = sched_getaffinity(nix::unistd::Pid::from_raw(0))?; // pid 0 means us
    let mut ret = Vec::with_capacity(n_workers);
    for i in 0..n_workers {
        let mut c = CpuSet::new();
        for j in 0..n_cores_per_worker {
            let k = core_offset + i * n_cores_per_worker + j;
            if !all.is_set(k)? {
                return nix::Result::Err(nix::errno::Errno::ENAVAIL);
            }
            c.set(k)?;
        }
        ret.push(c);
    }
    Ok(ret)
}

pub fn cpuset_range(begin: usize, end: Option<usize>) -> nix::Result<CpuSet> {
    let all = sched_getaffinity(nix::unistd::Pid::from_raw(0))?; // pid 0 means us
    let mut c = CpuSet::new();
    if let Some(end) = end {
        if begin > end {
            return nix::Result::Err(nix::errno::Errno::EINVAL);
        }
        for i in begin..=end {
            if !all.is_set(i)? {
                return nix::Result::Err(nix::errno::Errno::ENAVAIL);
            }
            c.set(i)?;
        }
    } else {
        for i in begin..CpuSet::count() {
            if all.is_set(i)? {
                c.set(i)?;
            }
        }
    }
    Ok(c)
}

fn cpuset_count(x: &CpuSet) -> usize {
    let mut ret = 0;
    for i in 0..CpuSet::count() {
        if x.is_set(i).unwrap_or(false) {
            ret += 1;
        }
    }
    ret
}

pub fn cpuset_replicate(x: &CpuSet) -> Vec<CpuSet> {
    let mut ret = vec![];
    let count = cpuset_count(x);
    ret.resize(count, *x);
    ret
}

pub fn cpusets_string(xs: &[CpuSet]) -> String {
    let n = CpuSet::count();
    xs.iter()
        .map(|c| {
            (0..n)
                .filter(|i| c.is_set(*i).unwrap_or(false))
                .map(|i| format!("{}", i))
                .collect::<Vec<String>>()
                .join(",")
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(feature = "asynk")]
pub mod asynk {
    use super::*;
    use tokio::sync::oneshot;

    type SenderElement = (Input, oneshot::Sender<OutputResult>);

    pub struct Pool {
        sender: Sender<SenderElement>,
        // TODO are these even useful?
        #[allow(dead_code)]
        handles: Vec<JoinHandleT>,
    }

    impl Pool {
        pub fn new(cores: &[CpuSet]) -> Self {
            let (i_s, i_r) = channel::bounded::<SenderElement>(cores.len() * 2);
            let handles: Vec<_> = cores
                .iter()
                .enumerate()
                .map(|(i, c)| spawn_worker(i, *c, i_r.clone()))
                .collect();
            Self {
                sender: i_s,
                handles: handles,
            }
        }

        #[allow(clippy::len_without_is_empty)]
        pub fn len(&self) -> usize {
            self.handles.len()
        }

        pub fn sender(&self) -> &Sender<SenderElement> {
            &self.sender
        }
    }

    fn spawn_worker(
        id: usize,
        cpuset: CpuSet,
        input: Receiver<(Input, oneshot::Sender<OutputResult>)>,
    ) -> JoinHandleT {
        spawn(move || {
            trace!("starting worker {id}");
            sched_setaffinity(nix::unistd::Pid::from_raw(0), &cpuset).unwrap();
            for (msg, output) in input.iter() {
                match output.send(run(msg)) {
                    Ok(_) => {}
                    Err(_) => {
                        // output got disconnected somehow
                        trace!("worker {id} got disconnected");
                        return;
                    }
                }
            }
            trace!("worker {id} shutting down");
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuset_good() {
        // TODO bad test only works on this machine
        const N: usize = 32;
        let set = cpuset(2, 15, 2).unwrap();
        //println!("{:?}", set);
        for x in set {
            let s: String = (0..N)
                .map(|i| if x.is_set(i).unwrap() { '1' } else { '_' })
                .collect();
            println!("{:?}", s);
        }
    }

    #[test]
    fn test_cpuset_bad() {
        // no longer requiring this pedantry
        //let _ = cpuset(1, 1, 2).unwrap_err();  // odd offset
        //let _ = cpuset(0, 1, 1).unwrap_err();  // odd cores per worker
        let _ = cpuset(2, 16, 2).unwrap_err(); // too many cores
    }
}
