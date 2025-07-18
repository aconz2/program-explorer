use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::fs::{DirEntry, File};
use std::io;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use command_fds::{CommandFdExt, FdMapping};
use rustix::fs::{chown, mkdir, open, Mode, OFlags};
use rustix::mount::MountFlags as MS;
use rustix::mount::{mount, mount_bind, mount_bind_recursive};
use rustix::process::{chdir, chroot};
use rustix::system::{reboot, RebootCommand};

use peinit::{read_io_file_config, write_io_file_response};
use peinit::{Config, Response, ResponseFormat, RootfsKind};
use waitid_timeout::{PidFd, PidFdWaiter, WaitIdDataOvertime};

const IMAGE_DEVICE: &CStr = c"/dev/pmem0";
const INOUT_DEVICE: &str = "/dev/pmem1";
const STDOUT_FILE: &str = "/run/output/stdout";
const STDERR_FILE: &str = "/run/output/stderr";
const RESPSONSE_JSON_STDOUT_SIZE: u64 = 1024;

//fn sha2_hex(buf: &[u8]) -> String {
//    use sha2::{Sha256,Digest};
//    use base16ct;
//    let hash = Sha256::digest(&buf);
//    base16ct::lower::encode_string(&hash)
//}

#[allow(dead_code)]
fn kernel_panic() {
    fs::write("/proc/sys/kernel/sysrq", b"1").unwrap();
    fs::write("/proc/sysrq-trigger", b"c").unwrap();
}

fn exit() {
    //kernel_panic();
    //unsafe { core::arch::asm!("hlt", options(att_syntax, nomem, nostack)); }
    //unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_HALT); }
    //unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_RESTART); }
    //unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_SW_SUSPEND); }
    let _ = reboot(RebootCommand::PowerOff);
    std::process::exit(1);
}

// NOTE: the host can still not receive this message if the pmem is configured incorrectly, for
// example by having discard_writes=on accidentally in which case the writes are silently dropped
// and also if the data wasn't sync'd then the host never sees our response
fn write_panic_response(message: &str) -> Result<(), peinit::Error> {
    println!("writing panic response: {message}");

    let response = Response::Panic {
        message: message.into(),
    };

    let mut f = File::create(INOUT_DEVICE).map_err(|_| peinit::Error::Io)?;
    write_io_file_response(&mut f, &response)?;
    // have gotten bit by the write not being visible since we exit so quickly after the write
    f.sync_data().map_err(|_| peinit::Error::Io)?;
    Ok(())
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
        let _ = write_panic_response(&format!("{}", p)).map_err(|e| {
            println!("Error writing panic response {e:?}");
        });
        exit();
    }));
}

// debugging code
//fn mountinfo(name: &str) {
//    if !name.is_empty() {
//        println!("=== {name} ===");
//    }
//    let root = std::fs::read_link("/proc/self/root").unwrap();
//    let cwd = std::fs::read_link("/proc/self/cwd").unwrap();
//    //let root_stats = statfs(root.to_str().unwrap());
//    let root_stats = statvfs(root.to_str().unwrap());
//    //let root_fsid = unsafe { std::mem::transmute::<libc::fsid_t, [libc::c_int; 2]>(root_stats.f_fsid) };
//
//    println!("root={root:?} root_fsid={:x} cwd={cwd:?}", root_stats.f_fsid);
//    let s = fs::read_to_string("/proc/self/mountinfo").unwrap();
//    let table: Vec<Vec<String>> = s.lines().map(|x| x.split(" ").map(|y| y.to_string()).collect()).collect();
//    for row in table {
//        println!("{:>2} {:>2} {:6} {:3} {:10} {:10}", row[0], row[1], row[2], row[3], row[4], row[7]);
//    }
//}
//
//fn statvfs(name: &str) -> libc::statvfs {
//    let name = CString::new(name).unwrap();
//    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
//    let ret = unsafe { libc::statvfs(name.as_ptr(), &mut stats) };
//    assert!(ret == 0);
//    stats
//}
//
//fn statfs(name: &str) -> libc::statfs {
//    let name = CString::new(name).unwrap();
//    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
//    let ret = unsafe { libc::statfs(name.as_ptr(), &mut stats) };
//    assert!(ret == 0);
//    stats
//}

