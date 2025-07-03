use pegh::Client;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None, disable_version_flag = true)]
struct Args {
    gist: String,

    #[arg(long)]
    version: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let args = Args::parse();

    let client = Client::new().unwrap();

    let gist = if let Some(version) = args.version {
        client.get_gist_version(&args.gist, &version).await.unwrap()
    } else {
        client.get_gist(&args.gist).await.unwrap()
    };

    if let Some(gist) = gist {
        println!("gist.version = {}", gist.version);
        println!("gist.versions:");
        for version in gist.versions {
            println!("- {version}");
        }
        for (name, contents) in &gist.files {
            println!("=== {name} ===");
            println!("{contents}");
        }
    } else {
        println!("oops not found");
    }
}
