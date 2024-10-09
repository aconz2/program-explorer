use std::os::fd::AsRawFd;
use std::io;
use std::time::Duration;
use std::process::Child;

use libc;
use libc::{c_int,idtype_t,id_t,siginfo_t};
use libc::rusage as rusage_t;

pub use mio_pidfd::PidFd;
use mio::{Poll,Token,Events,Interest};

#[cfg(not(target_os = "linux"))]
compile_error!("wait4 is a linux specific feature");

// TODO only on x86-64 I think
const NR_WAITID: c_int = 247;

// NOTE syscall takes care of only returning -1 and putting the error in errno
// I should probaly use syscalls crate or something to support more arches and then the error
// handling needs to get updated to check against -EINVAL etc directly
extern "C" {
    fn syscall(num: c_int, ...) -> c_int;
}

// int waitid(idtype_t idtype, id_t id, siginfo_t *infop, int options, struct rusage*);
unsafe fn sys_waitid(idtype: idtype_t, id: id_t, infop: &mut siginfo_t, options: c_int, rusagep: &mut rusage_t) -> c_int {
    syscall(NR_WAITID, idtype, id, infop  as *mut _, options, rusagep as *mut _)
}

#[derive(Debug)]
pub enum Error {
    FdConversion,
    PidConversion,
    Errno(i32),
}

pub enum WaitIdData {
    Exited{siginfo: siginfo_t, rusage: rusage_t},
    NotExited,
}

pub enum WaitIdDataOvertime {
    Exited{siginfo: siginfo_t, rusage: rusage_t},
    ExitedOvertime{siginfo: siginfo_t, rusage: rusage_t},
    NotExited,
}

fn waitid(idtype: idtype_t, id: id_t, options: c_int) -> io::Result<WaitIdData> {
    let mut siginfo: siginfo_t = unsafe { std::mem::zeroed() };
    let mut rusage:  rusage_t = unsafe { std::mem::zeroed() };
    let ret = unsafe { sys_waitid(idtype, id, &mut siginfo, options, &mut rusage) };
    match (ret, unsafe { siginfo.si_pid() }) {
        (0,  0) => { Ok(WaitIdData::NotExited) }
        (0,  _) => { Ok(WaitIdData::Exited{ siginfo, rusage }) }
        (_,  _) => { Err(io::Error::last_os_error()) }
    }
}

pub fn waitid_pidfd_exited_nohang<Fd: AsRawFd>(pidfd: &Fd) -> io::Result<WaitIdData> {
    let pidfd: u32 = pidfd.as_raw_fd().try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "pidfd into u32 failed"))?;
    waitid(libc::P_PIDFD, pidfd, libc::WEXITED | libc::WNOHANG)
}

// ugh pid is such a pain, command returns a u32 but pid_t is i32 but id_t is u32...
pub fn waitid_pid_exited_nohang(pid: u32) -> io::Result<WaitIdData> {
    waitid(libc::P_PID, pid, libc::WEXITED | libc::WNOHANG)
}

pub struct PidFdWaiter<'a> {
    poll: Poll,
    pidfd: &'a PidFd,
}

impl<'a> PidFdWaiter<'a> {
    pub fn new(pidfd: &'a mut PidFd) -> io::Result<Self> {
        let poll = Poll::new()?;
        poll.registry()
            .register(pidfd, Token(0), Interest::READABLE)?;
        Ok(Self { poll, pidfd })
    }

    pub fn kill(&mut self, signal: c_int) -> io::Result<()> {
        self.pidfd.kill(signal)
    }

    pub fn wait_timeout(&mut self, duration: Duration) -> io::Result<WaitIdData> {
        let mut events = Events::with_capacity(1);
        self.poll.poll(&mut events, Some(duration))?;
        if events.is_empty() {
            return Ok(WaitIdData::NotExited);
        }
        waitid_pidfd_exited_nohang(self.pidfd)
    }