// this lets crun do pivot_root even though we're running from initramfs
fn parent_rootfs(_pivot_dir: &CStr) -> io::Result<()> {
    // this is the thing from https://github.com/containers/bubblewrap/issues/592#issuecomment-2243087731
    //unshare(libc::CLONE_NEWNS)?;  // seems to be fine without this
    //mount(c"/", pivot_dir, None, libc::MS_BIND | libc::MS_REC | libc::MS_SILENT, None)?;
    //chdir(pivot_dir)?;
    //mount(pivot_dir, c"/", None, libc::MS_MOVE | libc::MS_SILENT, None)?;
    //chroot(c".")?;

    // from https://lore.kernel.org/linux-fsdevel/20200305193511.28621-1-ignat@cloudflare.com/T/
    // also seems to work okay
    //mountinfo("before"); println!("");
    mount_bind_recursive(c"/", c"/")?;
    //mountinfo("mount / /"); println!("");
    chdir(c"/..")?; // TODO: what??
                    //mountinfo("chdir /.."); println!();
    chroot(c".")?;
    //mountinfo("chroot ."); println!();
    Ok(())
}

fn unpack_input(archive: &str, dir: &str) -> Config {
    let mut file: File = open(archive, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .unwrap()
        .into();
    let (archive_size, config) = read_io_file_config(&mut file).unwrap();

    let fd_mappings = vec![FdMapping {
        parent_fd: file.into(),
        child_fd: 3,
    }];

    let mut cmd = if config.strace {
        Command::new("strace")
    } else {
        Command::new("/bin/pearchive")
    };
    if config.strace {
        cmd.arg("/bin/pearchive");
    }
    let ret = cmd
        .arg("unpackfd")
        .arg("3")
        .arg(dir)
        .arg(format!("{archive_size}"))
        .uid(1000)
        .gid(1000)
        .fd_mappings(fd_mappings)
        .unwrap()
        .status()
        .unwrap()
        .code()
        .expect("pearchive unpackdev had no status");
    assert!(ret == 0, "pearchive unpackdev failed with status {}", ret);

    config
}

fn pack_output<P: AsRef<OsStr>>(dir: P, archive: OwnedFd, strace: bool) {
    let fd_mappings = vec![FdMapping {
        parent_fd: archive,
        child_fd: 3,
    }];

    let mut cmd = if strace {
        Command::new("strace")
    } else {
        Command::new("/bin/pearchive")
    };
    if strace {
        cmd.arg("/bin/pearchive");
    }
    let ret = cmd
        .arg("packfd")
        .arg(dir)
        .arg("3")
        .uid(1000)
        .gid(1000)
        .fd_mappings(fd_mappings)
        .unwrap()
        .status()
        .unwrap()
        .code()
        .expect("pearchive packdev had no status");
    assert!(ret == 0, "pearchive packdev failed with status {}", ret);
}

fn run_container(config: &Config) -> io::Result<WaitIdDataOvertime> {
    let outfile = File::create_new(STDOUT_FILE).unwrap();
    let errfile = File::create_new(STDERR_FILE).unwrap();
    let run_input = Path::new("/run/input");
    let stdin: Stdio = config
        .stdin
        .clone()
        .and_then(|x| {
            // TODO this is annoying
            let p = match run_input.join(x).canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    return None;
                }
            };
            if !p.starts_with(run_input) {
                // println!("V warn stdin traversal avoided");
                return None;
            }
            match File::open(p) {
                Ok(f) => Some(Stdio::from(f)),
                Err(_) => None,
            }
        })
        .unwrap_or_else(Stdio::null);

    let start = Instant::now();
    let mut cmd = if config.strace {
        Command::new("/bin/strace")
    } else {
        Command::new("/bin/crun")
    };
    if config.strace {
        cmd.arg("-e")
            .arg("write,openat,unshare,clone,clone3,chdir")
            .arg("-f")
            .arg("-o")
            .arg("/run/crun.strace")
            .arg("--decode-pids=comm")
            .arg("/bin/crun");
    }
    if config.crun_debug {
        cmd.arg("--debug").arg("--log=/run/crun.log");
    }
    cmd.arg("run")
        .arg("-b") // --bundle
        .arg("/run/bundle")
        .arg("-d") // --detach
        .arg("--pid-file=/run/pid")
        .arg("cid-1234")
        .stdout(Stdio::from(outfile))
        .stderr(Stdio::from(errfile))
        .stdin(stdin);

    let exit_status = cmd.spawn().unwrap().wait().unwrap();

    let elapsed = start.elapsed();
    println!("V crun ran in {elapsed:?}");

    if config.strace {
        cat_file_if_exists("crun.strace", "/run/crun.strace");
    }
    if config.crun_debug {
        cat_file_if_exists("crun.log", "/run/crun.log");
    }

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
    let pid = fs::read_to_string("/run/pid")
        .unwrap()
        .parse::<i32>()
        .unwrap();

    // this can verify the Uid/Gid is not 0 0 0 0 DOES NOT WORK WITH STRACE
    // Command::new("/bin/busybox").arg("cat").arg(format!("/proc/{}/status", pid)).spawn().unwrap();
    let mut pidfd = PidFd::open(pid, 0).unwrap();
    let mut waiter = PidFdWaiter::new(&mut pidfd).unwrap();

    waiter.wait_timeout_or_kill(config.timeout)
}

