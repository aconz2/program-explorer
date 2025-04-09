use std::collections::BTreeMap;
use std::env;
use std::error;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::io::{Cursor, Read};
use std::process::{Command, Stdio};
use tar::Archive;

use oci_spec::image::{Digest, ImageIndex, ImageManifest};

use peimage::squash::squash;

// trying out this method of dealing with multiple error types
// https://doc.rust-lang.org/rust-by-example/error/multiple_error_types/boxing_errors.html
#[derive(Debug)]
enum PodmanLoadError {
    NoManifest,
    NoIndex,
    MissingBlob,
    BadBlobPath,
}
impl fmt::Display for PodmanLoadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl error::Error for PodmanLoadError {}

fn digest_to_string(digest: &Digest) -> Result<String, PodmanLoadError> {
    digest
        .to_string()
        .strip_prefix("sha256:")
        .map(|x| x.into())
        .ok_or(PodmanLoadError::BadBlobPath)
}

fn load_layers_from_podman(image: &str) -> Result<Vec<Vec<u8>>, Box<dyn error::Error>> {
    let child = Command::new("podman")
        .arg("image")
        .arg("save")
        .arg("--format=oci-archive")
        .arg(image)
        .stdout(Stdio::piped())
        .spawn()?;

    let mut archive = Archive::new(child.stdout.unwrap());
    let mut blobs = BTreeMap::new();
    let mut index: Option<ImageIndex> = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()? == <str as AsRef<OsStr>>::as_ref("index.json") {
            let _ = index.replace(ImageIndex::from_reader(&mut entry)?);
        } else {
            // have to read first before checking otherwise we try to take a mutable borrow
            // while we have an immutable borrow (annoying)
            let mut buf = vec![];
            entry.read_to_end(&mut buf)?;
            match entry.path()?.strip_prefix("blobs/sha256/") {
                Ok(blob) => {
                    blobs.insert(
                        blob.to_str()
                            .ok_or(PodmanLoadError::BadBlobPath)?
                            .to_string(),
                        buf,
                    );
                }
                _ => {}
            }
        }
    }
    let index = index.ok_or(PodmanLoadError::NoIndex)?;
    let manifest = index
        .manifests()
        .get(0)
        .ok_or(PodmanLoadError::NoManifest)?;
    // Digest should really implement Borrow<String>
    let manifest_blob = blobs
        .get(&digest_to_string(manifest.digest())?)
        .ok_or(PodmanLoadError::MissingBlob)?;
    let manifest = ImageManifest::from_reader(Cursor::new(manifest_blob))?;
    manifest
        .layers()
        .iter()
        .map(|x| {
            blobs
                .remove(&digest_to_string(x.digest())?)
                .ok_or(PodmanLoadError::MissingBlob)
                .map_err(|x| x.into())
        })
        .collect()
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let image = args.get(1).expect("give me an image name");

    let mut layers: Vec<_> = load_layers_from_podman(image)
        .expect("getting layers failed")
        .into_iter()
        .map(Cursor::new)
        .collect();

    squash(&mut layers, &mut io::stdout()).unwrap();
}
