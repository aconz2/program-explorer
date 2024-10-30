use std::time::Duration;
use std::fs::File;
use std::io;
use std::io::{Write,Seek,SeekFrom,Read};
use std::ffi::{OsStr,OsString};
use std::path::{Path,PathBuf};
use std::process::Command;

use tempfile::NamedTempFile;
use oci_spec::runtime as oci_runtime;
use oci_spec::image as oci_image;
use serde_json;
use bincode;
use byteorder::{WriteBytesExt,ReadBytesExt,LE};
use memmap2::{Mmap,MmapOptions};
use clap::{Parser};

use pearchive::{pack_dir_to_file,UnpackVisitor,unpack_visitor};
use peinit;
use peinit::{Response};
use peimage::PEImageIndex;

mod cloudhypervisor;
use crate::cloudhypervisor::{CloudHypervisorConfig,ChLogLevel};

mod worker;
// use crate::worker;

const PMEM_ALIGN_SIZE: u64 = 0x20_0000; // 2 MB
const UID: u32 = 1000;
const NIDS: u32 = 1000; // size of uid_gid_map

fn sha2_hex(buf: &[u8]) -> String {
    use sha2::{Sha256,Digest};
    use base16ct;
    let hash = Sha256::digest(&buf);
    base16ct::lower::encode_string(&hash)
}

fn round_up_to<const N: u64>(x: u64) -> u64 {
    if x == 0 { return N; }
    ((x + (N - 1)) / N) * N
}

fn round_up_file_to_pmem_size(f: &File) -> io::Result<u64> {
    let cur = f.metadata()?.len();
    let newlen = round_up_to::<PMEM_ALIGN_SIZE>(cur);
    if cur != newlen {
        let _ = f.set_len(newlen)?;
    }
    Ok(newlen)
}

// ImageConfiguration: {created, architecture, os, config: {Env, User, Entrypoint, Cmd, WorkingDir}, rootfs, ...}
// RuntimeSpec: {process: {terminal, user: {uid, gid}, args, env, cwd, capabilities, ...}

// the allocations in this make me a bit unhappy, but maybe its all worth it
fn create_runtime_spec(image_config: &oci_image::ImageConfiguration, run_args: &[String]) -> Option<oci_runtime::Spec> {
    //let spec: oci_runtime::Spec = Default::default();
    let mut spec = oci_runtime::Spec::rootless(1000, 1000);
    // ugh this api is horrible
    spec.set_hostname(Some("programexplorer".to_string()));


    // doing spec.set_uid_mappings sets the volume mount idmap, not the user namespace idmap
    if true {
        let map = oci_runtime::LinuxIdMappingBuilder::default()
            .host_id(UID)
            .container_id(0u32)
            .size(NIDS)
            .build()
            .unwrap();
        let linux = spec.linux_mut().as_mut().unwrap();
        linux
            .set_uid_mappings(Some(vec![map]))
            .set_gid_mappings(Some(vec![map]));
    }

    // sanity checks
    if *image_config.architecture() != oci_image::Arch::Amd64 { return None; }
    if *image_config.os() != oci_image::Os::Linux { return None; }

    // TODO how does oci-spec-rs deserialize the config .Env into .env ?

    // TODO add tmpfs of /tmp
    //      add the bind mounts of /run/{input,output}
    //      uid mapping isn't quite right, getting lots of nobody/nogroup
    //      which is because our uid_map only maps 1000 to 0, but the podman map
    //      maps 65k uids from 1- (starting at host 52488, which is my host subuid)

    // we "know" that a defaulted runtime spec has Some mounts
    {
        let mounts = spec.mounts_mut().as_mut().unwrap();

        // /tmp
        mounts.push(oci_runtime::MountBuilder::default()
            .destination("/tmp")
            .typ("tmpfs")
            .options(vec!["size=50%".into(), "mode=777".into()])
            .build()
            .unwrap()
            );

        // /run/pe/input
        mounts.push(oci_runtime::MountBuilder::default()
            .destination("/run/pe/input")
            .typ("bind")
            .source("/run/input")
            // idk should this be readonly?
            // TODO I don't fully understand why this is rbind
            // https://docs.kernel.org/filesystems/sharedsubtree.html
            .options(vec!["rw".into(), "rbind".into()])
            .build()
            .unwrap()
            );

        // /run/pe/output
        mounts.push(oci_runtime::MountBuilder::default()
            .destination("/run/pe/output")
            .typ("bind")
            .source("/run/output/dir")
            .options(vec!["rw".into(), "rbind".into()])
            .build()
            .unwrap()
            );
    }

    if let Some(config) = image_config.config() {
        // TODO: handle user
        // from oci-spec-rs/src/image/config.rs
        // user:
        //   For Linux based systems, all
        //   of the following are valid: user, uid, user:group,
        //   uid:gid, uid:group, user:gid. If group/gid is not
        //   specified, the default group and supplementary
        //   groups of the given user/uid in /etc/passwd from
        //   the container are applied.
        // let _ = config.exposed_ports; // ignoring network for now

        // we "know" that a defaulted runtime spec has Some process
        let process = spec.process_mut().as_mut().unwrap();

        if let Some(env) = config.env() {
            *process.env_mut() = Some(env.clone());
        }

        if run_args.is_empty() {
            let args = {
                let mut acc = vec![];
                if let Some(entrypoint) = config.entrypoint() { acc.extend_from_slice(entrypoint); }
                if let Some(cmd) = config.cmd()               { acc.extend_from_slice(cmd); }
                if acc.is_empty() { return None; }
                acc
            };
            process.set_args(Some(args));
        } else {
            process.set_args(Some(run_args.into()));
        }

        if let Some(cwd) = config.working_dir() { process.set_cwd(cwd.into()); }
    }

    Some(spec)
}

