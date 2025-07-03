use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode};
use zerocopy::{FromZeros, IntoBytes};

use crate::disk;
use crate::disk::{
    DirentFileType, InodeInfo, InodeType, Layout, Superblock, XattrEntry, XattrHeader,
    EROFS_NULL_ADDR, EROFS_SUPER_MAGIG_V1, EROFS_SUPER_OFFSET, INODE_ALIGNMENT,
};

const MAX_DEPTH: usize = 32; // TODO could be configurable

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
// A bit more detail, currently the strategy is TODO update this
// Phase 0:
//  - Resolve all hardlinks. If a hardlink resolves to a FlatInline node, we have to copy
//  the tail storage
// Phase 1:
//  - Add files which builds up an in memory tree of dirs + files
//  - Adding a file appends data to the output file in blocks
//  - Tail packed data is stored in memory (worst case here is every file is tail packed and we
//  store the sum total in memory. TODO is get the threshold right about when to use tail packing
// Phase 2:
//  - No more changes to the tree are allowed
//  - Walk dirs to compute how many blocks we'll need to store the dirents data and count dirs.
//  Also store the block addr of where the dirents will be for each dir
//  - We now know where our meta block start is
//  - Reserve enough space at the front of the meta block for the dir inodes. This makes sure we
//  can fit our root disk id in u16
// Phase 3:
//  - On dir enter, grab the next dir inode for the dir and write it's inode data to a buffer
//    - TODO this will have to change with tail packing since we won't know the tail packed data
//    for the inode until the pass up
//  - Write out file inodes (including tail packing) and record their disk id
//
//  - On dir exit, every child will have a disk id and we can
//    1) write out the dirents data at the recorded data block start
//  - Finish by writing the buffered dir inode data at the meta block start
//
// I'm not sure if the right thing to do is create dirs as necessary. For one, we probably want to
// create the root by default but that of course means assuming the uid/gid perms etc. But a lot of
// layers I've seen don't have an entry for the root dir. Many (most?) tar files do have a dir
// entry first for non-root dirs, like [DIR bin, FILE bin/bash], but I don't think that is
// guaranteed so I've gone for creating dirs when inserting files and then any DIR entry would get
// upsert'ed  with the metadata it has.
//
// Overall, paths are tricky (as always) and the rules for what paths we accept are:
// - `.` `./` `/` can be used for upsert_dir of the root
// - leading `/` or `./` can be used for files
// - trailing `/` is only allowed on dirs
// - things with `.` in the middle are skipped by Path::components internally
// - things with `..` are forbidden
// - the empty path is forbidden
//
// Xattrs
// - we don't build shared xattrs right now
// - we do support the builtin prefixes (see XATTR_BUILTIN_PREFIX_TABLE)
//
// Int sizes
// - inodes store the block address (really a block number which gets converted to an address by
// multiplying by the block size) as u32. File/Dir/Symlink structs here store it as a raw u32,
// which can be u32::MAX ie EROFS_NULL_ADDR if they don't store any data ie only tail data. It
// would maybe be nicer to do all this with Option<NonMaxU32> which I explored a bit but is a bet
// messy and not sure how much it improves things.
//
// TODO
// - link count, do they actually matter?
// - tail pack dirents
// - compression: lz4, zstd, deflate

#[derive(thiserror::Error, Debug)]
pub enum Error {
    FileExists,
    BadFilename,
    EmptyPath,
    EmptyFilename,
    NotADir,
    BlockNoTooBig,
    MetaBlockTooBig,
    FileBlockTooBig,
    InodeTooBig,
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
    HardlinkToDir,
    HardlinkNotResolved,
    HardlinkMultiNotHandled,
    UnhandledPrefixComponent,
    PathWithDotDot,
    WeirdPath,
    PathTrailingSlash,
    InsertFailed,
    UpsertFailed,
    MaxDepthExceeded,
    XattrKeyTooLong,
    XattrValueTooLong,
    TooManyXattrs,
    ModeShouldFitInU16,
    DirDiskIdMismatch { expected: Option<u32>, got: u32 },
    MaxSizeExceeded,
    Oob,
    Other(String),
    Io(#[from] std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    files: usize,
    symlinks: usize,
    dirs: usize,
    tails: usize,
    tail_size: usize,
    block_end_padding: usize,
}

#[derive(Default)]
pub struct BuilderConfig {
    pub max_file_size: Option<u64>,
    pub increment_uid_gid: Option<u32>,
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
    n_dirs: usize,
    n_inodes: u64,
    links: Vec<(PathBuf, PathBuf, Meta)>,
    inode_addr: u64,
    stats: Stats,
    max_depth: usize,
    max_file_size: u64,
    cur_file_size: u64,
}

pub type XattrMap = BTreeMap<Box<[u8]>, Box<[u8]>>;

#[derive(Debug)]
pub struct Meta {
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
    pub mode: Mode,
    pub xattrs: XattrMap,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            uid: 0,
            gid: 0,
            mtime: 0,
            mode: 0o755.into(),
            xattrs: XattrMap::new(),
        }
    }
}

#[derive(Default)]
struct Root {
    root: Dir,
}

#[derive(Debug, Default)]
struct File {
    meta: Meta,
    start_block: u32,
    n_links: u32,
    len: usize,
    tail: Option<Box<[u8]>>,
    disk_id: Option<u32>,
}

// We would almost never write the symlink data to a block but could happen
#[derive(Debug, Default)]
struct Symlink {
    meta: Meta,
    start_block: u32,
    n_links: u32,
    len: usize,
    tail: Option<Box<[u8]>>,
    disk_id: Option<u32>,
}

#[derive(Debug)]
struct Dir {
    children: BTreeMap<OsString, Dirent>,
    meta: Meta,
    disk_id: Option<u32>,
    // start of data block where dirents is located
    start_block: u32,
    // number of dirents in each block
    n_dirents_per_block: Vec<u16>,
    total_size: u64,
    // TODO  did this dirent have tail packing
    //tail: bool,
}

impl Default for Dir {
    fn default() -> Dir {
        let mut children = BTreeMap::new();
        children.insert(".".into(), Dirent::Dot);
        children.insert("..".into(), Dirent::DotDot);
        Dir {
            children,
            meta: Meta::default(),
            disk_id: None,
            start_block: EROFS_NULL_ADDR,
            n_dirents_per_block: vec![],
            total_size: 0,
        }
    }
}

#[derive(Debug)]
enum Dirent {
    File(File),
    Dir(Dir),
    Symlink(Symlink),
    Dot,
    DotDot,
}

