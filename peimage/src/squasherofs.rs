use std::env;

use peimage::mkfs::squash_erofs;
use peimage::ocidir::load_layers_from_oci;

fn main() {
    let args: Vec<_> = env::args().collect();
    let dir = args.get(1).expect("give me an oci dir");
    let image = args.get(2).expect("give me an image name or digest");
    let outfile = args.get(3).expect("give me an output file");

    if !outfile.ends_with(".erofs") {
        eprintln!("outfile should end with .erofs");
        std::process::exit(1);
    }

    let mut readers: Vec<_> = load_layers_from_oci(dir, image).expect("getting layers failed");

    let stats = squash_erofs(&mut readers, outfile).unwrap();
    eprintln!("{stats:?}");
}
