use clap::Parser;
use std::fs::File;
use std::process::Command;

// build a peinit with blocktesting
// cargo build --features=blocktesting --package=peinit --profile=dev --target x86_64-unknown-linux-musl && (cd .. && ./scripts/build-initramfs.sh)
//
// with vhost_user_block from ch
// cargo run -- --user-block /tmp/vhost_user_block.sock
// cd ~/Repos/cloud-hypervisor/vhost_user_block
// cargo run -- --block-backend path=../../program-explorer/busybox.erofs,socket=/tmp/vhost_user_block.sock,readonly=true
//
// with pevub
// cargo run -- --user-block /tmp/pevub.sock
// cd pevub
// env RUST_LOG=trace cargo run -- /tmp/pevub.sock
//
// with disk
// cargo run -- --disk ../busybox.erofs

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long)]
    strace: bool,

    #[arg(long)]
    ch_log: bool,

    #[arg(long)]
    user_block: Option<String>,

    #[arg(long)]
    disk: Option<String>,
}
fn main() {
    env_logger::init();
    let args = Args::parse();

    if args.user_block.is_none() && args.disk.is_none()
        || args.user_block.is_some() && args.disk.is_some()
    {
        println!("must give --user-block or --disk");
    }

    let mut cmd = Command::new(if args.strace {
        "strace"
    } else {
        "cloud-hypervisor"
    });
    if args.strace {
        cmd.arg("-o")
            .arg("/tmp/strace.out")
            .arg("-f")
            .arg("--")
            .arg("cloud-hypervisor");
    }
    cmd.arg("-v")
        .arg("--memory")
        .arg("size=1G,shared=on")
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

    if let Some(disk) = args.disk {
        cmd.arg("--disk")
            .arg(format!("path={},readonly=on,id=12345", disk));
    } else if let Some(socket) = args.user_block {
        cmd.arg("--disk")
            .arg(format!("vhost_user=on,socket={},id=12345,readonly=on", socket));
    } else {
        panic!("no --disk or --user-block");
    }

    let mut child = cmd.spawn().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    std::io::copy(
        &mut File::open("/tmp/ch-console").unwrap(),
        &mut std::io::stdout(),
    )
    .unwrap();
    if args.ch_log {
        std::io::copy(
            &mut File::open("/tmp/ch-log").unwrap(),
            &mut std::io::stdout(),
        )
        .unwrap();
    }
    if args.strace {
        std::io::copy(
            &mut File::open("/tmp/strace.out").unwrap(),
            &mut std::io::stdout(),
        )
        .unwrap();
    }
    println!();
}
