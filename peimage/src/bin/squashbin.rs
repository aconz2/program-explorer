use std::fs::File;
use std::io::BufWriter;
use std::os::fd::FromRawFd;

use peimage::squash::squash_to_tar;
use peoci::Compression;

fn main() {
    let mut layers: Vec<_> = std::env::args()
        .skip(1)
        .map(|x| (Compression::Gzip, File::open(x).unwrap()))
        .collect();

    let mut out = BufWriter::with_capacity(32 * 1024, unsafe { File::from_raw_fd(1) });
    squash_to_tar(&mut layers, &mut out).unwrap();
}

// cargo run --package peimage --bin squash /mnt/storage/program-explorer/ocidir/blobs/sha256/{7cf63256a31a4cc44f6defe8e1af95363aee5fa75f30a248d95cae684f87c53c,780fcebf8d094ef0ab389c7651dd0b1cc4530c9aba473c44359bf39bb0d770a8,e4d974df5c807a317b10ac80cf137857c9f5b7cd768fb54113f7d1cc1756504f}
