use std::os::fd::AsRawFd;
use std::fs;
use std::fs::File;
use std::path::{Path,PathBuf};
use std::process::{Command,Stdio,Child};
use std::os::unix::net::{UnixListener,UnixStream};
use std::io;

use std::ffi::OsString;
use std::time::Duration;

use rand::distributions::{Alphanumeric, DistString};
use waitid_timeout::{ChildWaitIdExt,WaitIdDataOvertime};
use libc;

#[derive(Debug)]
pub enum Error {
    WorkdirSetup,
    Spawn,
    Socket,
    //Api,
}

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
    log_file: Option<OsString>,
    console_file: Option<OsString>,
    err_file: OsString,
    child: Child,
    #[allow(dead_code)]
    socket_listen: UnixListener,
    socket_stream: UnixStream,
    args: Vec<OsString>,
}

struct TempDir {
    name: OsString
}

impl TempDir {
    fn new<P: AsRef<Path>>(dir: P) -> Option<Self> {
        let rng = Alphanumeric.sample_string(&mut rand::thread_rng(), 8);
        let ret = Self { name: dir.as_ref().join(format!("ch-{rng}")).into() };
        std::fs::create_dir(&ret.name).ok()?;
        Some(ret)
    }

    fn join<O: AsRef<Path>>(&self, other: O) -> PathBuf { self.as_ref().join(other) }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        return Path::new(&self.name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self);
    }
}

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
        // go from /tmp -> /tmp/ch-abcd1234
        let workdir = TempDir::new(config.workdir).ok_or(Error::WorkdirSetup)?;

        let err_file     : OsString = workdir.join("err").into();
        let log_file     : OsString = workdir.join("log").into();
        let console_file : OsString = workdir.join("console").into();

        let (listener, stream) = setup_socket(workdir.join("sock")).ok_or(Error::Socket)?;
        let err_file_h = File::create_new(&err_file).unwrap();

        let mut args = vec![];
        let child = {
            let socket_fd = listener.as_raw_fd();
            let mut x = Command::new(config.bin);
                x.stdin(Stdio::null())
                 .stdout(Stdio::null())
                 .stderr(Stdio::from(err_file_h))
                 .arg("--kernel").arg(config.kernel)
                 .arg("--initramfs").arg(config.initramfs)
                 .arg("--cpus").arg("boot=1")
                 .arg("--memory").arg("size=1024M")
                 .arg("--cmdline").arg("console=hvc0")
                 .arg("--api-socket").arg(format!("fd={socket_fd}"));

            if config.console {
                let f = console_file.to_str().unwrap();
                x.arg("--console").arg(format!("file={f}"));
            }
            if let Some(ref level) = config.log_level {
                x.arg("--log-file").arg(&log_file);
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
            console_file: if config.console { Some(console_file) } else { None },
            child: child,
            socket_listen: listener,
            socket_stream: stream,
            args: args,
        };
        return Ok(ret);
    }

    pub fn api(&mut self, method: &str, command: &str, data: Option<&str>) -> Result<Option<String>, api_client::Error> {
        api_client::simple_api_full_command_and_response(&mut self.socket_stream, method, command, data)
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub fn wait_timeout_or_kill(&mut self, duration: Duration) -> io::Result<WaitIdDataOvertime> {
        self.child.wait_timeout_or_kill(duration)
    }

    pub fn console_file(&self) -> Option<&OsString> {
        self.console_file.as_ref()
    }

    pub fn log_file(&self) -> Option<&OsString> {
        self.log_file.as_ref()
    }

    pub fn workdir(&self) -> &Path {
        self.workdir.as_ref()
    }

    pub fn args(&self) -> &[OsString] {
        self.args.as_slice()
    }

    pub fn err_file(&self) -> &OsString {
        &self.err_file
    }
}

// TODO I don't really like this because we regrab the pidfd and might already be killed etc
impl Drop for CloudHypervisor {
    fn drop(&mut self) {
        let _ = self.wait_timeout_or_kill(Duration::from_millis(5));
    }
}
