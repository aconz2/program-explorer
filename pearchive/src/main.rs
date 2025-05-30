use std::env;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::FromRawFd;
use std::path::Path;

use pearchive::{
    pack_dir_to_file, unpack_data_to_dir_with_unshare_chroot,
    unpack_file_to_dir_with_unshare_chroot,
};

use byteorder::{WriteBytesExt, LE};
use memmap2::MmapOptions;

#[derive(Debug)]
enum Error {
    MissingArg,
    Mmap,
}

/// args: <input dir> <output file>
#[allow(clippy::get_first)]
fn pack(args: &[String]) {
    let indir = args.get(0).ok_or(Error::MissingArg).unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();
    let indirpath = Path::new(indir);
    assert!(indirpath.is_dir(), "{:?} should be a dir", indirpath);

    let fileout = File::create(outname).unwrap();

    pack_dir_to_file(indirpath, fileout).unwrap();
}

/// args: <input file> <output dir>
#[allow(clippy::get_first)]
fn unpack(args: &[String]) {
    let inname = args.get(0).ok_or(Error::MissingArg).unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();

    let inpath = Path::new(&inname);
    let outpath = Path::new(&outname);
    // this fails when we try to use /dev/pmem
    // assert!(inpath.is_file(), "{:?} should be a file", inpath);
    assert!(outpath.is_dir(), "{:?} should be a dir", outpath);

    let file = File::open(inpath).unwrap();

    unpack_file_to_dir_with_unshare_chroot(file, outpath).unwrap();
}

/// args: <input fd> <output dir> <len>
/// uses stream offset as beginning of map
#[allow(clippy::get_first)]
fn unpackfd(args: &[String]) {
    let in_fd = args
        .get(0)
        .ok_or(Error::MissingArg)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();
    let len = args
        .get(2)
        .ok_or(Error::MissingArg)
        .unwrap()
        .parse::<usize>()
        .unwrap();

    let outpath = Path::new(&outname);

    assert!(outpath.is_dir(), "{:?} should be a dir", outpath);

    let mut file = unsafe { File::from_raw_fd(in_fd) };
    let offset = file.stream_position().unwrap();

    let mmap = unsafe {
        MmapOptions::new()
            .offset(offset)
            .len(len)
            .map(&file)
            .map_err(|_| Error::Mmap)
            .unwrap()
    };

    unpack_data_to_dir_with_unshare_chroot(mmap.as_ref(), outpath).unwrap();
}

/// args: <input dir> <output fd>
#[allow(clippy::get_first)]
fn packfd(args: &[String]) {
    let indir = args.get(0).ok_or(Error::MissingArg).unwrap();
    let out_fd = args
        .get(1)
        .ok_or(Error::MissingArg)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let indirpath = Path::new(indir);
    assert!(indirpath.is_dir(), "{:?} should be a dir", indirpath);

    let mut fileout = unsafe { File::from_raw_fd(out_fd) };
    let offset = fileout.stream_position().unwrap();

    // its a bit quirky that we move fileout in and get it back out, which should be the same as an
    // &mut, but then the type of BufWriter<&mut File> gets weird and I don't know what to do
    let mut fileout = pack_dir_to_file(indirpath, fileout).unwrap();

    let ending_offset = fileout.stream_position().unwrap();
    assert!(ending_offset > offset);
    let archive_size = ending_offset - offset;
    let encoded_size: u32 = archive_size.try_into().unwrap();
    fileout.seek(SeekFrom::Start(0)).unwrap();
    fileout.write_u32::<LE>(encoded_size).unwrap();
    // this is to be extra sure the write through the pmem device has finished
    // only hit a bad case in the panic handler's write not getting sync'd
    fileout.sync_data().unwrap();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("pack") => {
            pack(&args[2..]);
        }
        Some("unpack") => {
            unpack(&args[2..]);
        }
        Some("packfd") => {
            packfd(&args[2..]);
        }
        Some("unpackfd") => {
            unpackfd(&args[2..]);
        }
        _ => {
            println!("pack <input-dir> <output-file>");
            println!("unpack <input-file> <output-dir>");
            println!("packdev <input-file> <output-dir>");
            println!("unpackfd <input-fd> <output-dir> <len>");
            std::process::exit(1);
        }
    }
}
