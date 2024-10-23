use std::os::fd::AsRawFd;
use std::fs;
use std::path::{Path};
use std::process::{Command,Stdio,Child};
use std::os::unix::net::{UnixListener,UnixStream};
use std::io;

use std::ffi::OsString;
use std::time::Duration;

use tempfile::{TempDir,NamedTempFile};
use waitid_timeout::{ChildWaitIdExt,WaitIdDataOvertime};
use serde::Serialize;
use libc;
use api_client;

#[derive(Debug)]
pub enum Error {
    WorkdirSetup,
    TempfileSetup,
    Spawn,
    Socket,
    Api(api_client::Error),
    Overtime,
    Wait,
}

impl From<api_client::Error> for Error {
    fn from(e: api_client::Error) -> Self { Error::Api(e) }
}

#[allow(dead_code)]
pub enum ChLogLevel {
    Warn,
    Info,
    Debug,
    Trace,
}

pub struct CloudHypervisorConfig {
    pub workdir: OsString,
    pub bin: OsString,
    pub kernel: OsString,
    pub initramfs: OsString,
    pub console: bool,
    pub log_level: Option<ChLogLevel>,
    pub keep_args: bool,
}

pub struct CloudHypervisor {
    #[allow(dead_code)]
    workdir: TempDir,
    log_file: Option<NamedTempFile>,
    console_file: Option<NamedTempFile>,
    err_file: NamedTempFile,
    child: Child,
    #[allow(dead_code)]
    socket_listen: UnixListener,
    socket_stream: UnixStream,
    args: Vec<OsString>,
    //pidfd:
}

// TODO kinda weird b/c if ch doesn't even start this is useless
// pub struct CloudHypervisorPostMortem {
//     pub error: Error,
//     pub workdir: TempDir,
//     pub log_file: Option<OsString>,
//     pub console_file: Option<OsString>,
//     pub err_file: OsString,
//     pub args: Vec<OsString>,
// }

// struct TempDir {
//     name: OsString
// }
//
// impl TempDir {
//     fn new<P: AsRef<Path>>(dir: P) -> Option<Self> {
//         let rng = Alphanumeric.sample_string(&mut rand::thread_rng(), 8);
//         let ret = Self { name: dir.as_ref().join(format!("ch-{rng}")).into() };
//         std::fs::create_dir(&ret.name).ok()?;
//         Some(ret)
//     }
//
//     fn join<O: AsRef<Path>>(&self, other: O) -> PathBuf { self.as_ref().join(other) }
// }
//
// impl AsRef<Path> for TempDir {
//     fn as_ref(&self) -> &Path {
//         return Path::new(&self.name)
//     }
// }
//
// impl Drop for TempDir {
//     fn drop(&mut self) {
//         let _ = std::fs::remove_dir_all(self);
//     }
// }

fn setup_socket<P: AsRef<Path>>(path: P) -> Option<(UnixListener, UnixStream)> {
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    let stream = UnixStream::connect(&path).ok()?;
    // clear FD_CLOEXEC
    unsafe {
        let ret = libc::fcntl(listener.as_raw_fd(), libc::F_SETFD, 0);
        if ret < 0 { return None; }
    }
    return Some((listener, stream));
}

impl CloudHypervisor {

