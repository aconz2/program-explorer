use oci_spec::distribution::Reference;
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();

    let args: Vec<_> = std::env::args().collect();
    let image_ref: Reference = args.get(1).expect("give me an image ref").parse().unwrap();
    let outfile = args.get(2);

    println!("{:?}", image_ref);

    let cache = false;

    if cache {
        let peoci_cache_dir = std::env::vars()
            .find(|(k, _v)| k == "PEOCI_CACHE")
            .map(|(_, v)| v)
            .unwrap_or("~/.local/share/peoci".to_string());
        let mut client = peimage::ocidist_cache::Client::builder()
            .dir(peoci_cache_dir)
            .build()
            .unwrap();

        let res = client
            .get_image_manifest_and_configuration(&image_ref)
            .await
            .unwrap();
        println!("got manifest {:#?}", res.manifest());
        println!("got configuration {:#?}", res.configuration());
    } else {
        let mut client = peimage::ocidist::Client::new().unwrap();

        let manifest_response = client
            .get_image_manifest(&image_ref)
            .await
            .unwrap()
            .unwrap();
        let manifest = manifest_response.get().unwrap();
        println!("got manifest {:#?}", manifest);

        let index_response = client.get_image_index(&image_ref).await.unwrap().unwrap();
        let index = index_response.get().unwrap();
        println!("got index {:#?}", index);

        let configuration_response = client
            .get_image_configuration(&image_ref, manifest.config().digest())
            .await
            .unwrap()
            .unwrap();
        let config = configuration_response.get().unwrap();
        println!("got configuration {:#?}", config);

        // different api for cache
        //if let Some(outfile) = outfile {
        //    let mut writer = BufWriter::new(File::create(outfile).await.unwrap());
        //    let size = client
        //        .get_blob(&image_ref, manifest.layers()[0].digest(), &mut writer)
        //        .await
        //        .unwrap()
        //        .unwrap();
        //    writer.flush().await.unwrap();
        //    let file = writer.into_inner();
        //    println!(
        //        "wrote {size} bytes, file size is {}",
        //        file.metadata().await.unwrap().len()
        //    );
        //}
    }
}
