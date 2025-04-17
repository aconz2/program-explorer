use std::fs::File;

use memmap2::MmapOptions;
//use rustix::fs::FileType;

use peerofs::disk::{Erofs, Inode};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let image = args.get(1).expect("give me an image name");
    let file = File::open(image).expect("file open failed");
    let mmap = unsafe { MmapOptions::new().map(&file).expect("mmap failed") };

    let erofs = Erofs::new(&mmap).expect("fail to create view");

    println!("{:?}", erofs.sb);
    let root_dir = erofs.get_root_inode().expect("inode get failed");
    println!("{:?}", root_dir);
    println!("layout={:?}", root_dir.layout());
    let dirents = erofs.get_dirents(&root_dir).expect("get_dirents failed");
    //println!("{:?}", dirents);
    for item in dirents.iter().expect("couldn't create iterator") {
        println!("{:?}", item);
    }
}
