use std::time::Instant;

use memmap2::MmapOptions;
use oci_spec::image::{Arch, Os};
use peerofs::disk::Erofs;
use peimage_service::{Request, request_erofs_image};

async fn main_() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let socket_path = args.get(1).expect("give me a socket path");
    let reference = args.get(2).expect("give me an image reference");

    let request = Request::new(reference, &Arch::Amd64, &Os::Linux).unwrap();
    let t0 = Instant::now();
    let response = request_erofs_image(socket_path, request).await?;
    let elapsed = t0.elapsed().as_secs_f32();
    println!("got response in {elapsed:.3}s");

    let mmap = unsafe { MmapOptions::new().map(&response.fd)? };
    let erofs = Erofs::new(&mmap)?;
    let dir = erofs.get_root_inode()?;
    let dirents = erofs.get_dirents(&dir)?;

    for item in dirents.iter()? {
        let item = item?;
        let inode = erofs.get_inode_from_dirent(&item)?;
        println!(
            "  {:>20} {:4} {:?} {}/{} {:o}",
            item.name.escape_ascii().to_string(),
            item.disk_id,
            item.file_type,
            inode.uid(),
            inode.gid(),
            inode.mode()
        );
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    main_().await.unwrap();
}
