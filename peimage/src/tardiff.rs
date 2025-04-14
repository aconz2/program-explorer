use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::{env, error, fmt, io};

use sha2::{Digest, Sha256};
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

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum EntryTyp {
    File,
    Dir,
    Link,
    Symlink,
    Fifo,
}

type Ext = Vec<(String, Vec<u8>)>;

#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
struct Entry {
    typ: EntryTyp,
    path: PathBuf,
    data: Option<String>, // digest
    ext: Ext,
    link: Option<PathBuf>,
    mtime: u64,
    uid: u64,
    gid: u64,
    mode: u32,
}

#[derive(Debug)]
struct Diffs {
    in_left_but_not_right: Vec<Entry>,
    in_right_but_not_left: Vec<Entry>,
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

        let header = entry.header();
        let uid = header.uid().unwrap();
        let gid = header.gid().unwrap();
        let mode = header.mode().unwrap();
        let mtime = header.mtime().unwrap();
        let entry_type = header.entry_type();

        let typ = match entry_type {
            EntryType::Regular => EntryTyp::File,
            EntryType::Directory => EntryTyp::Dir,
            EntryType::Link => EntryTyp::Link,
            EntryType::Symlink => EntryTyp::Symlink,
            EntryType::Fifo => EntryTyp::Fifo,
            x => {
                panic!("unhandled entry type {x:?}");
            }
        };

        let link = match entry_type {
            tar::EntryType::Link | tar::EntryType::Symlink => {
                Some(entry.link_name()?.ok_or(TardiffError::NoLink)?.into())
            }
            _ => None,
        };

        let data = match entry_type {
            tar::EntryType::Regular => Some(sha_reader(&mut entry)?),
            _ => None,
        };

        let ext = {
            if let Some(ext) = entry.pax_extensions().unwrap() {
                ext.into_iter()
                    .map(|x| x.unwrap())
                    .map(|x| (x.key().unwrap().to_string(), Vec::from(x.value_bytes())))
                    .collect()
            } else {
                vec![]
            }
        };

        let e = Entry {
            typ,
            path,
            link,
            ext,
            data,
            uid,
            gid,
            mode,
            mtime,
        };

        ret.insert(e);
    }

    Ok(ret)
}

fn tardiff<R: Read>(left: R, right: R) -> Result<Diffs, Box<dyn error::Error>> {
    let left = gather_entries(&mut Archive::new(left))?;
    let right = gather_entries(&mut Archive::new(right))?;
    Ok(Diffs {
        in_left_but_not_right: left.difference(&right).cloned().collect(),
        in_right_but_not_left: right.difference(&left).cloned().collect(),
    })
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let left = args.get(1).expect("give me a left file");
    let right = args.get(2).expect("give me a right file");

    let diffs = tardiff(
        File::open(left).expect("couldn't open left"),
        File::open(right).expect("couldn't open left"),
    )
    .unwrap();

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
