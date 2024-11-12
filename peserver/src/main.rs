use std::io;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::fs::File;
use std::time::Duration;

use serde::{Deserialize,Serialize};
use clap::{Parser};
use nix::sys::socket::{UnixAddr,SockFlag,SockType,AddressFamily,MsgFlags,GetSockOpt,
                       socket,connect,send,recv};
use nix::sys::socket::sockopt::{SndBuf};
use byteorder::{ReadBytesExt,LE};
use tempfile;
use tempfile::NamedTempFile;

use peimage::PEImageMultiIndex;
use perunner::worker;
use perunner::cloudhypervisor::{ChLogLevel,CloudHypervisorConfig};


const MAX_CONFIG_SIZE: u32 = 4096;

#[derive(Serialize,Deserialize)]
struct WorkerInput {
    file: String,
}

#[derive(Serialize,Deserialize)]
struct WorkerOutput {
    status: i32
}

#[derive(Serialize,Deserialize)]
struct ApiV1iRequest {
    image: String,
    stdin: Option<String>,
    args: Vec<String>, // TODO handle entrypoint
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    // ugh where should things get stored
    #[arg(long, default_value = "../cloud-hypervisor-static")]
    ch: String,

    #[arg(long, default_value_t = 0)]
    id: usize,

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

// blargh we can't truncate at an offset I was reading off_t as the offset
// file formats
// 1) <u32: rest size>    <u32: config size (zeroes)> <u32: request size> <request ...> <archive ...>
//                                                    \-------------- rest size --------------------/
// 2) <u32: archive size> <u32: config size>          <u32: request size> <request ...> <archive ...> <config ...> <... padding ...>
// 2) <u32: rest size>    <u32: response size> <response ...> <archive ...> <... padding ...>
//                        \---------------- rest size --------------------/
fn handle_input(ch_config: &CloudHypervisorConfig,
                image_index: &PEImageMultiIndex,
                buf: &mut Vec<u8>,
                input: WorkerInput,
                )
    -> io::Result<WorkerOutput> {

    let timeout = Duration::from_millis(10_000);
    let ch_timeout = timeout + Duration::from_millis(500);

    let mut f = File::open(&input.file)?;
    let config_size = f.read_u32::<LE>()?;
    if config_size > MAX_CONFIG_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "config size too big"))
    }
    buf.resize(config_size as usize, 0);
    () = f.read_exact(buf)?;
    let request: ApiV1iRequest = serde_json::from_slice(&buf)?;

    let image_index_entry = image_index.get(&request.image)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no such image"))?;

    let runtime_spec = perunner::create_runtime_spec(&image_index_entry.image.config, &request.args)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "couldn't make runtime spec"))?;

    let pe_config = peinit::Config {
        timeout: timeout,
        oci_runtime_config: serde_json::to_string(&runtime_spec)?,
        uid_gid: perunner::UID,
        nids: perunner::NIDS,
        stdin: request.stdin,
        strace: false,
        crun_debug: false,
        rootfs_dir: image_index_entry.image.rootfs.clone(),
        rootfs_kind: image_index_entry.rootfs_kind,
    };

    let ntf = NamedTempFile::from_parts(f, tempfile::TempPath::from_path(input.file));
    let worker_input = worker::Input {
        id: 0,
        pe_config: pe_config,
        ch_config: ch_config.clone(),
        ch_timeout: ch_timeout,
        io_file: ntf,
        rootfs: image_index_entry.path.clone().into(),
    };

    let worker_output = {
        match worker::run(worker_input) {
            Ok(o) => o,
            Err(postmortem) => {
                // todo print logs
                return Err(io::Error::new(io::ErrorKind::InvalidData, "error running ch"));
            }
        }
    };
    Ok(WorkerOutput{ status: 200 })
}

fn main() {
    let args = Args::parse();

    let cwd = std::env::current_dir().unwrap();
    let ch_config = CloudHypervisorConfig {
        bin      : cwd.join(args.ch).into(),
        kernel   : cwd.join(args.kernel).into(),
        initramfs: cwd.join(args.initramfs).into(),
        log_level: Some(ChLogLevel::Warn),
        console  : false,
        keep_args: true,
        event_monitor: false,
    };

    eprintln!("worker {} starting up", args.id);

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

    let mut buf = [0; 1024];
    let mut request_buf = vec![0; 4096];

    // TODO cpuset

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
                        match handle_input(&ch_config, &image_index, &mut request_buf, inp) {
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

