use std::fmt;

use rustix::fs::FileType;
use zerocopy::byteorder::little_endian::{U16, U32, U64};
use zerocopy::{FromZeros, Immutable, IntoBytes, KnownLayout, TryFromBytes};

pub const EROFS_SUPER_OFFSET: usize = 1024;
pub const EROFS_SUPER_MAGIG_V1: u32 = 0xe0f5e1e2;

// NOTES:
// - inode ino is a sequential number, but will not match the nid you look it up with; ie the
// root_nid from the superblock is something like 26, and you use that to compute the address of
// the root inode, but that inode will have field ino=1. So I'm not sure what a good name for the
// on-disk ino id should be. Currently calling it disk_id; it is not really an id because it is
// used in direct addressing calculation
//
// Data Storage
// - FlatInline storage stores whole blocks worth of data starting at raw_block_addr (number) and
// then the remainder immediately follows the inode like FlatInline. Inline (also called tail
// packing) storage cannot cross a block boundary, so the maximum tail length is really the block
// size minus inode size (32 or 64 + xattrs). And if you can't fit in the current block, then you
// have to just skip to the start of the next block.
// - FlatPlain storage is like FlatInline but with no tail data. I was wondering why this exists
// and why not just have FlatInline, but if you are storing 8191 bytes for example, then if you
// always used FlatInline, you would store 1 block and 4095 bytes inline; whereas with FlatPlain
// you just store in 2 blocks
//
// Directories
// - dirents are stored in blocks of the block size. A single directory may span multiple blocks
// - Names are stored without null terminator, except the last one in a block. (see next)
// - The final name in a dirent block *may* have a null terminator if it ends before the block,
// otherwise the name's last byte is the last byte in the block.
// - dirents can be stored as either FlatInline or FlatPlain. If FlatInline and there is data
// stored in blocks, the dirent block will end before the tail data starts (since dirent blocks are
// max sized the block size).
// - dirent name_offset is relative to the start of the block or start of the tail data
// - dirents are sorted in name order EXCEPT for . and .. which are materialized on disk

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
    BlockLenShouldBeZero,
    NotCompressed,
}

