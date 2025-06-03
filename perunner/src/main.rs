use std::ffi::OsString;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use byteorder::{WriteBytesExt, LE};
use clap::Parser;
use memmap2::{Mmap, MmapOptions};
use oci_spec::image::{Arch, Os};

use pearchive::{pack_dir_to_writer, unpack_visitor, UnpackVisitor};
use peimage::index::{PEImageMultiIndex, PEImageMultiIndexKeyType};
use peinit::ResponseFormat;

use perunner::cloudhypervisor::{ChLogLevel, CloudHypervisorConfig, PathBufOrOwnedFd};
use perunner::create_runtime_spec;
use perunner::iofile::IoFileBuilder;
use perunner::worker;

//fn sha2_hex(buf: &[u8]) -> String {
//    use sha2::{Sha256,Digest};
//    use base16ct;
//    let hash = Sha256::digest(&buf);
//    base16ct::lower::encode_string(&hash)
//}

// this is kinda dupcliated with pearchive::packdev
// TODO this AsRawFd trait stems from pearchive which stems from using libc apis, I think they can
// be replaced and we just need AsFd
fn create_pack_file_from_dir<P: AsRef<Path>, W: Write + AsFd + Seek>(
    dir: &Option<P>,
    mut file: W,
    config: &peinit::Config,
) -> W {
    peinit::write_io_file_config(&mut file, config, 0).unwrap();
    if let Some(dir) = dir {
        let archive_start_pos = file.stream_position().unwrap();
        let mut file = pack_dir_to_writer(dir.as_ref(), file).unwrap();
        let archive_end_pos = file.stream_position().unwrap();
        let size: u32 = (archive_end_pos - archive_start_pos).try_into().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_u32::<LE>(size).unwrap();
        file
    } else {
        file
    }
}

fn escape_bytes(input: &[u8], output: &mut Vec<u8>) {
    output.clear();
    for b in input {
        match *b {
            b'\n' | b'\t' | b'\'' => output.push(*b),
            _ => {
                for e in std::ascii::escape_default(*b) {
                    output.push(e);
                }
            }
        }
    }
}

fn write_escaped<W: Write>(r: &[u8], w: &mut W) {
    let mut cur = r;
    let mut ebuf = Vec::with_capacity(8192);
    while !cur.is_empty() {
        let (l, r) = cur.split_at(std::cmp::min(cur.len(), 4096));
        escape_bytes(l, &mut ebuf);
        w.write_all(ebuf.as_slice()).unwrap();
        cur = r;
    }
}

struct UnpackVisitorPrinter {
    stdout: bool,
}

impl UnpackVisitor for UnpackVisitorPrinter {
    fn on_file(&mut self, name: &Path, data: &[u8]) -> bool {
        if self.stdout && AsRef::<Path>::as_ref(name) == AsRef::<Path>::as_ref("stdout") {
            write_escaped(data, &mut io::stdout());
        } else {
            eprintln!("=== {:?} ({}) ===", name, data.len());
            write_escaped(data, &mut io::stderr());
        }
        true
    }
}

fn dump_archive(mmap: &Mmap, stdout: bool) {
    let mut visitor = UnpackVisitorPrinter { stdout: stdout };
    unpack_visitor(mmap.as_ref(), &mut visitor).unwrap();
}

fn dump_file<F: Read>(name: &str, file: &mut F) {
    eprintln!("=== {} ===", name);
    let _ = io::copy(file, &mut io::stderr());
}

