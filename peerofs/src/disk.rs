use std::fmt;

use rustix::fs::FileType;
use zerocopy::byteorder::little_endian::{U16, U32, U64};
use zerocopy::{Immutable, KnownLayout, TryFromBytes};

const EROFS_SUPER_OFFSET: usize = 1024;
const EROFS_SUPER_MAGIG_V1: u32 = 0xe0f5e1e2;

// NOTES:
//  - inode ino is a sequential number, but will not match the nid you look it up with; ie the
//  root_nid from the superblock is something like 26, and you use that to compute the address of
//  the root inode, but that inode will have field ino=1. So I'm not sure what a good name for the
//  on-disk ino id should be.
//

#[derive(Debug)]
pub enum Error {
    BadSuperblock,
    BadMagic,
    BadConversion,
    BadCStr,
    Oob,
    NotDir,
    NotSymlink,
    NotRegDirLink,
    DirentBadSize,
    BadFileType,
    InodeTooBig,
    NotExpectingBlockData,
}

// NOTE: we are using byteorder endian aware types so that they get decoded on demand (noop on LE
// architectures, but they are all alignment 1. So after xattr_prefix_start, there is a 4 byte
// gap that is placed with C alignment rules to get packed_nid to alignment 8. But when we use
// alignment 1 types, that gap is closed and we are 4 bytes short, So _missing_4_bytes is
// inserted as manual padding fill the gap
#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Superblock {
    magic: U32,
    checksum: U32,
    feature_compat: U32,
    blkszbits: u8,
    sb_extslots: u8,
    root_disk_id: U16,
    inos: U64,
    build_time: U64,
    build_time_nsec: U32,
    blocks: U32,
    meta_blkaddr: U32,
    xattr_blkaddr: U32,
    uuid: [u8; 16],
    volume_name: [u8; 16],
    available_compr_algs_or_lz4_max_distance: U16,
    extra_devices: U16,
    devt_slotoff: U16,
    dirblkbits: u8,
    xattr_prefix_count: u8,
    xattr_prefix_start: U32,
    _missing_4_bytes: U32,
    packed_nid: U64,
    xattr_filter_reserved: u8,
    _reserved2: [u8; 23],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct InodeCompact {
    format_layout: U16,
    xattr_count: U16,
    mode: U16,
    nlink: U16,
    size: U32,
    _reserved: U32,
    info: InodeInfo,
    ino: U32,
    uid: U16,
    gid: U16,
    _reserved2: U32,
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct InodeExtended {
    format_layout: U16,
    xattr_count: U16,
    mode: U16,
    _reserved: U16,
    size: U64,
    info: InodeInfo,
    ino: U32,
    uid: U32,
    gid: U32,
    mtime: U64,
    mtime_nsec: U32,
    nlink: U32,
    _reserved2: [u8; 16],
}

#[derive(Copy, Clone, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ChunkInfo {
    format: U16,
    _reserved: U16,
}
#[derive(TryFromBytes, Immutable)]
#[repr(C)]
pub union InodeInfo {
    compressed_blocks: U32,
    raw_blkaddr: U32,
    rdev: U32,
    chunk_info: ChunkInfo,
}

impl fmt::Debug for InodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let a = unsafe { self.compressed_blocks };
        let b = unsafe { self.chunk_info.format };
        write!(f, "{} ({:x}) (chunk={:x})", a, a, b)
    }
}

