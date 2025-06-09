use std::process::Command;
use std::os::unix::net::{UnixListener,UnixStream};
use std::io::Read;
use std::time::{Instant, Duration};
use std::fs;

use api_client;

fn main() {
    let vsock_prefix = "/tmp/ch.vsock";
    let vsock_path = format!("{vsock_prefix}_42");
    let api_path = "/tmp/ch-api.sock";
    let _ = fs::remove_file(&vsock_prefix);
    let _ = fs::remove_file(&vsock_path);
    let _ = fs::remove_file(&api_path);
    let vsock_listener = UnixListener::bind(vsock_path).unwrap();

    let snapshot_dir = "vmsnapshot";
    let _ = fs::remove_dir_all(snapshot_dir);
    let _ = fs::create_dir(snapshot_dir);

    let t0 = Instant::now();
    println!("{} ms: pre spawn", 0);
    let mut child = Command::new("cloud-hypervisor")
        .arg("--kernel").arg("../vmlinux")
        .arg("--initramfs").arg("../target/debug/initramfs")
        .arg("--cpus").arg("boot=1")
        .arg("--memory").arg("size=1024M")
        .arg("--cmdline").arg("console=hvc0")
        //.arg("--console").arg("file=/tmp/ch-console")
        .arg("--console").arg("null")
        .arg("--api-socket").arg(format!("path={}", api_path))
        .arg("--vsock").arg(format!("cid=4,socket={}", vsock_prefix))
        .spawn()
        .unwrap();

    let (mut vsock_stream, _) = vsock_listener.accept().unwrap();
    println!("{} ms: accepted conn", t0.elapsed().as_millis());

    let mut buf = [0u8; 1];
    vsock_stream.read_exact(&mut buf).unwrap();

    println!("{} ms: read vsock", t0.elapsed().as_millis());

    // this is actually a race and below we have to loop; right thing is to pass an fd with an
    // already connected socket
    let mut api_sock = UnixStream::connect(api_path).unwrap();

    api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.pause", None).unwrap();

    let command = r#"{"destination_url": "file://DIR"}"#.replace("DIR", snapshot_dir);
    println!("{} ms: snapshotting", t0.elapsed().as_millis());
    api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.snapshot", Some(&command)).unwrap();
    println!("{} ms: snapshotted", t0.elapsed().as_millis());
    api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.shutdown", None).unwrap();
    println!("{} ms: shutdown", t0.elapsed().as_millis());

    child.kill().unwrap();
    child.wait().unwrap();
    println!("{} ms: child exited", t0.elapsed().as_millis());

    // ch complains if the vsock prefix exists
    let _ = fs::remove_file(&vsock_prefix);
    let _ = fs::remove_file(&api_path);

    println!("{} ms: starting from snapshot", t0.elapsed().as_millis());
    let mut child = Command::new("cloud-hypervisor")
        .arg("--api-socket").arg(format!("path={}", api_path))
        .arg("--restore").arg(format!("source_url=file://{}", snapshot_dir))
        .spawn()
        .unwrap();
    println!("{} ms: started from snapshot", t0.elapsed().as_millis());
    let mut api_sock = (|| {
        for _ in 0..100 {
            match UnixStream::connect(api_path) {
                Ok(sock) => { return sock; },
                Err(e) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        panic!("couldn't connect to api socket");
    })();
    println!("{} ms: connected to api", t0.elapsed().as_millis());
    //let res = api_client::simple_api_full_command_and_response(&mut api_sock, "GET", "vm.info", None).unwrap();
    //println!("res {:?}", res);
    let command = r#"{"id": "_vsock0"}"#;
    let res = api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.remove-device", Some(&command)).unwrap();
    let res = api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.resume", None).unwrap();
    println!("{} ms: resumed", t0.elapsed().as_millis());

    child.kill().unwrap();
}
