use std::fs;
use std::fs::File;
use std::process::{Stdio, Command};
use std::io::{Seek,Read,Write};
use std::os::fd::{AsRawFd,FromRawFd};
use std::os::unix::process::CommandExt;
use std::ffi::{CStr,OsStr};
use std::path::Path;
use std::io;
use std::time::Duration;
use std::time::Instant;

use peinit::{Config,Response,ExitKind};
use waitid_timeout::{PidFdWaiter,PidFd,WaitIdDataOvertime};

use byteorder::{ReadBytesExt,WriteBytesExt,LE};
use libc;
use bincode;

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

fn setup_panic() {
    std::panic::set_hook(Box::new(|p| {
        eprintln!("{p:}");
        exit();
    }));
}

fn check_libc(ret: i32) {
    if ret < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        panic!("fail with error {errno}");
    }
}

fn mount(source: &CStr, target: &CStr, filesystem: Option<&CStr>, flags: libc::c_ulong, data: Option<&CStr>) {
    let filesystem = filesystem.map_or(std::ptr::null(), |x| x.as_ptr());
    let data = data.map_or(std::ptr::null(), |x| x.as_ptr() as *const libc::c_void);
    check_libc(unsafe { libc::mount(source.as_ptr(), target.as_ptr(), filesystem, flags, data) });
}

fn unshare(flags: libc::c_int) { check_libc(unsafe { libc::unshare(flags) }); }
fn chdir(dir: &CStr) { check_libc(unsafe { libc::chdir(dir.as_ptr()) }); }
fn chroot(dir: &CStr) { check_libc(unsafe { libc::chroot(dir.as_ptr()) }); }
fn mkdir(dir: &CStr, mode: libc::mode_t) { check_libc(unsafe { libc::mkdir(dir.as_ptr(), mode) }); }
fn chmod(path: &CStr, mode: libc::mode_t) { check_libc(unsafe { libc::chmod(path.as_ptr(), mode) }); }

fn parent_rootfs() {
    let pivot_dir = c"/abc";
    unshare(libc::CLONE_NEWNS);
    mount(c"/", pivot_dir, None, libc::MS_BIND | libc::MS_REC | libc::MS_SILENT, None);
    chdir(pivot_dir);
    mount(pivot_dir, c"/", None, libc::MS_MOVE | libc::MS_SILENT, None);
    chroot(c".");
}

fn init_mounts() {
    mount(c"none", c"/proc",          Some(c"proc"),     libc::MS_SILENT, None);
    mount(c"none", c"/sys/fs/cgroup", Some(c"cgroup2"),  libc::MS_SILENT, None);
    mount(c"none", c"/dev",           Some(c"devtmpfs"), libc::MS_SILENT, None);
    mount(c"none", c"/run/output",    Some(c"tmpfs"),    libc::MS_SILENT, Some(c"size=2M,mode=777"));

    // the umask 022 means mkdir creates with 755, mkdir(1) does a mkdir then chmod. we could also
    // have set umask
    mkdir(c"/run/output/dir", 0o777);
    chmod(c"/run/output/dir", 0o777);
}

fn mount_pmems() {
    mount(c"/dev/pmem0", c"/mnt/rootfs", Some(c"squashfs"), libc::MS_SILENT, None);
}

