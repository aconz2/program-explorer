use std::fs::File;
use std::io::{BufReader,Read};

use peerofs::superblock::Superblock;

use rustix::fs::FileType;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let image = args.get(1).expect("give me an image name");
    let mut file = BufReader::new(File::open(image).expect("file open failed"));

    let sb = Superblock::from_reader(&mut file)
        .expect("read failed")
        .expect("invalid superblock");

    println!("{sb}");

    if let Some(inode) = sb.get_inode(&mut file, sb.root_inode()).expect("root nid lookup failed") {
        println!("root nid");
        println!("{inode}");

        match inode.file_type() {
            FileType::Directory => {
                println!("yo this is a directory!");
                // this is only valid for FLAT_INLINE
                let mut buf = vec![0; inode.size() as usize];
                file.read_exact(&mut buf).unwrap();
                println!("dir data\n{:?}", buf.as_slice().escape_ascii().to_string());
            }
            x => {
                println!("yo this is a {x:?}");
            }
        }
        //let mut buf = [0u8; 128];
        //file.read_exact(&mut buf);
        //println!("{:?}", buf);
    }

}
