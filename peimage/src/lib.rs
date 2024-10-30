use std::fs::File;
use std::io;
use std::io::{Seek,SeekFrom,Read};

use serde::Deserialize;
use serde_json;
use oci_spec::image as oci_image;
use byteorder::{ReadBytesExt,LE};

const INDEX_JSON_MAGIC: u64 = 0x1db56abd7b82da38;

#[derive(Debug, Deserialize)]
pub struct PEImageId {
    pub digest: String,
    pub repository: String,
    pub registry: String,
    pub tag: String,
}

#[derive(Debug, Deserialize)]
pub struct PEImageIndexEntry {
    pub rootfs: String,
    pub config: oci_image::ImageConfiguration,
    pub manifest: oci_image::ImageManifest,
    pub id: PEImageId,
}

#[derive(Debug, Deserialize)]
pub struct PEImageIndex {
    pub images: Vec<PEImageIndexEntry>
}

impl PEImageIndex {

    pub fn from_path<P: AsRef<std::path::Path>>(p: P) -> io::Result<Self> {
        Self::from_file(&mut File::open(p)?)
    }

    pub fn from_file(f: &mut File) -> io::Result<Self> {
        let len = f.metadata()?.len();
        if len < (8 + 4) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too short to have magic"))
        }
        f.seek(SeekFrom::End(-i64::from(8 + 4)))?;
        let data_size = f.read_u32::<LE>()?;
        let magic = f.read_u64::<LE>()?;
        if magic != INDEX_JSON_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file doesn't end with magic"))
        }
        if u64::from(data_size) + 8 + 4 > len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too short to hold index.json"))
        }
        f.seek(SeekFrom::End(-i64::from(8 + 4 + data_size)))?;
        let mut buf = vec![0; data_size as usize];
        f.read_exact(&mut buf)?;
        serde_json::from_slice(buf.as_slice())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "index.json not valid PEImageIndex"))
    }
}
