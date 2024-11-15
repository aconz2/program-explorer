use std::env;
use std::path::Path;
use std::fs::File;
use std::io::{Seek,SeekFrom};

use pearchive::{
    pack_dir_to_file,
    unpack_file_to_dir_with_unshare_chroot,
    unpack_data_to_dir_with_unshare_chroot,
};

use memmap2::MmapOptions;
use byteorder::{WriteBytesExt,ReadBytesExt,LE};

#[derive(Debug)]
enum Error {
    MissingArg,
    Mmap,
}


/// args: <input dir> <output file>
fn pack(args: &[String]) {
    let indir = args.get(0).ok_or(Error::MissingArg).unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();
    let indirpath = Path::new(indir);
    assert!(indirpath.is_dir(), "{:?} should be a dir", indirpath);

    let fileout = File::create(outname).unwrap();

    pack_dir_to_file(indirpath, fileout).unwrap();
}

/// args: <input file> <output dir>
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

/// args: <size> <output dir> <offset> <len>
fn unpackdev(args: &[String]) {
    let inname = args.get(0).ok_or(Error::MissingArg).unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();

    let offset: u64 = args.get(2).ok_or(Error::MissingArg).unwrap().parse::<u64>().unwrap();
    let len:    u64 = args.get(3).ok_or(Error::MissingArg).unwrap().parse::<u64>().unwrap();

    let inpath = Path::new(&inname);
    let outpath = Path::new(&outname);

    assert!(outpath.is_dir(), "{:?} should be a dir", outpath);

    let file = File::open(inpath).unwrap();
    let mmap = unsafe {
        MmapOptions::new()
            .offset(offset)
            .len(len.try_into().unwrap()) // when does a u64 not fit in usize????
            .map(&file)
            .map_err(|_| Error::Mmap)
            .unwrap()
    };

    unpack_data_to_dir_with_unshare_chroot(mmap.as_ref(), outpath).unwrap();
}

/// args: <input dir> <output file> <offset>
fn packdev(args: &[String]) {
    let indir = args.get(0).ok_or(Error::MissingArg).unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();
    let indirpath = Path::new(indir);
    assert!(indirpath.is_dir(), "{:?} should be a dir", indirpath);

    let offset: u64 = args.get(2).ok_or(Error::MissingArg).unwrap().parse::<u64>().unwrap();
    println!("packdev starting at offset {offset}");

    let mut fileout = File::create(outname).unwrap();
    fileout.seek(SeekFrom::Start(offset)).unwrap();

    // its a bit quirky that we move fileout in and get it back out, which should be the same as an
    // &mut, but then the type of BufWriter<&mut File> gets weird and I don't know what to do
    let mut fileout = pack_dir_to_file(indirpath, fileout).unwrap();

    let ending_offset = fileout.stream_position().unwrap();
    assert!(ending_offset > offset);
    let archive_size = ending_offset - offset;
    let encoded_size: u32 = archive_size.try_into().unwrap();
    fileout.seek(SeekFrom::Start(0)).unwrap();
    fileout.write_u32::<LE>(encoded_size).unwrap();
    println!("packdev archive_size {encoded_size}");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("pack")      => {      pack(&args[2..]); },
        Some("unpack")    => {    unpack(&args[2..]); },
        Some("packdev")    => {  packdev(&args[2..]); },
        Some("unpackdev") => { unpackdev(&args[2..]); },
        _ => {
            println!("pack <input-dir> <output-file>");
            println!("unpack <input-file> <output-dir>");
            println!("packdev <input-file> <output-dir> <offset>");
            println!("unpackdev <input-file> <output-dir> <offset> <len>");
            std::process::exit(1);
        }
    }
}
