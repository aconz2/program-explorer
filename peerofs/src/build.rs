use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use zerocopy::{IntoBytes,FromZeros};
use rustix::fs::FileType;

use crate::disk;
use crate::disk::{Superblock, EROFS_SUPER_MAGIG_V1, EROFS_SUPER_OFFSET, Inode, InodeExtended, InodeInfo, InodeType, Layout, DirentFileType};

// NOTES:
// Our strategy for building an erofs image is different than mkfs.erofs. From what I understand
// (when building from a tar stream), their approach first writes all file contents to something
// to the file starting at something like +2TB. They then write out all the metadata at the start
// of the file, then copy the files to close the gap in the middle. Here, we write file contents
// out starting at the beginning (leaving room for the superblock of course) and track the
// directory structure in memory, then write out the dirents at the end. The superblock
// meta_blkaddr makes this strategy very suitable and seems "right" to me. One drawback is that if
// we use tail packing, we have to keep the tails in memory until writing out the inodes.
//
// A bit more detail, currently the strategy is (logical phases, phase 2+3 are fused):
// Phase 1:
//  - Add files which builds up an in memory tree of dirs + files
//  - Adding a file appends data to the output file in blocks
//  - Tail packed data is stored in memory (worst case here is every file is tail packed and we
//  store the sum total in memory. TODO is get the threshold right about when to use tail packing
// Phase 2:
//  - No more changes to the tree are allowed
//  - Walk dirs to compute how many blocks we'll need to store the dirents data and count dirs.
//  Also store the block addr of where the dirents will be
//  - We now know where our meta block start is
//  - Reserve enough space at the front of the meta block for the dir inodes. This makes sure we
//  can fit our root disk id in u16
// Phase 3:
//  - Walking the tree post order, write out file inodes (including tail packing) and record their
//  disk id
//  - On dir exit, every child will have a disk id and we can
//    1) write out the dirents data at the recorded data block start
//    2) write out (buffered) the inode for this dir
//  - Finish by writing the buffered dir inode data
//
// TODO a lot
// BufWriter always flushes on seek, which is a bit annoying since I was expecting it to keep track of
// where we are and only flush if necessary

#[derive(Debug, PartialEq)]
pub enum Error {
    FileExists,
    BadFilename,
    EmptyPath,
    EmptyFilename,
    NotADir,
    MetaBlockTooBig,
    FileBlockTooBig,
    TailTooBig,
    NoMetaBlock,
    AddrLessThanMetaBlock,
    AddrNotAligned,
    DiskIdTooBig,
    NoDiskId,
    NoRootDiskId,
    RootDiskIdTooBig,
    NoStartBlock,
    IterFail,
    NameOffsetTooBig,
    UidGidTooBig,
    Other(String),
    Io(std::io::ErrorKind),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.kind())
    }
}

pub struct Builder<W: Write + Seek> {
    root: Option<Root>,
    increment_uid_gid: Option<u32>,
    writer: BufWriter<W>,
    superblock: Superblock,
    block_size_bits: u8,
    cur_data_block: u64,
    meta_block: Option<u64>,
    name_buf: Vec<u8>,
    dirent_buf: Vec<u8>,
    dir_inode_buf: Vec<u8>,
    n_dirs: usize,
    n_inodes: u64,
    dir_inode_id: u64,
}

#[derive(Debug)]
pub struct Meta {
    uid: u32,
    gid: u32,
    mtime: u64,
    mode: u16,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            uid: 0,
            gid: 0,
            mtime: 0,
            mode: 0o755,
        }
    }
}

#[derive(Default)]
struct Root {
    root: Dir,
}

#[derive(Debug)]
pub struct File {
    meta: Meta,
    start_block: u64,
    len: usize,
    tail: Option<Box<[u8]>>,
    disk_id: Option<u32>,
}

