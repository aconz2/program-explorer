use std::fs::File;
use std::process::Command;

// cargo build --features=blocktesting --package=peinit --profile=dev --target x86_64-unknown-linux-musl && (cd .. && ./scripts/build-initramfs.sh)
//
// with vhost_user_block in cloud-hypervisor/vhost_user_block
// cargo run -- --block-backend path=../../program-explorer/busybox.erofs,socket=/tmp/vhost_user_block.sock,readonly=true

fn main() {
    let strace = false;
    let ch_vhost_user_block = true;
    let mut cmd = Command::new(if strace { "strace" } else { "cloud-hypervisor" });
    if strace {
        cmd.arg("-o")
            .arg("/tmp/strace.out")
            .arg("-f")
            .arg("--")
            .arg("cloud-hypervisor");
    }
    cmd.arg("-v")
        .arg("--memory")
        .arg("size=1G")
        .arg("--cpus")
        .arg("boot=1")
        .arg("--kernel")
        .arg("../vmlinux")
        .arg("--initramfs")
        .arg("../target/debug/initramfs")
        .arg("--cmdline")
        .arg("console=hvc0")
        .arg("--console")
        .arg("file=/tmp/ch-console")
        .arg("--log-file")
        .arg("/tmp/ch-log");
    if ch_vhost_user_block {
        cmd
            .arg("--disk")
            .arg("path=../busybox.erofs,readonly=on,id=12345");
    } else {
        cmd
            .arg("--disk")
            .arg("socket=/tmp/vhost_user_block.sock");
    }

    let mut child = cmd.spawn().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    std::io::copy(
        &mut File::open("/tmp/ch-console").unwrap(),
        &mut std::io::stdout(),
    )
    .unwrap();
    std::io::copy(
        &mut File::open("/tmp/ch-log").unwrap(),
        &mut std::io::stdout(),
    )
    .unwrap();
    if strace {
        std::io::copy(
            &mut File::open("/tmp/strace.out").unwrap(),
            &mut std::io::stdout(),
        )
        .unwrap();
    }
    println!();
}
