use std::time::Duration;
use std::fs::File;
use std::io;
use std::io::{Write,Seek,SeekFrom,Read};
use std::ffi::OsString;
use std::path::Path;

use pearchive::pack_dir_to_file;
use peinit::{Config,Response};
use waitid_timeout::WaitIdDataOvertime;

use oci_spec::runtime as oci_runtime;
use oci_spec::image as oci_image;
use serde_json;
use bincode;
use byteorder::{WriteBytesExt,ReadBytesExt,LE};

mod cloudhypervisor;
use crate::cloudhypervisor::{CloudHypervisor,CloudHypervisorConfig};

const PMEM_ALIGN_SIZE: u64 = 0x20_0000; // 2 MB

#[derive(Debug)]
enum Error {
    Stat,
    Truncate,
}

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

fn round_up_to_pmem_size(f: &File) -> io::Result<u64> {
    let cur = f.metadata()?.len();
    let newlen = round_up_to::<PMEM_ALIGN_SIZE>(cur);
    let _ = f.set_len(newlen)?;
    Ok(newlen)
}

// ImageConfiguration: {created, architecture, os, config: {Env, User, Entrypoint, Cmd, WorkingDir}, rootfs, ...}
// RuntimeSpec: {process: {terminal, user: {uid, gid}, args, env, cwd, capabilities, ...}

