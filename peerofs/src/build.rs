use std::collections::BTreeMap;
use std::io::{Write,Seek,SeekFrom,BufWriter,Read};
use std::path::Path;
use std::ffi::{OsStr,OsString};
use std::os::unix::ffi::OsStrExt;

use zerocopy::IntoBytes;

use crate::disk::{Superblock, EROFS_SUPER_OFFSET, EROFS_SUPER_MAGIG_V1};

// NOTES:
// Our strategy for building an erofs image is different than mkfs.erofs. From what I understand
// (when building from a tar stream), their approach first writes all file contents to something
// to the file starting at something like +2TB. They then write out all the metadata at the start
// of the file, then copy the files to close the gap in the middle. Here, we write file contents
// out starting at the beginning (leaving room for the superblock of course) and track the
// directory structure in memory, then write out the dirents at the end. The superblock
// meta_blkaddr makes this strategy very suitable and seems "right" to me. One drawback is that if
// we use tail packing, we have to keep the tails in memory until writing out the inodes.

#[derive(Debug, PartialEq)]
pub enum Error {
    FileExists,
    BadFilename,
    EmptyPath,
    EmptyFilename,
    NotADir,
    MetaBlockTooBig,
    Other(String),
    Io(std::io::ErrorKind),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.kind())
    }
}

struct DiskId(u32);

#[derive(Default)]
pub struct Meta {
    uid: u32,
    gid: u32,
    mtime: u64,
}

#[derive(Default)]
struct Root {
    root: Dir,
}

pub struct File {
    meta: Meta,
    start_block: u64,
    block_len: usize,
    len: usize,
    tail: Option<Box<[u8]>>,
    inode: Option<u32>,
}

#[derive(Default)]
struct Dir {
    children: BTreeMap<OsString, Dirent>,
    meta: Meta,
    inode: Option<u32>,
    // will need to keep around the number of inodes in each dirent block
}

enum Dirent {
    File(File),
    Dir(Dir),
}

trait TreeVisitor {
    fn on_file(&mut self, file: &mut File) -> Result<(), Error>;
    fn on_dir(&mut self, dir: &mut Dir) -> Result<(), Error>;
}

// TODO maybe do this with an iter to not use stack
fn walk_tree<V: TreeVisitor>(dir: &mut Dir, visitor: &mut V) -> Result<(), Error> {
    for child in dir.children.values_mut() {
        match child {
            Dirent::File(f) => {
                visitor.on_file(f)?;
            }
            Dirent::Dir(d) => {
                let _ = walk_tree(d, visitor)?;
            }
        }
    }
    visitor.on_dir(dir)?;
    Ok(())
}

struct BuilderTreeVisitorPrepareDirents<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorPrepareDirents<'_, W> {
    fn on_file(&mut self, _file: &mut File) -> Result<(), Error> {
        Ok(())
    }
    fn on_dir(&mut self, dir: &mut Dir) -> Result<(), Error> {
        let n_blocks = dir.prepare_dirent_data(self.builder.cur_data_block)?;
        self.builder.cur_data_block += n_blocks;
        Ok(())
    }
}

struct BuilderTreeVisitorWriteFiles<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorWriteFiles<'_, W> {
    fn on_file(&mut self, file: &mut File) -> Result<(), Error> {
        todo!()
    }
    fn on_dir(&mut self, dir: &mut Dir) -> Result<(), Error> {
        todo!()
    }
}

pub struct Builder<W: Write + Seek> {
    root: Root,
    increment_uid_gid: Option<u32>,
    writer: BufWriter<W>,
    superblock: Superblock,
    block_size_bits: u8,
    cur_data_block: u64,
}

impl Root {
    fn add_file<P: AsRef<Path>>(&mut self, path: P, file: File) -> Result<(), Error> {
        let path = path.as_ref();
        // TODO allocating here, hard to cache the vector becuase of the lifetime on borrowed OsStr
        let (name, parents) = name_and_parents(path)?;
        let dir = self.get_or_create_dir(&parents)?;
        dir.children.insert(name.into(), Dirent::File(file));
        Ok(())
    }