#[derive(Default, Debug)]
struct Dir {
    children: BTreeMap<OsString, Dirent>,
    meta: Meta,
    disk_id: Option<u32>,
    // start of data block where dirents is located
    start_block: Option<u64>,
    // number of dirents in each block
    n_dirents_per_block: Vec<u16>,
    // TODO  did this dirent have tail packing
    //tail: bool,
}

#[derive(Debug)]
enum Dirent {
    File(File),
    Dir(Dir),
}

impl Dirent {
    fn disk_id(&self) -> Option<u32> {
        match self {
            Dirent::File(f) => f.disk_id,
            Dirent::Dir(d) => d.disk_id,
        }
    }

    fn file_type(&self) -> DirentFileType {
        match self {
            Dirent::File(_) => DirentFileType::RegularFile,
            Dirent::Dir(_) => DirentFileType::Directory,
        }
    }
}

trait TreeVisitor {
    fn on_file(&mut self, _file: &mut File) -> Result<(), Error> {
        Ok(())
    }
    fn on_dir_exit(&mut self, _dir: &mut Dir) -> Result<(), Error> {
        Ok(())
    }
    fn on_dir_enter(&mut self, _dir: &mut Dir) -> Result<(), Error> {
        Ok(())
    }
}

// TODO maybe do this with an iter to not use stack
fn walk_tree<V: TreeVisitor>(dir: &mut Dir, visitor: &mut V) -> Result<(), Error> {
    visitor.on_dir_enter(dir)?;
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
    visitor.on_dir_exit(dir)?;
    Ok(())
}

struct BuilderTreeVisitorPrepareDirents<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorPrepareDirents<'_, W> {
    fn on_dir_enter(&mut self, dir: &mut Dir) -> Result<(), Error> {
        let n_blocks =
            dir.prepare_dirent_data(self.builder.block_size(), self.builder.cur_data_block);
        self.builder.n_dirs += 1;
        self.builder.cur_data_block += n_blocks;
        Ok(())
    }
}

