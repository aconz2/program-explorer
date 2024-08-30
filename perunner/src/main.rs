use libc;
use std::fs;
use std::process::{Command};
use std::os::unix::process::CommandExt;
use api_client;
use vmm::config::PmemConfig:

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
    let api_socket_path = "/tmp/ch.sock";
    let _ = fs::remove_file(api_socket_path);

    let (parent_fd, child_fd) = unsafe {
        let mut fds = [-1, -1];
        check_libc(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()));
        (fds[0], fds[1])
    };

    let parent_copy_fd = parent_fd;
    let mut child = unsafe {
        let mut child = Command::new(CH_BINPATH)
            .arg("--kernel").arg(KERNEL_PATH)
            .arg("--initramfs").arg(INITRAMFS_PATH)
            .arg("--serial").arg("off")
            .arg("--cmdline").arg("console=hvc0")
            .arg("--cpus").arg("boot=1")
            .arg("--memory").arg("size=1024M,thp=on")
            // this should be an fd
            .arg("--api-socket").arg(format!("{child_fd}"))
            .pre_exec(move || {libc::close(parent_copy_fd); Ok(())})
            .spawn()
            .unwrap();

        check_libc(libc::close(child_fd));
        child
    };

    let pmemconfig = PmemConfig {
        file: "/home/andrew/Repos/program-explorer/gcc-14.1.0.sqfs",
        size: None,
        iommu: false,
        discard_writes: true,
        id: None,
        pci_segment: 0,
    };
    let resp = simple_api_command(parent_fd, "PUT", "vm.add-pmem", Some(pmemconfig));
    println!("{resp}");

    let _ = child.wait();
    
}
