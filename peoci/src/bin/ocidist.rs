use std::collections::BTreeMap;
use std::path::Path;

use oci_spec::{
    distribution::Reference,
    image::{Arch, Os},
};
use serde::Deserialize;
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
};

use peoci::ocidist::{Auth, AuthMap};

#[derive(Deserialize)]
struct AuthEntry {
    username: String,
    password: String,
}

type StoredAuth = BTreeMap<String, AuthEntry>;

fn load_stored_auth(p: impl AsRef<Path>) -> AuthMap {
    let stored: StoredAuth = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    stored
        .into_iter()
        .map(|(k, v)| (k, Auth::UserPass(v.username, v.password)))
        .collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();

    let args: Vec<_> = std::env::args().collect();
    let image_ref: Reference = args.get(1).expect("give me an image ref").parse().unwrap();

    let auth = if let Some(v) =
        std::env::vars().find_map(|(k, v)| if k == "PEOCI_AUTH" { Some(v) } else { None })
    {
        load_stored_auth(v)
    } else {
        BTreeMap::new()
    };

    println!("{:?}", image_ref);

    let cache = true;

    if cache {
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
        let client = peoci::ocidist_cache::Client::builder()
            .dir(peoci_cache_dir)
            .load_from_disk(true)
            .auth(auth)
            .build()
            .await
            .unwrap();

        let res = client
            .get_image_manifest_and_configuration(&image_ref)
            .await
            .unwrap();
        let manifest = res.manifest().unwrap();
        println!("got manifest {:#?}", res.manifest());
        println!("got configuration {:#?}", res.configuration());

        //let _fd = client
        //    .get_blob(&image_ref, manifest.layers()[0].digest())
        //    .await
        //    .unwrap();
        //println!("got blob {:?}", manifest.layers()[0].digest());
        let layers = client.get_layers(&image_ref, &manifest).await.unwrap();
        println!("got layers {:?}", layers);

        println!("{:#?}", client.stats().await);

        client.persist().unwrap();
    } else {
        let mut client = peoci::ocidist::Client::new().unwrap();

        client.set_auth(auth).await;

        let outfile = args.get(2);

        let image_ref = if image_ref.digest().is_some() {
            image_ref
        } else {
            let manifest_descriptor = client
                .get_matching_descriptor_from_index(&image_ref, Arch::Amd64, Os::Linux)
                .await
                .unwrap()
                .unwrap();
            image_ref.clone_with_digest(manifest_descriptor.digest().to_string())
        };

        let manifest_response = client
            .get_image_manifest(&image_ref)
            .await
            .unwrap()
            .unwrap();
        let manifest = manifest_response.get().unwrap();
        println!("got manifest {:#?}", manifest);

        let configuration_response = client
            .get_image_configuration(&image_ref, manifest.config())
            .await
            .unwrap()
            .unwrap();
        let config = configuration_response.get().unwrap();
        println!("got configuration {:#?}", config);

        if let Some(outfile) = outfile {
            let mut writer = BufWriter::new(File::create(outfile).await.unwrap());
            let size = client
                .get_blob(&image_ref, &manifest.layers()[0], &mut writer)
                .await
                .unwrap()
                .unwrap();
            writer.flush().await.unwrap();
            let file = writer.into_inner();
            println!(
                "wrote {size} bytes, file size is {}",
                file.metadata().await.unwrap().len()
            );
        }
    }
}