enum Inode {
    #[allow(dead_code)]
    Compact(disk::InodeCompact),
    Extended(disk::InodeExtended),
}

impl Dirent {
    //fn disk_id(&self) -> Option<u32> {
    //    match self {
    //        Dirent::File(f) => f.disk_id,
    //        Dirent::Dir(d) => d.disk_id,
    //        Dirent::Symlink(s) => s.disk_id,
    //        Dirent::Dot | Dirent::DotDot => None,
    //    }
    //}

    fn file_type(&self) -> DirentFileType {
        match self {
            Dirent::File(_) => DirentFileType::RegularFile,
            Dirent::Symlink(_) => DirentFileType::Symlink,
            Dirent::Dot | Dirent::DotDot | Dirent::Dir(_) => DirentFileType::Directory,
        }
    }
}

trait TreeVisitor {
    fn on_file(&mut self, _file: &mut File) -> Result<(), Error> {
        Ok(())
    }
    fn on_symlink(&mut self, _symlink: &mut Symlink) -> Result<(), Error> {
        Ok(())
    }
    fn on_dir_exit(&mut self, _dir: &mut Dir) -> Result<(), Error> {
        Ok(())
    }
    fn on_dir_enter(&mut self, _dir: &mut Dir) -> Result<(), Error> {
        Ok(())
    }
}

// Doing this with an iterator would be ideal to not blow the stack, but I'm not sure how/whether
// it is possible. As an Iterator it doesn't work since we need a lifetime. As a non-Iterator while
// let Some(event) = iter.next() thing it is hard to store the stack since we need to store an &mut
// Dir and also store the in progress iterator of children which requires a mutable borrow. Here
// they are nicely sequential so not a problem.
fn walk_tree<V: TreeVisitor>(
    dir: &mut Dir,
    visitor: &mut V,
    max_depth: usize,
) -> Result<(), Error> {
    fn recur<V: TreeVisitor>(
        dir: &mut Dir,
        visitor: &mut V,
        depth: usize,
        max_depth: usize,
    ) -> Result<(), Error> {
        if depth == max_depth {
            return Err(Error::MaxDepthExceeded);
        }
        visitor.on_dir_enter(dir)?;
        for child in dir.children.values_mut() {
            match child {
                Dirent::File(f) => {
                    visitor.on_file(f)?;
                }
                Dirent::Symlink(s) => {
                    visitor.on_symlink(s)?;
                }
                Dirent::Dir(d) => {
                    recur(d, visitor, depth + 1, max_depth)?;
                }
                Dirent::Dot | Dirent::DotDot => {}
            }
        }
        visitor.on_dir_exit(dir)?;
        Ok(())
    }

    recur(dir, visitor, 0, max_depth)
}

// reserve space for dirents in data section
struct BuilderTreeVisitorPrepareDirents<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
}

// write dir inodes so they fill the start of meta section
struct BuilderTreeVisitorWriteDirInodes<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
}

// write non dir inodes
struct BuilderTreeVisitorWriteInodes<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
}

// write out dirents into data section
struct BuilderTreeVisitorWriteDirents<'a, W: Write + Seek> {
    builder: &'a mut Builder<W>,
    parents: Vec<u32>,
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorPrepareDirents<'_, W> {
    fn on_dir_enter(&mut self, dir: &mut Dir) -> Result<(), Error> {
        let n_blocks =
            dir.prepare_dirent_data(self.builder.block_size(), self.builder.cur_data_block)?;

        self.builder.n_dirs += 1;
        self.builder.stats.dirs += 1;
        self.builder.cur_data_block += n_blocks;

        Ok(())
    }
}

fn make_inode(
    file_type: FileType,
    size: u64,
    start_block: u32, // can be EROFS_NULL_ADDR
    meta: &Meta,
    tail: &Option<Box<[u8]>>,
    n_links: u32,
) -> Result<disk::InodeExtended, Error> {
    let layout = if tail.is_some() {
        Layout::FlatInline
    } else {
        Layout::FlatPlain
    };
    let mut i = disk::InodeExtended::new_zeroed();
    i.format_layout = disk::Inode::format_layout(InodeType::Extended, layout).into();
    i.mode = make_mode(file_type, meta.mode)?.into();
    i.uid = meta.uid.into();
    i.gid = meta.gid.into();
    i.mtime = meta.mtime.into();
    i.nlink = n_links.into();
    i.info = InodeInfo::new_raw_blkaddr(start_block);
    i.size = size.into();

    Ok(i)
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorWriteDirInodes<'_, W> {
    fn on_dir_enter(&mut self, dir: &mut Dir) -> Result<(), Error> {
        let inode = Inode::Extended(make_inode(
            FileType::Directory,
            dir.total_size,
            dir.start_block,
            &dir.meta,
            &None,
            1,
        )?);

        let disk_id = self.builder.write_inode(inode, &None, &dir.meta.xattrs)?;

        let prev = dir.disk_id.replace(disk_id);
        assert!(prev.is_none());

        Ok(())
    }
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorWriteInodes<'_, W> {
    // TODO use a helper for the meta
    fn on_file(&mut self, file: &mut File) -> Result<(), Error> {
        self.builder.stats.files += 1;
        let inode = Inode::Extended(make_inode(
            FileType::RegularFile,
            file.len as u64,
            file.start_block,
            &file.meta,
            &file.tail,
            file.n_links,
        )?);

        let disk_id = self
            .builder
            .write_inode(inode, &file.tail, &file.meta.xattrs)?;
        let prev = file.disk_id.replace(disk_id);
        assert!(prev.is_none());
        Ok(())
    }

    fn on_symlink(&mut self, symlink: &mut Symlink) -> Result<(), Error> {
        self.builder.stats.symlinks += 1;
        let inode = Inode::Extended(make_inode(
            FileType::Symlink,
            symlink.len as u64,
            symlink.start_block,
            &symlink.meta,
            &symlink.tail,
            symlink.n_links,
        )?);

        let disk_id = self
            .builder
            .write_inode(inode, &symlink.tail, &symlink.meta.xattrs)?;
        let prev = symlink.disk_id.replace(disk_id);
        assert!(prev.is_none());
        Ok(())
    }
}