struct BuilderTreeVisitorWriteInodes<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorWriteInodes<'_, W> {
    fn on_file(&mut self, file: &mut File) -> Result<(), Error> {
        let inode = {
            let mut i = InodeExtended::new_zeroed();
            i.format_layout = Inode::format_layout(InodeType::Extended, Layout::FlatInline).into();
            i.mode = (FileType::RegularFile.as_raw_mode() as u16 | file.meta.mode).into();
            println!("file mode={:16b}", i.mode);
            i.uid = file.meta.uid.into();
            i.gid = file.meta.gid.into();
            i.mtime = file.meta.mtime.into();
            i.nlink = 1.into(); // TODO!
            i.info = InodeInfo::raw_block(
                file.start_block
                    .try_into()
                    .map_err(|_| Error::FileBlockTooBig)?,
            );
            i.size = (file.len as u64).into();
            self.builder.hook_inode_extended(&mut i)?;
            i
        };

        let disk_id = self.builder.write_inode(inode.as_bytes(), &file.tail)?;
        println!("file disk_id={}", disk_id);
        let prev = file.disk_id.replace(disk_id);
        if prev.is_some() {
            panic!("double insertion of file inode");
        }
        Ok(())
    }

    fn on_dir_exit(&mut self, dir: &mut Dir) -> Result<(), Error> {
        let start_block = dir.start_block.ok_or(Error::NoStartBlock)?;
        self.builder.seek_block(start_block)?;

        let disk_id: u32 = self.builder.dir_inode_id.try_into().map_err(|_| Error::DiskIdTooBig)?;
        self.builder.dir_inode_id += 2; // TODO this is only valid without tail packing

        let mut total_size = 0u64;
        let mut iter = dir.children.iter();

        let n_blocks = dir.n_dirents_per_block.len();

        for (block, count) in dir.n_dirents_per_block.iter().enumerate() {
            let count = *count;
            let mut name_offset = (count as usize) * std::mem::size_of::<disk::Dirent>();

            // on the first block we have . and ..
            let iter_count = if block == 0 {
                assert!(count >= 2);
                count - 2
            } else {
                count
            };

            if block == 0 {
                let dot = {
                    let mut d = disk::Dirent::new_zeroed();
                    d.disk_id = (disk_id as u64).into();
                    d.name_offset = name_offset.try_into().map_err(|_| Error::NameOffsetTooBig)?;
                    d.file_type = DirentFileType::Directory as u8;
                    d
                };
                name_offset += 1;
                let dotdot = {
                    let mut d = disk::Dirent::new_zeroed();
                    d.disk_id = (disk_id as u64).into(); // TODO this has to get updated argh!
                    d.name_offset = name_offset.try_into().map_err(|_| Error::NameOffsetTooBig)?;
                    d.file_type = DirentFileType::Directory as u8;
                    d
                };
                name_offset += 2;
                self.builder.dirent_buf.extend(dot.as_bytes());
                self.builder.dirent_buf.extend(dotdot.as_bytes());
                self.builder.name_buf.extend(b"...");
            }

            for _ in 0..iter_count {
                let (name, child) = iter.next().expect("Missing child");
                let disk_id = child.disk_id().ok_or(Error::NoDiskId)?;

                let dirent = {
                    let mut d = disk::Dirent::new_zeroed();
                    d.disk_id = (disk_id as u64).into();
                    d.name_offset = name_offset.try_into().map_err(|_| Error::NameOffsetTooBig)?;
                    d.file_type = child.file_type() as u8;
                    d
                };

                self.builder.dirent_buf.extend(dirent.as_bytes());
                self.builder.name_buf.extend(name.as_bytes());

                name_offset += name.as_bytes().len();
            }
            self.builder.writer.write_all(&self.builder.dirent_buf)?;
            self.builder.writer.write_all(&self.builder.name_buf)?;
            self.builder.name_buf.clear();
            self.builder.dirent_buf.clear();
            self.builder.zero_fill_rest_of_page()?;

            if block + 1 == n_blocks { // last iter
                total_size += self.builder.dirent_buf.len() as u64;
                total_size += self.builder.name_buf.len() as u64;
            } else {
                total_size += self.builder.block_size();
            }
        }

        let inode = {
            let mut i = InodeExtended::new_zeroed();
            i.format_layout = Inode::format_layout(InodeType::Extended, Layout::FlatPlain).into();
            i.mode = (FileType::Directory.as_raw_mode() as u16 | dir.meta.mode).into();
            println!("mode={:16b}", i.mode);
            i.uid = dir.meta.uid.into();
            i.gid = dir.meta.gid.into();
            i.mtime = dir.meta.mtime.into();
            i.nlink = 1.into(); // TODO!
            i.info = InodeInfo::raw_block(
                start_block
                    .try_into()
                    .map_err(|_| Error::FileBlockTooBig)?,
            );
            i.size = total_size.into();

            self.builder.hook_inode_extended(&mut i)?;
            i
        };

        let prev = dir.disk_id.replace(disk_id);
        println!("dir disk_id={}", self.builder.dir_inode_id);
        assert!(prev.is_none());
        self.builder.dir_inode_buf.extend(inode.as_bytes());
        Ok(())
    }
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
        // I guess one of the problems is that we want to Borrow or Clone + Into or something like
        // that since we want Borrow for lookup and then Clone + Into for insertion
        match self
            .children
            .entry(name.into())
            .or_insert_with(|| Dirent::Dir(Dir::default()))
        {
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

    // TODO not handling tail packing right now
    // fill in self.n_dirents_per_block which is the number of dirents that will be placed in the
    // corresponding block. Returns the number of blocks required to store all of the dirents
    // Each block stores as many dirents as possible, limited by
    //  1) name_offset is a u16 offset from the start of the block
    //  2) all names for a block must fit inside the block
    fn prepare_dirent_data(&mut self, block_size: u64, start_block: u64) -> u64 {
        let _ = self.start_block.insert(start_block);
        // we initialize with . and .. entries
        let mut len = 3u64;
        let mut count = 2u16;

        for name in self.children.keys() {
            let name_start = len + (std::mem::size_of::<disk::Dirent>() as u64);
            let additional_len = (std::mem::size_of::<disk::Dirent>() + name.len()) as u64;
            let next_len = len + additional_len;
            if next_len > block_size || name_start > std::u16::MAX as u64 {
                self.n_dirents_per_block.push(count);
                count = 1;
                len = additional_len;
            } else {
                count += 1;
                len = next_len;
            }
        }
        if count != 0 {
            self.n_dirents_per_block.push(count);
        }
        // TODO this check will change with tail packing
        let sum = self.n_dirents_per_block.iter().sum::<u16>() as usize;
        if self.children.len() + 2 != sum {
            panic!("not all children accounted for expected={} got={}", self.children.len(), sum);
        }
        self.n_dirents_per_block.len() as u64
    }
}