#[derive(Debug)]
pub enum Inode<'a> {
    Compact((u32, &'a InodeCompact)),
    Extended((u32, &'a InodeExtended)),
}

impl<'a> Inode<'a> {
    pub fn file_type(&self) -> FileType {
        match self {
            Inode::Compact((_, x)) => FileType::from_raw_mode(x.mode.into()),
            Inode::Extended((_, x)) => FileType::from_raw_mode(x.mode.into()),
        }
    }

    pub fn data_size(&self) -> u64 {
        match self {
            Inode::Compact((_, x)) => x.size.into(),
            Inode::Extended((_, x)) => x.size.into(),
        }
    }

    pub fn disk_id(&self) -> u32 {
        match self {
            Inode::Compact((id, _)) => *id,
            Inode::Extended((id, _)) => *id,
        }
    }

    pub fn size(&self) -> usize {
        match self {
            Inode::Compact(_) => std::mem::size_of::<InodeCompact>(),
            Inode::Extended(_) => std::mem::size_of::<InodeExtended>(),
        }
    }

    pub fn xattr_size(&self) -> usize {
        let count: usize = match self {
            Inode::Compact((_, x)) => x.xattr_count.into(),
            Inode::Extended((_, x)) => x.xattr_count.into(),
        };
        if count == 0 {
            0
        } else {
            std::mem::size_of::<XattrHeader>() + (count - 1) * std::mem::size_of::<u32>()
        }
    }

    pub fn ino(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => x.ino.into(),
            Inode::Extended((_, x)) => x.ino.into(),
        }
    }

    pub fn uid(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => x.uid.into(),
            Inode::Extended((_, x)) => x.uid.into(),
        }
    }

    pub fn gid(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => x.gid.into(),
            Inode::Extended((_, x)) => x.gid.into(),
        }
    }

    pub fn mode(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => x.mode.into(),
            Inode::Extended((_, x)) => x.mode.into(),
        }
    }

    pub fn layout(&self) -> Layout {
        let format_layout: u16 = match self {
            Inode::Compact((_, x)) => x.format_layout.into(),
            Inode::Extended((_, x)) => x.format_layout.into(),
        };
        ((format_layout >> 1) & 0x07)
            .try_into()
            .expect("should be validated on the way in")
    }

    pub fn raw_block_addr(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => unsafe { x.info.raw_blkaddr.into() },
            Inode::Extended((_, x)) => unsafe { x.info.raw_blkaddr.into() },
        }
    }

    pub fn block_addr(&self) -> Result<u64, Error> {
        match self.file_type() {
            FileType::RegularFile | FileType::Directory | FileType::Symlink => match self {
                Inode::Compact((_, x)) => Ok(unsafe { x.info.raw_blkaddr }.into()),
                Inode::Extended((_, x)) => Ok(unsafe { x.info.raw_blkaddr }.into()),
            },
            _ => Err(Error::NotRegDirLink),
        }
    }
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct XattrHeader {
    name_filter: U32,
    shared_count: u8,
    _reserved: [u8; 7],
    // u32 ids[]
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct XattrEntry {
    name_len: u8,
    name_index: u8,
    value_size: U16,
    // u8 name[]
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Dirent {
    disk_id: U64,
    name_offset: U16,
    file_type: u8,
    _reserved: u8,
}

#[derive(Debug)]
pub struct Dirents<'a> {
    data: (&'a [u8], &'a [u8]),
    block_size: usize,
}

impl<'a> Dirents<'a> {
    fn new(data: (&'a [u8], &'a [u8]), block_size: usize) -> Result<Self, Error> {
        Ok(Self { data, block_size })
    }

    pub fn iter(&'a self) -> Result<DirentsIterator<'a>, Error> {
        DirentsIterator::new(self.data, self.block_size)
    }
}

#[derive(Debug)]
pub struct DirentItem<'a> {
    pub disk_id: u64,
    pub file_type: DirentFileType,
    pub name: &'a [u8],
}

// this will either have
//   data: block, remaining: Some(tail)
//   data: tail, remaining: None
pub struct DirentsIterator<'a> {
    data: &'a [u8],
    remaining: Option<&'a [u8]>,
    i: u16,
    count: u16,
    block_size: usize,
}

impl<'a> DirentsIterator<'a> {
    fn new((block, tail): (&'a [u8], &'a [u8]), block_size: usize) -> Result<Self, Error> {
        let mut ret = Self {
            data: block,
            remaining: Some(tail),
            i: 0,
            count: 0,
            block_size,
        };
        ret.reset_count()?;
        Ok(ret)
    }

    fn reset_count(&mut self) -> Result<(), Error> {
        if self.data.is_empty() {
            if let Some(next) = self.remaining.take() {
                self.data = next;
            }
            if self.data.is_empty() {
                self.i = 0;
                self.count = 0;
                return Ok(());
            }
        }
        let (dirent, _) =
            Dirent::try_ref_from_prefix(&self.data).map_err(|_| Error::BadConversion)?;
        let offset: u16 = dirent.name_offset.into();
        let (count, rem) = div_mod_u16(offset, std::mem::size_of::<Dirent>().try_into().unwrap());
        if rem != 0 {
            return Err(Error::DirentBadSize);
        }
        self.i = 0;
        self.count = count;
        Ok(())
    }

    fn next_impl(&mut self) -> Result<DirentItem<'a>, Error> {
        let dirent = self.get(self.i.into())?;
        let disk_id: u64 = dirent.disk_id.into();
        let file_type: DirentFileType = dirent.file_type.try_into()?;
        let name_offset: usize = dirent.name_offset.into();
        // name_offset is referenced from the start of the block, not relative to the entry itself