// NOTE: we are using byteorder endian aware types so that they get decoded on demand (noop on LE
// architectures, but they are all alignment 1. So after xattr_prefix_start, there is a 4 byte
// gap that is placed with C alignment rules to get packed_nid to alignment 8. But when we use
// alignment 1 types, that gap is closed and we are 4 bytes short, So _missing_4_bytes is
// inserted as manual padding fill the gap
// I don't like the pub(crate) noise
#[derive(Debug, TryFromBytes, Immutable, KnownLayout, Default, IntoBytes)]
#[repr(C)]
pub struct Superblock {
    pub(crate) magic: U32,
    pub(crate) checksum: U32,
    pub(crate) feature_compat: U32,
    pub(crate) blkszbits: u8,
    pub(crate) sb_extslots: u8,
    pub(crate) root_disk_id: U16,
    pub(crate) inos: U64,
    pub(crate) build_time: U64,
    pub(crate) build_time_nsec: U32,
    pub(crate) blocks: U32,
    pub(crate) meta_blkaddr: U32,  // block number not addr
    pub(crate) xattr_blkaddr: U32, // block number not addr
    pub(crate) uuid: [u8; 16],
    pub(crate) volume_name: [u8; 16],
    pub(crate) available_compr_algs_or_lz4_max_distance: U16,
    pub(crate) extra_devices: U16,
    pub(crate) devt_slotoff: U16,
    pub(crate) dirblkbits: u8,
    pub(crate) xattr_prefix_count: u8,
    pub(crate) xattr_prefix_start: U32,
    _missing_4_bytes: U32,
    pub(crate) packed_nid: U64,
    pub(crate) xattr_filter_reserved: u8,
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

#[derive(Debug, Immutable, KnownLayout, FromZeros, IntoBytes)]
#[repr(C)]
pub struct InodeExtended {
    pub(crate) format_layout: U16,
    pub(crate) xattr_count: U16,
    pub(crate) mode: U16,
    _reserved: U16,
    pub(crate) size: U64,
    pub(crate) info: InodeInfo,
    pub(crate) ino: U32,
    pub(crate) uid: U32,
    pub(crate) gid: U32,
    pub(crate) mtime: U64,
    pub(crate) mtime_nsec: U32,
    pub(crate) nlink: U32,
    _reserved2: [u8; 16],
}

#[derive(Debug, PartialEq)]
pub enum Layout {
    FlatPlain = 0,
    CompressedFull = 1,
    FlatInline = 2,
    CompressedCompact = 3,
    ChunkBased = 4,
}

pub enum InodeType {
    Compact,
    Extended,
}

#[derive(Immutable, FromZeros, IntoBytes)]
#[repr(C)]
pub union InodeInfo {
    compressed_blocks: U32,
    raw_blkaddr: U32, // block number not addr
    rdev: U32,
    chunk_info: ChunkInfo,
}

#[derive(Copy, Clone, Immutable, KnownLayout, FromZeros, IntoBytes)]
#[repr(C)]
pub struct ChunkInfo {
    format: U16,
    _reserved: U16,
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

#[derive(Debug, Immutable, KnownLayout, FromZeros, IntoBytes)]
#[repr(C)]
pub struct Dirent {
    pub(crate) disk_id: U64,
    pub(crate) name_offset: U16,
    pub(crate) file_type: u8,
    _reserved: u8,
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
pub struct MapHeader {
    fragment_offset_or_data_size: FragmentOffsetOrDataSize,
    config: U16, // see MapHeaderConfig (bitwise)
    // bit 0-3: algorithm of head 1
    // bit 4-7: algorithm of head 2
    algorithm: u8,
    // bit 0-2: logical cluster bits - 12 (0 for 4096)
    // if bit 7 is set, then this whole 8 byte struct is interpreted as le64 with the high bit
    // cleared as the fragment offset
    cluster_bits: u8,
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
pub struct LogicalClusterIndex {
    advise: U16, // I think this is just type
    cluster_offset: U16,
    block_addr_or_delta: BlockAddrOrDelta,
}

#[derive(TryFromBytes, Immutable)]
#[repr(C)]
union BlockAddrOrDelta {
    block_addr: U32,
    delta: [U16; 2],
}

#[derive(TryFromBytes, Immutable)]
#[repr(C)]
union FragmentOffsetOrDataSize {
    fragment_offset: U32,
    data_size: MapDataSize,
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout, Copy, Clone)]
struct MapDataSize {
    _reserved: U16,
    data_size: U16,
}

pub enum LogicalClusterType {
    Plain = 0,
    Head1 = 1,
    NonHead = 2,
    Head2 = 3,
}

pub enum MapHeaderConfig {
    Compacted2B = 0x0001,
    BigPcluster1 = 0x0002,
    BigPcluster2 = 0x0004,
    InlinePcluster = 0x0008,
    InterlacedPcluster = 0x0010,
    FragmentPcluster = 0x0020,
}

pub enum CompressionType {
    Lz4 = 0,
    Lzma = 1,
    Deflate = 2,
    Zstd = 3,
}

// I think all of these are either preceded or post-ceded by a 2 byte length field
#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
struct Lz4CompressionConfig {
    max_distance: U16,
    max_pcluster_blocks: U16,
    _reserved: [u8; 10],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
struct LzmaCompressionConfig {
    dict_size: U32,
    format: U16,
    _reserved: [u8; 8],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
struct DeflateCompressionConfig {
    window_bits: u8,
    _reserved: [u8; 5],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
struct ZstdCompressionConfig {
    format: u8,
    window_log: u8,
    _reserved: [u8; 4],
}

impl fmt::Debug for InodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let a = unsafe { self.compressed_blocks };
        let b = unsafe { self.chunk_info.format };
        write!(f, "{} ({:x}) (chunk={:x})", a, a, b)
    }
}

impl fmt::Debug for BlockAddrOrDelta {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let a = unsafe { self.block_addr };
        let (d1, d2) = unsafe { (self.delta[0], self.delta[1]) };
        write!(f, "{} ({:x}) (delta=[{} {}])", a, a, d1, d2)
    }
}

impl fmt::Debug for FragmentOffsetOrDataSize {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let a = unsafe { self.fragment_offset };
        let b = unsafe { self.data_size };
        write!(f, "offset={} ({:x}) data_size={:?}", a, a, b.data_size)
    }
}

impl InodeInfo {
    pub fn raw_block(block: u32) -> Self {
        Self {
            raw_blkaddr: block.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Inode<'a> {
    Compact((u32, &'a InodeCompact)),
    Extended((u32, &'a InodeExtended)),
}

impl<'a> Inode<'a> {
    pub fn format_layout(typ: InodeType, layout: Layout) -> u16 {
        let format = match typ {
            InodeType::Compact => 0u16,
            InodeType::Extended => 1u16,
        };
        let layout = layout as u16;
        format | (layout << 1)
    }
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

    pub fn mode(&self) -> u16 {
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

    // TODO this is a pretty bad name since these are block numbers and not addrs
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

#[derive(Debug)]
pub struct Dirents<'a> {
    data: (&'a [u8], &'a [u8]),
    block_size: usize,
}

impl<'a> Dirents<'a> {
    fn new(data: (&'a [u8], &'a [u8]), block_size: usize) -> Result<Self, Error> {
        Ok(Self { data, block_size })
    }

    pub fn iter<'b>(&'b self) -> Result<DirentsIterator<'a>, Error> {
        DirentsIterator::new(self.data, self.block_size)
    }
    //pub fn iter(&'a self) -> Result<DirentsIterator<'a>, Error> {
    //    DirentsIterator::new(self.data, self.block_size)
    //}
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

impl Layout {
    fn is_compressed(&self) -> bool {
        match self {
            Layout::CompressedFull | Layout::CompressedCompact => true,
            _ => false,
        }
    }
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

    // returns a pair of slices, both of which could be empty in the extreme, that are the block
    // and tail packed data this inode references
    pub fn get_data(&self, inode: &Inode<'a>) -> Result<(&'a [u8], &'a [u8]), Error> {
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
                    if block_len != 0 {
                        return Err(Error::BlockLenShouldBeZero);
                    }
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

    pub fn get_map_header(&self, inode: &Inode<'a>) -> Result<&'a MapHeader, Error> {
        if !inode.layout().is_compressed() {
            return Err(Error::NotCompressed);
        }
        // for non compact, it is
        // full index_align is round_up(x, 8) + sizeof(MapHeader) + 8
        // it is Z_EROFS_FULL_INDEX_ALIGN(self.inode_offset() + inode_size + xattr_size)
        // //
        //  from there you can lookup a LogicalClusterIndex by logical cluster number (lcn) (I
        //  think)
        // for compact I think this is right
        let start = round_up_to::<8usize>(self.inode_end(inode) as usize);
        eprintln!(
            "start={} {:x} {:?}",
            start,
            start,
            &self.data[start..start + 128]
        );
        MapHeader::try_ref_from_prefix(self.data.get(start..).ok_or(Error::Oob)?)
            .map_err(|_| Error::BadConversion)
            .map(|(x, _)| x)
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

    //pub fn iter(&'a self) -> Result<ErofsIterator<'a>, Error> {
    //    ErofsIterator::new(self)
    //}
}

// todo was running into lifetime issues here
//enum ErofsIteratorState {
//    AtDir,
//    InDir,
//}
//
//pub struct ErofsIterator<'a> {
//    erofs: &'a Erofs<'a>,
//    path: PathBuf,
//    stack: Vec<(Inode<'a>, DirentsIterator<'a>)>,
//    state: ErofsIteratorState,
//}
//
//pub struct ErofsIteratorItem<'a> {
//    pub path: PathBuf,
//    pub inode: &'a Inode<'a>,
//}
//
//impl<'a> ErofsIterator<'a> {
//    fn new(erofs: &'a Erofs<'a>) -> Result<Self, Error> {
//        let root = erofs.get_root_inode()?;
//        let dirents = erofs.get_dirents(&root)?;
//        let iter = dirents.iter()?;
//        let stack = vec![(root, iter)];
//        let path = PathBuf::from("/");
//        let state = ErofsIteratorState::AtDir;
//
//        Ok(Self { erofs, path, stack, state })
//    }
//
//    fn next_impl(&'a mut self) -> Result<Option<ErofsIteratorItem<'a>>, Error> {
//        if let Some((inode, _iter)) = self.stack.last_mut() {
//            match self.state {
//                ErofsIteratorState::AtDir => {
//                    Ok(Some(ErofsIteratorItem{ path: self.path.clone(), inode }))
//                }
//                _ => { todo!(); }
//            }
//        } else {
//            Ok(None)
//        }
//    }
//}
//
//impl<'a, 'b> Iterator for ErofsIterator<'a> {
//    type Item = Result<ErofsIteratorItem<'b>, Error>;
//
//    fn next(&mut self) -> Option<Self::Item> {
//        self.next_impl().transpose()
//    }
//}

fn div_mod_u16(a: u16, b: u16) -> (u16, u16) {
    (a / b, a % b)
}

fn compute_block_tail_len(block_size: usize, size: usize) -> (usize, usize) {
    let num_blocks = size / block_size;
    let block_len = num_blocks * block_size;
    let tail_len = size - block_len;
    (block_len, tail_len)
}

fn round_up_to<const N: usize>(x: usize) -> usize {
    if x == 0 {
        return N;
    }
    ((x + (N - 1)) / N) * N
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
        assert_eq!(
            8,
            std::mem::size_of::<LogicalClusterIndex>(),
            "LogicalClusterIndex"
        );
        assert_eq!(
            14,
            std::mem::size_of::<Lz4CompressionConfig>(),
            "Lz4CompressionConfig"
        );
        assert_eq!(
            14,
            std::mem::size_of::<LzmaCompressionConfig>(),
            "LzmaCompressionConfig"
        );
        assert_eq!(
            6,
            std::mem::size_of::<DeflateCompressionConfig>(),
            "DeflateCompressionConfig"
        );
        assert_eq!(
            6,
            std::mem::size_of::<ZstdCompressionConfig>(),
            "ZstdCompressionConfig"
        );
    }

    #[test]
    fn test_compute_block_tail_len() {
        assert_eq!((4096, 0), compute_block_tail_len(4096, 4096));
        assert_eq!((4096, 1), compute_block_tail_len(4096, 4097));
        assert_eq!((0, 4095), compute_block_tail_len(4096, 4095));
    }
}
