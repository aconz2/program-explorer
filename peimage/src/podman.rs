use std::collections::BTreeMap;
use std::error;
use std::ffi::OsStr;
use std::fmt;
use std::io::{Cursor, Read, Write};
use std::process::{Command, Stdio};
use tar::Archive;
use tempfile::NamedTempFile;

use oci_spec::image::{Digest, ImageIndex, ImageManifest};

#[derive(Debug)]
enum PodmanLoadError {
    NoManifest,
    NoIndex,
    MissingBlob,
    BadBlobPath,
    NonUtf8Path,
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

pub fn load_layers_from_podman(image: &str) -> Result<Vec<Vec<u8>>, Box<dyn error::Error>> {
    let mut child = Command::new("podman")
        .arg("image")
        .arg("save")
        .arg("--format=oci-archive")
        .arg(image)
        .stdout(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("handle present");
    let mut archive = Archive::new(stdout);
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
            if let Ok(blob) = entry.path()?.strip_prefix("blobs/sha256/") {
                blobs.insert(
                    blob.to_str()
                        .ok_or(PodmanLoadError::BadBlobPath)?
                        .to_string(),
                    buf,
                );
            }
        }
    }

    let _ = child.wait()?;

    let index = index.ok_or(PodmanLoadError::NoIndex)?;
    let manifest = index
        .manifests()
        .first()
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

pub struct Rootfs {
    pub layers: Vec<Vec<u8>>,
    pub combined: Vec<u8>,
}

pub fn build_with_podman(containerfile: &str) -> Result<Rootfs, Box<dyn error::Error>> {
    let mut id_file = NamedTempFile::new()?;
    let mut child = Command::new("podman")
        .arg("build")
        .arg("--file=-")
        .arg("--no-hosts")
        .arg("--no-hostname")
        .arg("--network=none")
        .arg(format!(
            "--iidfile={}",
            id_file
                .path()
                .to_str()
                .ok_or(PodmanLoadError::NonUtf8Path)?
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("handle present");
    stdin.write_all(containerfile.as_bytes())?;
    drop(stdin);

    let _ = child.wait()?;

    let iid = {
        let mut buf = String::new();
        id_file.read_to_string(&mut buf)?;
        buf
    };

    let layers = load_layers_from_podman(&iid)?;

    let cid = {
        let output = Command::new("podman").arg("create").arg(&iid).output()?;
        String::from_utf8(output.stdout)?.trim().to_string()
    };

    let combined = {
        let output = Command::new("podman").arg("export").arg(&cid).output()?;
        output.stdout
    };

    let _ = Command::new("podman")
        .arg("rm")
        .arg(cid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    let _ = Command::new("podman")
        .arg("rmi")
        .arg(&iid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Ok(Rootfs { layers, combined })
}
