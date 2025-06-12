use std::process::Command;
use std::os::unix::net::{UnixListener,UnixStream};
use std::io::Read;
use std::time::{Instant, Duration};
use std::fs;
use std::path::Path;
use serde_json;

use api_client;

// there is no API way that I know of to modify the zone from shared to private, so we have to
// modify the config.json directly. There is not easy way to use cloud_hypervisor::vmm::VmConfig
// unfortunately
// https://github.com/cloud-hypervisor/cloud-hypervisor/issues/4732
fn modify_config(mut config: serde_json::Value, mem_add: usize) -> serde_json::Value {
    use serde_json::Map;
    let zones = config.get_mut("memory").unwrap().get_mut("zones").unwrap();
    assert!(zones.as_array().unwrap().len() == 1);
    let zone0 = zones.get_mut(0).unwrap().as_object_mut().unwrap();
    zone0["shared"] = false.into(); // enable CoW
    let mut zone1 = Map::new();
    zone1.insert("id".to_string(), "1".into());
    zone1.insert("size".to_string(), mem_add.into());
    zones.as_array_mut().unwrap().push(zone1.into());
    config
}

fn main() {
    let vsock_prefix = "/tmp/ch.vsock";
    let vsock_path = format!("{vsock_prefix}_42");
    let api_path = "/tmp/ch-api.sock";
    let _ = fs::remove_file(&vsock_prefix);
    let _ = fs::remove_file(&vsock_path);
    let _ = fs::remove_file(&api_path);
    let vsock_listener = UnixListener::bind(vsock_path).unwrap();

    let snapshot_dir = "vmsnapshot";
    let memfile = "memfile";
    //let _ = fs::remove_dir_all(snapshot_dir);

    let mem_total = 1 << 30; // 1G
    // mem has to be in multiples of 128
    let mem_initial = 128 * 1 << 20; // 128M == 2**27
    let mem_add = mem_total - mem_initial;

    Command::new("truncate").arg("--size=128M").arg(memfile).spawn().unwrap().wait().unwrap();

    if !fs::exists(snapshot_dir).unwrap_or(false) {
        let _ = fs::create_dir(snapshot_dir);
        let t0 = Instant::now();
        println!("{} ms: pre spawn", 0);
        let mut child = Command::new("cloud-hypervisor")
            //.arg("-v")
            .arg("--kernel").arg("../vmlinux")
            .arg("--initramfs").arg("../target/debug/initramfs")
            .arg("--cpus").arg("boot=1")
            //.arg("--memory").arg("size=1G")
            //.arg("--memory").arg(format!("size={},hotplug_size={}", mem_initial, mem_add))
            //.arg("--memory").arg(format!("size={},hotplug_size={},hotplug_method=virtio-mem", mem_initial, mem_add))

            .arg("--memory").arg("size=0,hotplug_method=virtio-mem")
            .arg("--memory-zone").arg(format!("id=0,size={},file=memfile,shared=true", mem_initial))

            .arg("--cmdline").arg("console=hvc0")
            .arg("--console").arg("file=/tmp/ch-console")
            //.arg("--console").arg("null")
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
        // we can either remove the vsock once we are paused, that way the restored guest doesn't
        // resume with it, or after restore and before resume, init waiting on the read will just get
        // conn interrupted or w/e either way
        // but this isn't actually causing a econnect in the guest
        //let command = r#"{"id": "_vsock0"}"#;
        //api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.remove-device", Some(&command)).unwrap();

        let command = r#"{"destination_url": "file://DIR"}"#.replace("DIR", snapshot_dir);
        println!("{} ms: snapshotting", t0.elapsed().as_millis());
        api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.snapshot", Some(&command)).unwrap();
        println!("{} ms: snapshotted", t0.elapsed().as_millis());
        api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.shutdown", None).unwrap();
        println!("{} ms: shutdown", t0.elapsed().as_millis());

        child.kill().unwrap();
        child.wait().unwrap();
        let console_text = fs::read_to_string("/tmp/ch-console").unwrap();
        fs::write(Path::new(snapshot_dir).join("console"), &console_text).unwrap();
        println!("{} ms: child exited", t0.elapsed().as_millis());

        let config_path = Path::new(snapshot_dir).join("config.json");
        let config_bytes = fs::read(&config_path).unwrap();
        let config_updated = modify_config(serde_json::from_slice(&config_bytes).unwrap(), mem_add);
        let mut f = std::fs::File::create(&config_path).unwrap();
        serde_json::to_writer(&mut f, &config_updated).unwrap();
    }


    // ch complains if the vsock prefix exists
    let _ = fs::remove_file(&vsock_prefix);
    let _ = fs::remove_file(&api_path);

    let t0 = Instant::now();
    println!("{} ms: starting from snapshot", t0.elapsed().as_millis());
    let mut child = Command::new("cloud-hypervisor")
    // getting some weird error about locking memory so using -m1
    //let mut child = Command::new("perf").arg("record").arg("--call-graph=dwarf").arg("-F5000").arg("-m32")/*.arg("--all-user")*/.arg("--").arg("cloud-hypervisor")
    // couldn't get perf kvm stat to work Error:No permissions to read /sys/kernel/tracing//events/kvm/kvm_entry
    //let mut child = Command::new("perf").arg("kvm").arg("stat").arg("record").arg("cloud-hypervisor")
    //let mut child = Command::new("strace").arg("-f").arg("-o").arg("strace.out").arg("--absolute-timestamps=precision:ms").arg("--relative-timestamps=ms").arg("cloud-hypervisor")
        .arg("-v")
        .arg("--api-socket").arg(format!("path={}", api_path))
        .arg("--restore").arg(format!("source_url=file://{}", snapshot_dir))
        //.arg("--console").arg("file=/tmp/ch-console-2")
        .spawn()
        .unwrap();
    println!("{} ms: started from snapshot", t0.elapsed().as_millis());
    let mut api_sock = (|| {
        for _ in 0..1000 {
            match UnixStream::connect(api_path) {
                Ok(sock) => { return sock; },
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        panic!("couldn't connect to api socket");
    })();
    println!("{} ms: connected to api", t0.elapsed().as_millis());
    //let res = api_client::simple_api_full_command_and_response(&mut api_sock, "GET", "vm.info", None).unwrap();
    //println!("res {:?}", res);
    let remove_vsock = true;
    if remove_vsock {
        let command = r#"{"id": "_vsock0"}"#;
        api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.remove-device", Some(&command)).unwrap();
    } else {
        todo!("will hang, idk how to restore the vsock");
    }

    //let resize = true;
    //if resize {
    //    println!("{} ms: resizing", t0.elapsed().as_millis());
    //    let command = r#"{"desired_ram": RAM}"#.replace("RAM", &format!("{}", mem_add));
    //    api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.resize", Some(&command)).unwrap();
    //    println!("{} ms: resized", t0.elapsed().as_millis());
    //}

    // zone
    //{
    //    println!("{} ms: adding zone", t0.elapsed().as_millis());
    //    let command = r#"{"desired_ram": RAM}"#.replace("RAM", &format!("{}", mem_add));
    //    api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.resize-zone", Some(&command)).unwrap();
    //    println!("{} ms: resized", t0.elapsed().as_millis());
    //}

    println!("{} ms: resuming", t0.elapsed().as_millis());
    api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vm.resume", None).unwrap();
    println!("{} ms: resumed", t0.elapsed().as_millis());

    //std::thread::sleep(Duration::from_millis(500));

    //api_client::simple_api_full_command_and_response(&mut api_sock, "PUT", "vmm.shutdown", None).unwrap();
    //println!("{} ms: shutdown", t0.elapsed().as_millis());

    //child.kill().unwrap();
    child.wait().unwrap();
    println!("{} ms: exited", t0.elapsed().as_millis());

    println!("console reconstructed");
    let original_console_text = fs::read_to_string(Path::new(snapshot_dir).join("console")).unwrap();
    let console_text = fs::read_to_string("/tmp/ch-console").unwrap();
    println!("{}", original_console_text);
    println!("--------- resume --------------");
    println!("{}", console_text);
}
