use std::fmt;
use std::num::NonZero;

use rustix::fs::FileType;
use zerocopy::byteorder::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, TryFromBytes};

pub const EROFS_SUPER_OFFSET: usize = 1024;
pub const EROFS_SUPER_MAGIG_V1: u32 = 0xe0f5e1e2;
pub const INODE_ALIGNMENT: u64 = 32;
// if an inode has only tail data, its blkaddr gets set to -1
pub const EROFS_NULL_ADDR: u32 = u32::MAX;

// NOTES:
// - inode ino is a sequential number, but will not match the nid you look it up with; ie the
// root_nid from the superblock is something like 26, and you use that to compute the address of
// the root inode, but that inode will have field ino=1. So I'm not sure what a good name for the
// on-disk ino id should be. Currently calling it disk_id; it is not really an id because it is
// used in direct addressing calculation
//
// Data Storage
// - FlatInline storage stores whole blocks worth of data starting at raw_block_addr (number) and
// then the remainder immediately follows the inode. Inline (also called tail
// packing) storage cannot cross a block boundary, so the maximum tail length is really the block
// size minus inode size (32 or 64 + xattrs). And if you can't fit in the current block, then you
// have to skip to the start of the next block.
// - FlatPlain storage is like FlatInline but with no tail data. I was wondering why this exists
// and why not just have FlatInline, but if you are storing 8191 bytes for example, then if you
// always used FlatInline, you would store 1 block and 4095 bytes inline; whereas with FlatPlain
// you just store in 2 blocks
// - TODO compressed storage
// - CompressedFull
//   - https://lwn.net/Articles/851132/
//   - immediately following the inode and xattr goes a MapHeader followed by a number of
//   LogicalClusterIndex. The logical clusters are then grouped into a physical cluster, with the
//   first lcluster having a HEAD type. The NONHEAD lclusters store an offset in both directions
//   delta[2] to get to the HEAD of its pcluster and the next HEAD (of the next pcluster). A
//   pcluster can have only 1 lcluster and it will be HEAD type. The number of clusters in a
//   pcluster is stored in the first NONHEAD lcluster (second cluster of the pcluster), except for
//   pclusters with only 1 lcluster, which can be differentiated by looking at the next lcluster
//   and checking if it is also HEAD. TBD how to tell the number of lclusters in an inode?
//
// Directories
// - dirents are stored in blocks up to the block size. A single directory may span multiple blocks
// - dirents can be stored as either FlatInline or FlatPlain. If FlatInline and there is data
// stored in blocks, the dirent block will end before the tail data starts (since dirent blocks are
// max sized the block size).
// - Names are stored without null terminator, except possibly the last one in a block. (see next)
// - The final name in a dirent block must have a null terminator if it ends before the block
// because there is no other way to know when it ends. Otherwise the name's last byte is the last
// byte in the block.
// - dirent name_offset is relative to the start of the block/tail
// - dirents are sorted in ascending name order
// - all dirs must store the . and ..
// - in the root's dirents, the .. entry points to itself
//
// Xattrs
// - xattr data is immediately after an inode and before the tail data
// - if an inode has no xattrs, there is no xattr header
// - xattrs are laid out after the xattr header as
//   XattrHeader u32{header.shared_count} (XattryEntry name value)+ padding?
//               |---------------------------------------------------------|
//                                   len aligned 4, xattr_count = len / 4 + 1
//                                   len = (xattr_count - 1) * 4
// - there is a built in list of prefixes and then an additional dynamic table of sb.xattr_prefix_count
// - in an inode, the xattr_count is NOT the number of xattrs. it is the total size of all the
// xattrs (including the shared ids) laid out divided by 4 and then +1 (why the +1??).
// - not sure yet whether xattr data is allowed to span multiple blocks
// - TODO don't know what name_filter does (okay looks like part of a bloom filter)
// - FYI security.selinux xattr values have a null terminator
// - right now xattr keys are &[u8] when to the kernel they are null terminated strings so can't
// meaningfully contain a null byte inside
//
// CRC
// - the crc field is computed with crc32c castagnoli by taking the first block, slicing off the first
// 1024 bytes, set the superblock crc field to 0, then compute it (over the whole block, not just
// the superblock since extensions use data stored immediately after the superblock)
// - initial value is u32::MAX
//
// Unions:
// initially I used a rust union where the C code used unions, but zerocopy IntoBytes requres
// [build]
// rustflags = "--cfg zerocopy_derive_union_into_bytes"
// in .cargo/config.toml and I wasn't super jazzed about that. So I switched the one union we
// currently need which is InodeInfo to be a struct and manually implement union like
// operations on the internal buffer. The two others are BlockAddrOrDelta and
// FragmentOffsetOrDataSize and currently aren't used. Even InodeInfo currently only needs
// raw_blkaddr
//
// TODO
// - take a pass through field names and rename them (I guess retaining a comment to the original
// field name)

