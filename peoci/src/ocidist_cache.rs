use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicU64};
use std::time::Instant;

use futures::{StreamExt, stream::FuturesOrdered};
use log::{error, info};
use moka::future::Cache;
use oci_spec::{
    OciSpecError,
    distribution::Reference,
    image::{Arch, Descriptor, Digest, ImageConfiguration, ImageManifest, Os},
};
use rustix::fd::OwnedFd;
use tokio::io::AsyncSeekExt;
use tokio::sync::Semaphore;

use crate::{
    blobcache,
    blobcache::{BlobKey, atomic_inc, atomic_take},
    ocidist, spec,
};

// This is a caching layer on top of ocidist that stores
// 1) references: quay.io/fedora/fedora:42 -> sha256:ffffffff
// 2) manifests: sha256:ffffffff -> ImageManifest and ImageConfiguration
// 3) blobs (layers): sha256:ffffffff -> file size (stored on disk)
// Remember that manifests store a digest pointer to the configuration, but here we store them
// together
// Currently references are looked up via the image index (same endpoint, different accept header)
// to pick out one which is amd64+linux (TODO is to support multi arch)
// Size limits are placed separately on the 3 caches and TODO is to expire reference cache entries
// so that we support eg fetching :latest tag once per day
// Cache persistence is mainly targeted at the always-running usecase of a service and not for
// interactive use since persist() will write the entire ref/manifests cache key+values every time.
// Persistence of the blobs are done with one blob per file like the ocidir layout of
// blobs/sha256/{digest}. Loading of the blob cache from this dir just inserts the key and size
// (necessary to limit max_capacity) and on expiry, the files are removed
// Max number of concurrent downloads are metered by a semaphore
//
// Currently the keys/values for digests are strings since they come with their multihash prefix,
// but they are twice the bytes (64 + 7 = 71 bytes for a sha256 32 byte (ideally 1 byte prefix for
// the multihash). moka cache has a 152 byte overhead per entry [1] so 223 vs 185 is only 80% size
// of. So I guess right now it is maybe not worthwhile to do a more optimized key type
// [1] https://github.com/moka-rs/moka/issues/201
//
// TODO should we really return Arc<Error>
// rate limits!
// should we cache 404's for some time?

