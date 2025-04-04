use std::fs::File;
use std::io;
use std::path::{Path,PathBuf};
use std::io::{Seek,SeekFrom,Read};
use std::collections::HashMap;

use serde::{Serialize,Deserialize};
use oci_spec::image as oci_image;
use byteorder::{ReadBytesExt,LE};
use peinit::RootfsKind;

const INDEX_JSON_MAGIC: u64 = 0x1db56abd7b82da38;

#[derive(Debug, Serialize, Deserialize, Clone)]
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

    pub fn upstream_link(&self) -> Option<String> {
        match self.registry.as_str() {
            "index.docker.io" => {
                let tag = &self.tag;
                let repository = &self.repository;
                let digest = self.digest.replace(":", "-");
                Some(format!("https://hub.docker.com/layers/{repository}/{tag}/images/{digest}"))
            }
            "quay.io" => {
                let repository = &self.repository;
                let digest = &self.digest;
                Some(format!("https://quay.io/repository/{repository}/{digest}"))
            }
            _ => None
        }
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

pub enum PEImageMultiIndexKeyType {
    Name,            // index.docker.io/library/busybox:1.37
    DigestWithSlash, // sha256/abcd1234 I wrongly thought the colon had to be escaped in urls
    Digest,          // sha256:abcd1234
}

pub struct PEImageMultiIndex {
    map: HashMap<String, PEImageMultiIndexEntry>,
    key_type: PEImageMultiIndexKeyType,
}

impl PEImageMultiIndex {
    pub fn new(key_type: PEImageMultiIndexKeyType) -> PEImageMultiIndex {
        Self {
            key_type: key_type,
            map: HashMap::new()
        }
    }

    pub fn from_paths<P: AsRef<Path>>(key_type: PEImageMultiIndexKeyType, paths: &[P]) -> io::Result<Self> {
        let mut ret = Self::new(key_type);
        for p in paths {
            ret.add_path(p)?;
        }
        Ok(ret)
    }

    pub fn from_paths_by_digest_with_colon<P: AsRef<Path>>(paths: &[P]) -> io::Result<Self> {
        Self::from_paths(PEImageMultiIndexKeyType::Digest, paths)
    }

    pub fn add_dir<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        fn is_erofs_or_sqfs(p: &Path) -> bool {
            match p.extension() {
                // boo we can't match a static str against OsStr...
                //Some("erofs") | Some("sqfs") => true,
                Some(s) => s == "erofs" || s == "sqfs",
                _ => false
            }
        }

        for entry in (path.as_ref().read_dir()?).flatten() {
            let p = entry.path();
            if p.is_file() && is_erofs_or_sqfs(&p) {
                self.add_path(p)?;
            }
        }
        Ok(())
    }

    pub fn add_path<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let idx = PEImageIndex::from_path(&path)?;
        let rootfs_kind = RootfsKind::try_from_path_name(&path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "couldn't determine rootfs kind"))?;
        let pathbuf: PathBuf = path.as_ref().to_path_buf();
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
            self.insert(&image.id, entry);
        }
        Ok(())
    }

    fn insert(&mut self, id: &PEImageId, entry: PEImageMultiIndexEntry) {
        match self.key_type {
            PEImageMultiIndexKeyType::Name => {
                self.map.insert(id.name(), entry);
            }
            PEImageMultiIndexKeyType::DigestWithSlash => {
                self.map.insert(id.digest.replace(":", "/"), entry);
            }
            PEImageMultiIndexKeyType::Digest => {
                self.map.insert(id.digest.clone(), entry);
            }
        }
    }

    pub fn get<'a>(&'a self, key: &str) -> Option<&'a PEImageMultiIndexEntry> {
        self.map.get(key)
    }

    pub fn map(&self) -> &HashMap<String, PEImageMultiIndexEntry> {
        &self.map
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Default for PEImageMultiIndex {
    fn default() -> PEImageMultiIndex {
        PEImageMultiIndex::new(PEImageMultiIndexKeyType::Digest)
    }
}
