use std::{io,fmt,error,env};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::fs::File;

use sha2::{Sha256,Digest};
use tar::{Archive, EntryType};

#[derive(Debug)]
enum TardiffError {
    NoLink,
}
impl fmt::Display for TardiffError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl error::Error for TardiffError {}

#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
enum Entry {
    File {path: PathBuf, digest: String},
    Dir {path: PathBuf},
    Link {path: PathBuf, link: PathBuf},
    Slink {path: PathBuf, link: PathBuf},
}

#[derive(Debug,Default)]
struct Diffs {
    in_left_but_not_right: Vec<Entry>,
    in_right_but_not_left: Vec<Entry>,
    //differing: Vec<(Entry, Entry)>,
}

fn sha_reader<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut hash = Sha256::new();
    io::copy(reader, &mut hash)?;

    Ok(base16ct::lower::encode_string(&hash.finalize()))
}

fn gather_entries<R: Read>(ar: &mut Archive<R>) -> Result<BTreeSet<Entry>, Box<dyn error::Error>> {
    let mut ret = BTreeSet::new();

    for entry in ar.entries()? {
        let mut entry = entry?;
        let path: PathBuf = entry.path()?.into();

        match entry.header().entry_type() {
            EntryType::Regular => {
                let digest = sha_reader(&mut entry)?;
                ret.insert(Entry::File{path, digest});
            }
            EntryType::Directory => {
                ret.insert(Entry::Dir{path});
            }
            EntryType::Link => {
                let link: PathBuf = entry.link_name()?.ok_or(TardiffError::NoLink)?.into();
                ret.insert(Entry::Link{path, link});
            }
            EntryType::Symlink => {
                let link: PathBuf = entry.link_name()?.ok_or(TardiffError::NoLink)?.into();
                ret.insert(Entry::Slink{path, link});
            }
            e => panic!("unhandled type {:?}", e),
        }
    }

    Ok(ret)
}

fn tardiff<R: Read>(left: R, right: R) -> Result<Diffs, Box<dyn error::Error>> {
    let left = gather_entries(&mut Archive::new(left))?;
    let right = gather_entries(&mut Archive::new(right))?;
    let mut ret = Diffs::default();
    ret.in_left_but_not_right = left.difference(&right).cloned().collect();
    ret.in_right_but_not_left = right.difference(&left).cloned().collect();
    Ok(ret)
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let left = args.get(1).expect("give me a left file");
    let right = args.get(2).expect("give me a right file");

    let diffs = tardiff(
        File::open(left).expect("couldn't open left"),
        File::open(right).expect("couldn't open left"),
    ).unwrap();

    println!("-------------------- in left but not right ----------------------");
    for entry in diffs.in_left_but_not_right.iter() {
        println!("{entry:?}");
    }

    println!("-------------------- in right but not left ----------------------");
    for entry in diffs.in_right_but_not_left.iter() {
        println!("{entry:?}");
    }
    //println!("{:?}", diffs.differing);
}
