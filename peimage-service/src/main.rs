use std::collections::BTreeMap;
use std::fs::File;
use std::io::IoSlice;
use std::io::Seek;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use log::{error, info};
use moka::future::Cache;
use oci_spec::{
    distribution::Reference,
    image::{Digest, ImageManifest},
};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener, ancillary::AncillaryMessageWriter};

use peimage::squash::squash_to_erofs;
use peimage_service::{Request, WireResponse};
use peoci::{
    blobcache,
    blobcache::BlobKey,
    compression::Compression,
    ocidist::{Auth, AuthMap},
    ocidist_cache::Client,
};
use perunner::iofile::round_up_file_to_pmem_size;

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
    OneshotSend,
    // not sure a better way to handle this
    #[error(transparent)]
    Arc(#[from] Arc<anyhow::Error>),
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
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

async fn handle_conn(
    worker_semaphore: Arc<Semaphore>,
    conn: &UnixSeqpacket,
    client: Client,
    img_cache: ImageCache,
    imgs_dir: Arc<OwnedFd>,
) -> anyhow::Result<(Digest, OwnedFd)> {
    let mut buf = [0; 1024];
    let len = conn.recv(&mut buf).await?;
    let (req, _) =
        bincode::decode_from_slice::<Request, _>(&buf[..len], bincode::config::standard())?;

    let reference = req.parse_reference().ok_or(Error::BadReference)?;

    let image_and_config = client
        .get_image_manifest_and_configuration(&reference)
        .await?;
    let digest = image_and_config.digest()?;
    let manifest = image_and_config.manifest()?;

    let (fd_tx, fd_rx) = tokio::sync::oneshot::channel();

    let key = BlobKey::new(digest.to_string()).ok_or(Error::BadDigest)?;
    let entry = img_cache
        .entry_by_ref(&key)
        .or_try_insert_with(make_erofs_image(
            worker_semaphore.clone(),
            client.clone(),
            &reference,
            &manifest,
            imgs_dir.clone(),
            &key,
            fd_tx,
        ))
        .await
        .map_err(Error::Arc)?;

    if entry.is_fresh() {
        //atomic_inc(&self.counters.blob_cache_miss);
        let digest = entry.key();
        let size = *entry.value();
        info!("img_cache miss digest={digest} size={size}");
    } else {
        //atomic_inc(&self.counters.blob_cache_hit);
        info!("img_cache hit digest={}", entry.key())
    }

    let fd = fd_rx.await?;

    Ok((digest, fd))
}

async fn make_erofs_image(
    worker_semaphore: Arc<Semaphore>,
    client: Client,
    reference: &Reference,
    manifest: &ImageManifest,
    imgs_dir: Arc<OwnedFd>,
    key: &BlobKey,
    fd_tx: tokio::sync::oneshot::Sender<OwnedFd>,
) -> anyhow::Result<u64> {
    let key = key.clone();
    let manifest = manifest.clone();
    let fds = client.get_layers(reference, &manifest).await?;

    let _guard = worker_semaphore.acquire().await;
    tokio::task::spawn_blocking(move || {
        let (mut file, guard) = blobcache::openat_create_write_with_guard(&imgs_dir, &key)?;

        let mut layers: Vec<_> = manifest
            .layers()
            .iter()
            .zip(fds.into_iter())
            .map(|(descriptor, fd)| -> Result<_, Error> {
                let comp: Compression = descriptor.try_into()?;
                let reader: File = fd.into();
                Ok((comp, reader))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let t0 = Instant::now();
        let (squash_stats, erofs_stats) = squash_to_erofs(&mut layers, &mut file)?;
        let elapsed = t0.elapsed().as_secs_f32();
        guard.success()?;
        round_up_file_to_pmem_size(&file)?;
        // ftruncate up to the right size
        let size = file.metadata()?.len();
        file.rewind()?;
        info!("built image for {key} size={size} Squash{squash_stats:?} Erofs{erofs_stats:?} in {elapsed:.2}s");
        if fd_tx.send(file.into()).is_err() {
            return Err(Error::OneshotSend.into());
        }
        Ok(size)
    })
    .await?
}

async fn make_cache(dir: impl AsRef<Path>) -> anyhow::Result<(ImageCache, OwnedFd)> {
    let cache_dir = blobcache::open_or_create_dir_at(None, dir.as_ref())?;
    let imgs_dir = blobcache::open_or_create_dir_at(Some(&cache_dir), "imgs")?;
    let imgs_dir_clone = imgs_dir.try_clone()?;

    let image_cache = Cache::builder()
        .max_capacity(blobcache::max_capacity(5_000_000_000))
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

async fn respond_ok(conn: UnixSeqpacket, digest: Digest, erofs_fd: OwnedFd) -> anyhow::Result<()> {
    let wire_response = WireResponse::Ok {
        manifest_digest: digest.to_string(),
    };
    let mut buf = [0; 1024];
    let n = bincode::encode_into_slice(&wire_response, &mut buf, bincode::config::standard())?;

    let mut ancillary_buffer = [0; 128];
    let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buffer);
    ancillary.add_fds(&[&erofs_fd])?;

    conn.send_vectored_with_ancillary(&[IoSlice::new(&buf[..n])], &mut ancillary)
        .await?;
    Ok(())
}

async fn respond_err(conn: UnixSeqpacket, error: anyhow::Error) -> anyhow::Result<()> {
    error!("responding_err {}", error);
    let wire_response = WireResponse::Err {
        message: "unexpected error".to_string(),
    };
    let mut buf = [0; 1024];
    let n = bincode::encode_into_slice(&wire_response, &mut buf, bincode::config::standard())?;
    conn.send(&buf[..n]).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let args: Vec<_> = std::env::args().collect();
    let socket_path = args.get(1).expect("give me a listening socket");

    let auth = if let Some(v) =
        std::env::vars().find_map(|(k, v)| if k == "PEOCI_AUTH" { Some(v) } else { None })
    {
        load_stored_auth(v).unwrap()
    } else {
        BTreeMap::new()
    };

    let peoci_cache_dir = std::env::vars()
        .find(|(k, _v)| k == "PEOCI_CACHE")
        .map(|(_, v)| Path::new(&v).to_owned())
        .unwrap_or_else(|| {
            Path::new(
                &std::env::vars()
                    .find(|(k, _v)| k == "HOME")
                    .map(|(_, v)| v)
                    .unwrap(),
            )
            .join(".local/share/peoci")
        });

    let (cache, imgs_dir) = make_cache(&peoci_cache_dir).await.unwrap();
    let imgs_dir = Arc::new(imgs_dir);

    let client = Client::builder()
        .dir(peoci_cache_dir)
        .load_from_disk(true)
        .auth(auth)
        .build()
        .await
        .unwrap();

    let worker_semaphore = Arc::new(Semaphore::new(1));

    std::fs::remove_file(socket_path).unwrap();
    let mut socket = UnixSeqpacketListener::bind_with_backlog(socket_path, 10).unwrap();

    loop {
        match socket.accept().await {
            Ok(conn) => {
                let worker_semaphore_ = worker_semaphore.clone();
                let client_ = client.clone();
                let cache_ = cache.clone();
                let imgs_dir_ = imgs_dir.clone();
                tokio::spawn(async move {
                    match handle_conn(worker_semaphore_, &conn, client_, cache_, imgs_dir_).await {
                        Ok((digest, fd)) => match respond_ok(conn, digest, fd).await {
                            Ok(_) => {}
                            Err(e) => {
                                error!("error sending fd {:?}", e);
                            }
                        },
                        Err(e) => match respond_err(conn, e).await {
                            Ok(_) => {}
                            Err(e) => {
                                error!("error sending error {:?}", e);
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