// on the wire, the client sends
//     <config size : u32le> <config> <archive>
// and the server reads the config, and writes its own config and the archive size computed from
// the content length
// the packfile input format is
//     <archive size : u32le> <config size : u32le> <config> <archive> <padding>
// the return output format is
//                            |--------- sent to client ---------|
//     <archive size : u32le> [ <config size> <config> <archive> ] <padding>

// how do you get away from this P1 P2 thing
fn create_pack_file_from_dir<P1: AsRef<Path>, P2: AsRef<Path>>(dir: Option<P1>, file: P2, config: &peinit::Config) {
    let mut f = File::create(file).unwrap();
    let config_bytes = bincode::serialize(&config).unwrap();
    if true {
        let hash_hex = sha2_hex(&config_bytes);
        println!("H config_bytes len {} {}", config_bytes.len(), hash_hex);
    }
    let config_size: u32 = config_bytes.len().try_into().unwrap();
    f.write_u32::<LE>(0).unwrap(); // or seek
    f.write_u32::<LE>(config_size).unwrap();
    f.write_all(config_bytes.as_slice()).unwrap();
    let archive_start_pos = f.stream_position().unwrap();
    let mut f = if let Some(dir) = dir {
        pack_dir_to_file(dir.as_ref(), f).unwrap()
    } else {
        f
    };
    let archive_end_pos = f.stream_position().unwrap();
    let size: u32 = (archive_end_pos - archive_start_pos).try_into().unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_u32::<LE>(size).unwrap();
    let _ = round_up_file_to_pmem_size(&f).unwrap();
}

fn escape_bytes(input: &[u8], output: &mut Vec<u8>) {
    output.clear();
    for b in input {
        if *b == b'\n' { output.push(*b) }
        else {
            for e in std::ascii::escape_default(*b) {
                output.push(e);
            }
        }
    }
}

fn write_escaped<W: Write>(r: &[u8], w: &mut W) {
    let mut cur = &r[..];
    let mut ebuf = vec![0; 8192];
    while !cur.is_empty() {
        let (l, r) = cur.split_at(std::cmp::min(cur.len(), 4096));
        escape_bytes(&l, &mut ebuf);
        w.write_all(ebuf.as_slice()).unwrap();
        cur = r;
    }
}

struct UnpackVisitorPrinter { }
impl UnpackVisitor for UnpackVisitorPrinter {
    fn on_file(&mut self, name: &PathBuf, data: &[u8]) -> bool {
        println!("=== {:?} ===", name);
        write_escaped(&data, &mut io::stdout());
        true
    }
}

fn parse_response(mut file: &NamedTempFile) -> (Response, Mmap) {
    file.seek(SeekFrom::Start(0)).unwrap();
    let archive_size = file.read_u32::<LE>().unwrap();
    let response_size = file.read_u32::<LE>().unwrap();

    let response: Response = {
        let mut buf = vec![0; response_size.try_into().unwrap()];
        file.read_exact(buf.as_mut_slice()).unwrap();

        if true {
            let hash_hex = sha2_hex(&buf);
            println!("H response_bytes len {} {}", response_size, hash_hex);
        }
        bincode::deserialize(&buf).unwrap()
    };

    let mapping = unsafe {
        MmapOptions::new()
        .offset((4 + 4 + response_size).into())
        .len(archive_size.try_into().unwrap())
        .map(file)
        .unwrap()
    };

    (response, mapping)
}

