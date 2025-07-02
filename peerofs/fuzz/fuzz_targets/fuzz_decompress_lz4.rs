#![no_main]

use std::fs;
use std::process::Command;

use libfuzzer_sys::fuzz_target;
use memmap2::MmapOptions;
use tempfile::{tempdir, NamedTempFile};

use peerofs::disk::{Erofs, Layout};

// TODO: not sure how effective this is, I have tried the -len_control flag to get it to use longer
// inputs but still seems to use short ones. Maybe it is better to take a sequence of Arbitrary Ops
// that are like len, kind, seed, where kind is one of Random, Repeat, Cycle, or something so that
// a small number of ops can produce a much bigger output.
// Basically I'm trying to produce outputs which have the property that some sections are well
// compressed so they end up in a pcluster spanning multiple lclusters of varying length, other
// sections become Plain literal blocks and all of this spanning different parts of the block size
// boundaries

fuzz_target!(|data: Vec<u8>| {
    let mut data = data;
    // this is enough to (almost?) always trigger compression
    for i in 0..4200 {
        data.push(i as u8);
    }
    println!("data len {}", data.len());
    let dir = tempdir().unwrap();
    let dest = NamedTempFile::new().unwrap();

    let filename = "file";
    let file = dir.path().join(&filename);
    fs::write(&file, &data).unwrap();

    let out = Command::new("mkfs.erofs")
        .arg(dest.path())
        .arg(dir.path())
        .arg(format!("-zlz4"))
        .arg("-b4096")
        .arg("-Elegacy-compress")
        .output()
        .unwrap();
    assert!(out.status.success());

    let mmap = unsafe { MmapOptions::new().map(&dest).unwrap() };
    let erofs = Erofs::new(&mmap).unwrap();

    let inode = erofs.lookup(&filename).unwrap().unwrap();
    let data_out = match inode.layout() {
        Layout::FlatInline | Layout::FlatPlain => {
            let (head, tail) = erofs.get_data(&inode).unwrap();
            [head, tail].concat()
        }
        Layout::CompressedFull => {
            erofs.get_compressed_data_vec(&inode).unwrap()
        }
        l => {
            panic!("not expecting layout {:?}", l)
        }
    };
    assert!(data == data_out);
});
