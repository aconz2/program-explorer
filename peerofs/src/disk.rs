use std::fmt;
#[allow(unused)]
use std::io::Write;
use std::num::NonZero;
use std::path::Path;

#[allow(unused)]
use log::trace;
use rustix::fs::FileType;
use zerocopy::byteorder::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, TryFromBytes};

use crate::decompressor;

pub const EROFS_SUPER_OFFSET: usize = 1024;
pub const EROFS_SUPER_MAGIG_V1: u32 = 0xe0f5e1e2;
pub const INODE_ALIGNMENT: u64 = 32;
// if an inode has only tail data, its blkaddr gets set to -1
pub const EROFS_NULL_ADDR: u32 = u32::MAX;

// NOTES:
// Blocks
// - superblock (SB): starts at byte 1024. Immediately after can be compression config structs and
// information about secondary devices used for storage
// - blocks: much of the layout is determined by a block (B), defaulting to 4096 but configurable in
// the SB blkszbits. Importantly, the SB field meta_blkaddr specifies the starting block number
// where the Inodes will be stored. A block address is found by multiplying the block number by
// the block size.
//
// Inodes
// - Inodes come in two sizes: 32 and 64 bytes InodeCompact and InodeExtended
// - An Inode address is computed as 32*X + meta_block*B. This holds for both InodeCompact and
// InodeExtended, even though the latter is 64 bytes. X here is a disk_id as found in the SB
// root_disk_id or a Dirent disk_id.
// - An Inode also has a field ino which is a sequential number, but you cannot find the Inode on disk
// given an ino!
//
// Data Storage
// - FlatInline storage stores whole blocks worth of data starting at raw_block_addr (number) and
// then the remainder immediately follows the inode. Inline (also called tail
// packing) storage cannot cross a block boundary, so the maximum tail length is really the block
// size minus inode size (32 or 64 + xattrs). And if you can't fit in the current block, then you
// have to skip to the start of the next block.
// - FlatPlain storage is like FlatInline but with no tail data. I was wondering why this exists
// and why not just have FlatInline, but if you are storing 8191 bytes for example, then if you
// always used FlatInline, you would store 1 block and 4095 bytes inline (which is impossible since
// tail cannot cross block boundary); whereas with FlatPlain you just store in 2 blocks
// - CompressedFull
//   - https://lwn.net/Articles/851132/
//   - TODO in the following I use the term pcluster but not sure this is actually correct
//   - immediately following the inode and xattr goes a MapHeader followed by a number of
//   LogicalClusterIndex (LCI/lcluster). For a file of size S there are ceil(S/B) LCI's where B is the
//   compression block size, which can be larger than the superblock block size as specified in the
//   MapHeader clusterbits.
//   - Each LCI represents a span of bytes at a logical address (LA) which starts at
//   i*B + cluster_offset for the i'th LCI and goes to the next LCI LA or EOF
//   - Multiple LCIs are grouped into a physical cluster (pcluster)  with the
//   first lcluster having a Head type, followed by 0 or more Nonhead lclusters.
//   - The Head LCI stores the block address of where the compressed bytes live for that pcluster
//   - The Nonhead LCI store an offset to their Head LCI in delta[0] and the next pcluster (Head or
//   Plain) in delta[1]. These are used during random reads to find the block needed to decompress
//   - Plain type LCI are uncompressed data of length up to B
//   - Head1 and Head2 are the two variants of Head LCI which allow multiple compression methods to
//   be used for the same file which is stored in the MapHeader algorithmtype
//
// Directories
// - Dirent's are stored as above, either as FlatPlain or FlatInline, and in descending sorted order
// - Dirent's are grouped together so that each group can fit in one block. The group layout is
// like {Dirent[], u8[][]}. The u8 data there are the names for those dirents. The names are
// referenced by the Dirent name_offset field which is relative to the start of the block.
// - The length of the name is found by looking at the next Dirent name_offset, except for the last
// Dirent in a group, whose name will either go to the end of the block, or be null terminated if
// it ends early.
// - The number of Dirent's in a group can be determined by looking at the first Dirent
// name_offset, which should be a multiple of size_of::<Dirent>=12, and dividing by 12
// - Dirent's are stored unaligned since they have size 12
// - all dirs must store the . and .., pointing to themselves and parent directory respectively
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
// in .cargo/config.toml and I wasn't super jazzed about that. So instead I switched the unions to
// structs with a buffer and functions to extract the fields out
//
//
// TODO
// - take a pass through field names and rename them (I guess retaining a comment to the original
// field name)
// - Head2 algorithm support
// - zstd, deflate compression