#[derive(thiserror::Error, Debug)]
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
    NotCompressedFull,
    InvalidXattrPrefix,
    BuiltinPrefixTooBig,
    XattrPrefixTableNotHandled,
    LayoutNotHandled(Layout),
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Immutable, KnownLayout, FromZeros, IntoBytes)]
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
    pub(crate) feature_incompat: U32,
    pub(crate) available_compr_algs_or_lz4_max_distance: U16,
    pub(crate) extra_devices: U16,
    pub(crate) devt_slotoff: U16,
    pub(crate) dirblkbits: u8,
    pub(crate) xattr_prefix_count: u8,
    pub(crate) xattr_prefix_start: U32,
    pub(crate) packed_nid: U64,
    pub(crate) xattr_filter_reserved: u8,
    _reserved2: [u8; 23],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout, IntoBytes)]
#[repr(C)]
pub struct InodeCompact {
    pub(crate) format_layout: U16,
    pub(crate) xattr_count: U16,
    pub(crate) mode: U16,
    pub(crate) nlink: U16,
    pub(crate) size: U32,
    _reserved: U32,
    pub(crate) info: InodeInfo,
    pub(crate) ino: U32,
    pub(crate) uid: U16,
    pub(crate) gid: U16,
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

#[derive(Debug, Immutable, FromZeros, IntoBytes)]
#[repr(C)]
pub struct InodeInfo {
    data: [u8; 4],
    // union
    //compressed_blocks: U32,
    //raw_blkaddr: U32, // block number not addr
    //rdev: U32,
    //chunk_info: ChunkInfo,
}

#[derive(Copy, Clone, Immutable, KnownLayout, FromZeros, IntoBytes)]
#[repr(C)]
pub struct ChunkInfo {
    format: U16,
    _reserved: U16,
}

#[derive(Debug, Immutable, KnownLayout, FromZeros, IntoBytes)]
#[repr(C)]
pub struct XattrHeader {
    name_filter: U32,
    shared_count: u8,
    _reserved: [u8; 7],
    // u32 shared_xattrs[]
}

#[derive(Debug, Immutable, KnownLayout, FromZeros, IntoBytes)]
#[repr(C)]
pub struct XattrEntry {
    pub(crate) name_len: u8,
    pub(crate) name_index: u8, // name_index is the prefix id (see XattrPrefix)
    pub(crate) value_size: U16,
    // u8 name[]
}

#[allow(dead_code)]
enum XattrBuiltinPrefix {
    User = 1,
    PosixAclAccess = 2,
    PosixAclDefault = 3,
    Trusted = 4,
    Lustre = 5, // I think this is unused
    Security = 6,
    #[allow(clippy::upper_case_acronyms)]
    MAX = 7,
}

const XATTR_BUILTIN_PREFIX_TABLE: [&[u8]; 6] = [
    b"user.",
    b"system.posix_acl_access",
    b"system.posix_acl_default",
    b"trusted",
    b"", // Lustre is unused I think
    b"security.",
];

#[derive(Debug, Immutable, KnownLayout, FromZeros, IntoBytes)]
#[repr(C)]
pub struct Dirent {
    pub(crate) disk_id: U64,
    pub(crate) name_offset: U16,
    pub(crate) file_type: u8,
    _reserved: u8,
}

#[derive(Debug, FromZeros, Immutable, KnownLayout)]
#[repr(C)]
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

#[derive(Immutable, KnownLayout, FromZeros)]
#[repr(C)]
pub struct LogicalClusterIndex {
    advise: U16, // I think this is just type
    cluster_offset: U16,
    block_addr_or_delta: BlockAddrOrDelta,
}

// TODO if/when these are needed, probably switch to a struct with union-like methods (as
// InodeInfo)
#[derive(Immutable, KnownLayout, FromZeros)]
#[repr(C)]
pub struct BlockAddrOrDelta {
    buf: [u8; 4],
}
impl BlockAddrOrDelta {
    fn block_addr(&self) -> U32 {
        self.buf.into()
    }

