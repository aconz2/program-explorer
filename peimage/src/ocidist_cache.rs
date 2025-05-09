use std::io::Cursor;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::Arc;

use log::{error, info, trace};
use moka::future::Cache;
use oci_spec::{
    distribution::Reference,
    image::{Arch, Digest, ImageConfiguration, ImageManifest, Os},
    OciSpecError,
};
use rustix::{fd::OwnedFd, fs::renameat};
use std::fs::File;

use crate::ocidist;

#[derive(Debug)]
pub enum Error {
    ClientError(ocidist::Error),
    Errno(rustix::io::Errno),
    NoCacheDir,
    BadDigest,
    ManifestNotFound,
    NoMatchingManifest,
    ConfigurationNotFound,
    BlobNotFound,
    SerError,
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

pub struct ClientBuilder {
    cache_dir: Option<PathBuf>,
    load_from_disk: bool,
    ref_capacity: u64,
    manifest_capacity: u64,
}

#[derive(bincode::Encode, bincode::Decode)]
pub struct PackedImageAndConfiguration {
    offset: usize, // offset of configuration
    data: Box<[u8]>,
}

impl Default for ClientBuilder {
    fn default() -> ClientBuilder {
        ClientBuilder {
            cache_dir: None,
            load_from_disk: false,
            ref_capacity: 10_000_1000,
            manifest_capacity: 10_000_1000,
        }
    }
}

struct Dirs {
    cache: OwnedFd,
    sha256: OwnedFd,
}

pub struct Client {
    client: ocidist::Client,
    dirs: Option<Dirs>,

    // stores ref quay.io/fedora/fedora:42 -> sha256:digest
    ref_cache: Cache<String, String>,

    // stores manifest sha256:digest -> image+configuration
    manifest_cache: Cache<String, Arc<PackedImageAndConfiguration>>,
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

impl ClientBuilder {
    pub fn dir(mut self, path: impl Into<PathBuf>) -> Self {
        let _ = self.cache_dir.replace(path.into());
        self
    }

    pub fn load_from_disk(mut self, load_from_disk: bool) -> Self {
        self.load_from_disk = load_from_disk;
        self
    }

    pub async fn build(self) -> Result<Client, Error> {
        if self.load_from_disk && self.cache_dir.is_none() {
            return Err(Error::NoCacheDir);
        }

        let dirs = self
            .cache_dir
            .as_ref()
            .map(|path| -> Result<_, Error> {
                let cache = open_or_create_dir_at(None, path)?;
                let blobs = open_or_create_dir_at(Some(&cache), "blobs")?;
                let sha256 = open_or_create_dir_at(Some(&blobs), "sha256")?;
                info!("init cache dir at {path:?}");
                Ok(Dirs { cache, sha256 })
            })
            .transpose()?;

        let client = ocidist::Client::new()?;
        let ref_cache = Cache::builder().max_capacity(self.ref_capacity).build();
        let manifest_cache = Cache::builder()
            .max_capacity(self.manifest_capacity)
            .build();

        let mut ret = Client {
            client,
            dirs,
            ref_cache,
            manifest_cache,
        };
        if self.load_from_disk {
            ret.load_ref_cache().await?;
            ret.load_manifest_cache().await?;
        }
        Ok(ret)
    }
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    async fn load_ref_cache(&mut self) -> Result<(), Error> {
        let dirs = self.dirs.as_ref().ok_or(Error::NoCacheDir)?;
        let Some(file) = openat_read(&dirs.cache, "ref")? else {
            return Ok(());
        };
        let entries: Vec<(String, String)> = match bincode::decode_from_reader(
            &mut BufReader::new(file),
            bincode::config::standard(),
        ) {
            Ok(x) => x,
            Err(e) => {
                error!("error loading from ref_cache {:?}", e);
                return Ok(());
            }
        };
        info!("loading {} entries into ref_cache", entries.len());
        for (k, v) in entries.into_iter() {
            self.ref_cache.insert(k, v).await;
        }
        Ok(())
    }

    async fn load_manifest_cache(&mut self) -> Result<(), Error> {
        let dirs = self.dirs.as_ref().ok_or(Error::NoCacheDir)?;
        let Some(file) = openat_read(&dirs.cache, "manifest")? else {
            return Ok(());
        };
        let entries: Vec<(String, PackedImageAndConfiguration)> = match bincode::decode_from_reader(
            &mut BufReader::new(file),
            bincode::config::standard(),
        ) {
            Ok(x) => x,
            Err(e) => {
                error!("error loading from manifest_cache {:?}", e);
                return Ok(());
            }
        };
        info!("loading {} entries into ref_cache", entries.len());
        for (k, v) in entries.into_iter() {
            self.manifest_cache.insert(k, v.into()).await;
        }
        Ok(())
    }

