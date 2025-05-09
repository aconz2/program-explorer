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
    DirIter,
    DigestAlgorithmNotHandled,
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

#[derive(Debug)]
pub struct Stats {
    // todo blob_cache_size
    // *_cache_count
    pub ref_cache_size: u64,
    pub manifest_cache_size: u64,
    pub ref_cache_hit: u64,
    pub ref_cache_miss: u64,
    pub manifest_cache_hit: u64,
    pub manifest_cache_miss: u64,
    pub blob_cache_hit: u64,
    pub blob_cache_miss: u64,
}

pub struct ClientBuilder {
    cache_dir: Option<PathBuf>,
    load_from_disk: bool,
    ref_capacity: u64,
    manifest_capacity: u64,
    blob_capacity: u64,
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
            ref_capacity: 10_000_000,
            manifest_capacity: 10_000_000,
            blob_capacity: 1_000_000_000,
        }
    }
}

struct Dirs {
    path: PathBuf,
    cache: OwnedFd,
    sha256: OwnedFd,
}

#[derive(Default)]
struct Counters {
    ref_cache_hit: u64,
    ref_cache_miss: u64,
    manifest_cache_hit: u64,
    manifest_cache_miss: u64,
    blob_cache_hit: u64,
    blob_cache_miss: u64,
}

pub struct Client {
    client: ocidist::Client,
    dirs: Option<Dirs>,

    // stores ref quay.io/fedora/fedora:42 -> manifest sha256:digest
    ref_cache: Cache<String, String>,

    // stores manifest sha256:digest -> image+configuration
    manifest_cache: Cache<String, Arc<PackedImageAndConfiguration>>,

    // stores blob sha256:digest -> filesize
    // file is located at blobs/{key.replace(":", "/")}
    blob_cache: Cache<String, u64>,