impl<W: Write + Seek> TreeVisitor for BuilderTreeVisitorWriteDirents<'_, W> {
    // write dirents in same order as we reserved their blocks so that writes are contiguous
    fn on_dir_enter(&mut self, dir: &mut Dir) -> Result<(), Error> {
        self.builder.seek_block(dir.start_block.into())?;

        let mut iter = dir.children.iter();

        //eprintln!("dir disk id={:?}", dir.disk_id.unwrap());
        for count in dir.n_dirents_per_block.iter() {
            let count = *count;
            let mut name_offset = (count as usize) * std::mem::size_of::<disk::Dirent>();
            let mut written = 0;

            for _ in 0..count {
                let (name, child) = iter.next().expect("Missing child");
                let disk_id = match child {
                    Dirent::File(f) => f.disk_id,
                    Dirent::Dir(d) => d.disk_id,
                    Dirent::Symlink(d) => d.disk_id,
                    Dirent::Dot => dir.disk_id,
                    // if there is no parent, then this is the root dir and we point to ourselves
                    Dirent::DotDot => self.parents.last().or(dir.disk_id.as_ref()).copied(),
                }
                .ok_or(Error::NoDiskId)?;
                //eprintln!("{:?} {}", name, disk_id);

                let dirent = {
                    let mut d = disk::Dirent::new_zeroed();
                    d.disk_id = (disk_id as u64).into();
                    d.name_offset = name_offset
                        .try_into()
                        .map_err(|_| Error::NameOffsetTooBig)?;
                    d.file_type = child.file_type() as u8;
                    d
                };

                written += dirent.as_bytes().len();
                self.builder.writer.write_all(dirent.as_bytes())?;
                self.builder.name_buf.extend(name.as_bytes());

                name_offset += name.as_bytes().len();
            }

            self.builder.writer.write_all(&self.builder.name_buf)?;
            written += self.builder.name_buf.len();
            self.builder.zero_fill_block(written)?;
            self.builder.cur_data_block += 1;

            self.builder.name_buf.clear();
        }

        self.parents.push(dir.disk_id.ok_or(Error::NoDiskId)?);
        Ok(())
    }

    fn on_dir_exit(&mut self, _dir: &mut Dir) -> Result<(), Error> {
        self.parents.pop();
        Ok(())
    }
}

impl Root {
    fn add_file<P: AsRef<Path>>(&mut self, path: P, file: File) -> Result<(), Error> {
        if path.as_ref().as_os_str().as_bytes().ends_with(b"/") {
            return Err(Error::PathTrailingSlash);
        }
        //eprintln!("add file {:?}", path.as_ref());
        self.insert(path, Dirent::File(file))
    }

    fn add_symlink<P: AsRef<Path>>(&mut self, path: P, symlink: Symlink) -> Result<(), Error> {
        if path.as_ref().as_os_str().as_bytes().ends_with(b"/") {
            return Err(Error::PathTrailingSlash);
        }
        //eprintln!("add symlink {:?} {:?}", path.as_ref(), symlink);
        self.insert(path, Dirent::Symlink(symlink))
    }

    fn upsert_dir<P: AsRef<Path>>(&mut self, path: P, meta: Meta) -> Result<(), Error> {
        //eprintln!("upsert dir {:?}", path.as_ref());
        match self.lookup_create(path.as_ref())? {
            (dir, None) => {
                // root
                dir.meta = meta;
                Ok(())
            }
            (dir, Some(name)) => {
                let dir = dir.get_or_create_dir(name)?;
                dir.meta = meta;
                Ok(())
            }
        }
    }

    fn insert<P: AsRef<Path>>(&mut self, path: P, entry: Dirent) -> Result<(), Error> {
        if let (dir, Some(name)) = self.lookup_create(path.as_ref())? {
            dir.children.insert(name.into(), entry);
            Ok(())
        } else {
            // this can only happen if trying to insert at the root like . ./ or /
            // for which the only valid thing to do is upsert_dir
            Err(Error::InsertFailed)
        }
    }

    // cannot lookup the root dir since we couldn't move it into a Dirent::Dir
    // but this is only used for resolving links and can't link to dir so okay
    fn get<P: AsRef<Path>>(&mut self, path: P) -> Result<Option<&mut Dirent>, Error> {
        let path = path.as_ref();
        if let (dir, Some(name)) = self.lookup(path)? {
            Ok(dir.children.get_mut(name))
        } else {
            Ok(None)
        }
    }

    fn lookup_create<'a>(
        &mut self,
        path: &'a Path,
    ) -> Result<(&mut Dir, Option<&'a OsStr>), Error> {
        self.lookup_impl(path, true)
    }

    fn lookup<'a>(&mut self, path: &'a Path) -> Result<(&mut Dir, Option<&'a OsStr>), Error> {
        self.lookup_impl(path, false)
    }

    // ugly but idk
    // returns the dir and optionally the final name component (since the root dir has no name).
    // takes/returns &mut to cover both cases of creating the dirs on the way or not
    // trailing slash should be handled by caller since it is okay for dirs but not for files
    fn lookup_impl<'a>(
        &mut self,
        path: &'a Path,
        create: bool,
    ) -> Result<(&mut Dir, Option<&'a OsStr>), Error> {
        use std::path::Component::*;

        match path.as_os_str().as_bytes() {
            b"" => {
                return Err(Error::EmptyPath);
            }
            b"." | b"./" | b"/" => {
                return Ok((&mut self.root, None));
            }
            _ => {}
        }

        let mut cur = &mut self.root;
        let mut iter = path.components().peekable();

        let name = {
            loop {
                if let Some(part) = iter.next() {
                    if iter.peek().is_none() {
                        match part {
                            Normal(part) => {
                                break Some(part);
                            }
                            CurDir => {
                                break None;
                            }
                            _ => {
                                return Err(Error::WeirdPath);
                            }
                        }
                    }
                    match part {
                        // CurDir/RootDir can only come at the beginning and we ignore it
                        CurDir | RootDir => {
                            continue;
                        }
                        Prefix(_) => {
                            return Err(Error::UnhandledPrefixComponent);
                        }
                        ParentDir => {
                            return Err(Error::PathWithDotDot);
                        }
                        Normal(part) => {
                            if create {
                                cur = cur.get_or_create_dir(part)?;
                            } else {
                                cur = cur.get_dir(part)?;
                            }
                        }
                    }
                } else {
                    break None;
                }
            }
        };
        Ok((cur, name))
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
    }

    fn get_dir(&mut self, name: &OsStr) -> Result<&mut Dir, Error> {
        match self.children.get_mut(name) {
            Some(Dirent::Dir(d)) => Ok(d),
            _ => Err(Error::NotADir),
        }
    }

    // TODO not handling tail packing right now
    // fill in self.n_dirents_per_block which is the number of dirents that will be placed in the
    // corresponding block. Returns the number of blocks required to store all of the dirents
    // Each block stores as many dirents as possible, limited by
    //  1) name_offset is a u16 offset from the start of the block
    //  2) all names for a block must fit inside the block
    fn prepare_dirent_data(&mut self, block_size: u64, start_block: u64) -> Result<u64, Error> {
        self.start_block = start_block.try_into().map_err(|_| Error::BlockNoTooBig)?;
        let mut len = 0u64;
        let mut count = 0u16;

        for name in self.children.keys() {
            //println!("name={:?}", name);
            let name_start = len + (std::mem::size_of::<disk::Dirent>() as u64);
            let additional_len = (std::mem::size_of::<disk::Dirent>() + name.len()) as u64;
            let next_len = len + additional_len;
            if next_len > block_size || name_start > u16::MAX as u64 {
                self.n_dirents_per_block.push(count);
                count = 1;
                len = additional_len;
            } else {
                count += 1;
                len = next_len;
            }
        }

        let mut total_size = block_size * self.n_dirents_per_block.len() as u64;

        if count != 0 {
            self.n_dirents_per_block.push(count);
            total_size += len;
        }
        self.total_size = total_size;

        // NOTE this check will change with tail packing
        let sum = self
            .n_dirents_per_block
            .iter()
            .map(|x| *x as usize)
            .sum::<usize>();
        if self.children.len() != sum {
            panic!(
                "not all children accounted for expected={} got={}",
                self.children.len(),
                sum
            );
        }
        Ok(self.n_dirents_per_block.len() as u64)
    }
}