        let name = if self.i < self.count - 1 {
            let next_dirent = self.get((self.i + 1).into())?;
            let next_offset: usize = next_dirent.name_offset.into();
            let name_len = next_offset - name_offset;
            self.data
                .get(name_offset..name_offset + name_len)
                .ok_or(Error::Oob)?
        } else {
            // last dirent in block
            let block_end = std::cmp::min(self.data.len(), self.block_size);
            let slice = self.data.get(name_offset..block_end).ok_or(Error::Oob)?;

            self.data = &self.data[block_end..];
            self.reset_count()?;

            if let Some(i) = slice.iter().position(|&x| x == 0) {
                &slice[..i]
            } else {
                slice
            }
        };

        self.i += 1;

        Ok(DirentItem {
            disk_id,
            file_type,
            name,
        })
    }

    fn get(&'a self, i: usize) -> Result<&'a Dirent, Error> {
        let offset = i * std::mem::size_of::<Dirent>();
        Dirent::try_ref_from_prefix(self.data.get(offset..).ok_or(Error::Oob)?)
            .map_err(|_| Error::BadConversion)
            .map(|(dirent, _)| dirent)
    }
}

impl<'a> Iterator for DirentsIterator<'a> {
    type Item = Result<DirentItem<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.i < self.count {
            Some(self.next_impl())
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub enum DirentFileType {
    Unknown = 0,
    RegularFile = 1,
    Directory = 2,
    CharacterDevice = 3,
    BlockDevice = 4,
    Fifo = 5,
    Socket = 6,
    Symlink = 7,
}

impl TryFrom<u8> for DirentFileType {
    type Error = Error;
    fn try_from(x: u8) -> Result<DirentFileType, Error> {
        use DirentFileType::*;
        match x {
            0 => Ok(Unknown),
            1 => Ok(RegularFile),
            2 => Ok(Directory),
            3 => Ok(CharacterDevice),
            4 => Ok(BlockDevice),
            5 => Ok(Fifo),
            6 => Ok(Socket),
            7 => Ok(Symlink),
            _ => Err(Error::BadFileType),
        }
    }
}

#[derive(Debug)]
pub enum Layout {
    FlatPlain = 0,
    CompressedFull = 1,
    FlatInline = 2,
    CompressedCompact = 3,
    ChunkBased = 4,
}

#[derive(Debug)]
pub struct InvalidLayout;

impl TryFrom<u16> for Layout {
    type Error = InvalidLayout;
    fn try_from(x: u16) -> Result<Layout, InvalidLayout> {
        use Layout::*;
        match x {
            0 => Ok(FlatPlain),
            1 => Ok(CompressedFull),
            2 => Ok(FlatInline),
            3 => Ok(CompressedCompact),
            4 => Ok(ChunkBased),
            _ => Err(InvalidLayout),
        }
    }
}

pub struct Erofs<'a> {
    data: &'a [u8],
    pub sb: &'a Superblock,
}

impl<'a> Erofs<'a> {
    pub fn new(data: &'a [u8]) -> Result<Erofs<'a>, Error> {
        let (sb, _) = Superblock::try_ref_from_prefix(&data[EROFS_SUPER_OFFSET..])
            .map_err(|_| Error::BadConversion)?;
        if sb.magic != EROFS_SUPER_MAGIG_V1 {
            return Err(Error::BadMagic);
        }
        Ok(Self { data, sb })
    }

    fn block_size(&self) -> u64 {
        1u64 << self.sb.blkszbits
    }

    fn block_offset(&self, block: u32) -> u64 {
        (block as u64) << self.sb.blkszbits
    }

    fn raw_inode_offset(&self, disk_id: u32) -> u64 {
        self.block_offset(self.sb.meta_blkaddr.into()) + 32u64 * disk_id as u64
    }

    fn inode_offset(&self, inode: &Inode<'a>) -> u64 {
        self.raw_inode_offset(inode.disk_id())
    }

    fn inode_end(&self, inode: &Inode<'a>) -> u64 {
        let start = self.inode_offset(inode);
        let inode_size = inode.size();
        let xattr_size = 0; // TODO
        start + inode_size as u64 + xattr_size
    }

    pub fn get_inode(&self, disk_id: u32) -> Result<Inode<'a>, Error> {
        let offset = self.raw_inode_offset(disk_id) as usize;
        let format_layout = self.data.get(offset).ok_or(Error::Oob)?;
        match format_layout & 1 {
            0 => InodeCompact::try_ref_from_prefix(&self.data[offset..])
                .map_err(|_| Error::BadConversion)
                .map(|(inode, _)| Inode::Compact((disk_id, inode))),
            1 => InodeExtended::try_ref_from_prefix(&self.data[offset..])
                .map_err(|_| Error::BadConversion)
                .map(|(inode, _)| Inode::Extended((disk_id, inode))),
            _ => unreachable!(),
        }
    }

