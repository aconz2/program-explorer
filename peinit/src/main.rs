//        int mount(const char *source, const char *target,
//                  const char *filesystemtype, unsigned long mountflags,
//                  const void *_Nullable data);
use libc;
use std::fs::File;
use std::process::{Stdio, Command};
use std::io::{Seek,Read};
//use std::os::unix::process::CommandExt;
use std::os::fd::{AsRawFd,FromRawFd};
use std::ffi::OsStr;

#[derive(Debug)]
enum Error {
    OpenDev,
    InotifyInit,
    InotifyAddWatch,
    InotifyRead,
}

fn size_of<T>(_t: T) -> usize { return std::mem::size_of::<T>(); }

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

unsafe fn parent_rootfs() {
    let pivot_dir = c"/abc";
    check_libc(libc::unshare(libc::CLONE_NEWNS));
    check_libc(libc::mount(c"/".as_ptr(), pivot_dir.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC | libc::MS_SILENT, std::ptr::null()));
    check_libc(libc::chdir(pivot_dir.as_ptr()));
    check_libc(libc::mount(pivot_dir.as_ptr(), c"/".as_ptr(), std::ptr::null(), libc::MS_MOVE | libc::MS_SILENT, std::ptr::null()));
    check_libc(libc::chroot(c".".as_ptr()));
}

unsafe fn init_mounts() {
    check_libc(libc::mount(c"none".as_ptr(), c"/proc".as_ptr(),          c"proc".as_ptr(),     libc::MS_SILENT, std::ptr::null()));
    check_libc(libc::mount(c"none".as_ptr(), c"/sys/fs/cgroup".as_ptr(), c"cgroup2".as_ptr(),  libc::MS_SILENT, std::ptr::null()));
    check_libc(libc::mount(c"none".as_ptr(), c"/dev".as_ptr(),           c"devtmpfs".as_ptr(), libc::MS_SILENT, std::ptr::null()));
    check_libc(libc::mount(c"none".as_ptr(), c"/run/output".as_ptr(),    c"tmpfs".as_ptr(),    libc::MS_SILENT, c"size=2M,mode=777".as_ptr() as *const libc::c_void));
    // the umask 022 means mkdir creates with 755, mkdir(1) does a mkdir then chmod. we could also
    // have set umask
    check_libc(libc::mkdir(c"/run/output/dir".as_ptr(), 0o777));
    check_libc(libc::chmod(c"/run/output/dir".as_ptr(), 0o777));
}

unsafe fn mount_pmems() {
    check_libc(libc::mount(c"/dev/pmem0".as_ptr(), c"/mnt/rootfs".as_ptr(), c"squashfs".as_ptr(), libc::MS_SILENT, std::ptr::null()));
    // check_libc(libc::mount(c"/dev/pmem1".as_ptr(), c"/run/input".as_ptr(),  c"squashfs".as_ptr(), libc::MS_SILENT, std::ptr::null()));
}

unsafe fn setup_overlay() {
    check_libc(libc::mount(c"none".as_ptr(), c"/run/bundle/rootfs".as_ptr(), c"overlay".as_ptr(), libc::MS_SILENT, c"lowerdir=/mnt/rootfs,upperdir=/mnt/upper,workdir=/mnt/work".as_ptr() as *const libc::c_void));
}

unsafe fn fstatat_exists(file: &File, name: &std::ffi::CStr) -> bool {
    let mut buf: libc::stat = std::mem::zeroed(); 
    let ret = libc::fstatat(file.as_raw_fd(), name.as_ptr(), &mut buf, 0);
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

    if files.iter().all(|file| unsafe { fstatat_exists(&dev_file, file) }) {
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
            if unsafe { fstatat_exists(&dev_file, file) } {
                println!("pmem exists");
                break;
            } else {
                // check one more time before blocking on reading inotify in case it got added
                // after we stat'd but before we created the watcher. idk this still isn't atomic
                // though
                if unsafe { fstatat_exists(&dev_file, file) } {
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
fn unpack_input(archive: &str, dir: &str) {
    let mut f = File::open(&archive).unwrap();
    let mut buf = [0u8; 4];

    f.read_exact(&mut buf).unwrap(); // header size
    let header_size = u32::from_le_bytes(buf);

    let mut header_data = Vec::with_capacity(header_size as usize); // todo uninit
    f.read_exact(header_data.as_mut_slice()).unwrap();

    f.read_exact(&mut buf).unwrap(); // archive size
    let archive_size = u32::from_le_bytes(buf);
    let offset = f.stream_position().unwrap();
    println!("read offset and archive size from as header_size={header_size} archive_size={archive_size} offset={offset}");
                                     
    //let ret = Command::new("/bin/pearchive")
    let ret = Command::new("strace").arg("-e").arg("mmap").arg("/bin/pearchive")
        .arg("unpackdev")
        .arg(archive)
        .arg(dir)
        .arg(format!("{offset}"))
        .arg(format!("{archive_size}"))
        .status()
        .unwrap()
        .success();
    assert!(ret);
}

fn pack_output<P: AsRef<OsStr>>(dir: P, archive: P) {
    let ret = Command::new("/bin/pearchive")
        .arg("pack")
        .arg(dir)
        .arg(archive)
        .status()
        .unwrap()
        .success();
    assert!(ret);
}

fn run_crun() {
    let outfile = File::create_new("/run/output/stdout").unwrap();
    let errfile = File::create_new("/run/output/stderr").unwrap();

    //let mut child = Command::new("strace").arg("-f").arg("--decode-pids=comm").arg("/bin/crun")
    let mut child = Command::new("/bin/crun")
        .arg("--debug")
        .arg("run")
        .arg("--bundle")
        .arg("/run/bundle")
        .arg("containerid-1234")
        //.uid(1000)
        //.gid(1000)
        .stdout(Stdio::from(outfile))
        .stderr(Stdio::from(errfile))
        .stdin(match File::open("/run/input/stdin") {
            Ok(f) => { Stdio::from(f) }
            Err(_) => { Stdio::null() }
        })
        .spawn()
        .unwrap();
    //Command::new("busybox").arg("ps").arg("-T").spawn().unwrap().wait();
    //let pid = child.id();
    //let uid_map = std::fs::read_to_string(format!("/proc/{pid}/uid_map")).unwrap();
    //println!("{uid_map}");
    // TODO we need to wait with timeout from here too
    let ecode = child.wait().unwrap();
    // TODO this is an ExitStatus and will have none exitcode if it is terminated by a signal
    println!("exit code of crun {ecode}");
}

fn main() {
    setup_panic();

    unsafe {
        init_mounts();
    }

    //              rootfs    input/output
    wait_for_pmem(&[c"pmem0", c"pmem1"]).unwrap();

    unsafe {
        mount_pmems();
        setup_overlay();
        parent_rootfs();
    }

    let inout_device = "/dev/pmem1";

    let _ = Command::new("busybox").arg("ls").arg("-lh").arg("/mnt/rootfs").spawn().unwrap().wait();
    // TODO we need to slice off the input config
    unpack_input(inout_device, "/run/input/dir");

    run_crun();

    // TODO we need to first write out the result metadata, so maybe pearchive needs to take an
    // open fd
    pack_output("/run/output", inout_device);

    exit()
    //check_libc(libc::setuid(1000));
}