impl<W: Write + Seek> Builder<W> {
    pub fn new(writer: W) -> Result<Self, Error> {
        let block_size_bits = 12; // TODO configurable
        let mut ret = Builder {
            root: Some(Root::default()),
            increment_uid_gid: None,
            writer: BufWriter::new(writer),
            superblock: Superblock::default(),
            cur_data_block: 1,
            block_size_bits,
            meta_block: None,
            name_buf: Vec::with_capacity(1 << block_size_bits),
            dirent_buf: Vec::with_capacity(1 << block_size_bits),
            dir_inode_buf: Vec::with_capacity(1 << block_size_bits),
            n_dirs: 0,
            n_inodes: 0,
            dir_inode_id: 0,
        };
        ret.seek_block(ret.cur_data_block)?;
        Ok(ret)
    }

    fn block_size(&self) -> u64 {
        1u64 << self.block_size_bits
    }

    fn block_no(&self, addr: u64) -> u64 {
        addr / self.block_size()
    }

    fn block_addr(&self, block: u64) -> u64 {
        block * self.block_size()
    }

    fn n_blocks_roundup(&self, len: u64) -> u64 {
        let n = len / self.block_size();
        let l = n * self.block_size();
        if l < len {
            n + 1
        } else {
            n
        }
    }

    fn seek_block(&mut self, block: u64) -> Result<(), Error> {
        let _ = self
            .writer
            .seek(SeekFrom::Start(self.block_addr(block)))?;
        Ok(())
    }

    fn seek_align(&mut self, size: usize, tail: &Option<Box<[u8]>>) -> Result<u64, Error> {
        let size = size as u64;
        let mut cur = self.writer.stream_position()?;
        let rem = cur % size;
        if rem != 0 {
            cur += rem;
        }
        if let Some(tail) = tail {
            let len = tail.len() as u64;
            if len > self.block_size() {
                return Err(Error::TailTooBig);
            }
            let block_no1 = self.block_no(cur);
            let block_no2 = self.block_no(cur + len);
            if block_no1 != block_no2 {
                cur = self.block_addr(block_no2);
            }
        }
        Ok(self.writer.seek(SeekFrom::Start(cur))?)
    }

    fn addr_to_disk_id(&self, addr: u64) -> Result<u32, Error> {
        let meta_addr = self.block_addr(self.meta_block.ok_or(Error::NoMetaBlock)?);
        if addr < meta_addr {
            return Err(Error::AddrLessThanMetaBlock);
        }
        let offset = addr - meta_addr;
        if offset % 4 != 0 {
            return Err(Error::AddrNotAligned);
        }
        (offset / 32).try_into().map_err(|_| Error::DiskIdTooBig)
    }

    fn zero_fill_rest_of_page(&mut self) -> Result<(), Error> {
        let cur = self.writer.stream_position()?;
        let rem = cur % self.block_size();
        if rem != 0 {
            let n = self.block_size() - rem;
            for _ in 0..n {
                self.writer.write_all(&[0])?;
            }
        }
        Ok(())
    }

