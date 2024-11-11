use std::io;
use std::os::fd::AsRawFd;

use serde::{Deserialize,Serialize};
use clap::{Parser};
use nix::sys::socket::{UnixAddr,SockFlag,SockType,AddressFamily,MsgFlags,GetSockOpt,
                       socket,connect,send,recv};
use nix::sys::socket::sockopt::{SndBuf};

use peimage::PEImageMultiIndex;

#[derive(Serialize,Deserialize)]
struct WorkerInput {
    file: String,
}

#[derive(Serialize,Deserialize)]
struct WorkerOutput {
    status: i32
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    // ugh where should things get stored
    #[arg(long, default_value = "../cloud-hypervisor-static")]
    ch: String,

    #[arg(long, default_value = "../vmlinux")]
    kernel: String,

    #[arg(long, default_value = "../initramfs")]
    initramfs: String,

    #[arg(long, default_value_t = 1000, help = "timeout (ms) crun waits for the container")]
    timeout: u64,

    #[arg(long, default_value_t = 200, help = "timeout (ms) the host waits in addition to timeout")]
    ch_timeout: u64,

    #[arg(long)]
    socket: String,

    #[arg(trailing_var_arg = true, allow_hyphen_values = false)]
    image_indexes: Vec<String>,
}

fn handle_input(inp: WorkerInput) -> io::Result<WorkerOutput> {
    // read from temp file to get the config size
    // read the config
    // lookup the image in the index
    // compute the ch config
    //         the oci runtime config
    //         the pe config
    // write data to ...
    // crap we want to write this json stuff to the header
    // but now we might overrun the archive
    // so do we put it at the end now? and truncate off the
    // beginning? I guess so
    // then it looks like <archive size> <config size> <archive... > <config ...> <padding ...>
    // which is fine, order doesn't really matter
    Ok(WorkerOutput{ status: 200 })
}

fn main() {
    let args = Args::parse();
    let image_index = PEImageMultiIndex::from_paths(&args.image_indexes)
        .expect("couldn't build image index");

    let sock = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .expect("couldn't create socket");
    let addr = UnixAddr::new(args.socket.as_str())
        .expect("bad address for socket");
    () = connect(sock.as_raw_fd(), &addr)
        .expect("couldn't connect socket");

    let sndbuf = SndBuf{}.get(&sock)
        .expect("couldn't get sendbuf size");
    eprintln!("sndbuf max size is {sndbuf}");

    let mut buf = [0; 4096];
    let mut outbuf = vec![0; 4096];

    loop {
        eprintln!("reading...");
        match recv(sock.as_raw_fd(), &mut buf, MsgFlags::empty()) {
            Ok(0) => {
                eprintln!("exiting");
                break;
            }
            Ok(n) => {
                eprintln!("read {n}");
                let output = match serde_json::from_slice(&buf[..n]) {
                    Ok(inp) => {
                        match handle_input(inp) {
                            Ok(o) => o,
                            Err(e) => {
                                eprintln!("err {e}");
                                WorkerOutput {status: 400}
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("err {e}");
                        WorkerOutput {status: 400}
                    }
                };
                match serde_json::to_vec(&output) { // why can't we pass the vec
                    Ok(vec) => {
                        match send(sock.as_raw_fd(), &mut vec.as_slice(), MsgFlags::empty()) {
                            Ok(n) => {
                                if n != vec.len() {
                                    panic!("should never have a partial write {} != {}", n, vec.len());
                                }
                            }
                            Err(e) => {
                                // todo is this eintr and maybe retry
                                panic!("{e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("oh no idk what to do {e}");
                        panic!("{e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("err {e}");
            }
        }
    }
}

