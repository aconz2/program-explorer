//use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
//use std::os::unix::net::{UnixListener,UnixStream};
use std::io;
use std::os::fd::OwnedFd;

use std::ffi::OsString;
use std::time::Duration;

use command_fds::{CommandFdExt, FdMapping};
use tempfile::NamedTempFile;
use waitid_timeout::{ChildWaitIdExt, WaitIdDataOvertime};
//use serde::Serialize;

//use api_client;

// todo thiserror
#[derive(Debug, Default)]
pub enum Error {
    #[default]
    Unk,
    TempfileSetup,
    Spawn,
    SpawnWithArgs(Vec<OsString>),
    Socket,
    //Api(api_client::Error),
    Overtime,
    Wait,
    BadExit,
    FdSetup,
}

//impl From<api_client::Error> for Error {
//    fn from(e: api_client::Error) -> Self {
//        Error::Api(e)
//    }
//}
//
#[allow(dead_code)]
#[derive(Clone)]
pub enum ChLogLevel {
    Warn,
    Info,
    Debug,
    Trace,
}

impl TryFrom<&str> for ChLogLevel {
    type Error = io::Error;
    fn try_from(x: &str) -> io::Result<Self> {
        match x {
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CloudHypervisorPmemMode {
    ReadOnly,
    ReadWrite,
}

impl CloudHypervisorPmemMode {
    fn discard_writes(&self) -> &'static str {
        match self {
            CloudHypervisorPmemMode::ReadOnly => "on",
            CloudHypervisorPmemMode::ReadWrite => "off",
        }
    }
}

#[derive(Debug)]
pub enum PathBufOrOwnedFd {
    PathBuf(PathBuf),
    Fd(OwnedFd), // k I think this should rather be BorrowedFd but then we need a lifetime
}

impl PathBufOrOwnedFd {
    pub fn try_clone(&self) -> Option<Self> {
        match self {
            PathBufOrOwnedFd::PathBuf(p) => Some(PathBufOrOwnedFd::PathBuf(p.clone())),
            PathBufOrOwnedFd::Fd(fd) => Some(PathBufOrOwnedFd::Fd(fd.try_clone().ok()?)),
        }
    }
}

//#[derive(Debug)]
//pub enum CloudHypervisorPmem {
//    One([(PathBufOrOwnedFd, CloudHypervisorPmemMode); 1]),
//    Two([(PathBufOrOwnedFd, CloudHypervisorPmemMode); 2]),
//}

#[derive(Clone)]
pub struct CloudHypervisorConfig {
    pub bin: OsString,
    pub kernel: OsString,
    pub initramfs: OsString,
    pub console: bool,
    pub log_level: Option<ChLogLevel>,
    pub keep_args: bool,
    pub event_monitor: bool,
}

pub struct CloudHypervisor {
    log_file: Option<NamedTempFile>,
    con_file: Option<NamedTempFile>,
    err_file: NamedTempFile,
    child: Child,
    //#[allow(dead_code)]
    //socket_listen: UnixListener,
    //socket_stream: UnixStream,
    args: Vec<OsString>,
    //pidfd:
}

pub struct CloudHypervisorLogs {
    pub log_file: Option<NamedTempFile>,
    pub con_file: Option<NamedTempFile>,
    pub err_file: Option<NamedTempFile>,
}

pub struct CloudHypervisorPostMortem {
    pub error: Error,
    pub logs: CloudHypervisorLogs,
    pub args: Option<Vec<OsString>>,
}

impl From<Error> for CloudHypervisorPostMortem {
    fn from(e: Error) -> Self {
        Self {
            error: e,
            args: None,
            logs: CloudHypervisorLogs {
                log_file: None,
                con_file: None,
                err_file: None,
            },
        }
    }
}

//fn rand_path_prefix(prefix: &str) -> PathBuf {
//    use rand::distributions::{Alphanumeric,DistString};
//    let rng = Alphanumeric.sample_string(&mut rand::thread_rng(), 8);
//    std::env::temp_dir().join(format!("{}{}", prefix, rng))
//}

//fn setup_socket<P: AsRef<Path>>(path: P) -> Option<(UnixListener, UnixStream)> {
//    let _ = fs::remove_file(&path);
//    let listener = UnixListener::bind(&path).ok()?;
//    let stream = UnixStream::connect(&path).ok()?;
//    // clear FD_CLOEXEC
//    unsafe {
//        let ret = libc::fcntl(listener.as_raw_fd(), libc::F_SETFD, 0);
//        if ret < 0 { return None; }
//    }
//    let _ = fs::remove_file(&path); // unlink since we've already connected
//    Some((listener, stream))
//}

// but still no fexecve to actually call ch from fd :(

impl CloudHypervisor {
    pub fn start(
        config: CloudHypervisorConfig,
        pmems: Vec<(PathBufOrOwnedFd, CloudHypervisorPmemMode)>,
    ) -> Result<Self, Error> {
        let err_file = NamedTempFile::with_prefix("err-").map_err(|_| Error::TempfileSetup)?;
        let log_file = NamedTempFile::with_prefix("log-").map_err(|_| Error::TempfileSetup)?;
        let con_file = NamedTempFile::with_prefix("con-").map_err(|_| Error::TempfileSetup)?;

        let mut child_fd_cur = 3;
        let mut fd_mappings = vec![];
        let pmem_paths_modes = pmems
            .into_iter()
            .map(|(path_or_fd, mode)| match path_or_fd {
                PathBufOrOwnedFd::PathBuf(p) => (p, mode),
                PathBufOrOwnedFd::Fd(fd) => {
                    let child_fd = child_fd_cur;
                    child_fd_cur += 1;
                    fd_mappings.push(FdMapping {
                        parent_fd: fd,
                        child_fd,
                    });
                    (PathBuf::from(format!("/dev/fd/{child_fd}")), mode)
                }
            })
            .collect::<Vec<_>>();

        let mut args = vec![];
        let child = {
            //let socket_fd = listener.as_raw_fd();
            let mut x = Command::new(config.bin);
            x.stdin(Stdio::null())
             .stdout(Stdio::null())
             .stderr(Stdio::from(err_file.reopen().unwrap()))
             .arg("--kernel").arg(config.kernel)
             .arg("--initramfs").arg(config.initramfs)
             .arg("--cpus").arg("boot=1")
             .arg("--memory").arg("size=1024M")
             // almalinux 9.5 doesn't have landlock enabled in the kernel config ...
             // zgrep -h "^CONFIG_SECURITY_LANDLOCK=" "/boot/config-$(uname -r)"
             //.arg("--landlock")
             //
             //.arg("--pvpanic")
             //.arg("--api-socket").arg(format!("fd={socket_fd}"))
             ;

            // NOTE: using --cmdline console=hvc0 --console off causes the guest
            //       to do bad things (guessing because its like a write to a bad "fd"?)
            //             --cmdline console=hvc0 --console null does work though
            if config.console {
                x.arg("--cmdline")
                    .arg("console=hvc0")
                    .arg("--console")
                    .arg(format!("file={:?}", con_file.path()));
            } else {
                x.arg("--console").arg("off");
            }
            if config.event_monitor {
                x.arg("--event-monitor").arg("fd=2");
            }
            if let Some(ref level) = config.log_level {
                x.arg("--log-file").arg(log_file.path());
                match level {
                    ChLogLevel::Warn => {}
                    ChLogLevel::Info => {
                        x.arg("-v");
                    }
                    ChLogLevel::Debug => {
                        x.arg("-vv");
                    }
                    ChLogLevel::Trace => {
                        x.arg("-vvv");
                    }
                }
            }

            if !pmem_paths_modes.is_empty() {
                x.arg("--pmem");
            }
            for (path, mode) in pmem_paths_modes.iter() {
                x.arg(format!("file={:?},discard_writes={}", path, mode.discard_writes()));
            }
            if config.keep_args {
                args.extend(x.get_args().map(|x| x.into()));
            }
            x.fd_mappings(fd_mappings).map_err(|_| Error::FdSetup)?;
            x.spawn().map_err(|_| Error::SpawnWithArgs(args.clone()))?
        };

        let ret = CloudHypervisor {
            err_file: err_file,
            log_file: if config.log_level.is_some() {
                Some(log_file)
            } else {
                None
            },
            con_file: if config.console { Some(con_file) } else { None },
            child: child,
            //socket_listen: listener,
            //socket_stream: stream,
            args: args,
        };
        Ok(ret)
    }

    //pub fn api(&mut self, method: &str, command: &str, data: Option<&str>) -> Result<Option<String>, Error> {
    //    Ok(api_client::simple_api_full_command_and_response(&mut self.socket_stream, method, command, data)?)
    //}
    //
    //fn add_pmem<P: AsRef<Path>>(&mut self, file: P, discard_writes: bool) -> Result<Option<String>, Error> {
    //    #[derive(Serialize)]
    //    struct AddPmem<'a> {
    //        file: &'a Path,
    //        discard_writes: bool
    //    }
    //    let data = serde_json::to_string(&AddPmem { file: file.as_ref(), discard_writes }).unwrap();
    //    self.api("PUT", "vm.add-pmem", Some(&data))
    //}
    //
    //pub fn add_pmem_ro<P: AsRef<Path>>(&mut self, file: P) -> Result<Option<String>, Error> {
    //    self.add_pmem(file, true)
    //}
    //
    //pub fn add_pmem_rw<P: AsRef<Path>>(&mut self, file: P) -> Result<Option<String>, Error> {
    //    self.add_pmem(file, false)
    //}

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub fn wait_timeout_or_kill(&mut self, duration: Duration) -> io::Result<WaitIdDataOvertime> {
        self.child.wait_timeout_or_kill(duration)
    }

    pub fn console_file(&self) -> Option<&NamedTempFile> {
        self.con_file.as_ref()
    }

    pub fn log_file(&self) -> Option<&NamedTempFile> {
        self.log_file.as_ref()
    }

    pub fn err_file(&self) -> &NamedTempFile {
        &self.err_file
    }

    pub fn args(&self) -> &[OsString] {
        self.args.as_slice()
    }

    pub fn postmortem(mut self, e: Error) -> CloudHypervisorPostMortem {
        let _ = self.kill();
        CloudHypervisorPostMortem {
            error: e,
            args: Some(self.args),
            logs: CloudHypervisorLogs {
                log_file: self.log_file,
                con_file: self.con_file,
                err_file: Some(self.err_file),
            },
        }
    }

    pub fn into_logs(mut self) -> CloudHypervisorLogs {
        let _ = self.kill();
        CloudHypervisorLogs {
            log_file: self.log_file,
            con_file: self.con_file,
            err_file: Some(self.err_file),
        }
    }
}