impl<W: Write + Seek> Builder<W> {
    pub fn new(writer: W, config: BuilderConfig) -> Result<Self, Error> {
        let block_size_bits = 12; // TODO configurable
        let mut ret = Builder {
            root: Some(Root::default()),
            increment_uid_gid: config.increment_uid_gid,
            writer: BufWriter::with_capacity(32 * 1024, writer),
            superblock: Superblock::new_zeroed(),
            cur_data_block: 1,
            block_size_bits,
            meta_block: None,
            name_buf: Vec::with_capacity(1 << block_size_bits),
            n_dirs: 0,
            n_inodes: 0,
            links: vec![],
            inode_addr: 0,
            stats: Stats::default(),
            max_depth: MAX_DEPTH,
            max_file_size: config.max_file_size.unwrap_or(u64::MAX),
            cur_file_size: 0,
        };
        // manually advance to first block
        ret.writer
            .seek(SeekFrom::Start(ret.block_addr(ret.cur_data_block)))?;
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

    fn seek_block(&mut self, block: u64) -> Result<(), Error> {
        if self.cur_data_block != block {
            let _ = self.writer.seek(SeekFrom::Start(self.block_addr(block)))?;
            self.cur_data_block = block;
        }
        Ok(())
    }

    fn zero_fill_block(&mut self, written: usize) -> Result<u64, Error> {
        self.zero_fill(written, self.block_size())
    }

    fn zero_fill(&mut self, written: usize, alignment: u64) -> Result<u64, Error> {
        let written = written as u64;
        let rem = written % alignment;
        if rem != 0 {
            let n = alignment - rem;
            self.stats.block_end_padding += n as usize;
            // TODO is there something better for bufwriter to zero fill like this?
            for _ in 0..n {
                self.writer.write_all(&[0])?;
            }
            Ok(n)
        } else {
            Ok(0)
        }
    }

    fn addr_to_disk_id(&self, addr: u64) -> Result<u32, Error> {
        let meta_addr = self.block_addr(self.meta_block.ok_or(Error::NoMetaBlock)?);
        //eprintln!("addr_to_disk_id addr={addr} meta_addr={meta_addr}");
        if addr < meta_addr {
            return Err(Error::AddrLessThanMetaBlock);
        }
        let offset = addr - meta_addr;
        if offset % INODE_ALIGNMENT != 0 {
            return Err(Error::AddrNotAligned);
        }
        (offset / INODE_ALIGNMENT)
            .try_into()
            .map_err(|_| Error::DiskIdTooBig)
    }

    // we require the size up front so that we can calculate tail len
    // also precondition is that writer is aligned to block size
    pub fn add_file<P: AsRef<Path>, R: Read>(
        &mut self,
        path: P,
        meta: Meta,
        len: usize,
        contents: &mut R,
    ) -> Result<(), Error> {
        if self.cur_file_size.saturating_add(len as u64) >= self.max_file_size {
            return Err(Error::MaxSizeExceeded);
        }

        let (n_blocks, block_len, tail_len) = size_tail_len(len, self.block_size_bits);
        if cfg!(debug_assertions) {
            let cur = self.writer.stream_position()?;
            if cur % self.block_size() != 0 {
                panic!(
                    "writer not aligned to block size {}, at {}",
                    self.block_size(),
                    cur
                );
            }
            if cur / self.block_size() != self.cur_data_block {
                panic!(
                    "writer not at cur={} cur_data_block={}, at {}",
                    cur,
                    self.cur_data_block,
                    cur / self.block_size()
                );
            }
        }

        let start_block = if block_len > 0 {
            self.cur_data_block
                .try_into()
                .map_err(|_| Error::BlockNoTooBig)?
        } else {
            EROFS_NULL_ADDR
        };

        if block_len > 0 {
            std::io::copy(&mut contents.take(block_len as u64), &mut self.writer)?;
            self.cur_data_block += n_blocks as u64;

            // block_len is not necessarily a multiple of blocks, so zero fill the rest
            // we could also seek but that causes a buffer flush and seek
            self.zero_fill_block(block_len)?;
        }
        let tail = if tail_len > 0 {
            // TODO could we ever figure out how to read into uninit vector?
            let mut buf = vec![0; tail_len];
            contents.read_exact(&mut buf)?;
            Some(buf.into_boxed_slice())
        } else {
            None
        };
        let file = File {
            meta: self.hook_meta(meta)?,
            start_block,
            len,
            tail,
            n_links: 1,
            ..Default::default()
        };
        self.root.as_mut().expect("not none").add_file(path, file)
    }

    fn hook_meta(&self, mut meta: Meta) -> Result<Meta, Error> {
        if let Some(inc) = self.increment_uid_gid {
            meta.uid = meta.uid.checked_add(inc).ok_or(Error::UidGidTooBig)?;
            meta.gid = meta.gid.checked_add(inc).ok_or(Error::UidGidTooBig)?;
        }
        Ok(meta)
    }

    pub fn upsert_dir<P: AsRef<Path>>(&mut self, path: P, meta: Meta) -> Result<(), Error> {
        let meta = self.hook_meta(meta)?;
        self.root.as_mut().expect("not none").upsert_dir(path, meta)
    }

    pub fn add_symlink<P1: AsRef<Path>, P2: AsRef<Path>>(
        &mut self,
        path: P1,
        link: P2,
        meta: Meta,
    ) -> Result<(), Error> {
        let data = link.as_ref().as_os_str().as_bytes();
        let len = data.len();
        let (n_blocks, block_len, tail_len) = size_tail_len(len, self.block_size_bits);

        let start_block = if block_len > 0 {
            self.cur_data_block
                .try_into()
                .map_err(|_| Error::BlockNoTooBig)?
        } else {
            EROFS_NULL_ADDR
        };
        if block_len > 0 {
            self.writer.write_all(&data[..block_len])?;
            self.zero_fill_block(block_len)?;
            self.cur_data_block += n_blocks as u64;
        }

        let tail = if tail_len > 0 {
            Some(data[block_len..].into())
        } else {
            None
        };

        let symlink = Symlink {
            meta: self.hook_meta(meta)?,
            start_block,
            len,
            tail,
            n_links: 1,
            ..Default::default()
        };
        self.root
            .as_mut()
            .expect("not none")
            .add_symlink(path, symlink)
    }

    pub fn add_link<P1: AsRef<Path>, P2: AsRef<Path>>(
        &mut self,
        path: P1,
        target: P2,
        meta: Meta,
    ) -> Result<(), Error> {
        self.links
            .push((path.as_ref().into(), target.as_ref().into(), meta));
        Ok(())
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
        self.writer.write_all(self.superblock.as_bytes())?;
        Ok(())
    }

    #[cfg(debug_assertions)]
    fn check_writer_alignment(&mut self, prepost: &str) {
        let cur = self.writer.stream_position().unwrap();
        if cur != self.inode_addr {
            panic!(
                "{}condition: writer mismatched with inode_addr cur={}, inode_addr={}",
                prepost, cur, self.inode_addr
            );
        }
        if cur % INODE_ALIGNMENT != 0 {
            panic!(
                "{}condition: writer not aligned to inode alignment {}, at {}",
                prepost, INODE_ALIGNMENT, cur
            );
        }
    }

    // NOTE: precondition is that our output stream is aligned to 32 bytes, this keeps us from
    // having to flush the BufWriter constantly
    // we also manually maintain the self.inode_addr
    // postcondition is that we are aligned to 32 bytes
    fn write_inode(
        &mut self,
        mut inode: Inode,
        tail: &Option<Box<[u8]>>,
        xattrs: &XattrMap,
    ) -> Result<u32, Error> {
        #[cfg(debug_assertions)]
        self.check_writer_alignment("pre");

        self.n_inodes += 1;

        let xattr_entries = make_xattr_entries(xattrs)?;
        let disk::XattrCountAndPadding {
            xattr_count,
            padding: xattr_padding,
        } = disk::xattr_count(xattr_entries.iter().map(|(_prefix_len, entry)| entry));
        let xattr_count: u16 = xattr_count.try_into().map_err(|_| Error::TooManyXattrs)?;
        let xattr_len = disk::xattr_count_to_len(xattr_count);

        match &mut inode {
            Inode::Compact(x) => {
                x.xattr_count = xattr_count.into();
            }
            Inode::Extended(x) => {
                x.xattr_count = xattr_count.into();
            }
        };

        let data = match &inode {
            Inode::Compact(x) => x.as_bytes(),
            Inode::Extended(x) => x.as_bytes(),
        };

        let total_len = data.len() + xattr_len + tail.as_ref().map(|x| x.len()).unwrap_or(0);

        if total_len as u64 > self.block_size() {
            return Err(Error::InodeTooBig);
        }

        // we have to check that our inode + xattrs + tail will fit within a block, if not, advance to the
        // next block
        // TODO there is a problem here if we have already made the tail too big to possibly fit if
        // there are a lot of xattrs; not sure how erofs handles this
        let block_no1 = self.block_no(self.inode_addr);
        let block_no2 = self.block_no(self.inode_addr + total_len as u64);
        if block_no1 != block_no2 {
            let padding = self.zero_fill_block(self.inode_addr as usize)?;
            debug_assert!(self.block_addr(block_no2) == self.inode_addr + padding);
            self.inode_addr = self.block_addr(block_no2);
        }

        debug_assert!(
            self.block_no(self.inode_addr) == self.block_no(self.inode_addr + total_len as u64)
        );

        let disk_id = self.addr_to_disk_id(self.inode_addr)?;

        self.writer.write_all(data)?;

        if !xattrs.is_empty() {
            let header = XattrHeader::new_zeroed();
            self.writer.write_all(header.as_bytes())?;
            for ((prefix_len, entry), (key, value)) in xattr_entries.into_iter().zip(xattrs.iter())
            {
                self.writer.write_all(entry.as_bytes())?;
                self.writer
                    .write_all(key.get(usize::from(prefix_len)..).ok_or(Error::Oob)?)?;
                self.writer.write_all(value)?;
            }
            for _ in 0..xattr_padding {
                self.writer.write_all(&[0])?;
            }
        }

        if let Some(tail) = tail {
            self.stats.tails += 1;
            self.stats.tail_size += tail.len();
            self.writer.write_all(tail)?;
        }

        let padding = self.zero_fill(total_len, INODE_ALIGNMENT)?;
        self.inode_addr += total_len as u64 + padding;

        #[cfg(debug_assertions)]
        self.check_writer_alignment("post");

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
        let max_depth = self.max_depth;
        walk_tree(
            &mut root.root,
            &mut BuilderTreeVisitorPrepareDirents { builder: self },
            max_depth,
        )?;
        // we are now done writing data, so we record the meta block number
        let meta_block = *self.meta_block.insert(self.cur_data_block);
        self.inode_addr = self.block_addr(meta_block);
        self.writer.seek(SeekFrom::Start(self.inode_addr))?;

        walk_tree(
            &mut root.root,
            &mut BuilderTreeVisitorWriteDirInodes { builder: self },
            max_depth,
        )?;

        if cfg!(debug_assertions) {
            // this is currently guaranteed without dirent tail packing but could change
            if self.inode_addr % INODE_ALIGNMENT != 0 {
                panic!(
                    "before writing inodes we must be aligned, at {}",
                    self.inode_addr
                );
            }
        }

        walk_tree(
            &mut root.root,
            &mut BuilderTreeVisitorWriteInodes { builder: self },
            max_depth,
        )?;

        walk_tree(
            &mut root.root,
            &mut BuilderTreeVisitorWriteDirents {
                builder: self,
                parents: vec![],
            },
            max_depth,
        )?;

        let _ = self.root.insert(root);
        Ok(())
    }

    fn resolve_links(&mut self) -> Result<(), Error> {
        let root = self.root.as_mut().expect("not none");
        for (path, target, meta) in std::mem::take(&mut self.links).into_iter() {
            let (start_block, len, tail) = {
                // TODO we're not handling the case of multiple hardlinks that try to get resolved
                // in the wrong order like:
                // FILE /x
                // LINK /y -> /z
                // LINK /z -> /x
                // is valid but we would try to resolve in the order they come and not in graph
                // order
                match root.get(&target)?.ok_or(Error::HardlinkNotResolved)? {
                    Dirent::File(f) => {
                        f.n_links += 1;
                        Ok((f.start_block, f.len, f.tail.clone()))
                    }
                    Dirent::Symlink(s) => {
                        s.n_links += 1;
                        Ok((s.start_block, s.len, s.tail.clone()))
                    }
                    Dirent::Dot | Dirent::DotDot | Dirent::Dir(_) => Err(Error::HardlinkToDir),
                }?
            };
            root.add_file(
                path,
                File {
                    meta,
                    start_block,
                    len,
                    tail,
                    n_links: 2,
                    ..Default::default()
                },
            )?;
        }
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), Error> {
        self.resolve_links()?;
        self.write_inodes()?;
        self.write_superblock()?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn into_inner(mut self) -> Result<(Stats, W), Error> {
        self.finalize()?;
        self.writer
            .into_inner()
            .map_err(|e| e.into_error().into())
            .map(|w| (self.stats, w))
    }
}

// not the prettiest return type but only has two callers
fn size_tail_len(len: usize, block_size_bits: u8) -> (usize, usize, usize) {
    let block_size = 1usize << block_size_bits;
    let n_blocks = len / block_size;
    let tail_len = len % block_size;
    if tail_len > block_size / 2 {
        (n_blocks + 1, len, 0)
    } else {
        (n_blocks, n_blocks * block_size, tail_len)
    }
}

fn make_mode(typ: FileType, mode: Mode) -> Result<u16, Error> {
    let result = typ.as_raw_mode() | mode.as_raw_mode();
    if result > u16::MAX as u32 {
        Err(Error::ModeShouldFitInU16)
    } else {
        Ok(result as u16)
    }
}

fn make_xattr_entries(xattrs: &XattrMap) -> Result<Vec<(u8, XattrEntry)>, Error> {
    let ret: Result<Vec<_>, _> = xattrs
        .iter()
        .map(|(key, value)| {
            let (prefix_id, prefix_len) = disk::xattr_builtin_prefix(key)
                .map(|x| (x.id, x.len))
                .unwrap_or((0, 0));
            assert!(prefix_len as usize <= key.len());
            let entry = XattrEntry {
                name_len: (key.len() - prefix_len as usize)
                    .try_into()
                    .map_err(|_| Error::XattrKeyTooLong)?,
                value_size: value
                    .len()
                    .try_into()
                    .map_err(|_| Error::XattrValueTooLong)?,
                name_index: prefix_id,
            };
            Ok((prefix_len, entry))
        })
        .collect();
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeSet, HashSet};
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use rustix::fs::Mode;
    use tempfile::NamedTempFile;

    // NOTE: we can't easily test links in a roundtrip fashion because links get read as normal
    // files, the only way to detect if they are a link is to check the meta_blkaddr of the inode
    // and even if you do, there is no distinguishing the "original" file and the hardlink

    #[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    enum EntryTyp {
        #[default]
        File,
        Dir,
        Link,
        Symlink,
        //Fifo,
    }

