use std::io;
use std::os::fd::AsRawFd;

use serde::{Deserialize,Serialize};
use clap::{Parser};
use nix::sys::socket::{UnixAddr,SockFlag,SockType,AddressFamily,MsgFlags,GetSockOpt,
                       socket,connect,send,recv};
use nix::sys::socket::sockopt::{SndBuf};

use peimage::PEImageMultiIndex;
use perunner::create_runtime_spec;
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

fn handle_input(ch_config: &CloudHypervisorConfig,
                image_index: &PEImageMultiIndex,
                buf: &mut Vec<u8>,
                inp: WorkerInput,
                )
    -> io::Result<WorkerOutput> {

    let timeout = Duration::from_millis(args.timeout);
    let ch_timeout = timeout + Duration::from_millis(args.ch_timeout);

    let file = File::open(inp.file)?;
    let config_size = f.read_u32::<LE>()?;
    if config_size > MAX_CONFIG_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "config size too big"))
    }
    buf.set_len(config_size);
    let config_buf = f.read_exact(&mut buf)?;
    let request: ApiV1iRequest = serde_json::deserialize(&config_buf)?;

    let image_index_entry = image_index.get(&request.image)
        .ok_or_else(|| Err(io::Error::new(io::ErrorKind::InvalidData, "no such image")))?;

    let pe_config = peinit::Config {
        timeout: timeout,
        oci_runtime_config: serde_json::to_string(&runtime_spec).unwrap(),
        uid_gid: worker::UID,
        nids: worker::NIDS,
        stdin: args.stdin,
        strace: args.strace,
        crun_debug: args.crun_debug,
        rootfs_dir: image_index_entry.image.rootfs.clone(),
        rootfs_kind: image_index_entry.rootfs_kind,
    };
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
    //
    let worker_input = worker::Input {
        id: 0,
        pe_config: pe_config,
        ch_config: ch_config.clone(),
        ch_timeout: ch_timeout,
        io_file: io_file,
        rootfs: image_index_entry.path.clone().into(),
    };

    worker::run(worker_input)
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

    let request_buf = vec![0; 4096];

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

