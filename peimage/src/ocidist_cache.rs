use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use log::{error, trace};
use moka::future::Cache;
use oci_spec::{
    distribution::Reference,
    image::{Digest, ImageConfiguration, ImageManifest},
    OciSpecError,
};
use rustix::fd::OwnedFd;

use crate::ocidist;

#[derive(Debug)]
pub enum Error {
    ClientError(ocidist::Error),
    Errno(rustix::io::Errno),
    NoCacheDir,
    BadDigest,
    ManifestNotFound,
    ConfigurationNotFound,
    BlobNotFound,
    OciSpecError(OciSpecError),
}

impl From<ocidist::Error> for Error {
    fn from(error: ocidist::Error) -> Self {
        Error::ClientError(error)
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(error: rustix::io::Errno) -> Self {
        Error::Errno(error)
    }
}

impl From<OciSpecError> for Error {
    fn from(error: OciSpecError) -> Self {
        Error::OciSpecError(error)
    }
}

impl From<Arc<Error>> for Error {
    fn from(error: Arc<Error>) -> Self {
        error.into()
    }
}

#[derive(Default)]
pub struct ClientBuilder {
    cache_dir: Option<PathBuf>,
}

pub struct PackedImageAndConfiguration {
    offset: usize, // offset of configuration
    data: Box<[u8]>,
}

pub struct Client {
    client: ocidist::Client,
    cache_dir: OwnedFd,
    sha256_dir: OwnedFd,

    // stores ref quay.io/fedora/fedora:42 -> sha256:digest
    ref_cache: Cache<String, String>,

    // stores manifest sha256:digest -> image+configuration
    manifest_cache: Cache<String, Arc<PackedImageAndConfiguration>>,
}

impl ClientBuilder {
    pub fn dir(mut self, path: impl Into<PathBuf>) -> Self {
        let _ = self.cache_dir.replace(path.into());
        self
    }

    pub fn build(self) -> Result<Client, Error> {
        let cache_dir =
            open_or_create_dir_at(None, self.cache_dir.as_ref().ok_or(Error::NoCacheDir)?)?;
        let blob_dir = open_or_create_dir_at(Some(&cache_dir), "blobs")?;
        let sha256_dir = open_or_create_dir_at(Some(&blob_dir), "sha256")?;
        let client = ocidist::Client::new()?;
        let ref_cache = Cache::builder()
            .max_capacity(10_000_000) // 10 MB
            .build();
        let manifest_cache = Cache::builder()
            .max_capacity(10_000_000) // 10 MB
            .build();
        Ok(Client {
            client,
            cache_dir,
            sha256_dir,
            ref_cache,
            manifest_cache,
        })
    }
}

impl PackedImageAndConfiguration {
    pub fn new(manifest: impl AsRef<[u8]>, configuration: impl AsRef<[u8]>) -> Self {
        let manifest = manifest.as_ref();
        let configuration = configuration.as_ref();
        let offset = manifest.len();
        let mut data = Vec::with_capacity(manifest.len() + configuration.len());
        data.extend(manifest);
        data.extend(configuration);
        Self {
            offset,
            data: data.into(),
        }
    }

    pub fn manifest(&self) -> Result<ImageManifest, OciSpecError> {
        ImageManifest::from_reader(Cursor::new(&self.data[..self.offset]))
    }

    pub fn configuration(&self) -> Result<ImageConfiguration, OciSpecError> {
        ImageConfiguration::from_reader(Cursor::new(&self.data[self.offset..]))
    }
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub async fn get_image_manifest_and_configuration(
        &mut self,
        reference: &Reference,
    ) -> Result<Arc<PackedImageAndConfiguration>, Error> {
        use std::str::FromStr;
        if let Some(digest) = reference.digest() {
            let result = self
                .manifest_cache
                .try_get_with(
                    digest.to_string(),
                    retreive_manifest(
                        self.client.clone(),
                        reference,
                        &Digest::from_str(digest).map_err(|_| Error::BadDigest)?,
                    ),
                )
                .await?;
            // TODO store in ref_cache
            Ok(result)
            // to not fill the cache with 404's, we
        } else {
            todo!()
        }
        //self.ref_cache.try_get_with()
    }

    pub async fn get_blob(
        &mut self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<Option<OwnedFd>, Error> {
        todo!()
    }
}

// this will return Error if digest not found
async fn retreive_manifest(
    mut client: ocidist::Client,
    reference: &Reference,
    digest: &Digest,
) -> Result<Arc<PackedImageAndConfiguration>, Error> {
    // TODO use image index
    let manifest_res = client
        .get_image_manifest(reference)
        .await?
        .ok_or(Error::ManifestNotFound)?;
    let manifest = manifest_res.get()?;
    let configuration_res = client
        .get_image_configuration(reference, manifest.config().digest())
        .await?
        .ok_or(Error::ConfigurationNotFound)?;
    Ok(PackedImageAndConfiguration::new(manifest_res.data(), configuration_res.data()).into())
}

fn open_or_create_dir_at(
    dir: Option<&OwnedFd>,
    path: impl rustix::path::Arg + Copy,
) -> Result<OwnedFd, Error> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    if let Some(dir) = dir {
        let _ = match rustix::fs::mkdirat(dir, path, Mode::from_bits_truncate(0o744)) {
            Ok(_) => Ok(()),
            Err(e) if e == Errno::EXIST => Ok(()),
            e => e,
        }?;
        Ok(rustix::fs::openat(
            dir,
            path,
            OFlags::DIRECTORY | OFlags::RDWR,
            Mode::empty(),
        )?)
    } else {
        let _ = match rustix::fs::mkdir(path, Mode::from_bits_truncate(0o744)) {
            Ok(_) => Ok(()),
            Err(e) if e == Errno::EXIST => Ok(()),
            e => e,
        }?;
        Ok(rustix::fs::open(
            path,
            OFlags::DIRECTORY | OFlags::RDWR,
            Mode::empty(),
        )?)
    }
}
