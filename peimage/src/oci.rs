use std::fs::File;
use std::path::Path;
use oci_spec::image::{Digest, ImageIndex, ImageManifest};

#[derive(Debug)]
pub enum Error {
    NoMatchingManifest,
    OciSpec,
    Io,
}

impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Error::Io
    }
}

impl From<oci_spec::OciSpecError> for Error {
    fn from(_: oci_spec::OciSpecError) -> Self {
        Error::OciSpec
    }
}

// sha256:foo -> sha256/foo
fn digest_path(d: &Digest) -> String {
    d.to_string().replacen(":", "/", 1)
}

pub fn load_layers_from_oci<P: AsRef<Path>>(dir: P, image: &str) -> Result<Vec<File>, Error> {
    let dir = dir.as_ref();
    let blobs = dir.join("blobs");

    let index = ImageIndex::from_file(dir.join("index.json"))?;
    let manifest = (if image.starts_with("sha256:") {
        index
            .manifests()
            .iter()
            .find(|x| x.digest().to_string() == image)
    } else {
        index.manifests().iter().find(|x| {
            if let Some(annotations) = x.annotations() {
                if let Some(name) = annotations.get("org.opencontainers.image.ref.name") {
                    return image == name;
                }
            }
            false
        })
    })
    .ok_or(Error::NoMatchingManifest)?;

    let image_manifest = ImageManifest::from_file(blobs.join(digest_path(manifest.digest())))?;

    // is there a nicer way to coerce things into the right error type here??

    image_manifest
        .layers()
        .iter()
        .map(|x| {
            File::open(blobs.join(digest_path(x.digest())))
                .map_err(Into::into)
        })
        .collect()
}