fn dump_archive(mmap: &Mmap) {
    let mut visitor = UnpackVisitorPrinter{};
    unpack_visitor(mmap.as_ref(), &mut visitor).unwrap();
}

fn dump_file<F: Read>(name: &str, file: &mut F) {
    println!("=== {} ===", name);
    let _ = io::copy(file, &mut io::stdout());
}
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    // ugh where should things get stored
    #[arg(long, default_value = "../cloud-hypervisor-static")]
    ch: OsString,

    #[arg(long, default_value = "../vmlinux")]
    kernel: OsString,

    #[arg(long, default_value = "../initramfs")]
    initramfs: OsString,

    // #[arg(long, default_value = "../gcc-14.1.0.sqfs")]
    // rootfs: OsString,

    #[arg(long, default_value = "../ocismall.sqfs")]
    index: OsString,

    // TODO
    // #[arg(long, default_value = "index.docker.io/library/busybox:1.36.0")]
    // container: OsString,

    #[arg(long, help = "name of dir to use as input dir")]
    input: Option<PathBuf>,

    #[arg(long, help = "name of file in input dir to use as stdin")]
    stdin: Option<String>,

    #[arg(long, default_value_t = 1000, help = "timeout (ms) crun waits for the container")]
    timeout: u64,

    #[arg(long, default_value_t = 200, help = "timeout (ms) the host waits in addition to timeout")]
    ch_timeout: u64,

    #[arg(long, help = "enable ch console")]
    console: bool,

    #[arg(long, help = "strace the crun")]
    strace: bool,

    #[arg(long, help = "pass --debug to crun")]
    crun_debug: bool,

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
    let cwd = std::env::current_dir().unwrap();

    // TODO This will disappear when we grab the image spec from the sqfs
    // I think we should do that now

    let pe_image_index = PEImageIndex::from_path(&args.index).unwrap();
    println!("index is {:#?}", pe_image_index);

    let image_index_entry = &pe_image_index.images[0];

    let timeout = Duration::from_millis(args.timeout);
    let ch_timeout = timeout + Duration::from_millis(args.ch_timeout);

    let runtime_spec = create_runtime_spec(&image_index_entry.config, &args.args).unwrap();
    //println!("{}", serde_json::to_string_pretty(runtime_spec.process().as_ref().unwrap()).unwrap());
    println!("{}", serde_json::to_string_pretty(&runtime_spec).unwrap());

    let ch_config = CloudHypervisorConfig {
        bin      : cwd.join(args.ch).into(),
        kernel   : cwd.join(args.kernel).into(),
        initramfs: cwd.join(args.initramfs).into(),
        log_level: Some(ChLogLevel::Warn),
        console  : args.console,
        keep_args: true,
    };

    let pe_config = peinit::Config {
        timeout: timeout,
        oci_runtime_config: serde_json::to_string(&runtime_spec).unwrap(),
        uid_gid: UID,
        nids: NIDS,
        stdin: args.stdin,
        strace: args.strace,
        crun_debug: args.crun_debug,
        rootfs_dir: image_index_entry.rootfs.clone(),
    };

    let io_file = NamedTempFile::new().unwrap();
    create_pack_file_from_dir(args.input, &io_file, &pe_config);

    let worker_input = worker::Input {
        id: 0,
        pe_config: pe_config,
        ch_config: ch_config,
        ch_timeout: ch_timeout,
        io_file: io_file,
        rootfs: cwd.join(args.index).into(),
    };

    match worker::run(worker_input) {
        Ok(worker::Output{mut io_file, ch_logs, id}) => {
            let _ = id;
            if let Some(mut err_file) = ch_logs.err_file { dump_file("ch err", &mut err_file); }
            if let Some(mut log_file) = ch_logs.log_file { dump_file("ch log", &mut log_file); }
            if let Some(mut con_file) = ch_logs.con_file { dump_file("ch con", &mut con_file); }

            let (response, archive_map) = parse_response(&mut io_file);
            println!("response {:#?}", response);

            dump_archive(&archive_map);

        }
        Err(e) => {
            println!("oh no something went bad {:?}", e.error);
            if let Some(args) = e.args {
                println!("launched ch with args {:?}", args);
            }
            if let Some(mut err_file) = e.logs.err_file { dump_file("ch err", &mut err_file); }
            if let Some(mut log_file) = e.logs.log_file { dump_file("ch log", &mut log_file); }
            if let Some(mut con_file) = e.logs.con_file { dump_file("ch con", &mut con_file); }
        }
    }
}
