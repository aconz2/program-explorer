use libc;
use std::process::{Command};
use std::os::unix::process::CommandExt;
use api_client;
use std::os::unix::net::{UnixStream, UnixListener};
use std::os::fd::{FromRawFd, IntoRawFd};
use core::mem;
use std::io;
use std::path::Path;
use std::ptr;
use std::os::unix::ffi::OsStrExt;

const CH_BINPATH:     &str = "/home/andrew/Repos/program-explorer/cloud-hypervisor-static";
const KERNEL_PATH:    &str = "/home/andrew/Repos/linux/vmlinux";
const INITRAMFS_PATH: &str = "/home/andrew/Repos/program-explorer/initramfs";

fn check_libc(ret: i32, msg: &str) {
    if ret < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        println!("fail with error {errno} {msg}");
        std::process::exit(1);
    }
}

// https://github.com/rust-lang/rust/blob/d7522d872601c5243899a813728a05cde1e5a8e2/library/std/src/os/unix/net/addr.rs#L28
fn sockaddr_un(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: All zeros is a valid representation for `sockaddr_un`.
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let bytes = path.as_os_str().as_bytes();

    // if bytes.contains(&0) {
    //     return Err(io::const_io_error!(
    //         io::ErrorKind::InvalidInput,
    //         "paths must not contain interior null bytes",
    //     ));
    // }

    // if bytes.len() >= addr.sun_path.len() {
    //     return Err(io::const_io_error!(
    //         io::ErrorKind::InvalidInput,
    //         "path must be shorter than SUN_LEN",
    //     ));
    // }
    // SAFETY: `bytes` and `addr.sun_path` are not overlapping and
    // both point to valid memory.
    // NOTE: We zeroed the memory above, so the path is already null
    // terminated.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), addr.sun_path.as_mut_ptr().cast(), bytes.len())
    };

    let mut len = 4 + bytes.len();
    match bytes.get(0) {
        Some(&0) | None => {}
        Some(_) => len += 1,
    }
    Ok((addr, len as libc::socklen_t))
}

fn main() {
    // let (parent_fd, child_fd) = unsafe {
    //     let mut fds = [-1, -1];
    //     check_libc(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()));
    //     (fds[0], fds[1])
    // };
    // println!("{parent_fd} {child_fd}");
    // let mut parent_stream = unsafe {
    // println!("hiiiiiiiii");
    //     let x = UnixStream::from_raw_fd(parent_fd);
    // println!("biiiiiii");
    // x
    // };
    // Socket::new_pair takes the flags, maybe use that
    // UnixStream::pair sets cloexec without option...
    //let (mut parent_stream, child_stream) = UnixStream::pair().unwrap();

    // from https://github.com/firecracker-microvm/micro-http/blob/8182cd5523b63ceb52ad9d0e7eb6fb95683e6d1b/src/server.rs#L785
    let path_to_socket = "/tmp/123abc";
    std::fs::remove_file(path_to_socket).unwrap_or(());
    //let socket_listener = UnixListener::bind(path_to_socket).unwrap();
    let socket_fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            println!("error socket {fd}");
            std::process::exit(1);
        }
        let (addr, len) = sockaddr_un(path_to_socket.as_ref()).unwrap();
        check_libc(libc::bind(
                    fd,
                    core::ptr::addr_of!(addr) as *const _,
                    len as _,
                ));
        let backlog = 2;
        check_libc(libc::listen(fd, backlog), "listen");
        fd
    };
    // let socket_fd = socket_listener.into_raw_fd();

    let mut child = unsafe {
        let mut child = Command::new(CH_BINPATH)
            //.arg("-vv")
            .arg("--kernel").arg(KERNEL_PATH)
            .arg("--initramfs").arg(INITRAMFS_PATH)
            .arg("--seccomp").arg("log")
            .arg("--serial").arg("off")
            .arg("--cmdline").arg("console=hvc0")
            .arg("--cpus").arg("boot=1")
            .arg("--memory").arg("size=1024M,thp=on")
            .arg("--api-socket").arg(format!("fd={socket_fd}"))
            //.pre_exec(move || {libc::close(parent_fd); Ok(())})
            .spawn()
            .unwrap();

        //check_libc(libc::close(child_fd));
        child
    };

    //let mut parent_stream = UnixStream::connect(path_to_socket).unwrap();
    let mut parent_stream = unsafe { UnixStream::from_raw_fd(socket_fd) };
    let pmemconfig = r#"{"file": "../gcc-14.1.0.sqfs", "discard_writes": true}"#;
    
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("sending message");
    // the non-full version just prepends vm. to the command string
    let resp = api_client::simple_api_full_command_and_response(&mut parent_stream, "PUT", "vm.add-pmem", Some(pmemconfig));
    println!("sent command");
    match resp {
        Ok(resp) => {let msg = resp.unwrap_or("<no response>".to_string()); println!("got response {msg}");}
        Err(e) => {println!("got err {e}");}
    }

    let _ = child.wait();
    
}