fn handle_worker_output(
    output: worker::OutputResult,
    response_format: &ResponseFormat,
    stdout: bool,
) {
    match output {
        Ok(worker::Output {
            io_file,
            ch_logs,
            id,
        }) => {
            let _ = id;
            if let Some(mut err_file) = ch_logs.err_file {
                dump_file("ch err", &mut err_file);
            }
            if let Some(mut log_file) = ch_logs.log_file {
                dump_file("ch log", &mut log_file);
            }
            if let Some(mut con_file) = ch_logs.con_file {
                dump_file("ch con", &mut con_file);
            }

            let mut file = io_file.into_inner();
            let (archive_size, response) = peinit::read_io_file_response(&mut file).unwrap();
            eprintln!("response {:#?}", response);
            match response_format {
                ResponseFormat::JsonV1 => {
                    println!("{}", serde_json::to_string_pretty(&response).unwrap());
                }
                ResponseFormat::PeArchiveV1 => {
                    let mapping = unsafe {
                        MmapOptions::new()
                            .offset(file.stream_position().unwrap())
                            .len(archive_size.try_into().unwrap())
                            .map(&file)
                            .unwrap()
                    };

                    dump_archive(&mapping, stdout);
                }
            }
        }
        Err(e) => {
            if let Some(mut err_file) = e.logs.err_file {
                dump_file("ch err", &mut err_file);
            }
            if let Some(mut log_file) = e.logs.log_file {
                dump_file("ch log", &mut log_file);
            }
            if let Some(mut con_file) = e.logs.con_file {
                dump_file("ch con", &mut con_file);
            }
            eprintln!("oh no something went bad {:?}", e.error);
            if let Some(args) = e.args {
                eprintln!("launched ch with args {:?}", args);
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    // ugh where should things get stored
    #[arg(long, default_value = "../cloud-hypervisor-static")]
    ch: OsString,

    #[arg(long, default_value = "../vmlinux")]
    kernel: OsString,

    #[arg(long, default_value = "../target/debug/initramfs")]
    initramfs: OsString,

    #[arg(long)]
    index: Option<String>,

    #[arg(long)]
    image_service: Option<String>,

    #[arg(long, default_value = "index.docker.io/library/busybox:1.36.0")]
    image: String,

    #[arg(long, help = "name of dir to use as input dir")]
    input: Option<PathBuf>,

    #[arg(long, help = "name of file in input dir to use as stdin")]
    stdin: Option<String>,

    #[arg(
        long,
        default_value_t = 1000,
        help = "timeout (ms) crun waits for the container"
    )]
    timeout: u64,

    #[arg(
        long,
        default_value_t = 200,
        help = "timeout (ms) the host waits in addition to timeout"
    )]
    ch_timeout: u64,

    #[arg(long, help = "enable ch console")]
    console: bool,

    #[arg(long, help = "enable ch event-monitor")]
    event_monitor: bool,

    #[arg(long, default_value = "warn", help = "ch log level")]
    ch_log_level: String,

    #[arg(long, help = "strace the crun")]
    strace: bool,

    #[arg(long, help = "pass --debug to crun")]
    crun_debug: bool,

    #[arg(long, help = "just build the spec and exit")]
    spec_only: bool,

    #[arg(long, help = "print some stuff to console about the kernel")]
    kernel_inspect: bool,

    #[arg(long, help = "use json output format")]
    json: bool,

    #[arg(long, help = "pipe stdout through")]
    stdout: bool,

    #[arg(long, default_value_t = 0, help = "num workers to run")]
    parallel: u64,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() {
    let args = {
        let mut args = Args::parse();
        if args.strace || args.crun_debug {
            args.console = true;
        }
        args
    };
    if args.index.is_some() && args.image_service.is_some() {
        eprintln!("--index and --image-service can't both be some");
        std::process::exit(1);
    }
    let ch_log_level: ChLogLevel = args.ch_log_level.as_str().try_into().unwrap();
    let cwd = std::env::current_dir().unwrap();

    // let subscriber = tracing_subscriber::fmt()
    //     .with_max_level(Level::TRACE)
    //     .finish();
    // tracing::subscriber::set_global_default(subscriber)
    //     .expect("setting default subscriber failed");
    //

    // bit nasty but trying to preserve handling of old multi-image images and new images from
    // image service (at least temporarily
    let (config, rootfs_dir, image_path_or_fd, manifest_digest) = {
        if let Some(index_path) = args.index {
            let mut index = PEImageMultiIndex::new(PEImageMultiIndexKeyType::Name);
            index
                .add_path(&index_path)
                .expect("failed to create image index");
            if let Some(image_index_entry) = index.get(&args.image) {
                let config: peoci::spec::ImageConfiguration =
                    (&image_index_entry.image.config).try_into().unwrap();
                if args.spec_only {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&image_index_entry.image.config).unwrap()
                    );
                }
                if false {
                    let fd: OwnedFd = std::fs::File::open(&image_index_entry.path).unwrap().into();
                    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).unwrap();
                    //let path = PathBuf::from(format!("/dev/fd/{}", fd.as_raw_fd()));
                    (
                        config,
                        Some(image_index_entry.image.rootfs.clone()),
                        PathBufOrOwnedFd::Fd(fd),
                        image_index_entry.image.id.digest.clone(),
                    )
                } else {
                    (
                        config,
                        Some(image_index_entry.image.rootfs.clone()),
                        PathBufOrOwnedFd::PathBuf(image_index_entry.path.clone()),
                        image_index_entry.image.id.digest.clone(),
                    )
                }
            } else {
                eprintln!(
                    "image {} not found in the index; available images are: ",
                    args.image
                );
                for (k, v) in index.map() {
                    eprintln!("  {} {}", k, v.image.id.digest);
                }
                panic!("image not present");
            }
        } else if let Some(image_service) = args.image_service {
            let request =
                peimage_service::Request::new(&args.image, &Arch::Amd64, &Os::Linux).unwrap();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .unwrap();
            let res = rt
                .block_on(peimage_service::request_erofs_image(image_service, request))
                .unwrap();
            if args.spec_only {
                println!("{:?}", res.config);
            }
            //rustix::io::fcntl_setfd(&res.fd, rustix::io::FdFlags::empty()).unwrap();

            (
                res.config,
                None,
                PathBufOrOwnedFd::Fd(res.fd),
                res.manifest_digest,
            )
        } else {
            panic!("--index and --image-service can't both be none");
        }
    };
    println!("{:?} {:?} {:?}", config, rootfs_dir, image_path_or_fd);

    let response_format = match args.json {
        true => ResponseFormat::JsonV1,
        false => ResponseFormat::PeArchiveV1,
    };

    let timeout = Duration::from_millis(args.timeout);
    let ch_timeout = timeout + Duration::from_millis(args.ch_timeout);

    let env = None;
    let runtime_spec = create_runtime_spec(&config, Some(&[]), Some(&args.args), env).unwrap();

    if args.spec_only {
        println!("{}", serde_json::to_string_pretty(&runtime_spec).unwrap());
        return;
    }

    let ch_config = CloudHypervisorConfig {
        bin: cwd.join(&args.ch).into(),
        kernel: cwd.join(&args.kernel).into(),
        initramfs: cwd.join(&args.initramfs).into(),
        log_level: Some(ch_log_level),
        console: args.console,
        keep_args: true,
        event_monitor: args.event_monitor,
    };

    let pe_config = peinit::Config {
        timeout: timeout,
        oci_runtime_config: serde_json::to_string(&runtime_spec).unwrap(),
        stdin: args.stdin,
        strace: args.strace,
        crun_debug: args.crun_debug,
        rootfs_dir: rootfs_dir,
        rootfs_kind: peinit::RootfsKind::Erofs,
        response_format: response_format,
        kernel_inspect: args.kernel_inspect,
        manifest_digest,
    };

    if args.parallel > 0 {
        let num_workers = args.parallel as usize;
        let cpus = worker::cpuset(2, num_workers, 2).expect("couldn't make cpuset");
        let mut pool = worker::Pool::new(&cpus);
        for id in 0..args.parallel {
            let io_file = {
                let builder = create_pack_file_from_dir(
                    &args.input,
                    IoFileBuilder::new().unwrap(),
                    &pe_config,
                );
                builder.finish().unwrap()
            };
            let worker_input = worker::Input {
                id: id,
                ch_config: ch_config.clone(),
                ch_timeout: ch_timeout,
                io_file: io_file,
                image: image_path_or_fd.try_clone().unwrap(),
            };
            pool.sender()
                .try_send(worker_input)
                .expect("couldn't submit work");
        }
        for id in 0..args.parallel {
            println!("hi trying to get work for {id}");
            let output = pool
                .receiver()
                .recv_timeout(ch_timeout)
                .expect("should have gotten a response by now");
            handle_worker_output(output, &response_format, args.stdout);
        }
        let pool = pool.close_sender();
        let _ = pool.shutdown();
    } else {
        let io_file = {
            let builder =
                create_pack_file_from_dir(&args.input, IoFileBuilder::new().unwrap(), &pe_config);
            builder.finish().unwrap()
        };
        //std::fs::copy(io_file.path(), "/tmp/perunner-io-file").unwrap();
        let worker_input = worker::Input {
            id: 0,
            ch_config: ch_config,
            ch_timeout: ch_timeout,
            io_file: io_file,
            image: image_path_or_fd,
        };
        handle_worker_output(worker::run(worker_input), &response_format, args.stdout);
    }
}
