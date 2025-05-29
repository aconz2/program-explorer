use std::collections::BTreeMap;
use std::fs::File;
use std::io::IoSlice;
use std::io::Seek;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicU64};
use std::time::Instant;

use clap::Parser;
use log::{error, info};
use moka::future::Cache;
use oci_spec::{
    distribution::Reference,
    image::{Arch, Digest, Os},
};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener, ancillary::AncillaryMessageWriter};

use peimage::squash::squash_to_erofs;
use peimage_service::{Request, WireResponse};
use peoci::{
    blobcache,
    blobcache::{BlobKey, atomic_inc, atomic_take},
    compression::Compression,
    ocidist,
    ocidist::{Auth, AuthMap},
    ocidist_cache,
    ocidist_cache::Client,
    spec,
};

// max sum of compressed layer sizes
const MAX_TOTAL_LAYER_SIZE: u64 = 2_000_000_000;
// this is the max erofs image size (of just the file data portion)
const MAX_IMAGE_SIZE: u64 = 3_000_000_000;

#[derive(Deserialize)]
struct AuthEntry {
    username: String,
    password: String,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    BadDigest,
    BadReference,
    BadLayerType(#[from] peoci::compression::Error),
    OneshotTx,
    OneshotRx,
    MissingFile,
    OpenFile,
    TotalLayerSizeTooBig,
    Arc(#[from] Arc<anyhow::Error>),
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Default)]
struct Counters {
    img_cache_hit: AtomicU64,
    img_cache_miss: AtomicU64,
}

#[derive(Debug)]
struct Stats {
    #[allow(dead_code)]
    img_cache_hit: u64,
    #[allow(dead_code)]
    img_cache_miss: u64,
}

type StoredAuth = BTreeMap<String, AuthEntry>;
type ImageCache = Cache<BlobKey, u64>;

fn load_stored_auth(p: impl AsRef<Path>) -> anyhow::Result<AuthMap> {
    let stored: StoredAuth = serde_json::from_str(&std::fs::read_to_string(p)?)?;
    Ok(stored
        .into_iter()
        .map(|(k, v)| (k, Auth::UserPass(v.username, v.password)))
        .collect::<AuthMap>())
}

pub fn round_up_file_to_pmem_size<F: rustix::fd::AsFd>(f: F) -> rustix::io::Result<u64> {
    fn round_up_to<const N: u64>(x: u64) -> u64 {
        if x == 0 {
            return N;
        }
        x.div_ceil(N) * N
    }
    const PMEM_ALIGN_SIZE: u64 = 0x20_0000; // 2 MB
    let stat = rustix::fs::fstat(&f)?;
    let cur = stat.st_size as u64;
    let newlen = round_up_to::<PMEM_ALIGN_SIZE>(cur);
    if cur != newlen {
        rustix::fs::ftruncate(f, newlen)?;
    }
    Ok(newlen)
}

async fn handle_conn(
    worker_semaphore: Arc<Semaphore>,
    conn: &UnixSeqpacket,
    client: Client,
    img_cache: ImageCache,
    imgs_dir: Arc<OwnedFd>,
    counters: Arc<Counters>,
) -> anyhow::Result<(Digest, spec::ImageConfiguration, OwnedFd)> {
    let mut buf = [0; 1024];
    let len = conn.recv(&mut buf).await?;
    let (req, _) =
        bincode::decode_from_slice::<Request, _>(&buf[..len], bincode::config::standard())?;

    let reference = req.parse_reference().ok_or(Error::BadReference)?;

    let image_and_config = client
        .get_image_manifest_and_configuration(&reference, Arch::Amd64, Os::Linux)
        .await?
        .get()?;

    let digest: Digest = image_and_config.manifest_digest.into();
    let config = image_and_config.configuration;

    let (fd_tx, fd_rx) = tokio::sync::oneshot::channel();

    let key = BlobKey::new(digest.to_string()).ok_or(Error::BadDigest)?;
    let entry = img_cache
        .entry_by_ref(&key)
        .or_try_insert_with(make_erofs_image(
            worker_semaphore,
            client,
            &reference,
            &image_and_config.manifest,
            &imgs_dir,
            &key,
            fd_tx,
        ))
        .await
        .map_err(Error::Arc)?;

    if entry.is_fresh() {
        atomic_inc(&counters.img_cache_miss);
        let size = *entry.value();
        info!("img_cache miss digest={key} size={size}");
        let fd = fd_rx.await.map_err(|_| Error::OneshotRx)?;
        Ok((digest, config, fd))
    } else {
        atomic_inc(&counters.img_cache_hit);
        info!("img_cache hit digest={key}");
        match blobcache::openat_read_key(&imgs_dir, &key) {
            Ok(Some(file)) => Ok((digest, config, file.into())),
            Ok(None) => {
                error!("image cache missing file {}", key);
                Err(Error::MissingFile.into())
            }
            Err(e) => {
                error!("error opening blob {:?}", e);
                Err(Error::OpenFile.into())
            }
        }
    }
}

async fn make_erofs_image(
    worker_semaphore: Arc<Semaphore>,
    client: Client,
    reference: &Reference,
    manifest: &peoci::spec::ImageManifest,
    imgs_dir: &Arc<OwnedFd>,
    key: &BlobKey,
    fd_tx: tokio::sync::oneshot::Sender<OwnedFd>,
) -> anyhow::Result<u64> {
    let key = key.clone();

    let total_layer_size = manifest
        .layers
        .iter()
        .map(|layer| layer.size)
        .fold(0u64, |x, y| x.saturating_add(y));

    if total_layer_size > MAX_TOTAL_LAYER_SIZE {
        return Err(Error::TotalLayerSizeTooBig.into());
    }

    let fds = client.get_layers(reference, manifest).await?;
    let mut layers: Vec<_> = manifest
        .layers
        .iter()
        .zip(fds.into_iter())
        .map(|(descriptor, fd)| -> Result<_, Error> {
            let comp: Compression = descriptor.into();
            let reader: File = fd.into();
            Ok((comp, reader))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let imgs_dir = imgs_dir.clone();

    let _guard = worker_semaphore.acquire().await;
    tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let (mut file, guard) = blobcache::openat_create_write_with_guard(&imgs_dir, &key)?;

        let t0 = Instant::now();
        let builder = peerofs::build::Builder::new(&mut file, peerofs::build::BuilderConfig{
            max_file_size: Some(MAX_IMAGE_SIZE),
            increment_uid_gid: Some(1000), // TODO magic constant
        })?;
        let (squash_stats, erofs_stats) = squash_to_erofs(&mut layers, builder)?;
        let elapsed = t0.elapsed().as_secs_f32();
        guard.success()?;
        round_up_file_to_pmem_size(&file)?;
        // ftruncate up to the right size
        let size = file.metadata()?.len();
        file.rewind()?;
        info!("built image for {key} size={size} Squash{squash_stats:?} Erofs{erofs_stats:?} in {elapsed:.2}s");
        if fd_tx.send(file.into()).is_err() {
            return Err(Error::OneshotTx.into());
        }
        Ok(size)
    })
    .await?
}

async fn make_img_cache(dir: impl AsRef<Path>, img_capacity: u64) -> anyhow::Result<(ImageCache, OwnedFd)> {
    let cache_dir = blobcache::open_or_create_dir_at(None, dir.as_ref())?;
    let imgs_dir = blobcache::open_or_create_dir_at(Some(&cache_dir), "imgs")?;
    let imgs_dir_clone = imgs_dir.try_clone()?;

    let image_cache = Cache::builder()
        .max_capacity(blobcache::max_capacity(img_capacity))
        .weigher(blobcache::weigher)
        .eviction_listener(move |k, v, reason| {
            blobcache::remove_blob("img", &imgs_dir_clone, k, v, reason);
        })
        .build();

    let mut acc = Vec::with_capacity(1024);
    blobcache::read_from_disk(&imgs_dir, |key, size| {
        acc.push((key, size));
    })?;
    let count = acc.len();
    for (key, size) in acc.into_iter() {
        image_cache.insert(key, size).await;
    }
    info!("loaded {count} entries into img cache");
    image_cache.run_pending_tasks().await;

    Ok((image_cache, imgs_dir))
}

async fn respond_ok(
    conn: UnixSeqpacket,
    digest: Digest,
    config: spec::ImageConfiguration,
    erofs_fd: OwnedFd,
) -> anyhow::Result<()> {
    let wire_response = WireResponse::Ok {
        config,
        manifest_digest: digest.to_string(),
    };
    let buf = bincode::encode_to_vec(&wire_response, bincode::config::standard())?;

    let mut ancillary_buffer = [0; 128];
    let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buffer);
    ancillary.add_fds(&[&erofs_fd])?;