    fn get_or_create_dir(&mut self, parts: &[&OsStr]) -> Result<&mut Dir, Error> {
        let mut cur = &mut self.root;
        for part in parts.iter() {
            cur = cur.get_or_create_dir(part)?;
        }
        Ok(cur)
    }
}

impl Dir {
    fn get_or_create_dir(&mut self, name: &OsStr) -> Result<&mut Dir, Error> {
        // annoying that there is no entry api without Borrow<Q> like get_mut b/c we have to
        // allocate just to lookup
        // https://internals.rust-lang.org/t/pre-rfc-abandonning-morals-in-the-name-of-performance-the-raw-entry-api/7043/50
        match self.children.entry(name.into()).or_insert_with(|| Dirent::Dir(Dir::default())) {
            Dirent::Dir(d) => Ok(d),
            _ => Err(Error::NotADir),
        }
        // this doesn't work because of double borrow
        //if let Some(ent) = self.children.get_mut(name) {
        //    match ent.as_mut() {
        //        Dirent::Dir(d) => {
        //            return Ok(d);
        //        }
        //        _ => {
        //            return Err(Error::NotADir);
        //        }
        //    }
        //}
        //let Dirent::Dir(d) = self.children.entry(name.into()).or_insert_with(|| Box::new(Dirent::Dir(Dir::default()))).as_mut() else {
        //    unreachable!()
        //};
        //Ok(d)
    }

    fn prepare_dirent_data(&mut self, data_block: u64) -> Result<u64, Error> {
        todo!()
    }
}

impl<W: Write + Seek> Builder<W> {
    pub fn new(writer: W) -> Result<Self, Error> {
        let block_size_bits = 12; // TODO configurable
        let mut ret = Builder {
            root: Root::default(),
            increment_uid_gid: None,
            writer: BufWriter::new(writer),
            superblock: Superblock::default(),
            cur_data_block: 1,
            block_size_bits,
        };
        ret.seek_block(ret.cur_data_block)?;
        Ok(ret)
    }

    fn block_size(&self) -> u64 {
        1u64 << self.block_size_bits
    }

    fn seek_block(&mut self, block: u64) -> Result<(), Error> {
        let _ = self.writer.seek(SeekFrom::Start(block * self.block_size()))?;
        Ok(())
    }

    // we require the size up front so that we can calculate tail len
    pub fn add_file<P: AsRef<Path>, R: Read>(&mut self, p: P, meta: Meta, len: usize, contents: &mut R) -> Result<(), Error> {
        let (n_blocks, block_len, tail_len) = size_tail_len(len, self.block_size_bits);
        // TODO if tail_len > block_size/2, skip tail packing
        let start_block = self.cur_data_block;
        self.seek_block(self.cur_data_block)?;
        std::io::copy(&mut contents.take(block_len as u64), &mut self.writer)?;
        self.cur_data_block += n_blocks as u64;
        let tail = if tail_len > 0 {
            let mut buf = vec![0; tail_len];
            contents.read_exact(&mut buf)?;
            Some(buf.into_boxed_slice())
        } else {
            None
        };
        let file = File {
            meta,
            start_block,
            block_len,
            len,
            tail,
            inode: None,
        };
        self.root.add_file(p, file)
    }

    fn write_superblock(&mut self) -> Result<(), Error> {
        self.superblock.magic = EROFS_SUPER_MAGIG_V1.into();
        self.superblock.blkszbits = self.block_size_bits;
        self.writer.seek(SeekFrom::Start(EROFS_SUPER_OFFSET as u64))?;
        self.superblock.write_to_io(&mut self.writer)?;
        Ok(())
    }

