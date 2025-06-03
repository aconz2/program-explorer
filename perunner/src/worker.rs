use crossbeam::channel;
use crossbeam::channel::{Receiver, Sender};
use std::os::fd::AsFd;
use std::thread;
use std::thread::{spawn, JoinHandle};
use std::time::Duration;
use waitid_timeout::{Siginfo, WaitIdDataOvertime};

use log::trace;
//use nix;
//use nix::sched::{sched_getaffinity, sched_setaffinity, CpuSet};
use rustix;
use rustix::thread::{sched_getaffinity, sched_setaffinity, CpuSet};

use crate::cloudhypervisor;
use crate::cloudhypervisor::{
    CloudHypervisor, CloudHypervisorConfig, CloudHypervisorLogs, CloudHypervisorPmemMode,
    CloudHypervisorPostMortem, PathBufOrOwnedFd,
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
        sched_setaffinity(None, &cpuset).unwrap();
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
    let pmems = vec![
        (input.image, CloudHypervisorPmemMode::ReadOnly),
        (
            // child process is scoped to this function, we keep input.io_file alive
            PathBufOrOwnedFd::Fd(input.io_file.as_fd().try_clone_to_owned().unwrap()),
            CloudHypervisorPmemMode::ReadWrite,
        ),
    ];
    let mut ch = {
        match CloudHypervisor::start(input.ch_config, pmems) {
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

//pub fn cpuset_all_ht() -> Option<Vec<CpuSet>> {
//    let all = sched_getaffinity(None).ok()?;
//    let mut ret = vec![];
//    let mut i = 0usize;
//    let count = CpuSet::new().count() as usize;
//    loop {
//        if i > count {
//            break;
//        }
//        if all.is_set(i) && all.is_set(i + 1) {
//            let mut c = CpuSet::new();
//            c.set(i);
//            c.set(i + 1);
//            ret.push(c);
//        }
//        i += 2;
//    }
//    Some(ret)
//}

pub fn cpuset(
    core_offset: usize,
    n_workers: usize,
    n_cores_per_worker: usize,
) -> Option<Vec<CpuSet>> {
    // restrict to even offset and even cores per worker to keep workers
    // on separate physical cores
    if core_offset % 2 == 1 {
        return None;
    }
    if n_cores_per_worker % 2 == 1 {
        return None;
    }
    let all = sched_getaffinity(None).ok()?; // None means current thread
    let mut ret = Vec::with_capacity(n_workers);
    for i in 0..n_workers {
        let mut c = CpuSet::new();
        for j in 0..n_cores_per_worker {
            let k = core_offset + i * n_cores_per_worker + j;
            if !all.is_set(k) {
                return None;
            }
            c.set(k);
        }
        ret.push(c);
    }
    Some(ret)
}

pub fn cpuset_range(begin: usize, end: Option<usize>) -> Option<CpuSet> {
    let all = sched_getaffinity(None).ok()?;
    let mut c = CpuSet::new();
    if let Some(end) = end {
        if begin > end {
            return None;
        }
        for i in begin..=end {
            if !all.is_set(i) {
                return None;
            }
            c.set(i);
        }
    } else {
        for i in begin..(all.count() as usize) {
            if all.is_set(i) {
                c.set(i);
            }
        }
    }
    Some(c)
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

    fn spawn_worker(id: usize, cpuset: CpuSet, input: Receiver<SenderElement>) -> JoinHandleT {
        spawn(move || {
            trace!("starting worker {id}");
            sched_setaffinity(None, &cpuset).unwrap();
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
    fn test_cpuset() {
        let xs = cpuset(2, 2, 2).unwrap(); // requires at least 6 cores for this to pass
        assert!(xs.iter().all(|x| !x.is_set(0) && !x.is_set(1)));
        let x = xs[0];
        assert!(x.is_set(2) && x.is_set(3));

        assert!(cpuset(1, 1, 2).is_none()); // odd offset
        assert!(cpuset(0, 1, 1).is_none()); // odd cores per worker
        assert!(cpuset(2, 16, 2).is_none()); // too many workers (on a 32 core machine)
    }

    #[test]
    fn test_cpuset_range() {
        let x = cpuset_range(2, None).unwrap();
        assert!(!x.is_set(0) && !x.is_set(1));
        assert!(x.is_set(2));

        let x = cpuset_range(2, Some(3)).unwrap();
        assert!(!x.is_set(0) && !x.is_set(1));
        assert!(x.is_set(2) && x.is_set(3));
        assert!(!x.is_set(4));
    }
}