    fn delta(&self) -> [U16; 2] {
        let a = [self.buf[0], self.buf[1]];
        let b = [self.buf[2], self.buf[3]];
        [a.into(), b.into()]
    }
}

#[derive(FromZeros, Immutable)]
#[repr(C)]
union FragmentOffsetOrDataSize {
    fragment_offset: U32,
    data_size: MapDataSize,
}

#[derive(Debug, FromZeros, Immutable, KnownLayout, Copy, Clone)]
#[repr(C)]
struct MapDataSize {
    _reserved: U16,
    data_size: U16,
}

#[derive(Debug, PartialEq, Eq)]
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
#[repr(C)]
struct Lz4CompressionConfig {
    max_distance: U16,
    max_pcluster_blocks: U16,
    _reserved: [u8; 10],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
struct LzmaCompressionConfig {
    dict_size: U32,
    format: U16,
    _reserved: [u8; 8],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
struct DeflateCompressionConfig {
    window_bits: u8,
    _reserved: [u8; 5],
}

#[derive(Debug, TryFromBytes, Immutable, KnownLayout)]
#[repr(C)]
struct ZstdCompressionConfig {
    format: u8,
    window_log: u8,
    _reserved: [u8; 4],
}

impl fmt::Debug for BlockAddrOrDelta {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("BlockAddrOrDelta")
            .field("blockaddr", &self.block_addr())
            .field("delta", &self.delta())
            .finish()
    }
}

impl fmt::Debug for LogicalClusterIndex {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // TODO CBLKCNT
        use LogicalClusterType::*;
        let t = self.typ();
        let mut d = f.debug_struct("LogicalClusterIndex");
        d.field("type", &t)
            .field("advise[3..15]", &(self.advise >> 2))
            .field("advise[0..15]", &self.advise)
            .field("cluster_offset", &self.cluster_offset);
        match t {
            Head1 | Head2 => {
                d.field("blkaddr", &self.block_addr_or_delta.block_addr());
            }
            _ => {
                d.field("delta", &self.block_addr_or_delta.delta());
            }
        }
        d.finish()
    }
}

impl LogicalClusterIndex {
    // idk how much I like r#type vs typ vs kind
    pub fn typ(&self) -> LogicalClusterType {
        use LogicalClusterType::*;
        match u16::from(self.advise) & 0b11 {
            0 => Plain,
            1 => Head1,
            2 => NonHead,
            3 => Head2,
            _ => unreachable!(),
        }
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
    pub fn new_raw_blkaddr(block: u32) -> Self {
        Self {
            data: U32::new(block).to_bytes(),
        }
    }

    pub fn raw_blkaddr(&self) -> u32 {
        U32::from_bytes(self.data).into()
    }

    pub fn raw_compressed_blocks(&self) -> u32 {
        U32::from_bytes(self.data).into()
    }

    // TODO this needs to handle the other union fields
}

#[derive(Debug, Clone)]
pub enum Inode<'a> {
    Compact((u32, &'a InodeCompact)),
    Extended((u32, &'a InodeExtended)),
}

impl Inode<'_> {
    pub fn new(disk_id: u32, data: &[u8]) -> Result<Inode, Error> {
        // InodeCompact and InodeExtended have the first field of format_layout: U16
        let (format_layout, _) = U16::read_from_prefix(data).map_err(|_| Error::BadConversion)?;
        // validate that the layout is valid
        let _ = Inode::get_layout(format_layout)?;
        match Inode::get_format(format_layout) {
            0 => InodeCompact::try_ref_from_prefix(data)
                .map_err(|_| Error::BadConversion)
                .map(|(inode, _)| Inode::Compact((disk_id, inode))),
            1 => InodeExtended::try_ref_from_prefix(data)
                .map_err(|_| Error::BadConversion)
                .map(|(inode, _)| Inode::Extended((disk_id, inode))),
            _ => unreachable!(),
        }
    }

    fn get_format(format_layout: U16) -> u16 {
        (format_layout & 1).into()
    }

    fn get_layout(format_layout: U16) -> Result<Layout, Error> {
        let x: u16 = ((format_layout >> 1) & 0x07).into();
        x.try_into().map_err(|_| Error::BadConversion)
    }

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

    fn xattr_count(&self) -> u16 {
        match self {
            Inode::Compact((_, x)) => x.xattr_count.into(),
            Inode::Extended((_, x)) => x.xattr_count.into(),
        }
    }

    fn xattr_len(&self) -> Option<usize> {
        let len = xattr_count_to_len(self.xattr_count());
        if len == 0 {
            None
        } else {
            Some(len)
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
        let format_layout = match self {
            Inode::Compact((_, x)) => x.format_layout,
            Inode::Extended((_, x)) => x.format_layout,
        };
        Inode::get_layout(format_layout).expect("validated in Inode::new")
    }

    // TODO this is a pretty bad name since these are block numbers and not addrs
    pub fn raw_block_addr(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => x.info.raw_blkaddr(),
            Inode::Extended((_, x)) => x.info.raw_blkaddr(),
        }
    }

    pub fn raw_compressed_blocks(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => x.info.raw_compressed_blocks(),
            Inode::Extended((_, x)) => x.info.raw_compressed_blocks(),
        }
    }

    pub fn block_addr(&self) -> Result<u64, Error> {
        match self.file_type() {
            FileType::RegularFile | FileType::Directory | FileType::Symlink => {
                Ok(self.raw_block_addr().into())
            }
            _ => Err(Error::NotRegDirLink),
        }
    }

    pub fn link_count(&self) -> u32 {
        match self {
            Inode::Compact((_, x)) => x.nlink.into(),
            Inode::Extended((_, x)) => x.nlink.into(),
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
        debug_assert!(!self.data.is_empty());
        let (dirent, _) =
            Dirent::try_ref_from_prefix(self.data).map_err(|_| Error::BadConversion)?;
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

        // addition cannot overflow because count < u16::MAX
        let name = if (self.i as u32) + 1 < (self.count as u32) {
            let next_dirent = self.get((self.i + 1).into())?;
            let next_offset: usize = next_dirent.name_offset.into();
            let name_len = next_offset - name_offset;
            self.i += 1;
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
pub enum XattrPrefix {
    Builtin(NonZero<u8>), // nonzero && (bit 7 was clear)
    Table(u8),            // possibly zero (but max 127) (bit 7 was set)
}

impl XattrPrefix {
    fn try_from(x: u8) -> Result<Option<XattrPrefix>, Error> {
        if x == 0 {
            Ok(None)
        } else if x & 0x80 != 0 {
            Ok(Some(XattrPrefix::Table(x & 0x7f)))
        } else if x < XattrBuiltinPrefix::MAX as u8 {
            // x != 0 as checked above
            Ok(Some(XattrPrefix::Builtin(NonZero::new(x).unwrap())))
        } else {
            Err(Error::InvalidXattrPrefix)
        }
    }
}

pub struct Xattrs<'a> {
    header: &'a XattrHeader,
    data: &'a [u8],
    shared_data: &'a [u8],
}

impl<'a> Xattrs<'a> {
    pub fn iter(&self) -> XattrsIterator<'a> {
        XattrsIterator {
            data: self.data,
            shared_data: self.shared_data,
            shared_remaining: self.header.shared_count,
        }
    }
}

#[derive(Debug)]
pub struct XattrItem<'a> {
    prefix: Option<XattrPrefix>,
    pub name: &'a [u8], // TODO maybe rename this key, though erofs calls it name
    pub value: &'a [u8],
}

pub struct XattrsIterator<'a> {
    data: &'a [u8],
    shared_data: &'a [u8],
    shared_remaining: u8,
}

impl<'a> XattrsIterator<'a> {
    // these two are slightly different, unshared needs to advance the offset aligned, idk which I
    // prefer and whether to unify

    fn next_shared(&mut self) -> Result<XattrItem<'a>, Error> {
        debug_assert!(self.shared_remaining > 0);
        self.shared_remaining -= 1;
        let (index, data) =
            U32::try_read_from_prefix(self.data).map_err(|_| Error::BadConversion)?;
        self.data = data;

        let offset = (index.get() as usize) * 4;
        let (entry, sdata) =
            XattrEntry::try_ref_from_prefix(self.shared_data.get(offset..).ok_or(Error::Oob)?)
                .map_err(|_| Error::BadConversion)?;
        let name_len = usize::from(entry.name_len);
        let value_len = usize::from(entry.value_size);
        let (name, sdata) = sdata.split_at_checked(name_len).ok_or(Error::Oob)?;
        let (value, _) = sdata.split_at_checked(value_len).ok_or(Error::Oob)?;
        let prefix = XattrPrefix::try_from(entry.name_index)?;
        Ok(XattrItem {
            prefix,
            name,
            value,
        })
    }

    fn next_unshared(&mut self) -> Result<XattrItem<'a>, Error> {
        let (entry, data) =
            XattrEntry::try_ref_from_prefix(self.data).map_err(|_| Error::BadConversion)?;
        let name_len = usize::from(entry.name_len);
        let value_len = usize::from(entry.value_size);

        let name = data.get(..name_len).ok_or(Error::Oob)?;
        let mut offset = name_len;
        let value = data.get(offset..offset + value_len).ok_or(Error::Oob)?;

        offset += value_len;
        offset = round_up_to::<{ std::mem::size_of::<XattrEntry>() }>(offset);

        self.data = data.get(offset..).ok_or(Error::Oob)?;
        if self.data.len() < std::mem::size_of::<XattrEntry>() {
            self.data = &[];
        }

        // NOTE: we try to convert the prefix here so that we have already advanced self.data,
        // otherwise on error, we leave it for an infinite loop
        let prefix = XattrPrefix::try_from(entry.name_index)?;
        Ok(XattrItem {
            prefix,
            name,
            value,
        })
    }
}

impl<'a> Iterator for XattrsIterator<'a> {
    type Item = Result<XattrItem<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.shared_remaining > 0 {
            Some(self.next_shared())
        } else if !self.data.is_empty() {
            Some(self.next_unshared())
        } else {
            None
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
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
        matches!(self, Layout::CompressedFull | Layout::CompressedCompact)
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
        let (sb, _) =
            Superblock::try_ref_from_prefix(data.get(EROFS_SUPER_OFFSET..).ok_or(Error::Oob)?)
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
        let xattr_size = inode.xattr_len().unwrap_or(0) as u64;
        start + inode_size as u64 + xattr_size
    }