    fn save_ref_cache(&mut self) -> Result<(), Error> {
        let dirs = self.dirs.as_ref().ok_or(Error::NoCacheDir)?;
        let entries: Vec<_> = self.ref_cache.iter().collect();
        let num_entries = entries.len();
        let mut bw = BufWriter::new(openat_create_write(&dirs.cache, "ref.tmp")?);
        let size = bincode::encode_into_std_write(&entries, &mut bw, bincode::config::standard())
            .map_err(|_| Error::SerError)?;
        renameat(&dirs.cache, "ref.tmp", &dirs.cache, "ref")?;
        info!("wrote {size} bytes, {num_entries} entries to ref_cache");
        Ok(())
    }

    fn save_manifest_cache(&mut self) -> Result<(), Error> {
        let dirs = self.dirs.as_ref().ok_or(Error::NoCacheDir)?;
        let entries: Vec<_> = self.manifest_cache.iter().collect();
        let num_entries = entries.len();
        let mut bw = BufWriter::new(openat_create_write(&dirs.cache, "manifest.tmp")?);
        let size = bincode::encode_into_std_write(&entries, &mut bw, bincode::config::standard())
            .map_err(|_| Error::SerError)?;
        renameat(&dirs.cache, "manifest.tmp", &dirs.cache, "manifest")?;
        info!("wrote {size} bytes, {num_entries} entries to manifest_cache");
        Ok(())
    }

    pub fn persist(&mut self) -> Result<(), Error> {
        self.save_ref_cache()?;
        self.save_manifest_cache()?;
        Ok(())
    }

    pub async fn get_image_manifest_and_configuration(
        &mut self,
        reference: &Reference,
    ) -> Result<Arc<PackedImageAndConfiguration>, Error> {
        let digest_string = if let Some(digest) = reference.digest() {
            digest
        } else {
            let entry = self
                .ref_cache
                .entry(reference.to_string())
                .or_try_insert_with(retreive_ref(self.client.clone(), reference))
                .await?;
            if entry.is_fresh() {
                info!(
                    "ref_cache miss ref={} digest={}",
                    entry.key(),
                    entry.value()
                )
            } else {
                info!("ref_cache hit ref={} digest={}", entry.key(), entry.value())
            }
            &entry.into_value()
        };
        use std::str::FromStr;
        let digest = Digest::from_str(digest_string).map_err(|_| Error::BadDigest)?;

        let entry = self
            .manifest_cache
            .entry(digest.to_string())
            .or_try_insert_with(retreive_manifest(self.client.clone(), reference, &digest))
            .await?;
        if entry.is_fresh() {
            info!("manifest_cache miss digest={}", entry.key())
        } else {
            info!("manifest_cache hit digest={}", entry.key())
        }
        Ok(entry.into_value())
    }

    pub async fn get_blob(
        &mut self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<Option<OwnedFd>, Error> {
        todo!()
    }
}

async fn retreive_ref(mut client: ocidist::Client, reference: &Reference) -> Result<String, Error> {
    let digest = client
        .lookup_image_index(reference, Arch::Amd64, Os::Linux)
        .await?
        .ok_or(Error::NoMatchingManifest)?;
    Ok(digest.to_string())
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

// I wish the std had things for at

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
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::PATH,
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
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::PATH,
            Mode::empty(),
        )?)
    }
}

fn openat_create_write(
    dir: &OwnedFd,
    name: impl rustix::path::Arg,
) -> Result<File, rustix::io::Errno> {
    use rustix::fs::OFlags;
    openat(
        dir,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
    )
}

fn openat_read(dir: &OwnedFd, name: impl rustix::path::Arg) -> Result<Option<File>, Error> {
    use rustix::fs::OFlags;
    match openat(dir, name, OFlags::RDONLY | OFlags::CLOEXEC) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e == rustix::io::Errno::NOENT => Ok(None),
        Err(e) => return Err(e.into()),
    }
}

fn openat(
    dir: &OwnedFd,
    name: impl rustix::path::Arg,
    flags: rustix::fs::OFlags,
) -> Result<File, rustix::io::Errno> {
    use rustix::fs::Mode;
    let fd = rustix::fs::openat(dir, name, flags, Mode::from_bits_truncate(0o744))?;
    Ok(fd.into())
}
