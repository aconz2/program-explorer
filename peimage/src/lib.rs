use std::fs::File;
use std::io;
use std::path::{Path,PathBuf};
use std::io::{Seek,SeekFrom,Read};
use std::collections::HashMap;

use serde::Deserialize;
use serde_json;
use oci_spec::image as oci_image;
use byteorder::{ReadBytesExt,LE};
use peinit::RootfsKind;

const INDEX_JSON_MAGIC: u64 = 0x1db56abd7b82da38;

#[derive(Debug, Deserialize, Clone)]
pub struct PEImageId {
    pub digest: String,
    pub repository: String,
    pub registry: String,
    pub tag: String,
}

impl PEImageId {
    pub fn name(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repository, self.tag)
    }
}

#[derive(Debug, Deserialize, Clone)]
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
    pub fn from_path<P: AsRef<Path>>(p: P) -> io::Result<Self> {
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

pub struct PEImageMultiIndexEntry {
    pub path: PathBuf,
    pub image: PEImageIndexEntry,
    pub rootfs_kind: RootfsKind,
}

pub struct PEImageMultiIndex {
    map: HashMap<String, PEImageMultiIndexEntry>,
}

impl PEImageMultiIndex {
    pub fn new() -> PEImageMultiIndex {
        Self { map: HashMap::new() }
    }

    pub fn from_paths(paths: &Vec<String>) -> io::Result<Self> {
        let mut ret = Self::new();
        for p in paths {
            ret = ret.add_path(&p)?;
        }
        Ok(ret)
    }

    pub fn add_path<P: AsRef<Path> + Into<PathBuf>>(mut self, path: P) -> io::Result<Self> {
        let idx = PEImageIndex::from_path(&path)?;
        let rootfs_kind = RootfsKind::try_from_path_name(&path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "couldn't determine rootfs kind"))?;
        let pathbuf: PathBuf = path.into();
        for image in idx.images {
            let key = image.id.name();
            if self.map.contains_key(&key) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate image id name"))
            }
            let entry = PEImageMultiIndexEntry{
                path: pathbuf.clone(),
                image: image.clone(),
                rootfs_kind: rootfs_kind,
            };
            self.map.insert(image.id.name(), entry);
        }
        Ok(self)
    }

    pub fn get<'a>(&'a self, key: &str) -> Option<&'a PEImageMultiIndexEntry> {
        self.map.get(key)
    }

    pub fn map<'a>(&'a self) -> &'a HashMap<String, PEImageMultiIndexEntry> {
        &self.map
    }
}
