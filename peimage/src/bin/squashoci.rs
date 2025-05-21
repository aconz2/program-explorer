use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::os::fd::FromRawFd;

use peoci::ocidir::load_layers_from_oci;
use peimage::squash::{squash_to_erofs, squash_to_tar};

fn main() {
    let args: Vec<_> = env::args().collect();
    let dir = args.get(1).expect("give me an oci dir");
    let image = args.get(2).expect("give me an image name or digest");
    let stdin = "-".to_string();
    let output = args.get(3).unwrap_or(&stdin);

    let mut readers: Vec<_> = load_layers_from_oci(dir, image).expect("getting layers failed");

    if output == "-" {
        let mut out = BufWriter::with_capacity(32 * 1024, unsafe { File::from_raw_fd(1) });
        let stats = squash_to_tar(&mut readers, &mut out).unwrap();
        eprintln!("{stats:?}");
    } else if output.ends_with(".tar") {
        let mut out = BufWriter::with_capacity(32 * 1024, File::create(output).unwrap());
        let stats = squash_to_tar(&mut readers, &mut out).unwrap();
        eprintln!("{stats:?}");
    } else if output.ends_with(".erofs") {
        let out = File::create(output).unwrap();
        let builder = peerofs::build::Builder::new(out, peerofs::build::BuilderConfig::default()).unwrap();
        let (squash_stats, erofs_stats) = squash_to_erofs(&mut readers, builder).unwrap();
        eprintln!("{squash_stats:?}");
        eprintln!("{erofs_stats:?}");
    }
}
