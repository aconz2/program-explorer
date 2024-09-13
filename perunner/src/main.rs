use std::time::Duration;
use std::fs::File;
use std::io;

// this seems so wrong, why don't I understand crates and modules and stuff
mod cloudhypervisor;
use crate::cloudhypervisor::{CloudHypervisor,CloudHypervisorConfig};
use std::ffi::OsString;

// fn mainold() {
//     let use_filepath = false;
//     let filepath = "/tmp/123abc";  // or whatever you like
// 
//     let sockaddr = if use_filepath {
//         fs::remove_file(filepath).unwrap_or(());
//         SocketAddr::from_pathname(filepath).unwrap()
//     } else {
//         // using the empty string in from_pathname uses the "Autobind feature" from unix(7) to
//         // bind to a random name that begins with a null byte and then 5 random bytes in [0-9a-f]
//         // these leading null byte names are called abstract names and slightly confusingly doing
//         // SocketAddr::from_abstract_name(b"") doesn't cause autobind because it always adds 1 to
//         // the addrlen
//         SocketAddr::from_pathname("").unwrap()
//     };
//     let socket_listener = UnixListener::bind_addr(&sockaddr).unwrap();
//     let boundaddr = socket_listener.local_addr().unwrap();
//     println!("sockaddr {sockaddr:?}");
//     println!("boundaddr {boundaddr:?}");
//     let mut parent_stream = UnixStream::connect_addr(&boundaddr).unwrap();
// 
//     // optional: since we are connected, we can unlink the file to make it inaccessible
//     // its a little annoying we can't make an abstract named socket inaccessible
//     if use_filepath {
//         fs::remove_file(filepath).unwrap();
//     }
// 
//     let mut child = {
//         let socket_fd = socket_listener.as_raw_fd();
//         // we have to clear FD_CLOEXEC which is unconditionally set by UnixListener
//         unsafe {
//             check_libc(libc::fcntl(socket_fd, libc::F_SETFD, 0), "fcntl");
//         }
// 
//         Command::new(CH_BINPATH)
//             .arg("-v")
//             .arg("--kernel").arg(KERNEL_PATH)
//             .arg("--initramfs").arg(INITRAMFS_PATH)
//             .arg("--serial").arg("off")
//             .arg("--cmdline").arg("console=hvc0")
//             .arg("--cpus").arg("boot=1")
//             .arg("--memory").arg("size=1024M")
//             .arg("--api-socket").arg(format!("fd={socket_fd}"))
//             .arg("--log-file").arg("/tmp/ch-log")
//             .arg("--console").arg("file=/tmp/ch-console")
//             .stdin(Stdio::null())
//             .stdout(Stdio::null())
//             .stderr(Stdio::null())
//             .spawn()
//             .unwrap()
//     };
//     
//     // I'm testing an init using inotify to wait for the pmem so delay a bit otherwise it will
//     // already be there
//     std::thread::sleep(Duration::from_millis(500));
//     let pmemconfig = r#"{"file": "../gcc-14.1.0.sqfs", "discard_writes": true}"#;
//     let resp = api_client::simple_api_full_command_and_response(&mut parent_stream, "PUT", "vm.add-pmem", Some(pmemconfig));
//     match resp {
//         Ok(resp) => {let msg = resp.unwrap_or("<no response>".to_string()); println!("got response {msg}");}
//         Err(e) => {println!("got err {e}");}
//     }
// 
//     let status_code = match child.wait_timeout(Duration::from_secs(2)).unwrap() {
//         Some(status) => status.code(),
//         None => {
//             child.kill().unwrap();
//             child.wait().unwrap().code()
//         }
//     };
//     println!("exit status {status_code:?}");
// 
//     println!("== log ==");
//     let _ = io::copy(&mut File::open("/tmp/ch-log").unwrap(), &mut io::stdout());
//     println!("== console ==");
//     let _ = io::copy(&mut File::open("/tmp/ch-console").unwrap(), &mut io::stdout());
// }

fn main() {

    let ch_binpath:     OsString = "/home/andrew/Repos/program-explorer/cloud-hypervisor-static".into();
    let kernel_path:    OsString = "/home/andrew/Repos/linux/vmlinux".into();
    let initramfs_path: OsString = "/home/andrew/Repos/program-explorer/initramfs".into();

    let mut ch = CloudHypervisor::start(CloudHypervisorConfig {
        workdir: "/tmp".into(),
        bin: ch_binpath,
        kernel: kernel_path,
        initramfs: initramfs_path,
        log: true,
        console: true,
    }).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let pmemconfig = r#"{"file": "../gcc-14.1.0.sqfs", "discard_writes": true}"#;
    let resp = ch.api("PUT", "vm.add-pmem", Some(pmemconfig));
    println!("{resp:?}");
    //ch.add_pmem_ro("../gcc-14.1.0.sqfs");
    match ch.wait_timeout_or_kill(Duration::from_secs(2)) {
        Some(status) => println!("exited with status {status:?}"),
        None => println!("either didn't exit or got killed"),
    }
    //let status = ch.status();

    println!("== log ==");
    let _ = io::copy(&mut File::open(ch.log_file().unwrap()).unwrap(), &mut io::stdout());
    println!("== console ==");
    let _ = io::copy(&mut File::open(ch.console_file().unwrap()).unwrap(), &mut io::stdout());

}
