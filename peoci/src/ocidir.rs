use std::fs::File;
use std::path::Path;

use crate::Compression;

use oci_spec::image::{Descriptor, Digest, ImageIndex, ImageManifest};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    NoMatchingManifest,
    OciSpec(#[from] oci_spec::OciSpecError),
    NoMediaType,
    BadMediaType,
    Io(#[from] std::io::Error),
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

// sha256:foo -> sha256/foo
fn digest_path(d: &Digest) -> String {
    d.to_string().replacen(":", "/", 1)
}

fn load_blob(blobs: &Path, layer: &Descriptor) -> Result<(Compression, File), Error> {
    // grr the image spec is a bit complicated with old stuff, there is both mediaType and
    // artifactType and we have to handle the docker ones in mediaType and the OCI ones in artifact
    // type
    let compression = layer.try_into().map_err(|_| Error::BadMediaType)?;
    let file = File::open(blobs.join(digest_path(layer.digest()))).map_err(Into::<Error>::into)?;
    Ok((compression, file))
}

pub fn load_layers_from_oci<P: AsRef<Path>>(
    dir: P,
    image: &str,
) -> Result<Vec<(Compression, File)>, Error> {
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
        .map(|x| load_blob(&blobs, x))
        .collect()
}
