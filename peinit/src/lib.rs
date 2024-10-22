use serde::{Serialize, Deserialize};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    // https://github.com/opencontainers/runtime-spec/blob/main/config.md
    // fully filled in config.json ready to pass to crun
    pub oci_runtime_config: String,
    pub uid_gid: u32,
    pub timeout: Duration,
    pub nids: u32,
    pub stdin: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Response {
    pub status  : ExitKind,
    pub panic   : Option<String>,
    pub siginfo : Option<SigInfoRedux>,
    pub rusage  : Option<Rusage>,
}

#[derive(Debug, Serialize, Deserialize,Default)]
pub enum ExitKind {
    Ok,
    Panic,
    Overtime,
    Abnormal,
    #[default]
    Unk,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TimeVal {
    pub sec: i64,
    pub usec: i64, // this is susec_t which is signed for some reason
}

// this is a portion of siginfo interpreted from waitid(2)
#[derive(Debug, Serialize, Deserialize)]
pub enum SigInfoRedux {
    Exited(i32), // this is really i8/u8 but
    Killed(i32),
    Dumped(i32),
    Stopped(i32),
    Trapped(i32),
    Continued(i32),
    Unk{status: i32, code: i32},
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Rusage {
    pub ru_utime    : TimeVal,     /* user CPU time used */
    pub ru_stime    : TimeVal,     /* system CPU time used */
    pub ru_maxrss   : i64,         /* maximum resident set size */
    pub ru_ixrss    : i64,         /* integral shared memory size */
    pub ru_idrss    : i64,         /* integral unshared data size */
    pub ru_isrss    : i64,         /* integral unshared stack size */
    pub ru_minflt   : i64,         /* page reclaims (soft page faults) */
    pub ru_majflt   : i64,         /* page faults (hard page faults) */
    pub ru_nswap    : i64,         /* swaps */
    pub ru_inblock  : i64,         /* block input operations */
    pub ru_oublock  : i64,         /* block output operations */
    pub ru_msgsnd   : i64,         /* IPC messages sent */
    pub ru_msgrcv   : i64,         /* IPC messages received */
    pub ru_nsignals : i64,         /* signals received */
    pub ru_nvcsw    : i64,         /* voluntary context switches */
    pub ru_nivcsw   : i64,         /* involuntary context switches */
}

// impl From<libc::c_int> for Status {
//     fn from(status: libc::c_int) -> Self {
//         Self {
//             status: if libc::WIFEXITED(status) { Some(libc::WEXITSTATUS(status) as u8) } else { None },
//             signal: if libc::WIFSIGNALED(status) { Some(libc::WTERMSIG(status)) } else { None },
//         }
//     }
// }

impl From<libc::siginfo_t> for SigInfoRedux {
    fn from(siginfo: libc::siginfo_t) -> Self {
        let status = unsafe { siginfo.si_status() }; // why is this unsafe?
        match siginfo.si_code {
            libc::CLD_EXITED => SigInfoRedux::Exited(status),
            libc::CLD_KILLED => SigInfoRedux::Killed(status),
            libc::CLD_DUMPED => SigInfoRedux::Dumped(status),
            libc::CLD_TRAPPED => SigInfoRedux::Trapped(status),
            libc::CLD_CONTINUED => SigInfoRedux::Continued(status),
            _ => SigInfoRedux::Unk{code: siginfo.si_code, status: status},
        }
    }
}

impl From<libc::timeval> for TimeVal {
    fn from(tv: libc::timeval) -> Self {
        TimeVal {
            sec:  tv.tv_sec,
            usec: tv.tv_usec,
        }
    }
}

impl From<libc::rusage> for Rusage {
    fn from(u: libc::rusage) -> Self {
        Rusage {
            ru_utime    : u.ru_utime.into(),
            ru_stime    : u.ru_stime.into(),
            ru_maxrss   : u.ru_maxrss,
            ru_ixrss    : u.ru_ixrss,
            ru_idrss    : u.ru_idrss,
            ru_isrss    : u.ru_isrss,
            ru_minflt   : u.ru_minflt,
            ru_majflt   : u.ru_majflt,
            ru_nswap    : u.ru_nswap,
            ru_inblock  : u.ru_inblock,
            ru_oublock  : u.ru_oublock,
            ru_msgsnd   : u.ru_msgsnd,
            ru_msgrcv   : u.ru_msgrcv,
            ru_nsignals : u.ru_nsignals,
            ru_nvcsw    : u.ru_nvcsw,
            ru_nivcsw   : u.ru_nivcsw,
        }
    }
}