    pub fn get_inode_dirent(&self, dirent: &DirentItem<'a>) -> Result<Inode<'a>, Error> {
        // idk why the dir disk id is a u64
        self.get_inode(dirent.disk_id.try_into().map_err(|_| Error::InodeTooBig)?)
    }

    pub fn get_root_inode(&self) -> Result<Inode<'a>, Error> {
        self.get_inode(self.sb.root_disk_id.into())
    }

    fn compute_block_tail_len(&self, size: u64) -> (usize, usize) {
        compute_block_tail_len(self.block_size() as usize, size as usize)
    }

    fn get_data(&self, inode: &Inode<'a>) -> Result<(&'a [u8], &'a [u8]), Error> {
        match inode.layout() {
            Layout::FlatInline => {
                let block_addr = inode.raw_block_addr();
                let (block_len, tail_len) = self.compute_block_tail_len(inode.data_size());

                let tail = {
                    let data_begin = self.inode_end(&inode) as usize;
                    self.data
                        .get(data_begin..data_begin + tail_len)
                        .ok_or(Error::Oob)?
                };
                let block = if block_addr == 0xffffffff {
                    &[]
                } else {
                    let data_begin = self.block_offset(inode.raw_block_addr()) as usize;
                    self.data
                        .get(data_begin..data_begin + block_len)
                        .ok_or(Error::Oob)?
                };
                Ok((block, tail))
            }
            Layout::FlatPlain => {
                let data_len = inode.data_size() as usize;
                let data_begin = self.block_offset(inode.raw_block_addr()) as usize;
                eprintln!("read begin={data_begin} len={data_len}");
                self.data
                    .get(data_begin..data_begin + data_len)
                    .ok_or(Error::Oob)
                    .map(|x| (x, &[][..]))
            }
            layout => todo!("layout={:?} {:?} {:?}", layout, inode, inode.file_type()),
        }
    }

    pub fn get_dirents(&self, inode: &Inode<'a>) -> Result<Dirents<'a>, Error> {
        if inode.file_type() != FileType::Directory {
            return Err(Error::NotDir);
        }
        let data = self.get_data(inode)?;
        Dirents::new(data, self.block_size() as usize)
    }

    pub fn get_symlink(&self, inode: &Inode<'a>) -> Result<&'a [u8], Error> {
        if inode.file_type() != FileType::Symlink {
            return Err(Error::NotSymlink);
        }
        let (block, tail) = self.get_data(inode)?;
        if !block.is_empty() {
            return Err(Error::NotExpectingBlockData);
        }
        Ok(tail)
    }
}

fn div_mod_u16(a: u16, b: u16) -> (u16, u16) {
    (a / b, a % b)
}

fn compute_block_tail_len(block_size: usize, size: usize) -> (usize, usize) {
    let num_blocks = size / block_size;
    let block_len = num_blocks * block_size;
    let tail_len = size - block_len;
    (block_len, tail_len)
}

// TODO:
//   xattr_entry
//   xattr_prefix
//   dirent
//   chunk_index
//   lz4_cfg
//   map/clusters

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sizeof() {
        assert_eq!(128, std::mem::size_of::<Superblock>(), "Superblock");
        assert_eq!(64, std::mem::size_of::<InodeExtended>(), "InodeExtended");
        assert_eq!(32, std::mem::size_of::<InodeCompact>(), "InodeCompact");
        assert_eq!(12, std::mem::size_of::<Dirent>(), "Dirent");
        assert_eq!(12, std::mem::size_of::<XattrHeader>(), "XattrHeader");
        assert_eq!(4, std::mem::size_of::<XattrEntry>(), "XattrEntry");
    }

    fn test_compute_block_tail_len() {
        assert_eq!((4096, 0), compute_block_tail_len(4096, 4096));
        assert_eq!((4096, 1), compute_block_tail_len(4096, 4097));
        assert_eq!((0, 4095), compute_block_tail_len(4096, 4095));
    }
}