    pub fn start(config: CloudHypervisorConfig) -> Result<Self, Error> {
        let workdir = TempDir::with_prefix_in("ch-", config.workdir)
            .map_err(|_| Error::WorkdirSetup)?;

        let err_file = NamedTempFile::with_prefix_in("err", &workdir)
            .map_err(|_| Error::TempfileSetup)?;
        let log_file = NamedTempFile::with_prefix_in("log", &workdir)
            .map_err(|_| Error::TempfileSetup)?;
        let con_file = NamedTempFile::with_prefix_in("con", &workdir)
            .map_err(|_| Error::TempfileSetup)?;

        let (listener, stream) = setup_socket(workdir.path().join("sock")).ok_or(Error::Socket)?;

        let mut args = vec![];
        let child = {
            let socket_fd = listener.as_raw_fd();
            let mut x = Command::new(config.bin);
            x.stdin(Stdio::null())
             .stdout(Stdio::null())
             .stderr(Stdio::from(err_file.reopen().unwrap()))
             .arg("--kernel").arg(config.kernel)
             .arg("--initramfs").arg(config.initramfs)
             .arg("--cpus").arg("boot=1")
             .arg("--memory").arg("size=1024M")
             .arg("--cmdline").arg("console=hvc0")
             .arg("--api-socket").arg(format!("fd={socket_fd}"));

            if config.console {
                x.arg("--console").arg(format!("file={:?}", con_file.path()));
            }
            if let Some(ref level) = config.log_level {
                x.arg("--log-file").arg(log_file.path());
                match level {
                    ChLogLevel::Warn  => { }
                    ChLogLevel::Info  => { x.arg("-v"); }
                    ChLogLevel::Debug => { x.arg("-vv"); }
                    ChLogLevel::Trace => { x.arg("-vvv"); }
                }
            }
            if config.keep_args {
                args.extend(x.get_args().map(|x| x.into()));
            }
            x.spawn().map_err(|_| Error::Spawn)?
        };

        let ret = CloudHypervisor {
            workdir: workdir,
            err_file: err_file,
            log_file:     if config.log_level.is_some() { Some(log_file) } else { None },
            console_file: if config.console { Some(con_file) } else { None },
            child: child,
            socket_listen: listener,
            socket_stream: stream,
            args: args,
        };
        Ok(ret)
    }

    pub fn api(&mut self, method: &str, command: &str, data: Option<&str>) -> Result<Option<String>, Error> {
        Ok(api_client::simple_api_full_command_and_response(&mut self.socket_stream, method, command, data)?)
    }

    fn add_pmem<P: AsRef<Path>>(&mut self, file: P, discard_writes: bool) -> Result<Option<String>, Error> {
        #[derive(Serialize)]
        struct AddPmem<'a> {
            file: &'a Path,
            discard_writes: bool
        }
        let data = serde_json::to_string(&AddPmem { file: file.as_ref(), discard_writes }).unwrap();
        self.api("PUT", "vm.add-pmem", Some(&data))
    }

    pub fn add_pmem_ro<P: AsRef<Path>>(&mut self, file: P) -> Result<Option<String>, Error> {
        self.add_pmem(file, true)
    }

    pub fn add_pmem_rw<P: AsRef<Path>>(&mut self, file: P) -> Result<Option<String>, Error> {
        self.add_pmem(file, false)
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub fn wait_timeout_or_kill(&mut self, duration: Duration) -> io::Result<WaitIdDataOvertime> {
        self.child.wait_timeout_or_kill(duration)
    }

    pub fn console_file(&self) -> Option<&NamedTempFile> {
        self.console_file.as_ref()
    }

    pub fn log_file(&self) -> Option<&NamedTempFile> {
        self.log_file.as_ref()
    }

    pub fn err_file(&self) -> &NamedTempFile {
        &self.err_file
    }

    pub fn workdir(&self) -> &Path {
        self.workdir.as_ref()
    }

    pub fn args(&self) -> &[OsString] {
        self.args.as_slice()
    }

    // pub fn postmortem(self, e: Error) -> CloudHypervisorPostMortem {
    //     CloudHypervisorPostMortem {
    //         error: e,
    //         workdir: self.workdir,
    //         log_file: self.log_file,
    //         console_file: self.console_file,
    //         err_file: self.err_file,
    //         args: self.args,

    //     }
    // }
}

// TODO I don't really like this because we regrab the pidfd and might already be killed etc
impl Drop for CloudHypervisor {
    fn drop(&mut self) {
        // TODO redo this
        //let _ = self.wait_timeout_or_kill(Duration::from_millis(5));
    }
}
