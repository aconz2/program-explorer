use std::time::Duration;
use std::fs::File;
use std::io::{Write,Seek,SeekFrom};
use std::ffi::OsString;
use std::path::Path;

use pearchive::pack_dir_to_file;
use peinit::Config;

use bincode;

mod cloudhypervisor;
use crate::cloudhypervisor::{CloudHypervisor,CloudHypervisorConfig};

const PMEM_ALIGN_SIZE: u64 = 0x20_0000; // 2 MB

#[derive(Debug)]
enum Error {
    Stat,
    Truncate,
}

fn round_up_to<const N: u64>(x: u64) -> u64 {
    if x == 0 { return N; }
    ((x + (N - 1)) / N) * N
}

fn round_up_to_pmem_size(f: &File) -> Result<u64, Error> {
    let cur = f.metadata().map_err(|_| Error::Stat)?.len();
    let newlen = round_up_to::<PMEM_ALIGN_SIZE>(cur);
    let _ = f.set_len(newlen).map_err(|_| Error::Truncate)?;
    Ok(newlen)
}

// on the wire, the client sends
//     <config size : u32le> <config> <archive>
// and the server reads the config, and writes its own config and the archive size computed from
// the content length
// the packfile input format is
//     <archive size : u32le> <config size : u32le> <config> <archive>
// the return output format is
//     <archive size : u32le> <config size> <config> <archive>

// how do you get away from this P1 P2 thing
fn create_pack_file_from_dir<P1: AsRef<Path>, P2: AsRef<Path>>(dir: &P1, file: &P2) {
    let mut f = File::create(file).unwrap();
    let config = Config { };
    let config_bytes = bincode::serialize(&config).unwrap();
    let config_size: u32 = config_bytes.len().try_into().unwrap();
    f.write_all(&[0u8; 4]).unwrap(); // archive size empty
    f.write_all(&(config_size.to_le_bytes())).unwrap(); // config size
    f.write_all(config_bytes.as_slice()).unwrap();
    let archive_start_pos = f.stream_position().unwrap();
    let mut f = pack_dir_to_file(dir.as_ref(), f).unwrap();
    let archive_end_pos = f.stream_position().unwrap();
    let size: u32 = (archive_end_pos - archive_start_pos).try_into().unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&(size.to_le_bytes())).unwrap();
    let _ = round_up_to_pmem_size(&f).unwrap();
}

fn main() {

    let ch_binpath:     OsString = "/home/andrew/Repos/program-explorer/cloud-hypervisor-static".into();
    let kernel_path:    OsString = "/home/andrew/Repos/linux/vmlinux".into();
    let initramfs_path: OsString = "/home/andrew/Repos/program-explorer/initramfs".into();
    let rootfs:         OsString = "/home/andrew/Repos/program-explorer/gcc-14.1.0.sqfs".into();
    let inputdir:       OsString = "/home/andrew/Repos/program-explorer/inputdir".into();

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

    { // pmem1
        let io_file = ch.workdir().join("io");
        create_pack_file_from_dir(&inputdir, &io_file);
        { let len = File::open(&io_file).unwrap().metadata().unwrap().len(); println!("perunner file has len: {}", len); }
        let pmemconfig = format!(r#"{{"file": {:?}, "discard_writes": true}}"#, io_file);
        println!("{}", pmemconfig);
        let resp = ch.api("PUT", "vm.add-pmem", Some(&pmemconfig));
        println!("{resp:?}");
    }

    match ch.wait_timeout_or_kill(Duration::from_secs(1)) {
        Some(status) => println!("exited with status {status:?}"),
        None => println!("either didn't exit or got killed"),
    }
    //let status = ch.status();

    use std::io;
    println!("== log ==");
    let _ = io::copy(&mut File::open(ch.log_file().unwrap()).unwrap(), &mut io::stdout());
    println!("== console ==");
    let _ = io::copy(&mut File::open(ch.console_file().unwrap()).unwrap(), &mut io::stdout());

}