    counters: Counters,
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
                Ok(Dirs {
                    path: path.into(),
                    cache,
                    sha256,
                })
            })
            .transpose()?;

        let client = ocidist::Client::new()?;

        let ref_cache = Cache::builder()
            .max_capacity(self.ref_capacity)
            .weigher(|k: &String, v: &String| (k.len() + v.len()).try_into().unwrap_or(u32::MAX))
            .build();

        let manifest_cache = Cache::builder()
            .max_capacity(self.manifest_capacity)
            // TODO maybe add a fixed cost per item (order 100 bytes for memory usage)
            .weigher(|k: &String, v: &Arc<PackedImageAndConfiguration>| {
                (k.len() + v.data.len()).try_into().unwrap_or(u32::MAX)
            })
            .build();

        let blob_cache = Cache::builder()
            // blobs are weighed in 1MB increments since we are limited to u32
            // TODO think about memory overhead for a given blob capacity because we can't have two
            // different limits
            .max_capacity(self.blob_capacity / 1_000_000)
            .weigher(|_: &String, size: &u64| {
                std::cmp::max(1, (size / 1_000_000).try_into().unwrap_or(u32::MAX))
            })
            // TODO needs eviction listener
            .build();

        let mut ret = Client {
            client,
            dirs,
            ref_cache,
            manifest_cache,
            blob_cache,
            counters: Counters::default(),
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
        let count = entries.len();
        for (k, v) in entries.into_iter() {
            self.ref_cache.insert(k, v).await;
        }
        info!("loaded {count} entries into ref_cache");
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
        let count = entries.len();
        for (k, v) in entries.into_iter() {
            self.manifest_cache.insert(k, v.into()).await;
        }
        info!("loaded {count} entries into manifest_cache");
        Ok(())
    }

    // bleh
    // todo handle more than sha256
    async fn load_blob_cache(&mut self) -> Result<(), Error> {
        let dirs = self.dirs.as_ref().ok_or(Error::NoCacheDir)?;
        let mut count = 0;
        for entry in
            std::fs::read_dir(dirs.path.join("blobs/sha256")).map_err(|_| Error::DirIter)?
        {
            if let Ok(entry) = entry {
                if let Ok(ft) = entry.file_type() {
                    if ft.is_file() {
                        if let Ok(name) = entry.file_name().into_string() {
                            if name.len() != 64 {
                                // also check lower hex?
                                error!(
                                    "weird path name in blobs/sha256 {:?}, removing",
                                    entry.file_name()
                                );
                                let _ = std::fs::remove_file(entry.path());
                                continue;
                            }
                            if let Ok(meta) = entry.metadata() {
                                count += 1;
                                self.blob_cache
                                    .insert(format!("sha256:{}", name), meta.len())
                                    .await;
                            } else {
                                error!(
                                    "couldn't get metadata for {:?}, skipping",
                                    entry.file_name()
                                );
                            }
                        } else {
                            error!(
                                "weird path name in blobs/sha256 {:?}, removing",
                                entry.file_name()
                            );
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
        info!("loaded {count} entries into blob cache");
        Ok(())
    }

    // fn save_blob_cache; not needed since blobs are written as they are fetched

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

    pub async fn stats(&mut self) -> Stats {
        use std::mem::take;
        self.ref_cache.run_pending_tasks().await;
        self.manifest_cache.run_pending_tasks().await;
        Stats {
            ref_cache_size: self.ref_cache.weighted_size(),
            manifest_cache_size: self.manifest_cache.weighted_size(),
            ref_cache_hit: take(&mut self.counters.ref_cache_hit),
            ref_cache_miss: take(&mut self.counters.ref_cache_miss),
            manifest_cache_hit: take(&mut self.counters.manifest_cache_hit),
            manifest_cache_miss: take(&mut self.counters.manifest_cache_miss),
            blob_cache_hit: take(&mut self.counters.blob_cache_hit),
            blob_cache_miss: take(&mut self.counters.blob_cache_miss),
        }
    }

    pub fn persist(&mut self) -> Result<(), Error> {
        self.save_ref_cache()?;
        self.save_manifest_cache()?;
        // nothing to do for blob cache
        Ok(())
    }

    pub async fn get_image_manifest_and_configuration(
        &mut self,
        reference: &Reference,
    ) -> Result<Arc<PackedImageAndConfiguration>, Arc<Error>> {
        let digest_string = if let Some(digest) = reference.digest() {
            digest
        } else {
            let entry = self
                .ref_cache
                .entry(reference.to_string())
                .or_try_insert_with(retreive_ref(self.client.clone(), reference))
                .await?;
            if entry.is_fresh() {
                self.counters.ref_cache_miss += 1;
                info!(
                    "ref_cache miss ref={} digest={}",
                    entry.key(),
                    entry.value()
                )
            } else {
                self.counters.ref_cache_hit += 1;
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
            self.counters.manifest_cache_miss += 1;
            info!("manifest_cache miss digest={}", entry.key())
        } else {
            self.counters.manifest_cache_hit += 1;
            info!("manifest_cache hit digest={}", entry.key())
        }
        Ok(entry.into_value())
    }

    pub async fn get_blob(
        &mut self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<OwnedFd, Arc<Error>> {
        // TODO maybe okay to grab a tempfile
        let dirs = self.dirs.as_ref().ok_or(Error::NoCacheDir)?;
        // hmm we have to have retreive_blob return what we store in the cache, but I'd like to
        // return an Fd from this so that the consumer has it guaranteed open and will remain open
        // if we remove it from the dir, but I'd rather not have to have an open fd for every file
        // in the cache...
        let entry = self
            .blob_cache
            .entry(digest.to_string())
            .or_try_insert_with(retreive_blob(
                self.client.clone(),
                reference,
                &digest,
                &dirs.sha256,
            ))
            .await?;
        if entry.is_fresh() {
            self.counters.blob_cache_miss += 1;
            info!("blob_cache miss digest={}", entry.key())
        } else {
            self.counters.blob_cache_hit += 1;
            info!("blob_cache hit digest={}", entry.key())
        }
        Ok(entry.into_value())
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

async fn retreive_blob(
    mut client: ocidist::Client,
    reference: &Reference,
    digest: &Digest,
    blob_dir: &OwnedFd,
) -> Result<u64, Error> {
    use oci_spec::image::DigestAlgorithm;
    // todo support more
    if *digest.algorithm() != DigestAlgorithm::Sha256 {
        return Err(Error::DigestAlgorithmNotHandled);
    }
    let fd: OwnedFd = openat_create_write(blob_dir, digest.digest())?.into();
    let mut file: tokio::fs::File = fd.into();
    let size = client
        .get_blob(reference, digest, &mut file)
        .await?
        .ok_or(Error::BlobNotFound)?;
    Ok(size as u64)
}

// I wish the std had things for at

fn open_or_create_dir_at(
    dir: Option<&OwnedFd>,
    path: impl rustix::path::Arg + Copy,
) -> Result<OwnedFd, Error> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    if let Some(dir) = dir {
        match rustix::fs::mkdirat(dir, path, Mode::from_bits_truncate(0o744)) {
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
        match rustix::fs::mkdir(path, Mode::from_bits_truncate(0o744)) {
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
        Err(e) => Err(e.into()),
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
