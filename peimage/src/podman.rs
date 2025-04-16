use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Cursor, Read, Write};
use std::process::{Command, Stdio};
use tar::Archive;
use tempfile::NamedTempFile;

use oci_spec::image::{Digest, ImageIndex, ImageManifest};

#[derive(Debug)]
pub enum Error {
    NoManifest,
    NoIndex,
    MissingBlob,
    BadBlobPath,
    NonUtf8Path,
    PodmanExport,
    PodmanBuild,
    PodmanRm,
    PodmanCreate,
    PodmanCreateId,
    Tempfile,
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

fn digest_to_string(digest: &Digest) -> Result<String, Error> {
    digest
        .to_string()
        .strip_prefix("sha256:")
        .map(|x| x.into())
        .ok_or(Error::BadBlobPath)
}

pub fn load_layers_from_podman(image: &str) -> Result<Vec<Vec<u8>>, Error> {
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
                        .ok_or(Error::BadBlobPath)?
                        .to_string(),
                    buf,
                );
            }
        }
    }

    let _ = child.wait()?;

    let index = index.ok_or(Error::NoIndex)?;
    let manifest = index
        .manifests()
        .first()
        .ok_or(Error::NoManifest)?;
    // Digest should really implement Borrow<String>
    let manifest_blob = blobs
        .get(&digest_to_string(manifest.digest())?)
        .ok_or(Error::MissingBlob)?;
    let manifest = ImageManifest::from_reader(Cursor::new(manifest_blob))?;
    manifest
        .layers()
        .iter()
        .map(|x| {
            blobs
                .remove(&digest_to_string(x.digest())?)
                .ok_or(Error::MissingBlob)
                .map_err(|x| x.into())
        })
        .collect()
}

pub struct Rootfs {
    pub layers: Vec<Vec<u8>>,
    pub combined: Vec<u8>,
}

pub fn build_with_podman(containerfile: &str) -> Result<Rootfs, Error> {
    let mut id_file = NamedTempFile::new().map_err(|_| Error::Tempfile)?;
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
                .ok_or(Error::NonUtf8Path)?
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::PodmanBuild)?;

    let mut stdin = child.stdin.take().expect("handle present");
    stdin.write_all(containerfile.as_bytes()).map_err(|_| Error::Io)?;
    drop(stdin);

    let _ = child.wait().map_err(|_| Error::PodmanBuild)?;

    let iid = {
        let mut buf = String::new();
        id_file.read_to_string(&mut buf)?;
        buf
    };

    let layers = load_layers_from_podman(&iid)?;

    let cid = {
        let output = Command::new("podman").arg("create").arg(&iid).output().map_err(|_| Error::PodmanCreate)?;
        String::from_utf8(output.stdout).map_err(|_| Error::PodmanCreateId)?.trim().to_string()
    };

    let combined = {
        let output = Command::new("podman").arg("export").arg(&cid).output().map_err(|_| Error::PodmanExport)?;
        output.stdout
    };

    let _ = Command::new("podman")
        .arg("rm")
        .arg(cid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| Error::PodmanRm)?;

    let _ = Command::new("podman")
        .arg("rmi")
        .arg(&iid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Ok(Rootfs { layers, combined })
}
