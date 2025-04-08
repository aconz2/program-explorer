use std::env;
use std::error;
use std::fmt;
use std::io;
use std::fs::File;
use std::path::PathBuf;

use oci_spec::image::{Digest, ImageIndex, ImageManifest};

use peimage::squash::squash;

// trying out this method of dealing with multiple error types
// https://doc.rust-lang.org/rust-by-example/error/multiple_error_types/boxing_errors.html
#[derive(Debug)]
enum OciLoadError {
    NoMatchingManifest
}
impl fmt::Display for OciLoadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl error::Error for OciLoadError {}

fn digest_path(d: &Digest) -> String {
    d.to_string().replacen(":", "/", 1)
}

fn load_layers_from_oci(dir: &str, image: &str) -> Result<Vec<File>, Box<dyn error::Error>> {
    let dir = PathBuf::from(dir);
    let blobs = dir.join("blobs");

    let index = ImageIndex::from_file(dir.join("index.json"))?;
    let manifest = (
        if image.starts_with("sha256:") {
            index.manifests().iter().find(|x| x.digest().to_string() == image)
        } else {
            index.manifests().iter().find(|x| {
                if let Some(annotations) = x.annotations() {
                    if let Some(name) = annotations.get("org.opencontainers.image.ref.name") {
                        return image == name;
                    }
                }
                false
            })
        }
    ).ok_or(OciLoadError::NoMatchingManifest)?;

    let image_manifest = ImageManifest::from_file(blobs.join(digest_path(manifest.digest())))?;

    // is there a nicer way to coerce things into the right error type here??

    image_manifest.layers()
        .iter()
        .map(|x| File::open(blobs.join(digest_path(x.digest()))).map_err(|x| Into::<Box<dyn error::Error>>::into(x)))
        .collect()
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let dir = args.get(1).expect("give me an oci dir");
    let image = args.get(2).expect("give me an image name or digest");

    let mut layers = load_layers_from_oci(dir, image).expect("getting layers failed");

    squash(&mut layers, &mut io::stdout()).unwrap();
}
