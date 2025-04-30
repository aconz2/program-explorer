use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::os::fd::FromRawFd;

use peimage::oci::load_layers_from_oci;
use peimage::squash::squash_to_tar;

fn main() {
    let args: Vec<_> = env::args().collect();
    let dir = args.get(1).expect("give me an oci dir");
    let image = args.get(2).expect("give me an image name or digest");

    let mut readers: Vec<_> = load_layers_from_oci(dir, image).expect("getting layers failed");

    let mut out = BufWriter::with_capacity(32 * 1024, unsafe { File::from_raw_fd(1) });
    // this doesn't respect the buffer at all (with or without .lock())
    //let mut out = BufWriter::new(io::stdout().lock());
    //let mut out = BufWriter::with_capacity(4096 * 8, File::create("/tmp/mytar").unwrap());
    let stats = squash_to_tar(&mut readers, &mut out).unwrap();
    eprintln!("{stats:?}");
}