    impl Into<FileType> for EntryTyp {
        fn into(self) -> FileType {
            match self {
                EntryTyp::Link | EntryTyp::File => FileType::RegularFile,
                EntryTyp::Dir => FileType::Directory,
                EntryTyp::Symlink => FileType::Symlink,
            }
        }
    }

    impl From<FileType> for EntryTyp {
        fn from(x: FileType) -> EntryTyp {
            match x {
                FileType::RegularFile => EntryTyp::File,
                FileType::Directory => EntryTyp::Dir,
                FileType::Symlink => EntryTyp::Symlink,
                FileType::Unknown => {
                    panic!("Tried to convert FileType::Unknown");
                }
                _ => todo!("file type conversion {:?}", x),
            }
        }
    }

    // E is a standalone redux Entry
    #[derive(Default, PartialOrd, Ord, PartialEq, Eq, Clone)]
    struct E {
        typ: EntryTyp,
        path: PathBuf,
        data: Option<Vec<u8>>,
        link: Option<PathBuf>,
        mtime: u64,
        uid: u32,
        gid: u32,
        mode: u16, // should really be Mode but clashes with BTreeSet
        xattrs: BTreeMap<Box<[u8]>, Box<[u8]>>,
    }

    impl std::fmt::Debug for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("E")
                .field("typ", &self.typ)
                .field("path", &self.path)
                .field("link", &self.link)
                .field("mtime", &self.mtime)
                .field("uid", &self.uid)
                .field("gid", &self.gid)
                .field("mode", &self.mode)
                .field(
                    "data",
                    &self.data.as_ref().map(|x| x.escape_ascii().to_string()),
                )
                .field(
                    "xattrs",
                    &self
                        .xattrs
                        .iter()
                        .map(|(k, v)| (k.escape_ascii().to_string(), v.escape_ascii().to_string()))
                        .collect::<Vec<_>>(),
                )
                .finish()
        }
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
        //fn fifo<P: Into<PathBuf>>(path: P) -> Self {
        //    Self {
        //        typ: EntryTyp::Fifo,
        //        path: path.into(),
        //        ..Default::default()
        //    }
        //}
        fn uid(mut self: Self, uid: u32) -> Self {
            self.uid = uid;
            self
        }
        fn xattr(mut self: Self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Self {
            self.xattrs
                .insert(key.as_ref().into(), value.as_ref().into());
            self
        }

        fn meta(&self) -> Meta {
            Meta {
                uid: self.uid,
                gid: self.gid,
                mtime: self.mtime,
                mode: Mode::from_raw_mode(self.mode as u32),
                xattrs: self.xattrs.clone(),
            }
        }
    }

    type EList = BTreeSet<E>;

    fn inode_to_e<'a, P: AsRef<Path>>(
        erofs: &'a disk::Erofs,
        inode: &disk::Inode<'a>,
        name: P,
    ) -> E {
        let data = if inode.file_type() == FileType::RegularFile {
            let (l, r) = erofs.get_data(inode).unwrap();
            Some([l, r].concat().into())
        } else {
            None
        };

        let link = if inode.file_type() == FileType::Symlink {
            let (l, r) = erofs.get_data(inode).unwrap();
            Some(String::from_utf8([l, r].concat().into()).unwrap().into())
        } else {
            None
        };

        let xattrs = if let Some(xattrs) = erofs.get_xattrs(inode).unwrap() {
            xattrs
                .iter()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let prefix = erofs.get_xattr_prefix(&entry).unwrap();
                    ([prefix, entry.name].concat().into(), entry.value.into())
                })
                .collect::<XattrMap>()
        } else {
            XattrMap::new()
        };

        E {
            typ: inode.file_type().into(),
            path: name.as_ref().into(),
            uid: inode.uid(),
            gid: inode.gid(),
            mode: Mode::from_raw_mode(inode.mode() as u32).as_raw_mode() as u16, // mask out the S_IFMT
            data: data,
            xattrs,
            link,
            ..Default::default()
        }
    }

    fn into_erofs<W: Write + Seek>(entries: &EList, writer: W) -> Result<W, Error> {
        let mut b = Builder::new(writer, BuilderConfig::default())?;
        for entry in entries.iter() {
            match &entry.typ {
                EntryTyp::File => {
                    let data = entry.data.as_ref().expect("file should have data");
                    b.add_file(
                        &entry.path,
                        entry.meta(),
                        data.len(),
                        &mut Cursor::new(&data),
                    )?;
                }
                EntryTyp::Symlink => {
                    b.add_symlink(
                        &entry.path,
                        entry.link.as_ref().expect("symlink should have link"),
                        entry.meta(),
                    )?;
                }
                EntryTyp::Link => {
                    b.add_link(
                        &entry.path,
                        entry.link.as_ref().expect("symlink should have link"),
                        entry.meta(),
                    )?;
                }
                EntryTyp::Dir => {
                    b.upsert_dir(&entry.path, entry.meta())?;
                }
            }
        }
        b.into_inner().map(|(_stats, w)| w)
    }

    fn erofs_to_elist(data: &[u8]) -> Result<EList, disk::Error> {
        let mut seen = HashSet::new();
        let mut ret = BTreeSet::new();

        let erofs = disk::Erofs::new(data)?;

        let root_inode = erofs.get_root_inode()?.disk_id();
        //eprintln!("root inode is {:?}", root_inode);
        let mut q = vec![(PathBuf::from("/"), root_inode)];
        seen.insert(root_inode);

        while let Some((name, cur)) = q.pop() {
            let inode = erofs.get_inode(cur)?;
            //eprintln!("processing {name:?} {}", cur);
            ret.insert(inode_to_e(&erofs, &inode, &name));
            match inode.file_type() {
                FileType::Directory => {
                    let dirents = erofs.get_dirents(&inode)?;
                    for item in dirents.iter()? {
                        let item = item?;
                        //eprintln!("item.name= {:?}", item.name);
                        if item.name == b"." || item.name == b".." {
                            continue;
                        }
                        let disk_id = item.disk_id.try_into().expect("why is this u64");
                        if !seen.insert(disk_id) {
                            continue;
                        }
                        let name = name.join(OsStr::from_bytes(item.name));
                        q.push((name, disk_id));
                    }
                }
                _ => {}
            }
        }
        Ok(ret)
    }

    fn erofs_roundtrip(entries: &EList) -> EList {
        let buf = into_erofs(entries, Cursor::new(vec![]))
            .unwrap()
            .into_inner();
        if false {
            let mut proc = std::process::Command::new("xxd")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            proc.stdin.as_mut().unwrap().write_all(&buf).unwrap();
            proc.wait().unwrap();
        }
        erofs_to_elist(&buf).unwrap()
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
    fn test_tree_operations() {
        // add "a/b" then "/a/b" and lookup with both "a/b" and "/a/b" and "./a/b" and should give the same positive result
        for prefix in ["", "/", "./"] {
            let mut tree = Root {
                root: Dir::default(),
            };
            tree.add_file(
                format!("{}{}", prefix, "a/b"),
                File {
                    start_block: 42,
                    ..File::default()
                },
            )
            .unwrap();
            assert!(tree.get("foo").unwrap().is_none());
            for prefix2 in ["", "/", "./"] {
                if let Dirent::File(f) = tree.get(format!("{}{}", prefix2, "a/b")).unwrap().unwrap()
                {
                    assert_eq!(f.start_block, 42);
                } else {
                    assert!(false);
                }
            }
        }

        {
            let mut tree = Root {
                root: Dir::default(),
            };
            tree.add_file(
                "/a/b",
                File {
                    start_block: 42,
                    ..File::default()
                },
            )
            .unwrap();
            assert!(tree.get("/a/../a/b").is_err());
            assert!(tree.get("").is_err());
            assert!(tree.lookup(Path::new(".")).unwrap().1.is_none());
            assert!(tree.lookup(Path::new("/")).unwrap().1.is_none());
        }

        {
            let mut tree = Root {
                root: Dir::default(),
            };
            tree.add_file(
                "/a/b",
                File {
                    start_block: 42,
                    ..File::default()
                },
            )
            .unwrap();
            tree.upsert_dir(
                "a",
                Meta {
                    uid: 42,
                    ..Meta::default()
                },
            )
            .unwrap();
            tree.upsert_dir(
                "a/",
                Meta {
                    uid: 42,
                    ..Meta::default()
                },
            )
            .unwrap();

            assert!(tree.upsert_dir("a/b", Meta::default()).is_err());

            tree.upsert_dir(
                "/",
                Meta {
                    uid: 42,
                    ..Meta::default()
                },
            )
            .unwrap();
            assert_eq!(tree.root.meta.uid, 42);
            tree.upsert_dir(
                ".",
                Meta {
                    uid: 43,
                    ..Meta::default()
                },
            )
            .unwrap();
            assert_eq!(tree.root.meta.uid, 43);
            tree.upsert_dir(
                "./",
                Meta {
                    uid: 44,
                    ..Meta::default()
                },
            )
            .unwrap();
            assert_eq!(tree.root.meta.uid, 44);
        }
    }

    #[test]
    fn test_builder_simple() -> Result<(), Error> {
        let mut b = Builder::new(NamedTempFile::new().expect("tf"), BuilderConfig::default())?;
        {
            let data = b"hello world";
            b.add_file(
                "/foo/bar",
                //"/foo",
                Meta::default(),
                data.len(),
                &mut Cursor::new(data),
            )?;
        }

        let (_stats, tf) = b.into_inner().expect("io fail");
        fsck_erofs(tf.path())?;
        Ok(())
    }

    macro_rules! check_erofs_fsck {
        ($entries:expr) => {{
            let entries = $entries.iter().cloned().collect::<EList>();
            let tf = NamedTempFile::new().expect("tf");
            let tf = into_erofs(&entries, tf).unwrap();
            let persist = false;
            let path = if persist {
                let p = Path::new("/tmp/peerofs.test.erofs");
                tf.persist(p).unwrap();
                p
            } else {
                tf.path()
            };
            let result = fsck_erofs(path);
            match result {
                Err(Error::Other(ref s)) => {
                    eprintln!("{}", s);
                }
                Err(ref e) => {
                    eprintln!("{:?}", e);
                }
                Ok(_) => {}
            }
            assert!(result.is_ok());
        }};
    }

    // we check subset because adding a file /foo will also create the root dir
    macro_rules! check_erofs_roundtrip {
        ($entries:expr) => {{
            let entries = $entries.iter().cloned().collect::<EList>();
            let got = erofs_roundtrip(&entries);
            let missing = entries.difference(&got).cloned().collect::<EList>();
            if !missing.is_empty() {
                eprintln!("got {:#?}", got);
            }
            assert_eq!(EList::new(), missing);
            //assert_eq!(
            //    entries,
            //    got
            //);
        }};
    }

    macro_rules! check_erofs {
        ($entries:expr) => {{
            let entries = $entries;
            check_erofs_fsck!(entries);
            check_erofs_roundtrip!(entries);
        }};
    }

    #[test]
    fn test_fsck_simple() {
        check_erofs!(vec![
            E::file("/x", b"hi").uid(1000),
            E::dir("/dir"),
            E::file("/dir/x", b"foo"),
            E::file("/a/b/c/d/e/x", b"foo"),
            E::symlink("/y", "/x"),
        ]);
    }

    #[test]
    fn test_fsck_tails() {
        // make the data bit more interesting
        fn d(size: usize) -> Vec<u8> {
            (0..size).map(|x| x as u8).collect()
        }
        check_erofs!(vec![
            E::file("/a", &d(0)),
            E::file("/b", &d(1)),
            E::file("/c", &d(2047)),
            E::file("/d", &d(2048)),
            E::file("/e", &d(2049)),
            E::file("/f", &d(4095)),
            E::file("/g", &d(4096)),
            E::file("/h", &d(4097)),
            E::file("/i", &d(4096 + 2047)),
            E::file("/j", &d(4096 + 2048)),
            E::file("/k", &d(4096 + 2049)),
        ]);
    }

    #[test]
    fn test_fsck_big_dir() {
        // a dirent is 12 bytes, there are always 2 entries for . and .. so always 2*12+3 = 27
        // bytes, 4096 - 27 = 4069
        // with 200 entries of name length 8, we have (12 + 88) * 40 = 4000, which leaves room for
        // one entry with 69 - 12 = 57 bytes long to exactly fill one block
        // start counting at 1000, this gives us names from 1000 to 1127 and then we append 20
        // 0's to the end to get total length 24
        //for i in start..(start + 128) {
        for delta in [-1, 0, 1] {
            let mut entries: Vec<_> = (0..40)
                .map(|i| E::file(format!("/{:088}", i), b"data"))
                .collect();
            let width = (57 + delta) as usize;
            // use fill character 9 so that this one is last (not necessary but easier to think
            // about)
            entries.push(E::file(format!("/{:z<width$}", "z"), b"data"));
            check_erofs!(entries);
        }
    }

    #[test]
    fn test_builder() {
        check_erofs!(vec![
            E::file("/x", b"hi").xattr("user.attr", "value"),
            E::symlink("/s1", "/x"),
            E::file("/dir/x", b"foo").xattr("user.attr", "value"),
            E::file("/y", b"hi").xattr("system.posix_acl_access", "some acl"),
            E::file("/z", b"hi").xattr("system.posix_acl_default", "some default"),
            E::file("/a", b"hi").xattr("trusted.foo", "foo"),
            E::file("/b", b"hi").xattr("security.bar", "bar"),
            E::file("/c", b"hi").xattr("notaprefix.somethingelse", "baz"),
            E::symlink("/s2", "/x"),
        ]);
    }

    #[test]
    fn test_max_depth() {
        let mut b = Builder::new(Cursor::new(vec![]), BuilderConfig::default()).unwrap();
        let depth = MAX_DEPTH;
        let mut very_deep_file = String::with_capacity(2 * depth);
        for _ in 0..depth {
            very_deep_file.push_str("d/");
        }
        very_deep_file.push_str("f");
        // should we limit the depth on insertion instead?
        b.add_file(very_deep_file, Meta::default(), 0, &mut Cursor::new(b""))
            .unwrap();
        let Err(Error::MaxDepthExceeded) = b.into_inner() else {
            panic!("should have been MaxDepthExceeded");
        };
    }

    #[test]
    fn test_link_count() {
        // TODO this test would fail if we added E::link("/z", "/y") which should give everyone a
        // link count of 3, but we aren't currently handling the transitive links like this
        let entries: EList = vec![E::file("/x", b"hi"), E::link("/y", "/x")]
            .into_iter()
            .collect();
        let buf = into_erofs(&entries, Cursor::new(vec![]))
            .unwrap()
            .into_inner();
        let erofs = disk::Erofs::new(&buf).unwrap();
        for item in erofs
            .get_dirents(&erofs.get_root_inode().unwrap())
            .unwrap()
            .iter()
            .unwrap()
        {
            let item = item.unwrap();
            match item.name {
                b"x" => {
                    let inode = erofs.get_inode_from_dirent(&item).unwrap();
                    assert_eq!(inode.link_count(), 2);
                }
                b"y" => {
                    let inode = erofs.get_inode_from_dirent(&item).unwrap();
                    assert_eq!(inode.link_count(), 2);
                }
                _ => {}
            }
        }
    }
}