#[derive(Debug, thiserror::Error)]
pub enum Error {
    ClientError(#[from] ocidist::Error),
    Io(#[from] std::io::Error),
    Errno(#[from] rustix::io::Errno),
    OciSpecError(#[from] OciSpecError),
    AcquireError(#[from] tokio::sync::AcquireError),
    EncodeError(#[from] bincode::error::EncodeError),
    DecodeError(#[from] bincode::error::DecodeError),
    SpecError(#[from] spec::Error),
    OneshotRx,
    OneshotTx,
    NoCacheDir,
    BadDigest,
    MissingDigest,
    ManifestNotFound,
    NoMatchingManifest,
    CachedFileSizeMismatch,
    ConfigurationNotFound,
    BlobNotFound,
    Ser,
    DirIter,
    DigestAlgorithmNotHandled,
    BlobMissing,
    FdClone,
    MaxConns,
    UnexpectedPanic,
    Canceled,
    Oob,
    MissingResult,
    Unknown,
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "ocidist_cache::Error::{:?}", self)
    }
}

#[derive(Debug)]
pub struct Stats {
    pub ref_cache_size: u64,
    pub manifest_cache_size: u64,
    pub blob_cache_size: u64,
    pub ref_cache_count: u64,
    pub manifest_cache_count: u64,
    pub blob_cache_count: u64,
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
    ref_capacity: u64,      // in bytes
    manifest_capacity: u64, // in bytes
    blob_capacity: u64,     // in bytes
    max_open_conns: usize,
    auth: Option<ocidist::AuthMap>,
}

#[derive(bincode::Encode, bincode::Decode)]
pub struct PackedImageAndConfiguration {
    data: Box<[u8]>,
}

struct Dirs {
    path: PathBuf, // only storing this for fs::read_dir ...
    cache: OwnedFd,
    blobs: OwnedFd,
}

#[derive(Default)]
struct Counters {
    ref_cache_hit: AtomicU64,
    ref_cache_miss: AtomicU64,
    manifest_cache_hit: AtomicU64,
    manifest_cache_miss: AtomicU64,
    blob_cache_hit: AtomicU64,
    blob_cache_miss: AtomicU64,
}

#[derive(Clone)]
pub struct Client {
    client: ocidist::Client,
    dirs: Arc<Dirs>,
    counters: Arc<Counters>,
    // TODO this should maybe be per registry + a global one?
    connection_semaphore: Arc<Semaphore>,

    // stores ref quay.io/fedora/fedora:42 -> manifest sha256:digest
    ref_cache: Cache<String, String>,

    // stores manifest sha256:digest -> image+configuration
    // is it okay to not include the reference? since sha, shouldn't matter
    // but more correct would be quay.io/fedora/fedora@sha256:digest
    manifest_cache: Cache<String, Arc<PackedImageAndConfiguration>>,

    // stores blob sha256:digest -> filesize
    // file is located at blobs/{key.replace(":", "/")}
    blob_cache: Cache<BlobKey, u64>,
}

impl Default for ClientBuilder {
    fn default() -> ClientBuilder {
        ClientBuilder {
            cache_dir: None,
            load_from_disk: false,
            ref_capacity: 10_000_000,
            manifest_capacity: 10_000_000,
            blob_capacity: 1_000_000_000,
            max_open_conns: 10,
            auth: None,
        }
    }
}

impl PackedImageAndConfiguration {
    pub fn new(
        digest: &Digest,
        manifest: &ImageManifest,
        configuration: &ImageConfiguration,
    ) -> Result<Self, Error> {
        let combined = spec::ImageManifestAndConfiguration {
            manifest_digest: digest.try_into()?,
            manifest: manifest.try_into()?,
            configuration: configuration.try_into()?,
        };
        let buf = bincode::encode_to_vec(&combined, bincode::config::standard())?;
        Ok(Self { data: buf.into() })
    }

    pub fn get(&self) -> Result<spec::ImageManifestAndConfiguration, Error> {
        Ok(bincode::decode_from_slice(&self.data, bincode::config::standard())?.0)
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

    pub fn max_open_conns(mut self, conns: usize) -> Self {
        self.max_open_conns = conns;
        self
    }

    pub fn auth(mut self, auth: ocidist::AuthMap) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn ref_capacity(mut self, cap: u64) -> Self {
        self.ref_capacity = cap;
        self
    }

    pub fn manifest_capacity(mut self, cap: u64) -> Self {
        self.manifest_capacity = cap;
        self
    }

    pub fn blob_capacity(mut self, cap: u64) -> Self {
        self.blob_capacity = cap;
        self
    }

    pub async fn build(self) -> Result<Client, Error> {
        if self.load_from_disk && self.cache_dir.is_none() {
            return Err(Error::NoCacheDir);
        }

        let dirs = {
            let path = self.cache_dir.ok_or(Error::NoCacheDir)?;
            let cache = blobcache::open_or_create_dir_at(None, &path)?;
            let blobs = blobcache::open_or_create_dir_at(Some(&cache), "blobs")?;
            Dirs { path, cache, blobs }
        };

        let blobs_clone = dirs.blobs.try_clone().map_err(|_| Error::FdClone)?;

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
            .max_capacity(blobcache::max_capacity(self.blob_capacity))
            .weigher(blobcache::weigher)
            .eviction_listener(move |k, v, reason| {
                blobcache::remove_blob("ocidist_cache", &blobs_clone, k, v, reason);
            })
            .build();

        let mut ret = Client {
            client,
            dirs: dirs.into(),
            ref_cache,
            manifest_cache,
            blob_cache,
            counters: Counters::default().into(),
            connection_semaphore: Arc::new(Semaphore::new(self.max_open_conns)),
        };
        if let Some(auth) = self.auth {
            ret.set_auth(auth).await;
        }
        if self.load_from_disk {
            info!("load cache from {:?}", ret.dirs.path);
            ret.load_ref_cache().await?;
            ret.load_manifest_cache().await?;
            ret.load_blob_cache().await?;
        }
        Ok(ret)
    }
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub async fn set_auth(&self, auth: ocidist::AuthMap) {
        self.client.set_auth(auth).await;
    }

    pub async fn stats(&self) -> Stats {
        self.ref_cache.run_pending_tasks().await;
        self.manifest_cache.run_pending_tasks().await;
        self.blob_cache.run_pending_tasks().await;
        Stats {
            ref_cache_size: self.ref_cache.weighted_size(),
            manifest_cache_size: self.manifest_cache.weighted_size(),
            blob_cache_size: self.blob_cache.weighted_size() * blobcache::BLOB_SIZE_DIVISOR,
            ref_cache_count: self.ref_cache.entry_count(),
            manifest_cache_count: self.manifest_cache.entry_count(),
            blob_cache_count: self.blob_cache.entry_count(),
            ref_cache_hit: atomic_take(&self.counters.ref_cache_hit),
            ref_cache_miss: atomic_take(&self.counters.ref_cache_miss),
            manifest_cache_hit: atomic_take(&self.counters.manifest_cache_hit),
            manifest_cache_miss: atomic_take(&self.counters.manifest_cache_miss),
            blob_cache_hit: atomic_take(&self.counters.blob_cache_hit),
            blob_cache_miss: atomic_take(&self.counters.blob_cache_miss),
        }
    }

    pub fn persist(&self) -> Result<(), Error> {
        self.save_ref_cache()?;
        self.save_manifest_cache()?;
        // nothing to do for blob cache
        Ok(())
    }

    pub async fn get_image_manifest_and_configuration(
        &self,
        reference: &Reference,
        arch: Arch,
        os: Os,
    ) -> Result<Arc<PackedImageAndConfiguration>, Arc<Error>> {
        // the digest from reference.digest() is not checked for validity in all cases, so if it is
        // present, we first check it. retreive_ref returns a string (since that is what we store
        // in the database, but it is derived from a Digest which has checked the validity already
        let digest_string = if let Some(digest_str) = reference.digest() {
            let digest: Digest = digest_str.parse().map_err(|_| Error::BadDigest)?;
            digest.to_string()
        } else {
            let entry = self
                .ref_cache
                .entry(reference.to_string())
                .or_try_insert_with(retreive_ref(
                    &self.client,
                    &self.connection_semaphore,
                    reference,
                    arch,
                    os,
                ))
                .await?;
            if entry.is_fresh() {
                atomic_inc(&self.counters.ref_cache_miss);
                info!(
                    "ref_cache miss ref={} digest={}",
                    entry.key(),
                    entry.value()
                )
            } else {
                atomic_inc(&self.counters.ref_cache_hit);
                info!("ref_cache hit ref={} digest={}", entry.key(), entry.value())
            }
            entry.into_value()
        };

        let reference = reference.clone_with_digest(digest_string.clone());

        let entry = self
            .manifest_cache
            .entry(digest_string)
            .or_try_insert_with(retreive_manifest_and_configuration(
                &self.client,
                &self.connection_semaphore,
                &reference,
            ))
            .await?;
        if entry.is_fresh() {
            atomic_inc(&self.counters.manifest_cache_miss);
            info!("manifest_cache miss digest={}", entry.key())
        } else {
            atomic_inc(&self.counters.manifest_cache_hit);
            info!("manifest_cache hit digest={}", entry.key())
        }
        Ok(entry.into_value())
    }

    pub async fn get_blob(
        &self,
        reference: &Reference,
        descriptor: &Descriptor,
    ) -> Result<OwnedFd, Arc<Error>> {
        let start = Instant::now();
        let key = BlobKey::new(descriptor.digest().to_string()).ok_or(Error::BadDigest)?;
        let (fd_tx, fd_rx) = tokio::sync::oneshot::channel();
        let entry = self
            .blob_cache
            .entry_by_ref(&key)
            .or_try_insert_with(retreive_blob(
                &self.client,
                &self.connection_semaphore,
                reference,
                descriptor,
                &self.dirs.blobs,
                &key,
                fd_tx,
            ))
            .await?;

        let fd = if entry.is_fresh() {
            atomic_inc(&self.counters.blob_cache_miss);
            let digest = entry.key();
            let size = *entry.value();
            let elapsed = start.elapsed();
            let speed = (size as f32) / 1_000_000.0 / elapsed.as_secs_f32();
            info!(
                "blob_cache miss digest={digest} size={size} elapsed={elapsed:?} speed={speed:.2} MB/s"
            );
            fd_rx.await.map_err(|_| Error::OneshotRx)?
        } else {
            atomic_inc(&self.counters.blob_cache_hit);
            info!("blob_cache hit digest={}", entry.key());
            blobcache::openat_read_key(&self.dirs.blobs, &key)
                .map_err(|e| Arc::new(e.into()))?
                .ok_or(Error::BlobMissing)?
                .into()
        };

        let stat = rustix::fs::fstat(&fd).map_err(|e| Arc::new(e.into()))?;
        let size: u64 = stat.st_size.try_into().unwrap_or(0);
        if size != descriptor.size() {
            error!(
                "file size mismatch for blob {}, descriptor={} file={}",
                descriptor.digest(),
                descriptor.size(),
                size
            );
            Err(Error::CachedFileSizeMismatch.into())
        } else {
            Ok(fd)
        }
    }

    pub async fn get_layers(
        &self,
        reference: &Reference,
        manifest: &spec::ImageManifest,
    ) -> Result<Vec<OwnedFd>, Arc<Error>> {
        let mut futs = FuturesOrdered::new();
        for layer in &manifest.layers {
            let descriptor = (*layer).into();
            futs.push_back(async move { self.get_blob(&reference, &descriptor).await });
        }
        // something like this should work maybe?
        // futs.collect::<Result<Vec<_>, _>>()
        let mut ret = Vec::with_capacity(futs.len());
        while let Some(result) = futs.next().await {
            match result {
                Ok(x) => {
                    ret.push(x);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(ret)
    }

    async fn load_ref_cache(&mut self) -> Result<(), Error> {
        let Some(file) = blobcache::openat_read(&self.dirs.cache, "ref")? else {
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

    async fn load_manifest_cache(&self) -> Result<(), Error> {
        let Some(file) = blobcache::openat_read(&self.dirs.cache, "manifest")? else {
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

    async fn load_blob_cache(&self) -> Result<(), Error> {
        // annoying we have to store them but we have to await the insert
        let mut acc = Vec::with_capacity(1024);
        blobcache::read_from_disk(&self.dirs.blobs, |key, size| {
            acc.push((key, size));
        })?;
        let count = acc.len();
        for (key, size) in acc.into_iter() {
            self.blob_cache.insert(key, size).await;
        }
        info!("loaded {count} entries into blob cache");
        Ok(())
    }

    // fn save_blob_cache; not needed since blobs are written as they are fetched

    fn save_ref_cache(&self) -> Result<(), Error> {
        let entries: Vec<_> = self.ref_cache.iter().collect();
        let num_entries = entries.len();
        let name = blobcache::GenericName::new("ref").unwrap();
        let (file, guard) =
            blobcache::openat_create_write_with_generic_guard(&self.dirs.cache, &name)?;
        let mut bw = BufWriter::new(file);
        let size = bincode::encode_into_std_write(&entries, &mut bw, bincode::config::standard())
            .map_err(|_| Error::Ser)?;
        guard.success()?;
        info!("wrote {size} bytes, {num_entries} entries to ref_cache");
        Ok(())
    }

    fn save_manifest_cache(&self) -> Result<(), Error> {
        let entries: Vec<_> = self.manifest_cache.iter().collect();
        let num_entries = entries.len();
        let name = blobcache::GenericName::new("manifest").unwrap();
        let (file, guard) =
            blobcache::openat_create_write_with_generic_guard(&self.dirs.cache, &name)?;
        let mut bw = BufWriter::new(file);
        let size = bincode::encode_into_std_write(&entries, &mut bw, bincode::config::standard())
            .map_err(|_| Error::Ser)?;
        guard.success()?;
        info!("wrote {size} bytes, {num_entries} entries to manifest_cache");
        Ok(())
    }
}

async fn retreive_ref(
    client: &ocidist::Client,
    semaphore: &Arc<Semaphore>,
    reference: &Reference,
    arch: Arch,
    os: Os,
) -> Result<String, Error> {
    let _permit = semaphore.acquire().await?;
    let descriptor = client
        .get_matching_descriptor_from_index(reference, arch, os)
        .await?
        .ok_or(Error::ManifestNotFound)?;
    // TODO for some images, is it possible they just don't have an image index and doing
    // get_image_manifest directly would work?
    Ok(descriptor.digest().to_string())
}

// this reference must have a digest (and we go from string to Digest back to String in the packed
// representation, but idk what else to do
async fn retreive_manifest_and_configuration(
    client: &ocidist::Client,
    semaphore: &Arc<Semaphore>,
    reference: &Reference,
) -> Result<Arc<PackedImageAndConfiguration>, Error> {
    let digest: Digest = reference
        .digest()
        .ok_or(Error::MissingDigest)?
        .parse()
        .map_err(|_| Error::BadDigest)?;
    let _permit = semaphore.acquire().await?;
    let manifest_res = client
        .get_image_manifest(reference)
        .await?
        .ok_or(Error::ManifestNotFound)?;
    let manifest = manifest_res.get()?;
    let configuration_res = client
        .get_image_configuration(reference, manifest.config())
        .await?
        .ok_or(Error::ConfigurationNotFound)?;
    let configuration = configuration_res.get()?;

    Ok(PackedImageAndConfiguration::new(&digest, &manifest, &configuration)?.into())
}

async fn retreive_blob(
    client: &ocidist::Client,
    semaphore: &Arc<Semaphore>,
    reference: &Reference,
    descriptor: &Descriptor,
    blob_dir: &OwnedFd,
    key: &BlobKey,
    fd_tx: tokio::sync::oneshot::Sender<OwnedFd>,
) -> Result<u64, Error> {
    let _permit = semaphore.acquire().await?;
    let (mut file, guard) = blobcache::openat_create_write_async_with_guard(blob_dir, key)?;
    let mut bw = tokio::io::BufWriter::with_capacity(32 * 1024, &mut file);
    let size = client
        .get_blob(reference, descriptor, &mut bw)
        .await?
        .ok_or(Error::BlobNotFound)?;
    // get_blob flushes the bufwriter
    guard.success()?;
    file.rewind().await?;
    let fd = file.into_std().await.into();
    if fd_tx.send(fd).is_err() {
        return Err(Error::OneshotTx);
    }
    Ok(size as u64)
}