#[derive(thiserror::Error, Debug, PartialEq)]
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
    Decompress,
    LciMalformed,
    Write,
    Underflow,
    UnknownCompression,
    Head2NotSupported,
    CompressionNotSupported(CompressionType),
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
    // NOTE: its actually the super block blocksize bits, not 12
    // bit 0-2: logical cluster bits - 12 (0 for 4096)
    // if bit 7 is set, then this whole 8 byte struct is interpreted as le64 with the high bit
    // cleared as the fragment offset
    cluster_bits: u8,
}

#[derive(Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct LogicalClusterIndex {
    advise: U16, // I think this is just type
    cluster_offset: U16,
    block_addr_or_delta: BlockAddrOrDelta,
}

#[derive(Immutable, KnownLayout, FromBytes)]
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

#[derive(Debug, PartialEq)]
pub enum CompressionType {
    Lz4 = 0,
    Lzma = 1,
    Deflate = 2,
    Zstd = 3,
}

impl TryFrom<u8> for CompressionType {
    type Error = Error;
    fn try_from(x: u8) -> Result<Self, Error> {
        use CompressionType::*;
        match x {
            0 => Ok(Lz4),
            1 => Ok(Lzma),
            2 => Ok(Deflate),
            3 => Ok(Zstd),
            _ => Err(Error::UnknownCompression),
        }
    }
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
            Head1 | Head2 | Plain => {
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

    pub fn is_head(&self) -> bool {
        matches!(
            self.typ(),
            LogicalClusterType::Head1 | LogicalClusterType::Head2
        )
    }

    pub fn cluster_offset(&self) -> usize {
        u16::from(self.cluster_offset) as usize
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

impl MapHeader {
    pub fn compression_type_1(&self) -> Result<CompressionType, Error> {
        (self.algorithm & 0b1111).try_into()
    }
    pub fn compression_type_2(&self) -> Result<CompressionType, Error> {
        ((self.algorithm >> 4) & 0b1111).try_into()
    }
    pub fn cluster_size_bits(&self) -> u8 {
        // bits 0-2
        self.cluster_bits & 0b111
    }
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

    pub fn get_logical_cluster_indices(
        &self,
        inode: &Inode<'a>,
    ) -> Result<&'a [LogicalClusterIndex], Error> {
        if inode.layout() != Layout::CompressedFull {
            return Err(Error::NotCompressedFull);
        }
        // NOTE the raw_compressed_blocks count is the number of physical clusters I think,
        // the number of LCI's is just the number of blocks necessary to cover the whole file size
        // TODO whether this is in superblock block size blocks or the map header block size
        let n = inode.data_size().div_ceil(self.block_size()) as usize;
        // TBD why there is a +8 here (it might be so that looking up the -1 LCI is valid?)
        let start = round_up_to::<8usize>(self.inode_end(inode) as usize)
            + std::mem::size_of::<MapHeader>()
            + 8;
        let data = self.data.get(start..).ok_or(Error::Oob)?;
        <[LogicalClusterIndex]>::ref_from_prefix_with_elems(data, n)
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

    pub fn get_compressed_data_vec(&self, inode: &Inode<'a>) -> Result<Vec<u8>, Error> {
        let mut buf = vec![];
        self.get_compressed_data(inode, &mut buf)?;
        Ok(buf)
    }

    pub fn get_decompressor(
        &self,
        compression_type: CompressionType,
    ) -> Result<Box<dyn decompressor::Decompressor>, Error> {
        match compression_type {
            #[cfg(feature = "lz4")]
            CompressionType::Lz4 => Ok(Box::new(decompressor::Lz4Decompressor)),
            t => Err(Error::CompressionNotSupported(t)),
        }
    }

    pub fn get_compressed_data<W>(&self, inode: &Inode<'a>, writer: &mut W) -> Result<(), Error>
    where
        W: Write,
    {
        let map_header = self.get_map_header(inode)?;

        // TODO handle head_2
        let compression_type_1 = map_header.compression_type_1()?;
        let decompressor_1 = self.get_decompressor(compression_type_1)?;
        let block_len = 1usize << (self.sb.blkszbits + map_header.cluster_size_bits());
        let file_size = inode.data_size() as usize;

        // lookup next head
        // length of pcluster is difference in logical address of the two heads
        // LA of an LCI = LCI_index * block_len + LCI_cluster_offset
        // with rearranging, you get (j-i)*block_len + next_cluster_offset + cur_cluster_offset
        fn pcluster_len(
            lcis: &[LogicalClusterIndex],
            i: usize,
            block_len: usize,
            file_size: usize,
        ) -> Result<(Option<usize>, usize), Error> {
            debug_assert!(lcis[i].is_head());
            // should always be ok b/c there is always a Plain LCI at the end (TODO not sure about
            // that)
            let cur = lcis.get(i).ok_or(Error::Oob)?;
            let next = lcis.get(i + 1).ok_or(Error::Oob)?;
            trace!("{}: {:?}", i, cur);
            trace!("{}: {:?}", i + 1, next);
            let (j, next_head) = match next.typ() {
                LogicalClusterType::Head1
                | LogicalClusterType::Head2
                | LogicalClusterType::Plain => (i + 1, next),
                LogicalClusterType::NonHead => {
                    let n = u16::from(next.block_addr_or_delta.delta()[1]) as usize;
                    trace!("trying to get {}/{}", i + 1 + n, lcis.len());
                    let Some(next_head) = lcis.get(i + 1 + n) else {
                        // it can be that there is no next head
                        if i + 1 + n == lcis.len() {
                            // TODO check sub
                            let len = file_size
                                .checked_sub(i * block_len + cur.cluster_offset())
                                .ok_or(Error::Underflow)?;
                            return Ok((None, len));
                        } else {
                            // true oob
                            return Err(Error::Oob);
                        }
                    };
                    trace!("{}: {:?}", i + 1 + n, next_head);
                    debug_assert!(next_head.typ() != LogicalClusterType::NonHead);
                    (i + 1 + n, next_head)
                }
            };
            // j > i
            let len = (j - i) * block_len + next_head.cluster_offset() - cur.cluster_offset();
            Ok((Some(j), len))
        }

        let lcis = self.get_logical_cluster_indices(inode)?;

        // not sure this is possible (and if so whether malformed or not)
        if lcis.is_empty() {
            return Ok(());
        }

        let mut total: usize = 0;
        let mut buf = vec![];

        let mut i_ = Some(0);

        // loop terminates either when we reach the last LCI that is Plain with 0 blkaddr
        // or pcluster_len can return None
        while let Some(i) = i_ {
            let cur = &lcis.get(i).ok_or(Error::Oob)?;
            match cur.typ() {
                // TODO different
                LogicalClusterType::Head1 => {
                    let block_addr: u32 = cur.block_addr_or_delta.block_addr().into();
                    let data_begin = self.block_offset(block_addr) as usize;
                    let data = self
                        .data
                        .get(data_begin..data_begin + block_len)
                        .ok_or(Error::Oob)?;
                    let (next_i, decompress_len) = pcluster_len(lcis, i, block_len, file_size)?;
                    trace!("lci {i} decompress_len={decompress_len} pa={data_begin}");

                    if buf.len() < decompress_len {
                        buf.resize(decompress_len, 0);
                    }
                    let decompressed_len =
                        // This highly depends on decompress_partial for slightly unknown reasons
                        decompressor_1.decompress(data, &mut buf, decompress_len)
                            .ok_or(Error::Decompress)?;
                    debug_assert!(decompressed_len == decompress_len);
                    writer
                        .write_all(&buf[..decompressed_len])
                        .map_err(|_| Error::Write)?;
                    total += decompressed_len;
                    trace!("written {total}");

                    i_ = next_i;
                }
                LogicalClusterType::Plain => {
                    trace!("{}: {:?}", i, cur);
                    let block_addr: u32 = cur.block_addr_or_delta.block_addr().into();
                    if block_addr == 0 {
                        if i + 1 == lcis.len() {
                            // this LCI is the last entry and is expected
                            break;
                        } else {
                            return Err(Error::LciMalformed);
                        }
                    }
                    // TODO can't there be a Plain at the end with partial data and we have to
                    // instead use the file size instead of the next LCI?
                    let next = lcis.get(i + 1).ok_or(Error::Oob)?;
                    trace!("{}: {:?}", i + 1, next);
                    let data_begin = self.block_offset(block_addr) as usize;
                    let data_len = block_len + next.cluster_offset() - cur.cluster_offset();
                    trace!("copying {data_len}");
                    let data = self
                        .data
                        .get(data_begin..data_begin + data_len)
                        .ok_or(Error::Oob)?;
                    writer.write_all(data).map_err(|_| Error::Write)?;
                    total += data_len;
                    trace!("written {total}");
                    i_ = Some(i + 1);
                }
                LogicalClusterType::Head2 => {
                    return Err(Error::Head2NotSupported);
                }
                LogicalClusterType::NonHead => {
                    return Err(Error::LciMalformed);
                }
            }
        }
        Ok(())
    }

    // TODO uses linear search
    pub fn lookup(&self, p: impl AsRef<Path>) -> Result<Option<Inode>, Error> {
        let mut cur = self.get_root_inode()?;
        'outer: for component in p.as_ref() {
            let dirents = self.get_dirents(&cur)?;
            for item in dirents.iter()? {
                let item = item?;
                if item.name == component.as_encoded_bytes() {
                    cur = self.get_inode_from_dirent(&item)?;
                    continue 'outer;
                }
            }
            return Ok(None);
        }
        Ok(Some(cur))
    }

    #[cfg(debug_assertions)]
    pub fn inspect(&self, inode: &Inode<'a>, after: usize) -> Result<(), Error> {
        fn p(xs: &[u8]) {
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

const _: () = {
    use std::mem::size_of;
    assert!(128 == size_of::<Superblock>());
    assert!(64 == size_of::<InodeExtended>());
    assert!(32 == size_of::<InodeCompact>());
    assert!(12 == size_of::<Dirent>());
    assert!(12 == size_of::<XattrHeader>());
    assert!(4 == size_of::<XattrEntry>());
    assert!(8 == size_of::<MapHeader>());
    assert!(8 == size_of::<LogicalClusterIndex>());
    assert!(14 == size_of::<Lz4CompressionConfig>());
    assert!(14 == size_of::<LzmaCompressionConfig>());
    assert!(6 == size_of::<DeflateCompressionConfig>());
    assert!(6 == size_of::<ZstdCompressionConfig>());
};

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

    fn inode_data(erofs: &Erofs, inode: &Inode) -> Box<[u8]> {
        let (head, tail) = erofs.get_data(inode).unwrap();
        [head, tail].concat().into()
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

        let out = Command::new("mkfs.erofs")
            .arg(dest.path())
            .arg(dir.path())
            .arg("-b4096") // block size
            // this means that if more than 2 files have the same xattr, it goes into the
            // shared xattr table
            .arg("-x2")
            .output()
            .unwrap();
        if !out.status.success() {
            println!("{}", out.stdout.escape_ascii());
            println!("{}", out.stderr.escape_ascii());
        }
        assert!(out.status.success());

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

    #[test]
    fn test_lookup() {
        let dir = tempdir().unwrap();
        let dest = NamedTempFile::new().unwrap();
        let files = vec!["a", "b", "c/foo/bar/baz", "d", "e/f"];
        for file in &files {
            let p = dir.path().join(file);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap()
            }
            fs::write(&p, p.file_name().unwrap().as_encoded_bytes()).unwrap();
        }

        let out = Command::new("mkfs.erofs")
            .arg(dest.path())
            .arg(dir.path())
            .output()
            .unwrap();
        if !out.status.success() {
            println!("{}", out.stdout.escape_ascii());
            println!("{}", out.stderr.escape_ascii());
        }
        assert!(out.status.success());

        let mmap = unsafe { MmapOptions::new().map(&dest).unwrap() };
        let erofs = Erofs::new(&mmap).unwrap();
        for file in &files {
            let Some(inode) = erofs.lookup(file).unwrap() else {
                panic!("failed to find {:?}", file);
            };
            let data = inode_data(&erofs, &inode);
            assert_eq!(
                data.as_ref(),
                Path::new(file).file_name().unwrap().as_encoded_bytes()
            );
        }
        assert!(erofs.lookup("not-a-file").unwrap().is_none());
        assert!(erofs.lookup("also/not-a-file").unwrap().is_none());
    }

    #[allow(dead_code)]
    fn test_legacy_compression_mkfs<F>(
        data: &[u8],
        block_size: usize,
        compression: &str,
        cb: F,
    ) -> Result<Vec<u8>, Error>
    where
        F: Fn(&[LogicalClusterIndex]),
    {
        let dir = tempdir().unwrap();
        let dest = NamedTempFile::new().unwrap();

        let filename = "file";
        let file = dir.path().join(&filename);
        fs::write(&file, data).unwrap();

        let out = Command::new("mkfs.erofs")
            .arg(dest.path())
            .arg(dir.path())
            .arg(format!("-z{compression}"))
            .arg(format!("-b{block_size}"))
            .arg("-Elegacy-compress")
            .output()
            .unwrap();
        if !out.status.success() {
            println!("{}", out.stdout.escape_ascii());
            println!("{}", out.stderr.escape_ascii());
        }
        assert!(out.status.success());

        let mmap = unsafe { MmapOptions::new().map(&dest).unwrap() };
        let erofs = Erofs::new(&mmap)?;

        let inode = erofs.lookup(&filename)?.unwrap();
        let lcis = erofs.get_logical_cluster_indices(&inode)?;
        cb(lcis);
        erofs.get_compressed_data_vec(&inode)
    }

    #[test]
    fn test_legacy_compression() {
        #[allow(unused_macros)]
        macro_rules! check {
            ($data:expr, $block_size:expr, $compression:expr) => {{
                check!($data, $block_size, $compression, |_| {});
            }};
            ($data:expr, $block_size:expr, $compression:expr, $cb:expr) => {{
                let data = &$data;
                let got =
                    test_legacy_compression_mkfs(data, $block_size, $compression, $cb).unwrap();
                assert_eq!(&got, data);
            }};
        }

        #[cfg(feature = "lz4")]
        {
            // 33 is the minimum needed to store as compressed data
            check!(vec![0u8; 4096 + 33], 4096, "lz4");
            // this excercises a case where there is no trailing Plain block
            check!(vec![0u8; 8192], 4096, "lz4", |lcis| {
                assert_eq!(lcis.len(), 2);
                assert!(lcis[1].typ() != LogicalClusterType::Plain);
            });
            check!(vec![0u8; 8193], 4096, "lz4", |lcis| {
                assert_eq!(lcis.len(), 3);
                assert!(lcis[2].typ() == LogicalClusterType::Plain);
            });
            {
                let mut buf = vec![];
                for i in 0..10000 {
                    buf.push(i as u8);
                }
                check!(buf, 4096, "lz4");
            }
            {
                // go above the PCLUSTER_MAX_SIZE of 1Mb
                let mut buf = vec![];
                for i in 0..(1024 * 1024 * 3) {
                    buf.push(i as u8);
                }
                check!(buf, 4096, "lz4");
            }
            {
                let mut buf = vec![];
                for i in 0..10000 {
                    buf.push(i as u8);
                }
                for _ in 0..10000 {
                    buf.push(0);
                }
                for i in 0..10000 {
                    buf.push(i as u8);
                }
                check!(buf, 4096, "lz4");
            }
        }

        #[cfg(not(feature = "lz4"))]
        {
            let data = vec![0u8; 10000];
            let got = test_legacy_compression_mkfs(&data, 4096, "lz4", |_| {});
            assert_eq!(
                got.unwrap_err(),
                Error::CompressionNotSupported(CompressionType::Lz4)
            );
        }
    }
}
