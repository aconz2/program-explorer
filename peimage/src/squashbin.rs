use std::fs::File;
use std::{io,env};
use std::io::{BufWriter,BufReader};

use peimage::squash::squash;

fn main() {
    let mut layers: Vec<_> = env::args()
        .skip(1)
        .map(|x| BufReader::new(File::open(x).unwrap()))
        //.map(|x| Archive::new(GzDecoder::new(x)))
        .collect();

    let mut out = BufWriter::new(io::stdout().lock());
    squash(&mut layers, &mut out).unwrap();
}

// cargo run --package peimage --bin squash /mnt/storage/program-explorer/ocidir/blobs/sha256/{7cf63256a31a4cc44f6defe8e1af95363aee5fa75f30a248d95cae684f87c53c,780fcebf8d094ef0ab389c7651dd0b1cc4530c9aba473c44359bf39bb0d770a8,e4d974df5c807a317b10ac80cf137857c9f5b7cd768fb54113f7d1cc1756504f}
