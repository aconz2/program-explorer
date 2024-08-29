//        int mount(const char *source, const char *target,
//                  const char *filesystemtype, unsigned long mountflags,
//                  const void *_Nullable data);
use libc;
use std::fs::File;
use std::process::{Stdio, Command};
use std::os::unix::process::CommandExt;

fn size_of<T>(_t: T) -> usize { return std::mem::size_of::<T>(); }

fn exit() {
    unsafe {
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    std::process::exit(1);
}

fn check_libc(ret: i32) {
    if ret < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        println!("fail with error {errno}");
        exit();
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
    check_libc(libc::mkdir(c"/run/output/dir".as_ptr(), 0o777));
}

unsafe fn mount_pmems() {
    check_libc(libc::mount(c"/dev/pmem0".as_ptr(), c"/mnt/rootfs".as_ptr(), c"squashfs".as_ptr(), libc::MS_SILENT, std::ptr::null()));
    check_libc(libc::mount(c"/dev/pmem1".as_ptr(), c"/run/input".as_ptr(),  c"squashfs".as_ptr(), libc::MS_SILENT, std::ptr::null()));
}

unsafe fn setup_overlay() {
    check_libc(libc::mount(c"none".as_ptr(), c"/run/bundle/rootfs".as_ptr(), c"overlay".as_ptr(), libc::MS_SILENT, c"lowerdir=/mnt/rootfs,upperdir=/mnt/upper,workdir=/mnt/work".as_ptr() as *const libc::c_void));
}

unsafe fn wait_for_pmem(files: &[&std::ffi::CStr]) {
    let mut inotify_fd: Option<i32> = None;
    let devfd = libc::open(c"/dev".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);

    let events: [libc::inotify_event; 4] = std::mem::zeroed();

    for file in files.iter() {
        loop {
            let mut buf: libc::stat = std::mem::zeroed(); 
            let ret = libc::fstatat(devfd, file.as_ptr(), &mut buf, 0);
            if ret == 0 {
                println!("pmem exists");
                break;
            } else {
                if inotify_fd.is_none() {
                    println!("using inotify");
                    let fd = libc::inotify_init1(libc::IN_CLOEXEC);
                    check_libc(fd);
                    inotify_fd = Some(fd);
                    let wd = libc::inotify_add_watch(fd, c"/dev".as_ptr(), libc::IN_CREATE);
                    check_libc(wd);
                }
                libc::read(inotify_fd.unwrap(), events.as_ptr() as *mut libc::c_void, size_of(events));
                // we don't bother checking what the events are, just trying again
            }
        }
    }
    libc::close(devfd);
    if let Some(fd) = inotify_fd {
        libc::close(fd);
    }
}

fn run_crun() {
    let outfile = File::create("/run/output/stdout").unwrap();
    let errfile = File::create("/run/output/stderr").unwrap();
    let infile =  File::open("/run/input/stdin").unwrap();
    let mut child = Command::new("/bin/crun")
        .arg("run")
        .arg("--bundle")
        .arg("/run/bundle")
        .arg("containerid-1234")
        //.uid(1000)
        //.gid(1000)
        //.stdout(Stdio::from(outfile))
        //.stderr(Stdio::from(errfile))
        //.stdin(Stdio::from(infile))
        .spawn()
        .unwrap();
    let ecode = child.wait().unwrap();
    // TODO this is an ExitStatus and will have none exitcode if it is terminated by a signal
    println!("exit code of crun {ecode}");
}


fn main() {
    unsafe {
        parent_rootfs();
        init_mounts();

        //              rootfs    input     output
        wait_for_pmem(&[c"pmem0", c"pmem1", c"pmem2"]);

        mount_pmems();

        setup_overlay();

        Command::new("busybox").arg("mount").spawn().unwrap().wait();
        Command::new("busybox").arg("ls").arg("-l").arg("/run/").spawn().unwrap().wait();
        Command::new("busybox").arg("ls").arg("-l").arg("/run/output").spawn().unwrap().wait();
        Command::new("busybox").arg("stat").arg("/run/output/dir").spawn().unwrap().wait();
        // Command::new("busybox").arg("ls").arg("-l").arg("/run/bundle/rootfs").spawn().unwrap();

        run_crun();
    }
    exit()
    //check_libc(libc::setuid(1000));
}