fn setup_overlay() {
    mount(c"none", c"/run/bundle/rootfs", Some(c"overlay"), libc::MS_SILENT, Some(c"lowerdir=/mnt/rootfs,upperdir=/mnt/upper,workdir=/mnt/work"));
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
        println!("using inotify");
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

// kinda intended to do this in process but learned you can't do unshare(CLONE_NEWUSER) in a
// threaded program
fn unpack_input(archive: &str, dir: &str) -> Config {
    let mut f = File::open(&archive).unwrap();
    let archive_size = f.read_u32::<LE>().unwrap();
    let config_size = f.read_u32::<LE>().unwrap();

    println!("VM   archive_size: {archive_size} config_size: {config_size}");

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
        println!("VM   config_bytes len {} {}", config_bytes.len(), hash_hex);
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
        .success();
    assert!(ret);

    config
}

fn pack_output<P: AsRef<OsStr> + AsRef<Path>>(response: &Response, dir: P, archive: P) {
    use std::io::{Seek,SeekFrom};
    // TODO this needs to write the <archive size> <config size> <config>, then do the pack
    let mut f = File::create(&archive).unwrap();
    let response_bytes = bincode::serialize(response).unwrap();

    if true {
        use sha2::{Sha256,Digest};
        use base16ct;
        let hash = Sha256::digest(&response_bytes);
        let hash_hex = base16ct::lower::encode_string(&hash);
        println!("VM   response_bytes len {} {}", response_bytes.len(), hash_hex);
    }

    let response_size: u32 = response_bytes.len().try_into().unwrap();
    f.seek(SeekFrom::Start(4)).unwrap();  // packdev fills in the <archive size>
    f.write_u32::<LE>(response_size).unwrap();
    f.write_all(&response_bytes).unwrap();
    let offset = f.stream_position().unwrap();

    let ret = Command::new("/bin/pearchive")
        .arg("packdev")
        .arg(dir)
        .arg(archive)
        .arg(format!("{offset}"))
        .status()
        .unwrap()
        .success();
    assert!(ret);
}

// fn wait4(pid: i32) -> Result<(libc::c_int, libc::rusage), Error> {
//     let mut status: libc::c_int = 0;
//     let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
//     let ret = unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
//     if ret < 0 { return Err(Error::Wait4); }
//     Ok((status, rusage))
// }

fn run_container(duration: Duration) -> io::Result<WaitIdDataOvertime> {
    let outfile = File::create_new("/run/output/stdout").unwrap();
    let errfile = File::create_new("/run/output/stderr").unwrap();

    //let child = Command::new("strace").arg("-f").arg("--decode-pids=comm").arg("/bin/crun")
    let start = Instant::now();
    let _ = Command::new("/bin/crun")
        // TODO can we get debug info on another fd?
        //.arg("--debug")
        .arg("run")
        .arg("-b") // --bundle
        .arg("/run/bundle")
        .arg("-d") // --detach
        .arg("--pid-file=/run/pid")
        .arg("containerid-1234")
        .uid(1000)
        .gid(1000)
        .stdout(Stdio::from(outfile))
        .stderr(Stdio::from(errfile))
        .stdin(match File::open("/run/input/stdin") {
            Ok(f) => { Stdio::from(f) }
            Err(_) => { Stdio::null() }
        })
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    let elapsed = start.elapsed();
    println!("VM   crun ran in {elapsed:?}");
    // we wait on crun since it should run to completion and leave the pid in pidfd

    //Command::new("busybox").arg("ls").arg("/run").spawn().unwrap().wait().unwrap();
    let pid = fs::read_to_string("/run/pid").unwrap().parse::<i32>().unwrap();
    let mut pidfd = PidFd::open(pid, 0).unwrap();
    let mut waiter = PidFdWaiter::new(&mut pidfd).unwrap();

    waiter.wait_timeout_or_kill(duration)
}

// TODO think about how to give a more bulletproof indication if we panic to our host
// maybe something like reading in a challenge hash and writing out the hash of it
fn main() {
    setup_panic();

    init_mounts();

    //              rootfs    input/output
    wait_for_pmem(&[c"pmem0", c"pmem1"]).unwrap();

    mount_pmems();
    setup_overlay();
    parent_rootfs();

    let inout_device = "/dev/pmem1";

    // let _ = Command::new("busybox").arg("ls").arg("-lh").arg("/mnt/rootfs").spawn().unwrap().wait();

    let config = unpack_input(inout_device, "/run/input/dir");
    println!("VM   config is {config:?}");
    fs::write("/run/bundle/config.json", config.oci_runtime_config.as_bytes()).unwrap();

    let container_output = run_container(config.timeout);
    let response = match container_output {
        Err(_) | Ok(WaitIdDataOvertime::NotExited) => {
            Response {
                status: ExitKind::Abnormal,
                siginfo: None,
                rusage: None,
            }
        }
        Ok(WaitIdDataOvertime::Exited{siginfo, rusage}) => {
            Response {
                status: ExitKind::Ok,
                siginfo: Some(siginfo.into()),
                rusage: Some(rusage.into()),
            }
        }
        Ok(WaitIdDataOvertime::ExitedOvertime{siginfo, rusage}) => {
            Response {
                status: ExitKind::Overtime,
                siginfo: Some(siginfo.into()),
                rusage: Some(rusage.into()),
            }
        }
    };

    pack_output(&response, "/run/output", inout_device);

    exit()
}