    // okay so we first have to write all dirents so that they can go into the data block
    // can probably buffer all the dirent data in memory, and just reserve a spot for it in the
    // data section
    // then write out all the inodes
    // then fill in the inodes in the dirents
    fn write_inodes(&mut self) -> Result<(), Error> {
        //self.superblock.meta_blkaddr = self.cur_data_block.try_into().map_err(|_| Error::MetaBlockTooBig)?;
        {
            let mut v = BuilderTreeVisitorPrepareDirents { builder: self };
            walk_tree(&mut self.root.root, &mut v)?;
        }
        {
            let mut v = BuilderTreeVisitorWriteFiles { builder: self };
            walk_tree(&mut self.root.root, &mut v)?;
        }
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), Error> {
        self.write_superblock()?;
        self.write_inodes()?;
        Ok(())
    }

    pub fn into_inner(mut self) -> Result<W, Error> {
        self.finalize()?;
        self.writer.into_inner().map_err(|e| Error::Io(e.error().kind()))
    }
}

fn path_not_file(p: &Path) -> bool {
    let b = p.as_os_str().as_bytes();
    b.ends_with(b"/") || b.ends_with(b"/.") || b.ends_with(b"/..")
}

fn name_and_parents<'a>(p: &'a Path) -> Result<(&'a OsStr, Vec<&'a OsStr>), Error> {
    if path_not_file(p) {
        return Err(Error::BadFilename);
    }
    let mut ret: Vec<_> = p.iter()
        .filter(|x| *x != "/")
        .collect();

    if let Some(last) = ret.pop() {
        if last.is_empty() {
            // should be unreachable
            Err(Error::EmptyFilename)
        } else {
            Ok((last, ret))
        }
    } else {
        Err(Error::EmptyPath)
    }
}

fn size_tail_len(len: usize, block_size_bits: u8) -> (usize, usize, usize) {
    let block_size = 1usize << block_size_bits;
    let n_blocks = len / block_size;
    let tail_len = len % block_size;
    (n_blocks, n_blocks * block_size, tail_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::NamedTempFile;
    use std::process::Command;
    use std::path::Path;
    use std::io::Cursor;

    fn dump_erofs<P: AsRef<Path>>(path: P) -> Result<(), Error> {
        //eprintln!("!! len={:?}", path.as_ref().metadata()?.len());
        //Command::new("xxd").arg(path.as_ref()).status()?;
        let output = Command::new("dump.erofs")
            .arg(path.as_ref())
            .output()
            .expect("dump.erofs failed to run");
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::Other(String::from_utf8_lossy(&output.stderr).into()))
        }
    }

    #[test]
    fn test_name_and_parents() {
        {
            let p = Path::new("/a/b");
            assert_eq!(name_and_parents(p).unwrap(), (OsStr::new("b"), vec![OsStr::new("a")]));
        }
        {
            let p = Path::new("a/b");
            assert_eq!(name_and_parents(p).unwrap(), (OsStr::new("b"), vec![OsStr::new("a")]));
        }
        {
            let p = Path::new("/a");
            assert_eq!(name_and_parents(p).unwrap(), (OsStr::new("a"), vec![]));
        }
        {
            let p = Path::new("a");
            assert_eq!(name_and_parents(p).unwrap(), (OsStr::new("a"), vec![]));
        }
        assert_eq!(name_and_parents(Path::new("/a/")).unwrap_err(), Error::BadFilename);
        assert_eq!(name_and_parents(Path::new("/")).unwrap_err(), Error::BadFilename);
    }

    #[test]
    fn test_builder() -> Result<(), Error> {
        let mut b = Builder::new(NamedTempFile::new().expect("tf"))?;
        {
            let data = b"hello world";
            b.add_file("/foo/bar", Meta::default(), data.len(), &mut Cursor::new(data))?;
        }

        let tf = b.into_inner().expect("io fail");
        dump_erofs(tf.path())?;
        Ok(())
    }
}