// the allocations in this make me a bit unhappy, but maybe its all worth it
fn create_runtime_spec(image_config: &oci_image::ImageConfiguration, run_args: &[String]) -> Option<oci_runtime::Spec> {
    //let spec: oci_runtime::Spec = Default::default();
    let mut spec = oci_runtime::Spec::rootless(1000, 1000);

    // sanity checks
    if *image_config.architecture() != oci_image::Arch::Amd64 { return None; }
    if *image_config.os() != oci_image::Os::Linux { return None; }

    // TODO how does oci-spec-rs deserialize the config .Env into .env ?

    // TODO add tmpfs of /tmp
    //      add the bind mounts of /run/{input,output}
    //      uid mapping isn't quite right, getting lots of nobody/nogroup
    //      which is because our uid_map only maps 1000 to 0, but the podman map
    //      maps 65k uids from 1- (starting at host 52488, which is my host subuid)

    // we "know" that a defaulted runtime spec has Some process
    let process = spec.process_mut().as_mut().unwrap();

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

        // TODO will take args from user here as well
        // what is with some things having set_ and some having _mut ??

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
fn create_pack_file_from_dir<P1: AsRef<Path>, P2: AsRef<Path>>(dir: P1, file: P2, config: &Config) {
    let mut f = File::create(file).unwrap();
    let config_bytes = bincode::serialize(&config).unwrap();
    if true {
        let hash_hex = sha2_hex(&config_bytes);
        println!("HOST config_bytes len {} {}", config_bytes.len(), hash_hex);
    }
    let config_size: u32 = config_bytes.len().try_into().unwrap();
    f.write_u32::<LE>(0).unwrap(); // or seek
    f.write_u32::<LE>(config_size).unwrap();
    f.write_all(config_bytes.as_slice()).unwrap();
    let archive_start_pos = f.stream_position().unwrap();
    let mut f = pack_dir_to_file(dir.as_ref(), f).unwrap();
    let archive_end_pos = f.stream_position().unwrap();
    let size: u32 = (archive_end_pos - archive_start_pos).try_into().unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_u32::<LE>(size).unwrap();
    let _ = round_up_to_pmem_size(&f).unwrap();
}

fn write_escaped<W: Write, R: Read>(r: &mut R, size: u32, w: &mut W) {
    let mut rem = size as usize;
    let mut buf = vec![0; 4096];
    let mut ebuf = vec![0; 8192];
    while rem > 0 {
        let read = r.read(&mut buf).unwrap();
        if read <= 0 { panic!("bad read"); }
        // for an oversized file we might get back junk data
        let i = std::cmp::min(read, rem);
        let data = &buf[..i];
        assert!(i <= rem);
        rem -= i;
        let n = escape_bytes::escape_into(&mut ebuf, data).unwrap();
        w.write_all(&ebuf[..n]).unwrap();
    }
}

fn main() {

    let ch_binpath:     OsString = "/home/andrew/Repos/program-explorer/cloud-hypervisor-static".into();
    let kernel_path:    OsString = "/home/andrew/Repos/linux/vmlinux".into();
    let initramfs_path: OsString = "/home/andrew/Repos/program-explorer/initramfs".into();
    let rootfs:         OsString = "/home/andrew/Repos/program-explorer/gcc-14.1.0.sqfs".into();
    let inputdir:       OsString = "/home/andrew/Repos/program-explorer/inputdir".into();
    let image_spec:     OsString = "/home/andrew/Repos/program-explorer/gcc-14.1.0-image-spec.json".into();

    let mut ch = CloudHypervisor::start(CloudHypervisorConfig {
        workdir: "/tmp".into(),
        bin: ch_binpath,
        kernel: kernel_path,
        initramfs: initramfs_path,
        log: true,
        console: true,
    }).unwrap();

    { // pmem0
        let pmemconfig = format!(r#"{{"file": {:?}, "discard_writes": true}}"#, rootfs);
        println!("{}", pmemconfig);
        let resp = ch.api("PUT", "vm.add-pmem", Some(&pmemconfig));
        println!("{resp:?}");
    }

    use std::env;
    let args: Vec<_> = env::args().collect();
    let run_args = &args[1..];

    let image_spec = oci_image::ImageConfiguration::from_file(image_spec).unwrap();
    let runtime_spec = create_runtime_spec(&image_spec, run_args).unwrap();
    //println!("{}", serde_json::to_string_pretty(runtime_spec.process().as_ref().unwrap()).unwrap());
    println!("{}", serde_json::to_string_pretty(&runtime_spec).unwrap());

    let timeout = Duration::from_millis(1000);
    let ch_timeout = timeout + Duration::from_millis(200);
    let config = Config {
        timeout: timeout,
        oci_runtime_config: serde_json::to_string(&runtime_spec).unwrap()
    };

    { // pmem1
        let io_file = ch.workdir().join("io");
        create_pack_file_from_dir(&inputdir, &io_file, &config);
        // { let len = File::open(&io_file).unwrap().metadata().unwrap().len(); println!("perunner file has len: {}", len); }
        let pmemconfig = format!(r#"{{"file": {:?}, "discard_writes": false}}"#, io_file);
        println!("{}", pmemconfig);
        let resp = ch.api("PUT", "vm.add-pmem", Some(&pmemconfig));
        println!("{resp:?}");
    }

    match ch.wait_timeout_or_kill(ch_timeout) {
        Ok(WaitIdDataOvertime::NotExited) => {
            println!("HOST warning ch didn't exit, this is real bad!");
            ch.kill().unwrap();
        }
        Ok(WaitIdDataOvertime::Exited{..}) => {
            println!("HOST ch exited on time");
        }
        Ok(WaitIdDataOvertime::ExitedOvertime{..}) => {
            println!("HOST ch ran over time and was successfully killed");
        }
        Err(e) => {
            println!("HOST warning ch ran into an error waiting {e:?}");
        }
        // Some(status) => println!("exited with status {status:?}"),
        // None => println!("either didn't exit or got killed"),
    }
    //let status = ch.status();

    // okay so we also have to determine whether peinit exited okay
    // b/c if not then the archive and possibly the Response is messed up
    // and cloud-hypervisor will exit okay
    //

    println!("== log ==");
    let _ = io::copy(&mut File::open(ch.log_file().unwrap()).unwrap(), &mut io::stdout());
    println!("== console ==");
    let _ = io::copy(&mut File::open(ch.console_file().unwrap()).unwrap(), &mut io::stdout());

    println!("== archive out ==");
    {
        let io_filepath = ch.workdir().join("io");
        std::fs::copy(&io_filepath, "/tmp/pe-io").unwrap();
        // let mut buf = Vec::with_capacity(4096);
        let mut file = File::open(io_filepath).unwrap();
        let archive_size = file.read_u32::<LE>().unwrap();
        let response_size = file.read_u32::<LE>().unwrap();
        // todo wtf is going on with the options
        //let response: Response = bincode::options()
        //    .with_fixint_encoding()
        //    .allow_trailing_bytes()
        //    .with_limit(response_size.into())
        //    .deserialize_from(&mut file)
        //    .unwrap();
        println!("HOST archive size {archive_size} response_size {response_size}");
        let response: Response = {
            let mut buf = vec![0; response_size.try_into().unwrap()];
            file.read_exact(buf.as_mut_slice()).unwrap();

            if true {
                let hash_hex = sha2_hex(&buf);
                println!("HOST response_bytes len {} {}", response_size, hash_hex);
            }
            bincode::deserialize(&buf).unwrap()
        };
        println!("HOST got response {response:#?}");
        println!("== archvive raw ==");
        write_escaped(&mut file, archive_size, &mut io::stdout());
        println!("\n== /archvive raw ==");
    }

    // use std::process::Command;
    // use peinit::Rusage;
    // use waitid_timeout::{WaitIdData,ChildWaitIdExt};
    // match Command::new("gcc").arg("-v").spawn().unwrap().wait_timeout(Duration::from_millis(100)) {
    //     Ok(WaitIdData::Exited{rusage,..}) => {
    //         let rusage: Rusage = rusage.into();
    //         println!("host gcc -v {rusage:#?}");
    //     }
    //     _ => todo!()
    // }
}