#[cfg(not(feature="snapshotting"))]
fn snapshot() {
}

#[cfg(feature="snapshotting")]
fn snapshot() {
    use std::io::Write;
    use vsock::{VsockStream, VMADDR_CID_HOST};
    let mut vsock = {
        loop {
            match VsockStream::connect_with_cid_port(VMADDR_CID_HOST, 42) {
                Ok(sock) => { break sock; }
                Err(e) => {
                    println!("error connecting {:?}", e);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }

        }
    };
    println!("{} ms: connected to vsock", t0.elapsed().as_millis());
    let mut buf = [0u8; 1];
    vsock.write_all(&mut buf).unwrap(); // signal ready
    println!("{} ms: written to vsock", t0.elapsed().as_millis());
    // read doesn't error out if we disconnect the vsock after pause + before snapshot
    match vsock.read_exact(&mut buf) {
        Ok(_) => {println!("got okay from read");}
        Err(e) => {println!("got error was expecting from read {:?}", e);}
    }
    println!("{} ms: vsock read", t0.elapsed().as_millis());
    //std::thread::sleep(std::time::Duration::from_millis(500));
    println!("{} ms: exiting", t0.elapsed().as_millis());
    // TODO This is still WIP experimental so we just exit immediately to measure resume time
    exit();
}

#[cfg(not(feature="blocktesting"))]
fn block_testing() {
}

#[cfg(feature="blocktesting")]
fn block_testing() {
    use std::io::Seek;
    let mut buf = vec![0; 4096];
    //Command::new("busybox").arg("ls").arg("-l").arg("/dev").spawn().unwrap().wait().unwrap();
    let mut f = File::open("/dev/vda").unwrap();
    f.read_exact(&mut buf).unwrap();
    for byte in buf.iter().take(100) {
        print!("{:x}", byte);
    }
    println!("\n----------------------------");
    for _ in 0..2046 {
        f.read_exact(&mut buf).unwrap();
    }
    f.read_exact(&mut buf).unwrap();
    for byte in buf.iter().take(100) {
        print!("{:x}", byte);
    }
    println!("");
}

fn main() {
    #[cfg(feature="snapshotting")]
    let t0 = std::time::Instant::now();
    setup_panic();
    #[cfg(feature="snapshotting")]
    println!("{} ms: setup_panic", t0.elapsed().as_millis());

    parent_rootfs(c"/abc").unwrap();
    #[cfg(feature="snapshotting")]
    println!("{} ms: parent_rootfs", t0.elapsed().as_millis());

    {
        // initial mounts
        mount(c"none", c"/proc", c"proc", MS::SILENT, None).unwrap();
        mount(c"none", c"/sys/fs/cgroup", c"cgroup2", MS::SILENT, None).unwrap();
        mount(c"none", c"/dev", c"devtmpfs", MS::SILENT, None).unwrap();
        mount(
            c"none",
            c"/run/output",
            c"tmpfs",
            MS::SILENT,
            Some(c"size=2M,mode=777"),
        )
        .unwrap();
        // the umask 022 means mkdir creates with 755, mkdir(1) does a mkdir then chmod. we could also
        // have set umask
        mkdir(c"/run/output/dir", 0o777.into()).unwrap();
        //chmod(c"/run/output/dir", 0o777).unwrap();
        chown(
            c"/run/output/dir",
            Some(rustix::fs::Uid::from_raw(1000)),
            Some(rustix::fs::Gid::from_raw(1000)),
        )
        .unwrap();
    }
    #[cfg(feature="snapshotting")]
    println!("{} ms: mount stuff", t0.elapsed().as_millis());

    snapshot();
    block_testing();

    let config = unpack_input(INOUT_DEVICE, "/run/input");

    // mount index
    let rootfs_kind = match config.rootfs_kind {
        RootfsKind::Sqfs => c"squashfs",
        RootfsKind::Erofs => c"erofs",
    };

    // rootfs_dir can be None, in which case this isn't a multi-image
    if let Some(rootfs_dir) = config.rootfs_dir.as_ref() {
        mount(IMAGE_DEVICE, c"/mnt/image", rootfs_kind, MS::SILENT, None).unwrap();
        let rootfs_dir = CString::new(format!("/mnt/image/{}", rootfs_dir)).unwrap();
        mount_bind(&rootfs_dir, c"/mnt/rootfs").unwrap();
    } else {
        mount(IMAGE_DEVICE, c"/mnt/rootfs", rootfs_kind, MS::SILENT, None).unwrap();
    }

    // We have to use an overlayfs because we have a read only rootfs and want to mount in
    // /run/pe/{input,output} and be writable
    mount(
        c"none",
        c"/run/bundle/rootfs",
        c"overlay",
        MS::SILENT,
        Some(c"lowerdir=/mnt/rootfs,upperdir=/mnt/upper,workdir=/mnt/work"),
    )
    .unwrap();

    // println!("V config is {config:?}");
    fs::write(
        "/run/bundle/config.json",
        config.oci_runtime_config.as_bytes(),
    )
    .unwrap();

    if config.kernel_inspect {
        walkdir_files("/proc/sys".as_ref(), &|entry: &DirEntry| {
            println!(
                "{:?} {}",
                entry.path(),
                fs::read_to_string(entry.path())
                    .unwrap_or_else(|_| "\n".to_string())
                    .trim_end()
            );
        })
        .unwrap();
    }

    let container_output = run_container(&config);

    let (stdout, stderr) = match config.response_format {
        ResponseFormat::PeArchiveV1 => (None, None),
        ResponseFormat::JsonV1 => (
            read_if_exists_max_len_lossy(STDOUT_FILE, RESPSONSE_JSON_STDOUT_SIZE),
            read_if_exists_max_len_lossy(STDERR_FILE, RESPSONSE_JSON_STDOUT_SIZE),
        ),
    };

    let response = match container_output {
        Err(e) => Response::Panic {
            message: format!("{:?}", e),
        },
        Ok(WaitIdDataOvertime::NotExited) => Response::Panic {
            message: "ch not exited overtime".into(),
        },
        Ok(WaitIdDataOvertime::Exited { siginfo, rusage }) => Response::Ok {
            siginfo: siginfo.into(),
            rusage: rusage.into(),
            stdout: stdout,
            stderr: stderr,
            manifest_digest: config.manifest_digest,
        },
        Ok(WaitIdDataOvertime::ExitedOvertime { siginfo, rusage }) => Response::Overtime {
            siginfo: siginfo.into(),
            rusage: rusage.into(),
            stdout: stdout,
            stderr: stderr,
            manifest_digest: config.manifest_digest,
        },
    };

    {
        let mut f: File = open(INOUT_DEVICE, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .unwrap()
            .into();
        write_io_file_response(&mut f, &response).unwrap();

        match config.response_format {
            ResponseFormat::PeArchiveV1 => {
                pack_output("/run/output", f.into(), config.strace);
            }
            ResponseFormat::JsonV1 => {}
        }
    }

    exit()
}

fn read_n_or_str_error<P: AsRef<Path> + std::fmt::Display>(path: P, n: usize) -> String {
    match File::open(&path) {
        Err(e) => format!("error opening file {} {:?}", path, e),
        Ok(f) => {
            let mut buf = String::with_capacity(n);
            match f.take(n as u64).read_to_string(&mut buf) {
                Ok(_) => buf,
                Err(e) => format!("error reading file {} {:?}", path, e),
            }
        }
    }
}

fn read_if_exists_max_len_lossy<P: AsRef<Path>>(p: P, len: u64) -> Option<String> {
    let f = File::open(p).ok()?;
    let mut buf = vec![];
    let _ = f.take(len).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into())
}

fn cat_file_if_exists<P: AsRef<Path>>(name: &str, file: P) {
    if let Ok(mut f) = File::open(file) {
        println!("=== {name} ===");
        let _ = io::copy(&mut f, &mut io::stdout());
        println!("======");
    }
}

// https://doc.rust-lang.org/std/fs/fn.read_dir.html
fn walkdir_files(dir: &Path, cb: &dyn Fn(&DirEntry)) -> io::Result<()> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walkdir_files(&path, cb)?;
            } else {
                cb(&entry);
            }
        }
    }
    Ok(())
}
