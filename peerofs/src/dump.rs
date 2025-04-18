use std::collections::HashSet;
use std::fs::File;

use memmap2::MmapOptions;
use rustix::fs::FileType;

use peerofs::disk::{DirentFileType, Erofs, Error, Inode, Layout};

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
            //FileType::RegularFile => {
            //}
            FileType::Directory => {
                let dirents = erofs.get_dirents(&inode)?;
                //eprintln!("iterating dirent id {:?}", inode.disk_id());
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

fn all_inodes<'a>(erofs: &Erofs<'a>) -> Result<Vec<Inode<'a>>, Error> {
    let mut seen = HashSet::new();
    let mut ret = vec![];
    let mut q = vec![erofs.get_root_inode()?.disk_id()];

    while let Some(cur) = q.pop() {
        if !seen.insert(cur) {
            continue;
        }
        let inode = erofs.get_inode(cur)?;
        match inode.file_type() {
            FileType::Directory => {
                let dirents = erofs.get_dirents(&inode)?;
                //eprintln!("iterating dirent id {:?}", inode.disk_id());
                for item in dirents.iter()? {
                    let item = item?;
                    q.push(item.disk_id.try_into().expect("why is this u64"));
                }
            }
            _ => {}
        }
        ret.push(inode);
    }
    Ok(ret)
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let image = args.get(1).expect("give me an image name");
    let file = File::open(image).expect("file open failed");
    let mmap = unsafe { MmapOptions::new().map(&file).expect("mmap failed") };

    let erofs = Erofs::new(&mmap).expect("fail to create view");

    println!("{:?}", erofs.sb);
    //if false {
    //    let node = erofs.get_root_inode().expect("inode get failed");
    //    println!(
    //        "{:?} {:x} {}",
    //        node.layout(),
    //        node.raw_block_addr(),
    //        node.data_size()
    //    );
    //    // this is for clang:17 no lz4
    //    let node = erofs.get_inode(2427390).expect("inode get failed");
    //    println!(
    //        "{:?} {:x} {}",
    //        node.layout(),
    //        node.raw_block_addr(),
    //        node.data_size()
    //    );
    //    let node = erofs.get_inode(39099352).expect("inode get failed");
    //    println!(
    //        "{:?} {:x} {}",
    //        node.layout(),
    //        node.raw_block_addr(),
    //        node.data_size()
    //    );
    //}
    let dir = erofs.get_root_inode().expect("inode get failed");
    //println!("{:?}", root_dir);
    //let dir = erofs.get_inode(2427390).expect("inode get failed"); //
    //let dir = erofs.get_inode(39099352).expect("inode get failed"); // usr/share/doc
    println!("{:?}", dir);
    println!("layout={:?}", dir.layout());
    let dirents = erofs.get_dirents(&dir).expect("get_dirents failed");

    for item in dirents.iter().expect("couldn't create iterator") {
        let item = item.expect("bad item");
        let inode = erofs.get_inode_dirent(&item).unwrap();
        print!(
            "  {:>20} {:4} {:?} {}/{} {:o}",
            item.name.escape_ascii().to_string(),
            item.disk_id,
            item.file_type,
            inode.uid(),
            inode.gid(),
            inode.mode()
        );
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
            DirentFileType::RegularFile => {
                let inode = erofs.get_inode_dirent(&item).unwrap();
                print!(" {} ({:?} block={:x})", inode.data_size(), inode.layout(), inode.raw_block_addr());
            }
            _ => {}
        }
        println!("");
    }

    let inodes = all_inodes(&erofs).expect("inode gather fail");
    if let Some(inode) = inodes.iter().find(|x| x.layout() == Layout::CompressedCompact) {
        println!("inode disk_id={:?} {:?} {:?} size={:?} {:?}", inode.disk_id(), inode.file_type(), inode.layout(), inode.data_size(), inode.raw_block_addr());
        let map = erofs.get_map_header(&inode).expect("failed to get map header");
        println!("{:?}", map);
    }

    //if let Some(inode) = find_with_xattr(&erofs).unwrap() {
    //    println!("yo got inode with erofs {:?}", inode);
    //} else {
    //    println!("didn't find anything with nonzero xattr size");
    //}
}