    pub fn wait_timeout_or_kill(&mut self, duration: Duration) -> io::Result<WaitIdDataOvertime> {
        match self.wait_timeout(duration) {
            Ok(WaitIdData::NotExited) => {
                self.kill(libc::SIGKILL)?;
                match self.wait_timeout(Duration::from_millis(10)) {
                    Ok(WaitIdData::Exited{siginfo, rusage}) => Ok(WaitIdDataOvertime::ExitedOvertime{siginfo, rusage}),
                    Ok(WaitIdData::NotExited)               => Ok(WaitIdDataOvertime::NotExited),
                    Err(e) => Err(e),
                }
            }
            Ok(WaitIdData::Exited{siginfo, rusage}) => Ok(WaitIdDataOvertime::Exited{siginfo, rusage}),
            Err(e) => Err(e),
        }
    }
}

pub trait ChildWaitIdExt {
    fn wait_timeout(&self, duration: Duration) -> io::Result<WaitIdData>;
    fn wait_timeout_or_kill(&self, duration: Duration) -> io::Result<WaitIdDataOvertime>;
}

impl ChildWaitIdExt for Child {
    fn wait_timeout(&self, duration: Duration) -> io::Result<WaitIdData> {
        let mut pidfd = PidFd::new(self)?;
        let mut waiter = PidFdWaiter::new(&mut pidfd)?;
        waiter.wait_timeout(duration)
    }

