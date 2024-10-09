use libc;
use libc::{c_int,idtype_t,id_t,siginfo_t};
use libc::rusage as rusage_t;
use std::os::fd::AsRawFd;

#[cfg(not(target_os = "linux"))]
compile_error!("wait4 is a linux specific feature");

// TODO only on x86-64 I think
const NR_WAITID: c_int = 247;

extern "C" {
    fn syscall(num: c_int, ...) -> c_int;
}


// int waitid(idtype_t idtype, id_t id, siginfo_t *infop, int options, struct rusage*);
unsafe fn sys_waitid(idtype: idtype_t, id: id_t, infop: &mut siginfo_t, options: c_int, rusagep: &mut rusage_t) -> c_int {
    syscall(NR_WAITID, idtype, id, infop  as *mut _, options, rusagep as *mut _)
}

#[derive(Debug)]
pub enum Error {
}

pub enum WaitPidData {
    Exited{siginfo: siginfo_t, rusage: rusage_t},
    NotExited,
}

const NEG_EAGAIN: c_int = -libc::EAGAIN;

fn waitid_pidfd_exited<Rfd: AsRawFd>(pidfd: &Rfd) -> Result<WaitPidData, c_int> {
    let mut siginfo: siginfo_t = unsafe { std::mem::zeroed() };
    let mut rusage:  rusage_t = unsafe { std::mem::zeroed() };
    let ret = unsafe { sys_waitid(libc::P_PIDFD, pidfd.as_raw_fd() as u32, &mut siginfo, libc::WEXITED | libc::WNOHANG, &mut rusage) };
    match ret {
        0             => { Ok(WaitPidData::Exited{ siginfo, rusage }) }
        NEG_EAGAIN    => { Ok(WaitPidData::NotExited) },
        errno         => { Err(errno) }
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

// ugh pid is such a pain, command returns a u32 but pid_t is i32 but id_t is u32...
fn waitid_pid_exited(pid: u32) -> Result<WaitPidData, c_int> {
    let mut siginfo: siginfo_t = unsafe { std::mem::zeroed() };
    let mut rusage:  rusage_t = unsafe { std::mem::zeroed() };
    let ret = unsafe { sys_waitid(libc::P_PID, pid, &mut siginfo, libc::WEXITED | libc::WNOHANG, &mut rusage) };
    // println!("ret = {ret}");
    // show_siginfo(&siginfo);
    match (ret, unsafe { siginfo.si_pid() }) {
        (0,   0) => { Ok(WaitPidData::NotExited) }
        (0,   _) => { Ok(WaitPidData::Exited{ siginfo, rusage }) }
        (err, _)  => { Err(err) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;

    #[test]
    fn waitpid() {
        let mut child = Command::new("sh").arg("-c").arg("exit 11").spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
        match waitid_pid_exited(child.id()) {
            Ok(WaitPidData::Exited{siginfo, ..}) => {
                assert_eq!(child.id(), unsafe { siginfo.si_pid().try_into().unwrap() });
                assert_eq!(libc::CLD_EXITED, siginfo.si_code);
                assert_eq!(11, unsafe { siginfo.si_status() });
            },
            Ok(WaitPidData::NotExited) => { panic!("got NotExited and I shouldnt"); }
            Err(err)                   => { panic!("got err={err} and I shouldnt"); }
        }

        let mut child = Command::new("sh").arg("-c").arg("sleep 1; echo hi").spawn().unwrap();

        match waitid_pid_exited(child.id()) {
            Ok(WaitPidData::NotExited) => {}
            Ok(WaitPidData::Exited{..}) => { panic!("got data and I shouldnt"); }
            Err(err)                    => { panic!("got err={err} and I shouldnt"); }
        }
        child.kill().unwrap();
        // delay the second wait a bit so that the signal gets delivered ...
        std::thread::sleep(std::time::Duration::from_millis(1));
        match waitid_pid_exited(child.id()) {
            Ok(WaitPidData::Exited{siginfo, ..}) => {
                assert_eq!(child.id(), unsafe { siginfo.si_pid().try_into().unwrap() });
                assert_eq!(libc::CLD_KILLED, siginfo.si_code);
                assert_eq!(libc::SIGKILL, unsafe { siginfo.si_status() });
            }
            Ok(WaitPidData::NotExited) => { panic!("expected an exit"); }
            Err(_)                     => { panic!("got err and I shouldnt"); }
        }
    }
}
