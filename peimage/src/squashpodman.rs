use std::fs::File;
use std::io::{BufWriter, Cursor};
use std::os::fd::FromRawFd;

use peimage::podman::load_layers_from_podman;
use peimage::squash::squash;

// trying out this method of dealing with multiple error types
// https://doc.rust-lang.org/rust-by-example/error/multiple_error_types/boxing_errors.html

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let image = args.get(1).expect("give me an image name");

    let mut layers: Vec<_> = load_layers_from_podman(image)
        .expect("getting layers failed")
        .into_iter()
        .map(Cursor::new)
        .collect();

    let mut out = BufWriter::with_capacity(32 * 1024, unsafe { File::from_raw_fd(1) });
    squash(&mut layers, &mut out).unwrap();
}