    /// if you get Ok(WaitIdDataOvertime::NotExited) from this, something has gone pretty wrong and
    /// the child is probably not reaped, idk what else to do though
    // TODO the second 10ms wait for reaping is pretty hacky
    fn wait_timeout_or_kill(&self, duration: Duration) -> io::Result<WaitIdDataOvertime> {
        let mut pidfd = PidFd::new(self)?;
        let mut waiter = PidFdWaiter::new(&mut pidfd)?;
        waiter.wait_timeout_or_kill(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use std::process::Command;

    fn assert_exited(result: io::Result<WaitIdData>, pid: u32, status: i32) {
        match result {
            Ok(WaitIdData::Exited{siginfo, ..}) => {
                assert_eq!(pid, unsafe { siginfo.si_pid().try_into().unwrap() });
                assert_eq!(libc::CLD_EXITED, siginfo.si_code);
                assert_eq!(status, unsafe { siginfo.si_status() });
            },
            Ok(WaitIdData::NotExited) => { panic!("got NotExited and I shouldnt"); }
            Err(err)                  => { panic!("got err={err:?} and I shouldnt"); }
        }
    }

    fn assert_signaled(result: io::Result<WaitIdData>, pid: u32, signal: i32) {
        match result {
            Ok(WaitIdData::Exited{siginfo, ..}) => {
                assert_eq!(pid, unsafe { siginfo.si_pid().try_into().unwrap() });
                assert_eq!(libc::CLD_KILLED, siginfo.si_code);
                assert_eq!(signal, unsafe { siginfo.si_status() });
            }
            Ok(WaitIdData::NotExited) => { panic!("expected an exit"); }
            Err(err)                  => { panic!("got err={err:?} and I shouldnt"); }
        }
    }

    fn assert_not_exited(result: io::Result<WaitIdData>) {
        match result {
            Ok(WaitIdData::NotExited) => {}
            Ok(WaitIdData::Exited{..}) => { panic!("got data and I shouldnt"); }
            Err(err)                   => { panic!("got err={err:?} and I shouldnt"); }
        }
    }

    #[test]
    fn wait_pid_exit() {
        let child = Command::new("sh").arg("-c").arg("exit 11").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let ret = waitid_pid_exited_nohang(child.id());
        assert_exited(ret, child.id(), 11);
    }

    #[test]
    fn wait_pid_exit_doesnt_trigger_for_stop() {
        let mut child = Command::new("sh").arg("-c").arg("sleep 1000").spawn().unwrap();
        let pid = child.id();
        let ret = waitid_pid_exited_nohang(child.id());
        assert_not_exited(ret);
        unsafe {
            let ret = libc::kill(pid.try_into().unwrap(), libc::SIGSTOP);
            assert_eq!(ret, 0);
        }
        std::thread::sleep(Duration::from_millis(5));
        let ret = waitid_pid_exited_nohang(child.id());
        assert_not_exited(ret);
        child.kill().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let ret = waitid_pid_exited_nohang(child.id());
        assert_signaled(ret, child.id(), libc::SIGKILL);
    }

    #[test]
    fn wait_pid_signal() {
        let mut child = Command::new("sh").arg("-c").arg("sleep 1").spawn().unwrap();
        let ret = waitid_pid_exited_nohang(child.id());
        assert_not_exited(ret);
        child.kill().unwrap();
        // delay the second wait a bit so that the signal gets delivered ...
        std::thread::sleep(Duration::from_millis(5));
        let ret = waitid_pid_exited_nohang(child.id());
        assert_signaled(ret, child.id(), libc::SIGKILL);
    }

    #[test]
    fn wait_pidfd_exit() {
        let child = Command::new("sh").arg("-c").arg("exit 11").spawn().unwrap();
        let pidfd = PidFd::new(&child).unwrap();

        std::thread::sleep(Duration::from_millis(10));
        let ret =  waitid_pidfd_exited_nohang(&pidfd);
        assert_exited(ret, child.id(), 11);
    }

    #[test]
    fn wait_pidfd_signal() {
        let mut child = Command::new("sh").arg("-c").arg("sleep 1").spawn().unwrap();
        let pidfd = PidFd::new(&child).unwrap();

        let ret = waitid_pidfd_exited_nohang(&pidfd);
        assert_not_exited(ret);
        child.kill().unwrap();
        // delay the second wait a bit so that the signal gets delivered ...
        std::thread::sleep(Duration::from_millis(5));
        let ret = waitid_pidfd_exited_nohang(&pidfd);
        assert_signaled(ret, child.id(), libc::SIGKILL);
    }

    #[test]
    fn wait_timeout_signal() {
        let mut child = Command::new("sh").arg("-c").arg("sleep 1").spawn().unwrap();
        let mut pidfd = PidFd::new(&child).unwrap();
        let mut waiter = PidFdWaiter::new(&mut pidfd).unwrap();

        let ret = waiter.wait_timeout(Duration::from_millis(1));
        assert_not_exited(ret);
        child.kill().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let ret = waiter.wait_timeout(Duration::from_millis(1));
        assert_signaled(ret, child.id(), libc::SIGKILL);
    }

    /// timeout 1000ms for a process that sleeps for 50ms, we should not wait the whole 1000ms
    #[test]
    fn wait_timeout_exited() {
        let child = Command::new("sh").arg("-c").arg("sleep 0.050; exit 11").spawn().unwrap();
        let mut pidfd = PidFd::new(&child).unwrap();
        let mut waiter = PidFdWaiter::new(&mut pidfd).unwrap();
        let start = Instant::now();
        let ret = waiter.wait_timeout(Duration::from_millis(1000));
        assert_exited(ret, child.id(), 11);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(100));
    }

    #[test]
    fn child_wait_timeout() {
        let child = Command::new("sh").arg("-c").arg("sleep 0.050; exit 11").spawn().unwrap();
        let ret = child.wait_timeout(Duration::from_millis(1000));
        assert_exited(ret, child.id(), 11);
    }

    #[test]
    fn child_wait_timeout_kill() {
        let child = Command::new("sh").arg("-c").arg("sleep 1000").spawn().unwrap();
        let start = Instant::now();
        match child.wait_timeout_or_kill(Duration::from_millis(50)) {
            Ok(WaitIdDataOvertime::ExitedOvertime{siginfo, ..}) => {
                assert_eq!(child.id(), unsafe { siginfo.si_pid().try_into().unwrap() });
                assert_eq!(libc::CLD_KILLED, siginfo.si_code);
                assert_eq!(libc::SIGKILL, unsafe { siginfo.si_status() });
            }
            _ => { panic!("should have gotten exitedovertime"); }
        }
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(100));
    }
}

// bro siginfo_t is so confusing! the linux struct is in sigaction(2) but I think there's crazy
// variation between posix impl's
#[allow(dead_code)]
fn show_siginfo(s: &siginfo_t) {
    unsafe {
    println!("siginfo: {{ si_pid: {}, si_uid: {}, si_signo: {}, si_errno: {}, si_status: {}, si_code: {} }}",
        s.si_pid(), s.si_uid(), s.si_signo, s.si_errno, s.si_status(), s.si_code,
        );
    }
}
