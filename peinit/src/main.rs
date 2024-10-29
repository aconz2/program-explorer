use std::fs;
use std::fs::File;
use std::process::{Stdio, Command};
use std::io::{Seek,Read,Write,SeekFrom};
use std::os::fd::{AsRawFd,FromRawFd};
//use std::os::unix::process::CommandExt;
use std::ffi::{CStr,OsStr,CString};
use std::path::Path;
use std::io;
use std::time::Instant;

use peinit::{Config,Response,ExitKind};
use waitid_timeout::{PidFdWaiter,PidFd,WaitIdDataOvertime};

use byteorder::{ReadBytesExt,WriteBytesExt,LE};
use libc;
use bincode;

const INOUT_DEVICE: &str = "/dev/pmem1";

#[derive(Debug)]
enum Error {
    OpenDev,
    InotifyInit,
    InotifyAddWatch,
    InotifyRead,
}

fn size_of<T>(_t: T) -> usize { return std::mem::size_of::<T>(); }

fn sha2_hex(buf: &[u8]) -> String {
    use sha2::{Sha256,Digest};
    use base16ct;
    let hash = Sha256::digest(&buf);
    base16ct::lower::encode_string(&hash)
}

fn exit() {
    unsafe {
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    std::process::exit(1);
}

fn write_panic_response(message: &str) {
    let response = Response {
        status: ExitKind::Panic,
        panic: Some(message.into()),
        ..Default::default()
    };

    fn try_(result: io::Result<()>) {
        if result.is_err() {
            println!("V got an error {result:?}");
        }
    }
    match bincode::serialize(&response) {
        Ok(ser) => {
            println!("V panic response bytes len {}", ser.len());
            let f = File::create(INOUT_DEVICE);
            if f.is_err() {
                println!("V couldnt open inout device!");
                return
            }
            let mut f = f.unwrap();
            try_(f.write_u32::<LE>(0));
            try_(f.write_u32::<LE>(ser.len().try_into().unwrap()));
            try_(f.write_all(&ser));
            try_(f.sync_data());
            println!("V wrote panic response");
        }
        Err(e) => {
            println!("V couldnt serialize panic response {e:?}");
        }
    }
}

fn setup_panic() {
    std::panic::set_hook(Box::new(|p| {
        //if let Some(s) = p.payload().downcast_ref::<&str>() {
        //    write_panic_response(s);
        //} else if let Some(s) = p.payload().downcast_ref::<String>() {
        //    write_panic_response(&s);
        //} else {
        //    write_panic_response("unknown panic");
        //}
        write_panic_response(&format!("{}", p));
        exit();
    }));
}

fn check_libc(ret: i32) -> io::Result<()> {
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn mount(source: &CStr, target: &CStr, filesystem: Option<&CStr>, flags: libc::c_ulong, data: Option<&CStr>) -> io::Result<()> {
    let filesystem = filesystem.map_or(std::ptr::null(), |x| x.as_ptr());
    let data = data.map_or(std::ptr::null(), |x| x.as_ptr() as *const libc::c_void);
    check_libc(unsafe { libc::mount(source.as_ptr(), target.as_ptr(), filesystem, flags, data) })
}

fn unshare(flags: libc::c_int) -> io::Result<()> { check_libc(unsafe { libc::unshare(flags) }) }
fn chdir(dir: &CStr) -> io::Result<()> { check_libc(unsafe { libc::chdir(dir.as_ptr()) }) }
fn chroot(dir: &CStr) -> io::Result<()> { check_libc(unsafe { libc::chroot(dir.as_ptr()) }) }
fn mkdir(dir: &CStr, mode: libc::mode_t) -> io::Result<()> { check_libc(unsafe { libc::mkdir(dir.as_ptr(), mode) }) }
fn chmod(path: &CStr, mode: libc::mode_t) -> io::Result<()> { check_libc(unsafe { libc::chmod(path.as_ptr(), mode) }) }

fn parent_rootfs() -> io::Result<()> {
    let pivot_dir = c"/abc";
    unshare(libc::CLONE_NEWNS)?;
    mount(c"/", pivot_dir, None, libc::MS_BIND | libc::MS_REC | libc::MS_SILENT, None)?;
    chdir(pivot_dir)?;
    mount(pivot_dir, c"/", None, libc::MS_MOVE | libc::MS_SILENT, None)?;
    chroot(c".")?;
    Ok(())
}

fn fstatat_exists(file: &File, name: &std::ffi::CStr) -> bool {
    let mut buf: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::fstatat(file.as_raw_fd(), name.as_ptr(), &mut buf, 0) };
    ret == 0
}

fn wait_for_pmem(files: &[&std::ffi::CStr]) -> Result<(), Error> {
    let dev_file = unsafe {
        let ret = libc::open(c"/dev".as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
        if ret < 0 {
            return Err(Error::OpenDev);
        }
        File::from_raw_fd(ret)
    };

    if files.iter().all(|file| fstatat_exists(&dev_file, file)) {
        return Ok(());
    }

    let inotify_file: File = unsafe {
        println!("V using inotify");
        let fd = libc::inotify_init1(libc::IN_CLOEXEC);
        if fd < 0 {
            return Err(Error::InotifyInit);
        }

        File::from_raw_fd(fd)
    };
    let ret = unsafe { libc::inotify_add_watch(inotify_file.as_raw_fd(), c"/dev".as_ptr(), libc::IN_CREATE) };
    if ret < 0 {
        return Err(Error::InotifyAddWatch);
    }
    let events: [libc::inotify_event; 4] = unsafe { std::mem::zeroed() };

    for file in files.iter() {
        loop {
            if fstatat_exists(&dev_file, file) {
                println!("pmem exists");
                break;
            } else {
                // check one more time before blocking on reading inotify in case it got added
                // after we stat'd but before we created the watcher. idk this still isn't atomic
                // though
                if fstatat_exists(&dev_file, file) {
                    println!("pmem exists");
                    break;
                } else {
                    let ret = unsafe { libc::read(inotify_file.as_raw_fd(), events.as_ptr() as *mut libc::c_void, size_of(events)) };
                    if ret < 0 {
                        return Err(Error::InotifyRead);
                    }
                    // we don't bother checking what the events are, just trying again
                }
            }
        }
    }
    Ok(())
}

// kinda intended to do this in-process but learned you can't do unshare(CLONE_NEWUSER) in a
// threaded program
fn unpack_input(archive: &str, dir: &str) -> Config {
    let mut f = File::open(&archive).unwrap();
    let archive_size = f.read_u32::<LE>().unwrap();
    let config_size = f.read_u32::<LE>().unwrap();

    println!("V archive_size: {archive_size} config_size: {config_size}");

    let config_bytes = {
        // let mut buf: Vec::<u8> = Vec::with_capacity(config_size as usize); // todo uninit
        // f.read_exact(buf.spare_capacity_mut()).unwrap();
        // buf.set_len(config_size as usize);
        let mut buf = vec![0; config_size as usize];
        f.read_exact(buf.as_mut_slice()).unwrap();
        buf
    };

    if true {
        let hash_hex = sha2_hex(&config_bytes);
        println!("V config_bytes len {} {}", config_bytes.len(), hash_hex);
    }
    let config: Config = bincode::deserialize(config_bytes.as_slice()).unwrap();


    let offset = f.stream_position().unwrap();
    // println!("read offset and archive size from as config_size={config_size} archive_size={archive_size} offset={offset}");

    let ret = Command::new("/bin/pearchive")
    //let ret = Command::new("strace").arg("-e").arg("mmap").arg("/bin/pearchive")
        .arg("unpackdev")
        .arg(archive)
        .arg(dir)
        .arg(format!("{offset}"))
        .arg(format!("{archive_size}"))
        .status()
        .unwrap()
        .code()
        .expect("pearchive unpackdev had no status");
    assert!(ret == 0, "pearchive unpackdev failed with status {}", ret);

    config
}

fn pack_output<P: AsRef<OsStr> + AsRef<Path>>(response: &Response, dir: P, archive: P) {
    Command::new("/bin/busybox").arg("ls").arg("-lh").arg("/run/output").spawn().unwrap().wait().unwrap();
    let mut f = File::create(&archive).unwrap();
    let response_bytes = bincode::serialize(response).unwrap();

    if true {
        use sha2::{Sha256,Digest};
        use base16ct;
        let hash = Sha256::digest(&response_bytes);
        let hash_hex = base16ct::lower::encode_string(&hash);
        println!("V response_bytes len {} {}", response_bytes.len(), hash_hex);
    }

    let response_size: u32 = response_bytes.len().try_into().unwrap();
    f.seek(SeekFrom::Start(4)).unwrap();  // packdev fills in the <archive size>
    f.write_u32::<LE>(response_size).unwrap();
    f.write_all(&response_bytes).unwrap();
    let offset = f.stream_position().unwrap();

    //let ret = Command::new("strace").arg("/bin/pearchive")
    let ret = Command::new("/bin/pearchive")
        .arg("packdev")
        .arg(dir)
        .arg(archive)
        .arg(format!("{offset}"))
        .status()
        .unwrap()
        .code()
        .expect("pearchive packdev had no status");
    assert!(ret == 0, "pearchive packdev failed with status {}", ret);
}

fn read_n_or_str_error<P: AsRef<Path> + std::fmt::Display>(path: P, n: usize) -> String {
    match File::open(&path) {
        Err(e) => format!("error opening file {} {:?}", path, e),
        Ok(f) => {
            let mut buf = String::with_capacity(n);
            match f.take(n as u64).read_to_string(&mut buf) {
                Ok(_) => buf,
                Err(e) => format!("error reading file {} {:?}", path, e)
            }
        }
    }
}

fn cat_file_if_exists<P: AsRef<Path>>(name: &str, file: P) {
    if let Ok(mut f ) = File::open(file) {
        println!("=== {name} ===");
        let _ = io::copy(&mut f, &mut io::stdout());
        println!("======");
    }
}

fn run_container(config: &Config) -> io::Result<WaitIdDataOvertime> {
    let outfile = File::create_new("/run/output/stdout").unwrap();
    let errfile = File::create_new("/run/output/stderr").unwrap();
    let run_input = Path::new("/run/input");
    let stdin: Stdio = config.stdin.clone().map(|x| {
        // TODO this is annoying
        let p = match run_input.join(x).canonicalize() {
            Ok(p) => { p },
            Err(_) => { return None; },
        };
        if !p.starts_with(run_input) {
            // println!("V warn stdin traversal avoided");
            return None;
        }
        match File::open(p) {
            Ok(f) => { Some(Stdio::from(f)) }
            Err(_) => { None }
        }
    }).flatten().unwrap_or_else(|| Stdio::null());

    let start = Instant::now();
    let mut cmd = if config.strace {
        Command::new("/bin/strace")
    } else {
        Command::new("/bin/crun")
    };
    if config.strace {
        cmd.arg("-e").arg("write,openat,unshare,clone,clone3").arg("-f").arg("-o").arg("/run/crun.strace").arg("--decode-pids=comm").arg("/bin/crun");
    }
    if config.crun_debug {
        cmd.arg("--debug").arg("--log=/run/crun.log");
    }
    cmd
        .arg("run")
        .arg("-b") // --bundle
        .arg("/run/bundle")
        .arg("-d") // --detach
        .arg("--pid-file=/run/pid")
        .arg("cid-1234")
        .stdout(Stdio::from(outfile))
        .stderr(Stdio::from(errfile))
        .stdin(stdin);

    let exit_status = cmd
        .spawn()
        .unwrap()
        .wait()
        .unwrap();

    let elapsed = start.elapsed();
    println!("V crun ran in {elapsed:?}");

    if config.strace { cat_file_if_exists("crun.strace", "/run/crun.strace"); }
    if config.crun_debug {cat_file_if_exists("crun.log", "/run/crun.log"); }

    if !exit_status.success() {
        // println!("V crun stdout");
        // io::copy(&mut File::open("/run/output/stdout").unwrap(), &mut io::stdout());
        // println!("V crun stderr");
        // io::copy(&mut File::open("/run/output/stderr").unwrap(), &mut io::stdout());
        //let stderr = fs::read_to_string("/run/output/stderr").unwrap();

        let stderr = read_n_or_str_error("/run/output/stderr", 2000);
        panic!("crun unclean exit status {:?} {}", exit_status, stderr);
    }
    // we wait on crun since it should run to completion and leave the pid in pidfd

    //Command::new("busybox").arg("ls").arg("/run").spawn().unwrap().wait().unwrap();
    let pid = fs::read_to_string("/run/pid").unwrap().parse::<i32>().unwrap();

    // Command::new("/bin/busybox").arg("cat").arg("/run/crun/cid-1234/status").spawn().unwrap().wait().unwrap();
    // this can verify the Uid/Gid is not 0 0 0 0 DOES NOT WORK WITH STRACE
    // Command::new("/bin/busybox").arg("cat").arg(format!("/proc/{}/status", pid)).spawn().unwrap();
    let mut pidfd = PidFd::open(pid, 0).unwrap();
    let mut waiter = PidFdWaiter::new(&mut pidfd).unwrap();

    waiter.wait_timeout_or_kill(config.timeout)
}

fn main() {
    setup_panic();

    parent_rootfs();

    { // initial mounts
        mount(c"none", c"/proc",          Some(c"proc"),     libc::MS_SILENT, None).unwrap();
        mount(c"none", c"/sys/fs/cgroup", Some(c"cgroup2"),  libc::MS_SILENT, None).unwrap();
        mount(c"none", c"/dev",           Some(c"devtmpfs"), libc::MS_SILENT, None).unwrap();
        mount(c"none", c"/run/output",    Some(c"tmpfs"),    libc::MS_SILENT, Some(c"size=2M,mode=777")).unwrap();

        // the umask 022 means mkdir creates with 755, mkdir(1) does a mkdir then chmod. we could also
        // have set umask
        mkdir(c"/run/output/dir", 0o777).unwrap();
        chmod(c"/run/output/dir", 0o777).unwrap();
    }

    //              rootfs    input/output
    wait_for_pmem(&[c"pmem0", c"pmem1"]).unwrap();

    // mount index
    mount(c"/dev/pmem0", c"/mnt/index", Some(c"squashfs"), libc::MS_SILENT, None).unwrap();

    let config = unpack_input(INOUT_DEVICE, "/run/input");

    let rootfs_dir = CString::new(format!("/mnt/index/{}", config.rootfs_dir)).unwrap();
    let _ = Command::new("busybox").arg("ls").arg("-ln").arg("/mnt/index").spawn().unwrap().wait();
    mount(&rootfs_dir, c"/mnt/rootfs", None, libc::MS_SILENT | libc::MS_BIND, None).unwrap();
    let _ = Command::new("busybox").arg("ls").arg("-ln").arg("/mnt/rootfs").spawn().unwrap().wait();

    mount(c"none", c"/run/bundle/rootfs", Some(c"overlay"), libc::MS_SILENT, Some(c"lowerdir=/mnt/rootfs,upperdir=/mnt/upper,workdir=/mnt/work"));

    // let _ = Command::new("busybox").arg("ls").arg("-lh").arg("/mnt/rootfs").spawn().unwrap().wait();

    // println!("V config is {config:?}");
    fs::write("/run/bundle/config.json", config.oci_runtime_config.as_bytes()).unwrap();

    // let _ = Command::new("busybox").arg("ls").arg("-ln").arg("/mnt/rootfs").spawn().unwrap().wait();

    let container_output = run_container(&config);
    let response = match container_output {
        Err(_) | Ok(WaitIdDataOvertime::NotExited) => {
            Response {
                status: ExitKind::Abnormal,
                siginfo: None,
                rusage: None,
                panic: None,
            }
        }
        Ok(WaitIdDataOvertime::Exited{siginfo, rusage}) => {
            Response {
                status: ExitKind::Ok,
                siginfo: Some(siginfo.into()),
                rusage: Some(rusage.into()),
                panic: None,
            }
        }
        Ok(WaitIdDataOvertime::ExitedOvertime{siginfo, rusage}) => {
            Response {
                status: ExitKind::Overtime,
                siginfo: Some(siginfo.into()),
                rusage: Some(rusage.into()),
                panic: None,
            }
        }
    };

    pack_output(&response, "/run/output", INOUT_DEVICE);

    exit()
}
