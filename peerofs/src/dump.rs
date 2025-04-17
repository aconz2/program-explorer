use std::fs::File;
use std::collections::HashSet;

use memmap2::MmapOptions;
use rustix::fs::FileType;

use peerofs::disk::{Erofs, Inode, DirentFileType, Error};

fn find_with_xattr<'a>(erofs: &Erofs<'a>) -> Result<Option<Inode<'a>>, Error> {
    let mut seen = HashSet::new();
    let mut q = vec![erofs.get_root_inode()?.disk_id()];

    while let Some(cur) = q.pop() {
        if !seen.insert(cur) {
            continue;
        }
        let inode = erofs.get_inode(cur)?;
        if inode.xattr_size() > 0 {
            return Ok(Some(inode));
        }
        match inode.file_type() {
            FileType::Directory => {
                let dirents = erofs.get_dirents(&inode)?;
                for item in dirents.iter()? {
                    let item = item?;
                    q.push(item.disk_id.try_into().expect("why is this u64"));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let image = args.get(1).expect("give me an image name");
    let file = File::open(image).expect("file open failed");
    let mmap = unsafe { MmapOptions::new().map(&file).expect("mmap failed") };

    let erofs = Erofs::new(&mmap).expect("fail to create view");

    println!("{:?}", erofs.sb);
    //let root_dir = erofs.get_root_inode().expect("inode get failed");
    //println!("{:?}", root_dir);
    let dir = erofs.get_inode(2427390).expect("inode get failed");
    //println!("layout={:?}", root_dir.layout());
    let dirents = erofs.get_dirents(&dir).expect("get_dirents failed");

    for item in dirents.iter().expect("couldn't create iterator") {
        let item = item.expect("bad item");
        let inode = erofs.get_inode_dirent(&item).unwrap();
        print!("  {:>20} {:4} {:?} {}/{} {:o}", item.name.escape_ascii().to_string(), item.disk_id, item.file_type, inode.uid(), inode.gid(), inode.mode());
        //println!("{:?}", inode);
        match item.file_type {
            //DirentFileType::Directory => {
            //    let child_inode = erofs.get_inode_dirent(&item).expect("fail to get child inode");
            //    let dir_dirents = erofs.get_dirents(&child_inode).expect("fail to get child dirents");
            //    for item in dirents.iter().expect("couldn't create iterator") {
            //        println!("  {:?}", item);
            //    }
            //}
            DirentFileType::Symlink => {
                let inode = erofs.get_inode_dirent(&item).unwrap();
                let link = erofs.get_symlink(&inode).unwrap();
                print!(" -> {}", link.escape_ascii().to_string());
            }
            _ => {
            }
        }
        println!("");
    }

    if let Some(inode) = find_with_xattr(&erofs).unwrap() {
        println!("yo got inode with erofs {:?}", inode);
    } else {
        println!("didn't find anything with nonzero xattr size");
    }
}
