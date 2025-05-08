use oci_spec::distribution::Reference;
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
};

use peimage::ocidist::Client;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let args: Vec<_> = std::env::args().collect();
    let image_ref: Reference = args.get(1).expect("give me an image ref").parse().unwrap();
    let outfile = args.get(2).expect("give me an outfile");

    println!("{:?}", image_ref);
    let mut client = Client::new().unwrap();
    let manifest_response = client.get_manifest(&image_ref).await.unwrap().unwrap();
    let manifest = manifest_response.get().unwrap();
    println!("got manifest {:#?}", manifest);

    let configuration_response = client
        .get_image_config(&image_ref, manifest.config().digest())
        .await
        .unwrap()
        .unwrap();
    let config = configuration_response.get().unwrap();
    println!("got configuration {:#?}", config);

    let mut writer = BufWriter::new(File::create(outfile).await.unwrap());
    let size = client
        .get_blob(&image_ref, manifest.layers()[0].digest(), &mut writer)
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
