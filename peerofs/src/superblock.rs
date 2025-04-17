use std::io::{Seek,Read,SeekFrom};
use std::fmt;

use crate::util::read_u8_array;
use crate::inode::Inode;

use byteorder::{ReadBytesExt,LittleEndian as LE};

const EROFS_SUPER_MAGIC_V1: u32 = 0xE0F5E1E2;

// incomplete list of things not handled
// sb_extslots sb_size = 128 + sb_extslots * EROFS_SB_EXTSLOT_SIZE(16)

// corresponds to erofs_super_block; also see erofs_sb_info
#[repr(C)]
#[derive(Debug)]
pub struct Superblock {
    magic: u32,
    checksum: u32,
    feature_compat: u32,
    blkszbits: u8,
    sb_extslots: u8,
    root_nid: u16,
    inos: u64,
    build_time: u64,
    build_time_nsec: u32,
    blocks: u32,
    meta_blkaddr: u32,
    xattr_blkaddr: u32,
    uuid: [u8; 16],
    volume_name: [u8; 16],
    available_compr_algs_or_lz4_max_distance: u16,
    extra_devices: u16,
    devt_slotoff: u16,
    dirblkbits: u8,
    xattr_prefix_count: u8,
    xattr_prefix_start: u32,
    packed_nid: u64,
    xattr_filter_reserved: u8,
    _reserved2: [u8; 23],
}

impl Superblock {
    pub fn from_reader<R: Read + Seek>(reader: &mut R) -> std::io::Result<Option<Self>> {
        reader.seek(SeekFrom::Start(1024))?; // EROFS_SUPER_OFFSET
        let ret = Superblock {
            magic: reader.read_u32::<LE>()?,
            checksum: reader.read_u32::<LE>()?,
            feature_compat: reader.read_u32::<LE>()?,
            blkszbits: reader.read_u8()?,
            sb_extslots: reader.read_u8()?,
            root_nid: reader.read_u16::<LE>()?,
            inos: reader.read_u64::<LE>()?,
            build_time: reader.read_u64::<LE>()?,
            build_time_nsec: reader.read_u32::<LE>()?,
            blocks: reader.read_u32::<LE>()?,
            meta_blkaddr: reader.read_u32::<LE>()?,
            xattr_blkaddr: reader.read_u32::<LE>()?,
            uuid: read_u8_array::<16, R>(reader)?,
            volume_name: read_u8_array::<16, R>(reader)?,
            available_compr_algs_or_lz4_max_distance: reader.read_u16::<LE>()?,
            extra_devices: reader.read_u16::<LE>()?,
            devt_slotoff: reader.read_u16::<LE>()?,
            dirblkbits: reader.read_u8()?,
            xattr_prefix_count: reader.read_u8()?,
            xattr_prefix_start: reader.read_u32::<LE>()?,
            packed_nid: reader.read_u64::<LE>()?,
            xattr_filter_reserved: reader.read_u8()?,
            _reserved2: read_u8_array::<23, R>(reader)?,
        };
        Ok(ret.validate())
    }

    fn block_size(&self) -> u64 {
        1u64 << self.blkszbits
    }

    fn block_offset(&self, block: u32) -> u64 {
        (block as u64) << self.blkszbits
    }

    pub fn root_inode(&self) -> u32 {
        self.root_nid as u32
    }

    fn inode_offset(&self, id: u32) -> u64 {
        let id = id as u64;
        self.block_offset(self.meta_blkaddr) + 32u64 * id
    }

    pub fn get_inode<R: Read + Seek>(&self, reader: &mut R, id: u32) -> std::io::Result<Option<Inode>> {
        reader.seek(SeekFrom::Start(self.inode_offset(id)))?;
        Inode::from_reader(reader)
    }

    fn validate(self) -> Option<Self> {
        if self.magic != EROFS_SUPER_MAGIC_V1 { return None; }
        // TODO checksum
        Some(self)
    }
}

impl fmt::Display for Superblock {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Superblock:")?;
        writeln!(f, "       magic: {:x}", self.magic)?;
        writeln!(f, "    checksum: {:x}", self.checksum)?;
        writeln!(f, "  block size: {} ({})", self.blkszbits, self.block_size())?;
        writeln!(f, "    root_nid: {}", self.root_nid)?;
        writeln!(f, "        inos: {}", self.inos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sizeof_superblock() {
        assert_eq!(128, std::mem::size_of::<Superblock>());
    }
}
