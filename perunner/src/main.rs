use libc;
use std::process::{Command};
use std::os::unix::process::CommandExt;
use api_client;
use std::os::unix::net::UnixStream;
use std::os::fd::{FromRawFd};

const CH_BINPATH:     &str = "/home/andrew/Repos/program-explorer/cloud-hypervisor-static";
const KERNEL_PATH:    &str = "/home/andrew/Repos/linux/vmlinux";
const INITRAMFS_PATH: &str = "/home/andrew/Repos/program-explorer/initramfs";

fn check_libc(ret: i32) {
    if ret < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        println!("fail with error {errno}");
        std::process::exit(1);
    }
}

fn main() {
    let (parent_fd, child_fd) = unsafe {
        let mut fds = [-1, -1];
        check_libc(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()));
        (fds[0], fds[1])
    };
    println!("{parent_fd} {child_fd}");
    let mut parent_stream = unsafe {
    println!("hiiiiiiiii");
        let x = UnixStream::from_raw_fd(parent_fd);
    println!("biiiiiii");
    x
    };
    // Socket::new_pair takes the flags, maybe use that
    // UnixStream::pair sets cloexec without option...
    //let (mut parent_stream, child_stream) = UnixStream::pair().unwrap();


    let mut child = unsafe {
        let mut child = Command::new(CH_BINPATH)
            .arg("--kernel").arg(KERNEL_PATH)
            .arg("--initramfs").arg(INITRAMFS_PATH)
            .arg("--seccomp").arg("log")
            .arg("--serial").arg("off")
            .arg("--cmdline").arg("console=hvc0")
            .arg("--cpus").arg("boot=1")
            .arg("--memory").arg("size=1024M,thp=on")
            .arg("--api-socket").arg(format!("fd={child_fd}"))
            .pre_exec(move || {libc::close(parent_fd); Ok(())})
            .spawn()
            .unwrap();

        check_libc(libc::close(child_fd));
        child
    };

    let pmemconfig = r#"{"file": "gcc-14.1.0.sqfs", "discard_writes": true}"#;
    
    // the non-full version just prepends vm. to the command string
    let resp = api_client::simple_api_full_command_and_response(&mut parent_stream, "PUT", "vm.add-pmem", Some(pmemconfig));
    println!("sent command");
    match resp {
        Ok(resp) => {let msg = resp.unwrap_or("<no response>".to_string()); println!("got response {msg}");}
        Err(e) => {println!("got err {e}");}
    }

    let _ = child.wait();
    
}
