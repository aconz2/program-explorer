use std::env;
use std::path::Path;
use std::fs::File;

use pearchive::{pack_dir_to_file,unpack_file_to_dir_with_unshare_chroot};

#[derive(Debug)]
enum Error {
    MissingArg,
}


/// args: <input dir> <output file>
fn pack(args: &[String]) {
    let indir = args.get(0).ok_or(Error::MissingArg).unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();
    let indirpath = Path::new(indir);
    assert!(indirpath.is_dir(), "{:?} should be a dir", indirpath);

    let fileout = File::create(outname).unwrap();

    let _ = pack_dir_to_file(indirpath, fileout).unwrap();
}

/// args: <input file> <output dir>
fn unpack(args: &[String]) {
    let inname = args.get(0).ok_or(Error::MissingArg).unwrap();
    let outname = args.get(1).ok_or(Error::MissingArg).unwrap();

    let inpath = Path::new(&inname);
    let outpath = Path::new(&outname);
    assert!(inpath.is_file(), "{:?} should be a file", inpath);
    assert!(outpath.is_dir(), "{:?} should be a dir", outpath);

    let file = File::open(inpath).unwrap();

    unpack_file_to_dir_with_unshare_chroot(file, outpath).unwrap();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("pack")   => {   pack(&args[2..]); },
        Some("unpack") => { unpack(&args[2..]); },
        _ => {
            println!("pack <input-dir> <output-file>");
            println!("unpack <input-file> <output-dir>");
        }
    }
}
