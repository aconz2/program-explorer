use std::collections::BTreeMap;
use std::fs::File;
use std::io::IoSlice;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;

use log::{info, trace};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener, ancillary::AncillaryMessageWriter};

use peimage::squash::squash_to_erofs;
use peimage_service::{Error, Request};
use peoci::{
    Compression,
    ocidist::{Auth, AuthMap},
    ocidist_cache::Client,
};

#[derive(Deserialize)]
struct AuthEntry {
    username: String,
    password: String,
}

type StoredAuth = BTreeMap<String, AuthEntry>;

//enum Error {
//}

fn load_stored_auth(p: impl AsRef<Path>) -> AuthMap {
    let stored: StoredAuth = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    stored
        .into_iter()
        .map(|(k, v)| (k, Auth::UserPass(v.username, v.password)))
        .collect()
}

async fn handle_conn(
    worker_lock: Arc<Mutex<()>>,
    conn: UnixSeqpacket,
    client: Client,
) -> Result<(), Error> {
    let mut buf = [0; 1024];
    let len = conn.recv(&mut buf).await?;
    let (req, _) =
        bincode::decode_from_slice::<Request, _>(&buf[..len], bincode::config::standard())?;

    let reference = req.parse_reference().unwrap();

    let image_and_config = client
        .get_image_manifest_and_configuration(&reference)
        .await
        .unwrap();
    let manifest = image_and_config.manifest().unwrap();
    let fds = client.get_layers(&reference, &manifest).await.unwrap();

    let mut layers: Vec<_> = manifest
        .layers()
        .iter()
        .zip(fds.into_iter())
        .map(|(info, fd)| -> Result<_, Error> {
            let c: Compression = info.media_type().try_into().unwrap();
            let reader: File = fd.into();
            Ok((c, reader))
        })
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let erofs_fd: OwnedFd = {
        let _guard = worker_lock.lock().await;
        let handle = std::thread::spawn(move || {
            // TODO this should be gotten from a cache
            let mut outfile = File::create("/tmp/foo").unwrap(); // Todo
            let (_squash_stats, _erofs_stats) = squash_to_erofs(&mut layers, &mut outfile).unwrap();
            info!(
                "building req {}/{} stats",
                reference.registry(),
                reference.repository()
            );
            outfile.into()
        });
        // TODO this is totally wrong to wait blocking here
        handle.join()
    }
    .unwrap();

    let mut ancillary_buffer = [0; 128];
    let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buffer);
    ancillary.add_fds(&[&erofs_fd]).unwrap();

    // todo write the WireResponse into buf
    let bufs = [IoSlice::new(&buf)];
    conn.send_vectored_with_ancillary(&bufs, &mut ancillary)
        .await
        .unwrap();

    // todo add wireresponse

    Ok(())
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
                tokio::spawn(handle_conn(worker_lock.clone(), conn, client.clone()));
            }
            Err(e) => {
                trace!("error accept {:?}", e);
            }
        }
    }
}