    fn hook_inode_extended(&self, inode: &mut InodeExtended) -> Result<(), Error> {
        if let Some(inc) = self.increment_uid_gid {
            inode.uid = (inode.uid + inc).try_into()
                .map_err(|_| Error::UidGidTooBig)?;
            inode.gid = (inode.gid + inc).try_into()
                .map_err(|_| Error::UidGidTooBig)?;
        }
        Ok(())
    }

    // we require the size up front so that we can calculate tail len
    pub fn add_file<P: AsRef<Path>, R: Read>(
        &mut self,
        p: P,
        meta: Meta,
        len: usize,
        contents: &mut R,
    ) -> Result<(), Error> {
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
            len,
            tail,
            disk_id: None,
        };
        self.root.as_mut().expect("not none").add_file(p, file)
    }

    fn write_superblock(&mut self) -> Result<(), Error> {
        self.superblock.magic = EROFS_SUPER_MAGIG_V1.into();
        self.superblock.blkszbits = self.block_size_bits;
        self.superblock.meta_blkaddr = self
            .meta_block
            .ok_or(Error::NoMetaBlock)?
            .try_into()
            .map_err(|_| Error::MetaBlockTooBig)?;
        self.superblock.root_disk_id = self
            .root
            .as_ref()
            .expect("not none")
            .root
            .disk_id
            .ok_or(Error::NoRootDiskId)?
            .try_into()
            .map_err(|_| Error::RootDiskIdTooBig)?;
        self.superblock.inos = self.n_inodes.into();
        //self.superblock.feature_compat = 1.into();
        // TODO checksum (and turn on feature_compat)

        self.writer
            .seek(SeekFrom::Start(EROFS_SUPER_OFFSET as u64))?;
        self.superblock.write_to_io(&mut self.writer)?;
        Ok(())
    }

    fn write_inode(&mut self, data: &[u8], tail: &Option<Box<[u8]>>) -> Result<u32, Error> {
        self.n_inodes += 1;
        let addr = self.seek_align(4, tail)?;
        let disk_id = self.addr_to_disk_id(addr)?;
        self.writer.write_all(data)?;
        if let Some(tail) = tail {
            self.writer.write_all(tail)?;
        }
        Ok(disk_id)
    }

    // okay so we first have to write all dirents so that they can go into the data block
    // can probably buffer all the dirent data in memory, and just reserve a spot for it in the
    // data section
    // then write out all the inodes
    // then fill in the inodes in the dirents
    fn write_inodes(&mut self) -> Result<(), Error> {
        // TODO what is the nicer way to write this??!! We take the root so that we can borrow it
        // as mut along with self, but then we have to put it back
        // and if we error, then we don't get to put it back...
        let mut root = self.root.take().expect("not none");
        // TODO okay so in this pass I think we reserve meta space for all the dirs
        walk_tree(
            &mut root.root,
            &mut BuilderTreeVisitorPrepareDirents { builder: self },
        )?;
        // we are now done writing data, so we record the meta block number
        let meta_block = *self.meta_block.insert(self.cur_data_block);
        // NOTE without tail packing for dir
        let reserve_for_dirs = (std::mem::size_of::<disk::InodeExtended>() * self.n_dirs) as u64;
        //self.cur_data_block += self.n_blocks_roundup(reserve_for_dirs as u64);
        self.writer.seek(SeekFrom::Start(self.block_addr(meta_block) + reserve_for_dirs))?;
        walk_tree(
            &mut root.root,
            &mut BuilderTreeVisitorWriteInodes { builder: self },
        )?;

        //println!("{:#?}", root.root);
        self.seek_block(meta_block)?;
        if reserve_for_dirs != self.dir_inode_buf.len() as u64 {
            panic!("size mismatch");
        }
        self.writer.write_all(&self.dir_inode_buf)?;
        let _ = self.root.insert(root);
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), Error> {
        self.write_inodes()?;
        self.write_superblock()?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn into_inner(mut self) -> Result<W, Error> {
        self.finalize()?;
        self.writer
            .into_inner()
            .map_err(|e| Error::Io(e.error().kind()))
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
    let mut ret: Vec<_> = p.iter().filter(|x| *x != "/").collect();

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

    use std::io::Cursor;
    use std::path::{Path,PathBuf};
    use std::collections::{BTreeSet,HashSet};
    use std::process::Command;
    use tempfile::NamedTempFile;

    // sorted list of (key,value) bytes
    type Ext = Vec<(String, Vec<u8>)>;

    #[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    enum EntryTyp {
        #[default]
        File,
        Dir,
        Link,
        Symlink,
        Fifo,
    }

    impl Into<FileType> for EntryTyp {
        fn into(self) -> FileType {
            match self {
                EntryTyp::File => FileType::RegularFile,
                EntryTyp::Dir => FileType::Directory,
                _ => todo!()
            }
        }
    }

    impl From<FileType> for EntryTyp {
        fn from(x: FileType) -> EntryTyp {
            match x {
                FileType::RegularFile => EntryTyp::File,
                FileType::Directory => EntryTyp::Dir,
                _ => todo!()
            }
        }
    }

    // E is a standalone redux Entry
    #[derive(Default, Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
    struct E {
        typ: EntryTyp,
        path: PathBuf,
        data: Option<Vec<u8>>,
        ext: Ext,
        link: Option<PathBuf>,
        mtime: u64,
        uid: u32,
        gid: u32,
        mode: u16,
    }

    impl E {
        fn file<P: Into<PathBuf>>(path: P, data: &[u8]) -> Self {
            Self {
                typ: EntryTyp::File,
                path: path.into(),
                data: Some(Vec::from(data)),
                mode: 0o744,
                ..Default::default()
            }
        }
        fn dir<P: Into<PathBuf>>(path: P) -> Self {
            Self {
                typ: EntryTyp::Dir,
                path: path.into(),
                ..Default::default()
            }
        }
        fn link<P1: Into<PathBuf>, P2: Into<PathBuf>>(path: P1, link: P2) -> Self {
            Self {
                typ: EntryTyp::Link,
                path: path.into(),
                link: Some(link.into()),
                ..Default::default()
            }
        }
        fn symlink<P1: Into<PathBuf>, P2: Into<PathBuf>>(path: P1, link: P2) -> Self {
            Self {
                typ: EntryTyp::Symlink,
                path: path.into(),
                link: Some(link.into()),
                ..Default::default()
            }
        }
        fn fifo<P: Into<PathBuf>>(path: P) -> Self {
            Self {
                typ: EntryTyp::Fifo,
                path: path.into(),
                ..Default::default()
            }
        }
        fn with_uid(mut self: Self, uid: u32) -> Self {
            self.uid = uid;
            self
        }

        fn meta(&self) -> Meta {
            let ft: FileType = self.typ.clone().into();
            Meta {
                uid: self.uid,
                gid: self.gid,
                mtime: self.mtime,
                mode: ft.as_raw_mode() as u16 | self.mode,
            }
        }
    }

    type EList = BTreeSet<E>;

    fn inode_to_e<'a, P: AsRef<Path>>(name: P, inode: &Inode<'a>) -> E {
        E {
            typ: inode.file_type().into(),
            path: name.as_ref().into(),
            uid: inode.uid(),
            gid: inode.gid(),
            mode: inode.mode(),
            ..Default::default()
        }
    }

    fn into_erofs<W: Write + Seek>(entries: &EList, writer: W) -> Result<W, Error> {
        let mut b = Builder::new(writer)?;
        for entry in entries.iter() {
            match entry.typ {
                EntryTyp::File => {
                    let data = entry.data.as_ref().expect("file should have data");
                    let meta = entry.meta();
                    b.add_file(&entry.path, entry.meta(), data.len(), &mut Cursor::new(&data))?;
                }
                _ => todo!("unhandled typ")
            }
        }
        b.into_inner()
    }

    fn erofs_to_mem(data: &[u8]) -> Result<EList, disk::Error> {
        let mut seen = HashSet::new();
        let mut ret = BTreeSet::new();

        let erofs = disk::Erofs::new(data)?;

        let mut q = vec![(PathBuf::from("/"), erofs.get_root_inode()?.disk_id())];

        while let Some((name, cur)) = q.pop() {
            if !seen.insert(cur) {
                continue;
            }
            let inode = erofs.get_inode(cur)?;
            match inode.file_type() {
                FileType::Directory => {
                    let dirents = erofs.get_dirents(&inode)?;
                    for item in dirents.iter()? {
                        let item = item?;
                        let name = name.join(OsStr::from_bytes(item.name));
                        q.push((name, item.disk_id.try_into().expect("why is this u64")));
                    }
                }
                _ => {}
            }
            ret.insert(inode_to_e(name, &inode));
        }
        Ok(ret)
    }

    fn erofs_roundtrip(entries: &EList) -> EList {
        let buf = into_erofs(entries, Cursor::new(vec![])).unwrap().into_inner();
        erofs_to_mem(&buf).unwrap()
    }

    fn fsck_erofs<P: AsRef<Path>>(path: P) -> Result<(), Error> {
        //eprintln!("!! len={:?}", path.as_ref().metadata()?.len());
        //Command::new("xxd").arg(path.as_ref()).status()?;
        let output = Command::new("fsck.erofs")
            .arg(path.as_ref())
            .output()
            .expect("fsck.erofs failed to run");
        //println!("stdout {}", String::from_utf8_lossy(&output.stdout));
        //println!("stderr {}", String::from_utf8_lossy(&output.stderr));
        if output.status.success() && output.stderr.is_empty() {
            Ok(())
        } else {
            Err(Error::Other(String::from_utf8_lossy(&output.stderr).into()))
        }
    }

    #[test]
    fn test_name_and_parents() {
        {
            let p = Path::new("/a/b");
            assert_eq!(
                name_and_parents(p).unwrap(),
                (OsStr::new("b"), vec![OsStr::new("a")])
            );
        }
        {
            let p = Path::new("a/b");
            assert_eq!(
                name_and_parents(p).unwrap(),
                (OsStr::new("b"), vec![OsStr::new("a")])
            );
        }
        {
            let p = Path::new("/a");
            assert_eq!(name_and_parents(p).unwrap(), (OsStr::new("a"), vec![]));
        }
        {
            let p = Path::new("a");
            assert_eq!(name_and_parents(p).unwrap(), (OsStr::new("a"), vec![]));
        }
        assert_eq!(
            name_and_parents(Path::new("/a/")).unwrap_err(),
            Error::BadFilename
        );
        assert_eq!(
            name_and_parents(Path::new("/")).unwrap_err(),
            Error::BadFilename
        );
    }

    #[test]
    fn test_builder_simple() -> Result<(), Error> {
        let path = Path::new("/tmp/peerofs.test.erofs");
        let mut b = Builder::new(std::fs::File::create(path).expect("tf"))?;
        //let mut b = Builder::new(NamedTempFile::new().expect("tf"))?;
        {
            let data = b"hello world";
            b.add_file(
                //"/foo/bar",
                "/foo",
                Meta::default(),
                data.len(),
                &mut Cursor::new(data),
            )?;
        }

        let tf = b.into_inner().expect("io fail");
        //fsck_erofs(tf.path())?;
        fsck_erofs(path)?;
        Ok(())
    }

    macro_rules! check_erofs_roundtrip {
        ($entries:expr) => {{
            let entries = $entries.into_iter().collect::<EList>();
            assert_eq!(
                entries,
                erofs_roundtrip(&entries)
            );
        }};
    }

    #[test]
    fn test_builder() {
        check_erofs_roundtrip!(
            vec![
                E::file("/x", b"hello world")
            ]
        );
    }
}