    pub fn compute_checksum(&self) -> Result<u32, Error> {
        Ok(crc32c(
            u32::from(self.sb.magic)
                .to_le_bytes()
                .iter()
                .chain(0u32.to_le_bytes().iter())
                .chain(
                    self.data
                        .get(EROFS_SUPER_OFFSET + 8..self.block_size() as usize)
                        .ok_or(Error::Oob)?,
                ),
        ))
    }

    pub fn check_checksum(&self) -> Result<bool, Error> {
        Ok(self.compute_checksum()? == self.sb.checksum.into())
    }

    pub fn get_inode(&self, disk_id: u32) -> Result<Inode<'a>, Error> {
        let offset = self.raw_inode_offset(disk_id) as usize;
        Inode::new(disk_id, self.data.get(offset..).ok_or(Error::Oob)?)
    }

    pub fn get_inode_from_dirent(&self, dirent: &DirentItem<'a>) -> Result<Inode<'a>, Error> {
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
                    let data_begin = self.inode_end(inode) as usize;
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
                if data_len == 0 {
                    return Ok(([].as_ref(), [].as_ref()));
                }
                let data_begin = self.block_offset(inode.raw_block_addr()) as usize;
                self.data
                    .get(data_begin..data_begin + data_len)
                    .ok_or(Error::Oob)
                    .map(|x| (x, [].as_ref()))
            }
            layout => Err(Error::LayoutNotHandled(layout)),
        }
    }

    pub fn get_dirents(&self, inode: &Inode<'a>) -> Result<Dirents<'a>, Error> {
        if inode.file_type() != FileType::Directory {
            return Err(Error::NotDir);
        }
        let data = self.get_data(inode)?;
        Dirents::new(data, self.block_size() as usize)
    }

    fn xattr_shared_data(&self) -> Result<&'a [u8], Error> {
        // unfortunately we don't know an upper length bound ahead of time so we just have to slice
        // open ended
        let offset = self.block_offset(self.sb.xattr_blkaddr.into()) as usize;
        self.data.get(offset..).ok_or(Error::Oob)
    }

    pub fn get_xattrs(&self, inode: &Inode<'a>) -> Result<Option<Xattrs<'a>>, Error> {
        if let Some(size) = inode.xattr_len() {
            let offset = (self.inode_offset(inode) + inode.size() as u64) as usize;
            let data = self.data.get(offset..offset + size).ok_or(Error::Oob)?;
            let shared_data = self.xattr_shared_data()?;
            let (header, data) =
                XattrHeader::try_ref_from_prefix(data).map_err(|_| Error::BadConversion)?;
            Ok(Some(Xattrs {
                header,
                data,
                shared_data,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn get_xattr_prefix(&self, item: &XattrItem<'a>) -> Result<&'a [u8], Error> {
        match item.prefix {
            None => Ok(&[]),
            Some(XattrPrefix::Builtin(i)) => {
                XATTR_BUILTIN_PREFIX_TABLE
                    // will not underflow since i NonZero
                    .get((i.get() - 1) as usize)
                    // this is checked during construction so shouldn't happen
                    .ok_or(Error::BuiltinPrefixTooBig)
                    .copied()
            }
            _ => Err(Error::XattrPrefixTableNotHandled),
        }
    }

    pub fn get_map_header(&self, inode: &Inode<'a>) -> Result<&'a MapHeader, Error> {
        if !inode.layout().is_compressed() {
            return Err(Error::NotCompressed);
        }
        let start = round_up_to::<8usize>(self.inode_end(inode) as usize);
        MapHeader::try_ref_from_prefix(self.data.get(start..).ok_or(Error::Oob)?)
            .map_err(|_| Error::BadConversion)
            .map(|(x, _)| x)
    }

    pub fn get_logical_cluster_index(
        &self,
        inode: &Inode<'a>,
        i: usize,
    ) -> Result<&'a LogicalClusterIndex, Error> {
        if inode.layout() != Layout::CompressedFull {
            return Err(Error::NotCompressedFull);
        }
        // TODO bounds check i
        // TBD why there is a +8 here
        let start = round_up_to::<8usize>(self.inode_end(inode) as usize)
            + std::mem::size_of::<MapHeader>()
            + 8;
        let offset = i * std::mem::size_of::<LogicalClusterIndex>();
        LogicalClusterIndex::try_ref_from_prefix(self.data.get(start + offset..).ok_or(Error::Oob)?)
            .map_err(|_| Error::BadConversion)
            .map(|(x, _)| x)
    }

    pub fn get_symlink(&self, inode: &Inode<'a>) -> Result<&'a [u8], Error> {
        if inode.file_type() != FileType::Symlink {
            return Err(Error::NotSymlink);
        }
        let (block, tail) = self.get_data(inode)?;
        // TODO I don't know if this is always right
        if !block.is_empty() {
            return Err(Error::NotExpectingBlockData);
        }
        Ok(tail)
    }

    #[cfg(debug_assertions)]
    pub fn inspect(&self, inode: &Inode<'a>, after: usize) -> Result<(), Error> {
        fn p(xs: &[u8]) -> () {
            for (i, byte) in xs.iter().enumerate() {
                if i > 0 && i % 4 == 0 {
                    print!(" ");
                }
                print!("{byte:02x}")
            }
        }

        let start = self.inode_offset(inode) as usize;
        let size = inode.size();
        let xattr_start = start + size;
        let xattr_len = inode.xattr_len();

        println!("inode: {start:x}-{:x}:", start + size);
        p(self.data.get(start..start + size).ok_or(Error::Oob)?);
        println!();
        if let Some(xattr_len) = xattr_len {
            println!("xattr: {xattr_start:x}-{:x}:", xattr_start + xattr_len);
            p(self
                .data
                .get(xattr_start..xattr_start + xattr_len)
                .ok_or(Error::Oob)?);
            println!();
        }

        let after_start = if inode.layout().is_compressed() {
            let start = round_up_to::<8usize>(self.inode_end(inode) as usize);
            let len = std::mem::size_of::<MapHeader>();
            println!("map_header: {start:x}-{:x}:", start + len);
            p(self.data.get(start..start + len).ok_or(Error::Oob)?);
            println!();
            start + len
        } else {
            xattr_start + xattr_len.unwrap_or(0)
        };

        if after > 0 {
            println!("after: {after_start:x}-{:x}:", after_start + after);
            p(self
                .data
                .get(after_start..after_start + after)
                .ok_or(Error::Oob)?);
            println!();
        }

        Ok(())
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

// wish this could work
//fn round_up_for<T>(x: usize) -> usize {
//    round_up_to<{std::mem::size_of::<T>}>(x)
//}

pub fn round_up_to<const N: usize>(x: usize) -> usize {
    if x == 0 {
        return N;
    }
    x.div_ceil(N) * N
}

#[derive(Default)]
pub struct XattrCountAndPadding {
    pub xattr_count: usize,
    pub padding: usize,
}

// compute the xattr_count field for an inode given the sequence of key,value lengths
// note that this doesn't include the size of XattrHeader as that is implicitly included if
// count != 0
// entries should already have their prefixes accounted for in name_len
// returns the xattr_count field and the padding required
pub fn xattr_count<'a>(x: impl Iterator<Item = &'a XattrEntry>) -> XattrCountAndPadding {
    let len = x
        .map(|entry| {
            usize::from(entry.name_len)
                + usize::from(entry.value_size)
                + std::mem::size_of::<XattrEntry>()
        })
        .sum::<usize>();
    // len can only be zero if count was zero since we add sizeof(XattrEntry)
    if len == 0 {
        XattrCountAndPadding::default()
    } else {
        let padded = round_up_to::<{ std::mem::size_of::<XattrEntry>() }>(len);
        let padding = padded - len; // cannot underflow
        let xattr_count = padded / 4 + 1;
        XattrCountAndPadding {
            xattr_count,
            padding,
        }
    }
}

pub fn xattr_count_to_len(count: u16) -> usize {
    if count == 0 {
        0
    } else {
        std::mem::size_of::<XattrHeader>()
            + (count as usize - 1) * std::mem::size_of::<XattrEntry>()
    }
}

pub struct XattrBuiltinPrefixWithLen {
    pub id: u8,
    pub len: u8,
}

pub fn xattr_builtin_prefix(key: &[u8]) -> Option<XattrBuiltinPrefixWithLen> {
    XATTR_BUILTIN_PREFIX_TABLE
        .iter()
        .enumerate()
        .find_map(|(i, prefix)| {
            if key.starts_with(prefix) {
                // will not overflow because table is small
                // prefix.len() is u8 because table is static and they are short
                Some(XattrBuiltinPrefixWithLen {
                    id: (i + 1) as u8,
                    len: prefix.len() as u8,
                })
            } else {
                None
            }
        })
}

// This is a translated version of what appears in erofs-utils
// I didn't think a specialized or tabled algo was necessary since we only ever compute up to a
// single page
fn crc32c<'a>(data: impl IntoIterator<Item = &'a u8>) -> u32 {
    let poly = 0x82F63B78;
    let mut crc = u32::MAX;
    for x in data {
        crc ^= *x as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (if crc & 1 == 0 { 0 } else { poly });
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use memmap2::MmapOptions;
    use rustix::fs::XattrFlags;
    use tempfile::{tempdir, NamedTempFile};

    #[test]
    fn test_sizeof() {
        assert_eq!(128, std::mem::size_of::<Superblock>(), "Superblock");
        assert_eq!(64, std::mem::size_of::<InodeExtended>(), "InodeExtended");
        assert_eq!(32, std::mem::size_of::<InodeCompact>(), "InodeCompact");
        assert_eq!(12, std::mem::size_of::<Dirent>(), "Dirent");
        assert_eq!(12, std::mem::size_of::<XattrHeader>(), "XattrHeader");
        assert_eq!(4, std::mem::size_of::<XattrEntry>(), "XattrEntry");
        assert_eq!(8, std::mem::size_of::<MapHeader>(), "MapHeader");
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

    #[test]
    fn test_round_up_to() {
        assert_eq!(128, round_up_to::<128>(0));
        assert_eq!(128, round_up_to::<128>(127));
        assert_eq!(128, round_up_to::<128>(128));
        assert_eq!(256, round_up_to::<128>(129));
    }

    fn set_xattr(p: impl rustix::path::Arg, k: &str, v: impl AsRef<[u8]>) {
        rustix::fs::setxattr(p, k, v.as_ref(), XattrFlags::CREATE).unwrap();
    }

    fn get_xattr(p: impl rustix::path::Arg, k: &str) -> Option<Box<[u8]>> {
        let mut buf = vec![0; 128];
        match rustix::fs::getxattr(p, k, &mut buf) {
            Ok(size) => {
                buf.resize(size, 0);
                Some(buf.into())
            }
            Err(e) if e == rustix::io::Errno::NODATA => None,
            e => {
                panic!("get_xattr failed {:?}", e);
            }
        }
    }

    #[test]
    fn test_with_mkfs() {
        let dir = tempdir().unwrap();
        let dest = NamedTempFile::new().unwrap();

        let pa = dir.path().join("a");
        let pb = dir.path().join("b");
        let pc = dir.path().join("c");
        let pd = dir.path().join("d");

        let data_b = vec![0; 4097];
        let data_c = vec![0; 4096];

        fs::write(&pa, b"hello world").unwrap();
        fs::write(&pb, &data_b).unwrap();
        fs::write(&pc, &data_c).unwrap();
        symlink(&pa, &pd).unwrap();

        for p in [&pa, &pb, &pc] {
            set_xattr(p, "user.shared", "value-shared");
        }

        set_xattr(&pa, "user.attr", "unique-a");
        set_xattr(&pb, "user.attr", "unique-b");
        set_xattr(&pc, "user.attr", "unique-c");

        let success = Command::new("mkfs.erofs")
            .arg(dest.path())
            .arg(dir.path())
            .arg("-b4096") // block size
            .arg("-x2") // this means that if more than 2 files have the same xattr, it goes into the
            // shared xattr table
            .status()
            .unwrap()
            .success();
        assert!(success);

        // on test systems with selinux, all these tempfiles will have gotten labeled with
        // security.selinux and thus they all go into the shared xattr table
        let expected_shared_count = if get_xattr(&pa, "security.selinux").is_some() {
            2 // security.selinux, user.shared
        } else {
            1 // user.shared
        };

        let mmap = unsafe { MmapOptions::new().map(&dest).unwrap() };
        let erofs = Erofs::new(&mmap).unwrap();

        assert_eq!(erofs.block_size(), 4096);
        assert!(erofs.check_checksum().unwrap());

        let root = erofs.get_root_inode().unwrap();
        let dirents = erofs.get_dirents(&root).unwrap();

        fn xattr_map(erofs: &Erofs, xattrs: &Xattrs) -> BTreeMap<String, Box<[u8]>> {
            xattrs
                .iter()
                .map(|item| {
                    let item = item.unwrap();
                    let key = String::from_utf8(
                        [erofs.get_xattr_prefix(&item).unwrap(), item.name].concat(),
                    )
                    .unwrap();
                    (key, item.value.into())
                })
                .collect()
        }

        fn inode_data(erofs: &Erofs, inode: &Inode) -> Box<[u8]> {
            let (head, tail) = erofs.get_data(inode).unwrap();
            [head, tail].concat().into()
        }

        for item in dirents.iter().unwrap() {
            let item = item.unwrap();
            let inode = erofs.get_inode_from_dirent(&item).unwrap();
            // why isn't there String::from_utf8_slice?
            match String::from_utf8(item.name.into()).unwrap().as_str() {
                "." => {
                    assert_eq!(item.file_type, DirentFileType::Directory);
                }
                ".." => {}
                "a" => {
                    assert_eq!(item.file_type, DirentFileType::RegularFile);
                    assert_eq!(inode.file_type(), FileType::RegularFile);
                    let xattrs = erofs.get_xattrs(&inode).unwrap().unwrap();
                    assert_eq!(xattrs.header.shared_count, expected_shared_count);
                    let map = xattr_map(&erofs, &xattrs);
                    assert_eq!(map["user.shared"].as_ref(), b"value-shared");
                    assert_eq!(map["user.attr"].as_ref(), b"unique-a");
                    assert_eq!(inode.data_size() as usize, b"hello world".len());
                    assert_eq!(inode_data(&erofs, &inode).as_ref(), b"hello world");
                }
                "b" => {
                    assert_eq!(item.file_type, DirentFileType::RegularFile);
                    assert_eq!(inode.file_type(), FileType::RegularFile);
                    let xattrs = erofs.get_xattrs(&inode).unwrap().unwrap();
                    assert_eq!(xattrs.header.shared_count, expected_shared_count);
                    let map = xattr_map(&erofs, &xattrs);
                    assert_eq!(map["user.shared"].as_ref(), b"value-shared");
                    assert_eq!(map["user.attr"].as_ref(), b"unique-b");
                    assert_eq!(inode.data_size() as usize, data_b.len());
                    assert_eq!(inode_data(&erofs, &inode).as_ref(), data_b);
                }
                "c" => {
                    assert_eq!(item.file_type, DirentFileType::RegularFile);
                    assert_eq!(inode.file_type(), FileType::RegularFile);
                    let xattrs = erofs.get_xattrs(&inode).unwrap().unwrap();
                    assert_eq!(xattrs.header.shared_count, expected_shared_count);
                    let map = xattr_map(&erofs, &xattrs);
                    assert_eq!(map["user.shared"].as_ref(), b"value-shared");
                    assert_eq!(map["user.attr"].as_ref(), b"unique-c");
                    assert_eq!(inode.data_size() as usize, data_c.len());
                    assert_eq!(inode_data(&erofs, &inode).as_ref(), data_c);
                }
                "d" => {
                    assert_eq!(item.file_type, DirentFileType::Symlink);
                    assert_eq!(inode.file_type(), FileType::Symlink);
                    // the symlink does get the absolute path...
                    assert_eq!(
                        pa.as_os_str().as_encoded_bytes(),
                        erofs.get_symlink(&inode).unwrap()
                    );
                }
                name => {
                    assert!(false, "got unexpected file name {}", name);
                }
            }
        }
    }
}
