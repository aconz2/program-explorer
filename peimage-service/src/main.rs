use std::collections::BTreeMap;
use std::fs::File;
use std::io::IoSlice;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Error;
use log::{error, info, trace};
use moka::future::Cache;
use oci_spec::{distribution::Reference, image::ImageManifest};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener, ancillary::AncillaryMessageWriter};

use peimage::squash::squash_to_erofs;
use peimage_service::Request;
use peoci::{
    Compression, blobcache,
    blobcache::BlobKey,
    ocidist::{Auth, AuthMap},
    ocidist_cache::Client,
};

#[derive(Deserialize)]
struct AuthEntry {
    username: String,
    password: String,
}

#[derive(Debug, thiserror::Error)]
enum Er {
    #[error("do you reall have to do this for each variant")]
    BadDigest,
    #[error("do you reall have to do this for each variant")]
    BadReference,
    #[error("do you reall have to do this for each variant")]
    MissingFile,
    #[error("do you reall have to do this for each variant")]
    OpenBlob,
}

type StoredAuth = BTreeMap<String, AuthEntry>;
type ImageCache = Cache<BlobKey, u64>;

fn load_stored_auth(p: impl AsRef<Path>) -> AuthMap {
    let stored: StoredAuth = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    stored
        .into_iter()
        .map(|(k, v)| (k, Auth::UserPass(v.username, v.password)))
        .collect()
}

async fn handle_conn(
    worker_lock: Arc<Mutex<()>>,
    conn: &UnixSeqpacket,
    client: Client,
    img_cache: ImageCache,
    imgs_dir: Arc<OwnedFd>,
) -> Result<OwnedFd, Error> {
    let mut buf = [0; 1024];
    let len = conn.recv(&mut buf).await?;
    let (req, _) =
        bincode::decode_from_slice::<Request, _>(&buf[..len], bincode::config::standard())?;

    let reference = req.parse_reference().ok_or(Er::BadReference)?;

    let image_and_config = client
        .get_image_manifest_and_configuration(&reference)
        .await
        .unwrap();
    let digest = image_and_config.digest()?;
    let manifest = image_and_config.manifest()?;

    let start = Instant::now();
    let key = BlobKey::new(digest.to_string()).ok_or(Er::BadDigest)?;
    let entry = img_cache
        .entry_by_ref(&key)
        .or_try_insert_with(make_erofs_image(
            worker_lock.clone(),
            client.clone(),
            &reference,
            &manifest,
            imgs_dir.clone(),
            &key,
        ))
        .await
        .unwrap();

    if entry.is_fresh() {
        //atomic_inc(&self.counters.blob_cache_miss);
        let digest = entry.key();
        let size = *entry.value();
        let elapsed = start.elapsed();
        let speed = (size as f32) / 1_000_000.0 / elapsed.as_secs_f32();
        info!(
            "img_cache miss digest={digest} size={size} elapsed={elapsed:?} speed={speed:.2} MB/s"
        );
    } else {
        //atomic_inc(&self.counters.blob_cache_hit);
        info!("img_cache hit digest={}", entry.key())
    }

    // b/c or_try_insert_with must return the V stored in the cache and we don't want to store
    // the ownedfd in the cache, then we have to race here and just try opening it after
    // checking/populating the cache. Not ideal but only way it isn't here is if it was
    // immediately removed from the cache
    match blobcache::openat_read_key(&imgs_dir, &key) {
        Ok(Some(file)) => Ok(file.into()),
        Ok(None) => {
            error!("blob cache missing file {}", digest);
            Err(Er::MissingFile.into())
        }
        Err(e) => {
            error!("error opening blob {:?}", e);
            Err(Er::OpenBlob.into())
        }
    }
}

async fn make_erofs_image(
    worker_lock: Arc<Mutex<()>>,
    client: Client,
    reference: &Reference,
    manifest: &ImageManifest,
    imgs_dir: Arc<OwnedFd>,
    key: &BlobKey,
) -> Result<u64, Error> {
    let key = key.clone();
    let manifest = manifest.clone();

    // ugh really don't want to do stderr
    let fds = client.get_layers(&reference, &manifest).await.unwrap();

    let _guard = worker_lock.lock().await;
    tokio::task::spawn_blocking(move || {
        let (mut file, guard) = blobcache::openat_create_write_with_guard(&imgs_dir, &key)?;

        let mut layers: Vec<_> = manifest
            .layers()
            .iter()
            .zip(fds.into_iter())
            .map(|(descriptor, fd)| -> Result<_, Error> {
                // todo handle docker media type
                //let c: Compression = info.media_type().try_into().unwrap();
                let c: Compression = descriptor.try_into().unwrap();
                let reader: File = fd.into();
                Ok((c, reader))
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        squash_to_erofs(&mut layers, &mut file).unwrap();
        guard.success()?;
        // ftruncate up to the right size
        let size = file.metadata()?.len();
        Ok(size)
    })
    .await?
}

async fn make_cache(dir: impl AsRef<Path>) -> Result<(ImageCache, OwnedFd), Error> {
    let cache_dir = blobcache::open_or_create_dir_at(None, dir.as_ref()).unwrap();
    let imgs_dir = blobcache::open_or_create_dir_at(Some(&cache_dir), "imgs").unwrap();
    let imgs_dir_clone = imgs_dir.try_clone().unwrap();

    let image_cache = Cache::builder()
        .max_capacity(blobcache::max_capacity(1_000_000_000))
        .weigher(blobcache::weigher)
        .eviction_listener(move |k, v, reason| {
            blobcache::remove_blob("ocidist_cache", &imgs_dir_clone, k, v, reason);
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
    info!("loaded {count} entries into blob cache");

    Ok((image_cache, imgs_dir))
}

async fn respond_fd(_conn: UnixSeqpacket, _fd: OwnedFd) -> Result<(), Error> {
    //let mut ancillary_buffer = [0; 128];
    //let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buffer);
    //ancillary.add_fds(&[&erofs_fd]).unwrap();
    //
    //// todo write the WireResponse into buf
    //let bufs = [IoSlice::new(&buf)];
    //conn.send_vectored_with_ancillary(&bufs, &mut ancillary)
    //    .await
    //    .unwrap();
    todo!()
}

async fn respond_error(_conn: UnixSeqpacket) -> Result<(), Error> {
    todo!()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().collect();

    let auth = if let Some(v) =
        std::env::vars().find_map(|(k, v)| if k == "PEOCI_AUTH" { Some(v) } else { None })
    {
        load_stored_auth(v)
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

    let worker_lock = Arc::new(Mutex::new(()));

    let socket_path = args.get(1).expect("give me a listening socket");

    let mut socket = UnixSeqpacketListener::bind_with_backlog(socket_path, 10).unwrap();

    loop {
        match socket.accept().await {
            Ok(conn) => {
                let worker_lock_ = worker_lock.clone();
                let client_ = client.clone();
                let cache_ = cache.clone();
                let imgs_dir_ = imgs_dir.clone();
                tokio::spawn(async move {
                    match handle_conn(worker_lock_, &conn, client_, cache_, imgs_dir_).await {
                        Ok(fd) => match respond_fd(conn, fd).await {
                            Ok(_) => {}
                            Err(e) => {
                                error!("error sending fd {:?}", e);
                            }
                        },
                        Err(e) => match respond_error(conn).await {
                            Ok(_) => {}
                            Err(e) => {
                                error!("error sending error {:?}", e);
                            }
                        },
                    }
                });
            }
            Err(e) => {
                trace!("error accept {:?}", e);
            }
        }
    }
}