    conn.send_vectored_with_ancillary(&[IoSlice::new(&buf)], &mut ancillary)
        .await?;
    Ok(())
}

// these errors are super leaky but not sure something nicer right now
async fn respond_err(conn: UnixSeqpacket, error: anyhow::Error) -> anyhow::Result<()> {
    error!("responding_err {}", error);

    let wire_response = {
        // I don't love this, but plumbing up things more directly into either the Ok so that we
        // consider some "errors" not Err doesn't play well with ? which is nice. This is the
        // classic thing with web servers too of wanting to use ? but not bubble up too much b/c
        // you want to send something to the client. Note the Arc too which comes in because moka
        // Cache or_try_insert_with always returns an Arc error in case there are multiple clients
        // waiting for the result of a computation, they all get a shared reference
        if let Some(e) = error.downcast_ref::<Arc<ocidist_cache::Error>>() {
            match **e {
                ocidist_cache::Error::ManifestNotFound => Some(WireResponse::ManifestNotFound),
                ocidist_cache::Error::NoMatchingManifest => Some(WireResponse::NoMatchingManifest),
                ocidist_cache::Error::ClientError(ocidist::Error::RatelimitExceeded) => {
                    Some(WireResponse::RatelimitExceeded)
                }
                _ => None,
            }
        } else if let Some(e) = error.downcast_ref::<Arc<Error>>() {
            match **e {
                Error::TotalLayerSizeTooBig => Some(WireResponse::ImageTooBig),
                _ => None,
            }
        } else if let Some(e) = error.downcast_ref::<Arc<peimage::squash::Error>>() {
            match **e {
                peimage::squash::Error::Erofs(peerofs::build::Error::MaxSizeExceeded) => {
                    Some(WireResponse::ImageTooBig)
                }
                _ => None,
            }
        } else {
            None
        }
    }
    .unwrap_or_else(|| WireResponse::Err {
        message: "unexpected error".to_string(),
    });
    let buf = bincode::encode_to_vec(&wire_response, bincode::config::standard())?;
    conn.send(&buf).await?;
    Ok(())
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long)]
    listen: String,

    #[arg(long)]
    auth: String,

    #[arg(long)]
    cache: Option<PathBuf>,

    #[arg(long, default_value_t = 10)]
    backlog: u32,

    #[arg(long, default_value_t = 3600)]
    persist_period: u64,

    #[arg(long, default_value_t = 10_000_000)]
    ref_capacity: u64,

    #[arg(long, default_value_t = 10_000_000)]
    manifest_capacity: u64,

    #[arg(long, default_value_t = 50_000_000_000)]
    blob_capacity: u64,

    #[arg(long, default_value_t = 50_000_000_000)]
    img_capacity: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let args = Args::parse();

    let auth = load_stored_auth(args.auth).unwrap();
    info!("loaded {} entries into auth", auth.len());

    let cache_dir = args.cache.unwrap_or_else(|| {
        let home = std::env::vars()
            .find(|(k, _v)| k == "HOME")
            .map(|(_, v)| v)
            .unwrap_or_else(|| "/".to_string());
        PathBuf::from(home).join(".local/share/peoci")
    });

    let (cache, imgs_dir) = make_img_cache(&cache_dir, args.img_capacity).await.unwrap();
    let imgs_dir = Arc::new(imgs_dir);

    let client = Client::builder()
        .dir(cache_dir)
        .load_from_disk(true)
        .auth(auth)
        .ref_capacity(args.ref_capacity)
        .manifest_capacity(args.manifest_capacity)
        .blob_capacity(args.blob_capacity)
        .build()
        .await
        .unwrap();

    let worker_semaphore = Arc::new(Semaphore::new(1));
    let counters = Arc::new(Counters::default());

    let _ = std::fs::remove_file(&args.listen);
    let mut socket =
        UnixSeqpacketListener::bind_with_backlog(args.listen, args.backlog.try_into().unwrap())
            .unwrap();

    let cache_persist_period = tokio::time::Duration::from_secs(args.persist_period);
    let mut cache_persist_timer = tokio::time::interval(cache_persist_period);
    cache_persist_timer.tick().await; // discard first tick that fires immediately

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("got shutdown");
                if let Err(e) = client.persist() {
                    error!("error while persisting {e}");
                }
                break;
            }
            _ = cache_persist_timer.tick() => {
                let stats = Stats {
                    img_cache_hit: atomic_take(&counters.img_cache_hit),
                    img_cache_miss: atomic_take(&counters.img_cache_miss),
                };
                info!("client stats {:?}", client.stats().await);
                info!("img    stats {:?}", stats);
                // TODO img cache stats
                info!("saving cache");
                if let Err(e) = client.persist() {
                    error!("error while persisting {e}");
                }
            }
            accept = socket.accept() => {
                 match accept {
                    Ok(conn) => {
                        let worker_semaphore_ = worker_semaphore.clone();
                        let client_ = client.clone();
                        let cache_ = cache.clone();
                        let imgs_dir_ = imgs_dir.clone();
                        let counters_ = counters.clone();
                        tokio::spawn(async move {
                            match handle_conn(worker_semaphore_, &conn, client_, cache_, imgs_dir_, counters_).await {
                                Ok((digest, config, fd)) => match respond_ok(conn, digest, config, fd).await {
                                    Ok(_) => {}
                                    Err(e) => {
                                        error!("error sending ok {:?}", e);
                                    }
                                },
                                Err(e) => match respond_err(conn, e).await {
                                    Ok(_) => {}
                                    Err(e) => {
                                        error!("error sending err {:?}", e);
                                    }
                                },
                            }
                        });
                    }
                    Err(e) => {
                        error!("accept {}", e);
                    }
                }
            }
        }
    }
}
