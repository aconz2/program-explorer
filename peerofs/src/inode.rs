use std::io::{Seek,Read};
use std::fmt;

use crate::util::read_u8_array;
//use crate::superblock::Superblock;

use byteorder::{ReadBytesExt,LittleEndian as LE};
use rustix::fs::FileType;

#[repr(C)]
#[derive(Debug)]
pub struct InodeCompact {
    format_layout: u16,
    xattr_count: u16,
    mode: u16,
    nlink: u16,
    size: u32,
    _reserved: u32,
    compressed_blocks_or_raw_blkaddr_or_rdev: u32, // TODO chunk_info
    ino: u32,
    uid: u16,
    gid: u16,
    _reserved2: u32
}

impl InodeCompact {
    fn from_reader<R: Read + Seek>(format_layout: u16, reader: &mut R) -> std::io::Result<Option<Self>> {
        Ok(Some(InodeCompact {
            format_layout: format_layout,
            xattr_count: reader.read_u16::<LE>()?,
            mode: reader.read_u16::<LE>()?,
            nlink: reader.read_u16::<LE>()?,
            size: reader.read_u32::<LE>()?,
            _reserved: reader.read_u32::<LE>()?,
            compressed_blocks_or_raw_blkaddr_or_rdev: reader.read_u32::<LE>()?,
            ino: reader.read_u32::<LE>()?,
            uid: reader.read_u16::<LE>()?,
            gid: reader.read_u16::<LE>()?,
            _reserved2: reader.read_u32::<LE>()?,
        }))
    }

    fn layout(&self) -> Layout {
        (self.format_layout >> 1).try_into()
        .expect("should be validated on the way in")
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct InodeExtended {
    format_layout: u16,
    xattr_count: u16,
    mode: u16,
    _reserved: u16,
    size: u64,
    compressed_blocks_or_raw_blkaddr_or_rdev: u32, // TODO chunk_info
    ino: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    mtime_nsec: u32,
    nlink: u32,
    _reserved2: [u8; 16],
}

impl InodeExtended {
    fn from_reader<R: Read + Seek>(format_layout: u16, reader: &mut R) -> std::io::Result<Option<Self>> {
        Ok(Some(InodeExtended {
            format_layout: format_layout,
            xattr_count: reader.read_u16::<LE>()?,
            mode: reader.read_u16::<LE>()?,
            _reserved: reader.read_u16::<LE>()?,
            size: reader.read_u64::<LE>()?,
            compressed_blocks_or_raw_blkaddr_or_rdev: reader.read_u32::<LE>()?,
            ino: reader.read_u32::<LE>()?,
            uid: reader.read_u32::<LE>()?,
            gid: reader.read_u32::<LE>()?,
            mtime: reader.read_u64::<LE>()?,
            mtime_nsec: reader.read_u32::<LE>()?,
            nlink: reader.read_u32::<LE>()?,
            _reserved2: read_u8_array::<16, R>(reader)?,
        }))
    }

    pub fn layout(&self) -> Layout {
        (self.format_layout >> 1).try_into()
        .expect("should be validated on the way in")
    }
}

#[derive(Debug)]
pub enum Inode {
    Compact(InodeCompact),
    Extended(InodeExtended),
}

impl Inode{
    pub fn from_reader<R: Read + Seek>(reader: &mut R) -> std::io::Result<Option<Self>> {
        let format_layout = reader.read_u16::<LE>()?;
        let format = format_layout & 0x01;
        //let layout = (format_layout >> 1) & 0x07;
        match format {
            0 => Ok(InodeCompact::from_reader(format_layout, reader)?.map(|x| Inode::Compact(x))),
            1 => Ok(InodeExtended::from_reader(format_layout, reader)?.map(|x| Inode::Extended(x))),
            _ => unreachable!()
        }
    }

    pub fn file_type(&self) -> FileType {
        match self {
            Inode::Compact(x) => FileType::from_raw_mode(x.mode.into()),
            Inode::Extended(x) => FileType::from_raw_mode(x.mode.into()),
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Inode::Compact(x) => x.size as u64,
            Inode::Extended(x) => x.size,
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct Dirent {
    id: u64,
    nameoff: u16,
    file_type: u8, // this type is the S_IF* >> 12
    _reserved: u8,
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

impl fmt::Display for Inode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Inode::Compact(x) => fmt::Display::fmt(x, f),
            Inode::Extended(x) => fmt::Display::fmt(x, f),
        }
    }
}

impl fmt::Display for InodeCompact {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "InodeCompact:")?;
        writeln!(f, "  mode: {:o}", self.mode)?;
        writeln!(f, "  uid: {:o}", self.uid)?;
        writeln!(f, "  gid: {:o}", self.gid)?;
        writeln!(f, "nlink: {:o}", self.nlink)
    }
}

impl fmt::Display for InodeExtended {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "InodeExtended:")?;
        writeln!(f, " layout: {:?}", self.layout())?;
        writeln!(f, "   mode: {:o}", self.mode)?;
        writeln!(f, "    uid: {}", self.uid)?;
        writeln!(f, "    gid: {}", self.gid)?;
        writeln!(f, "  nlink: {}", self.nlink)?;
        writeln!(f, "   size: {}", self.size)?;
        writeln!(f, "  mtime: {} {}ns", self.mtime, self.mtime_nsec)?;
        writeln!(f, "  raw_blkaddr: {}", self.compressed_blocks_or_raw_blkaddr_or_rdev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sizeof_inode() {
        assert_eq!(32, std::mem::size_of::<InodeCompact>());
        assert_eq!(64, std::mem::size_of::<InodeExtended>());
    }
}
