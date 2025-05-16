use std::ffi::CStr;
use std::sync::{Arc, atomic::AtomicU64};

use log::{error, info};
use moka::notification::RemovalCause;
use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags},
    io::Errno,
};

use oci_spec::image::Digest;

// this whole file is maybe a bad idea
// I tried to write a struct BlobCache that wrapped a moka::Cache+OwnedFd into a file manager,
// especially since it makes the "get or download/produce and save in the cache but also give me an
// OwnedFd" more reusable, but ultimately didn't go great. So instead I put some building blocks in
// here and ended up having to duplicate the file guard business for the blobs which can have an
// algo, and GenericName (terrible name choice) for ref and manifest files in the overall cache dir
// The fileguard is nice because it takes care of writing to a _tmp file and then either renaming
// or unlinking depending on whether the caller calls .success()
// I know doing this all with *at is probably pointless esp since there isn't uniform support for
// it with eg renameat and unlinkat missing BENEATH, but was maybe worth a try

// moka::Cache stores the capacity of each entry as u32, so for blobs which might approach 4 GB, we
// track their size in KB so that a single blob up to 4 TB is supported
pub const BLOB_SIZE_DIVISOR: u64 = 1_000;

// BlobKey is a digest
// https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests
// it can technically contain . in the algo-separator but I'm not accepting that as it makes it
// easy to check for a string that won't traverse directories
#[derive(Hash, Eq, PartialEq, Clone)]
pub struct BlobKey(String);

impl BlobKey {
    pub fn new(s: String) -> Option<Self> {
        if s.contains(".") || s.contains("/") {
            return None;
        }
        match s.split_once(":") {
            Some((l, r)) if l.is_empty() || r.is_empty() => None,
            None => None,
            _ => Some(Self(s)),
        }
    }

    fn from_cstr_parts(a: &CStr, b: &CStr) -> Option<Self> {
        let a = a.to_str().ok()?;
        let b = b.to_str().ok()?;
        BlobKey::new(format!("{}:{}", a, b))
    }

    fn as_path(&self) -> String {
        self.0.replacen(":", "/", 1)
    }

    fn parts(&self) -> (&str, &str) {
        // checked in constructor
        self.0.split_once(":").unwrap()
    }

    fn with_tmp_suffix(&self) -> Self {
        Self(format!("{}_tmp", self.0))
    }
}

impl TryFrom<&Digest> for BlobKey {
    type Error = ();
    fn try_from(digest: &Digest) -> Result<Self, Self::Error> {
        Self::new(digest.to_string()).ok_or(())
    }
}

impl std::fmt::Display for BlobKey {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub struct FileGuard<'a> {
    dir: &'a OwnedFd,
    key: Option<&'a BlobKey>,
}

impl<'a> FileGuard<'a> {
    fn new(dir: &'a OwnedFd, key: &'a BlobKey) -> FileGuard<'a> {
        Self {
            dir,
            key: Some(key),
        }
    }

    pub fn success(mut self) -> Result<(), Errno> {
        if let Some(key) = self.key.take() {
            rustix::fs::renameat(
                &self.dir,
                key.with_tmp_suffix().as_path(),
                &self.dir,
                key.as_path(),
            )?;
        }
        Ok(())
    }
}

impl Drop for FileGuard<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            match unlinkat(self.dir, &key.with_tmp_suffix()) {
                Ok(()) => {}
                Err(e) => {
                    error!(
                        "error on BlobCacheFileGuard drop trying to delete {} {:?}",
                        key, e
                    );
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct GenericName<'a>(&'a str);

impl<'a> GenericName<'a> {
    pub fn new(name: &'a str) -> Option<GenericName<'a>> {
        if name.contains(".") || name.contains("/") {
            None
        } else {
            Some(Self(name))
        }
    }

    fn with_tmp_suffix(&self) -> String {
        format!("{}_tmp", self.0)
    }
}

pub struct GenericFileGuard<'a> {
    dir: &'a OwnedFd,
    name: Option<&'a GenericName<'a>>,
}

impl<'a> GenericFileGuard<'a> {
    fn new(dir: &'a OwnedFd, name: &'a GenericName<'a>) -> GenericFileGuard<'a> {
        Self {
            dir,
            name: Some(name),
        }
    }

    pub fn success(mut self) -> Result<(), Errno> {
        if let Some(name) = self.name.take() {
            rustix::fs::renameat(&self.dir, name.with_tmp_suffix(), &self.dir, name.0)?;
        }
        Ok(())
    }
}

impl Drop for GenericFileGuard<'_> {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            match rustix::fs::unlinkat(self.dir, name.with_tmp_suffix(), AtFlags::empty()) {
                Ok(()) => {}
                Err(e) => {
                    error!(
                        "error on GenericCacheFileGuard drop trying to delete {:?} {:?}",
                        name, e
                    );
                }
            }
        }
    }
}

pub fn max_capacity(x: u64) -> u64 {
    x / BLOB_SIZE_DIVISOR
}

pub fn weigher(_key: &BlobKey, size: &u64) -> u32 {
    std::cmp::max(1, size / BLOB_SIZE_DIVISOR)
        .try_into()
        .unwrap_or(u32::MAX)
}

pub fn remove_blob(
    name: &str,
    blob_dir: &OwnedFd,
    key: Arc<BlobKey>,
    _value: u64,
    cause: RemovalCause,
) {
    info!("blob({}) {} removed due to {:?}", name, key, cause);

    if let Err(e) = unlinkat(blob_dir, &key) {
        error!("blob unlinkat return error {:?}", e);
    }
}

// we only care about reading two levels deep
pub fn read_from_disk(dir: &OwnedFd, mut f: impl FnMut(BlobKey, u64)) -> Result<(), Errno> {
    let mut dir_reader = Dir::read_from(dir)?;
    dir_reader.rewind();
    while let Some(entry_dir) = dir_reader.read() {
        let entry_dir = entry_dir?;
        if entry_dir.file_name() == c"." || entry_dir.file_name() == c".." {
            continue;
        }
        if entry_dir.file_type() != FileType::Directory {
            continue;
        }
        let sub_dir = rustix::fs::openat(
            dir,
            entry_dir.file_name(),
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut sub_dir_reader = Dir::read_from(&sub_dir)?;
        while let Some(entry_file) = sub_dir_reader.read() {
            let entry_file = entry_file?;
            if entry_file.file_name() == c"." || entry_file.file_name() == c".." {
                continue;
            }
            if let Some(key) =
                BlobKey::from_cstr_parts(entry_dir.file_name(), entry_file.file_name())
            {
                let stat = rustix::fs::statat(&sub_dir, entry_file.file_name(), AtFlags::empty())?;
                f(key, stat.st_size as u64);
            } else {
                error!(
                    "got weird path {:?} {:?}",
                    entry_dir.file_name(),
                    entry_file.file_name()
                );
            }
        }
    }

    Ok(())
}

pub fn openat_create_write_with_guard<'a>(
    dir: &'a OwnedFd,
    key: &'a BlobKey,
) -> Result<(std::fs::File, FileGuard<'a>), Errno> {
    let file = openat_create_write(dir, &key.with_tmp_suffix())?;
    let guard = FileGuard::new(dir, key);
    Ok((file, guard))
}

pub fn openat_create_write_with_generic_guard<'a>(
    dir: &'a OwnedFd,
    name: &'a GenericName,
) -> Result<(std::fs::File, GenericFileGuard<'a>), Errno> {
    let fd = rustix::fs::openat2(
        dir,
        name.with_tmp_suffix(),
        OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
        ResolveFlags::BENEATH,
    )?;
    let guard = GenericFileGuard::new(dir, name);
    Ok((fd.into(), guard))
}

pub fn openat_create_write_async_with_guard<'a>(
    dir: &'a OwnedFd,
    key: &'a BlobKey,
) -> Result<(tokio::fs::File, FileGuard<'a>), Errno> {
    let file = openat_create_write_async(dir, &key.with_tmp_suffix())?;
    let guard = FileGuard::new(dir, key);
    Ok((file, guard))
}

pub fn openat_read(
    dir: &OwnedFd,
    name: impl rustix::path::Arg,
) -> Result<Option<std::fs::File>, Errno> {
    match rustix::fs::openat2(
        dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH,
    ) {
        Ok(f) => Ok(Some(f.into())),
        Err(e) if e == Errno::NOENT => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn openat_read_key(dir: &OwnedFd, key: &BlobKey) -> Result<Option<std::fs::File>, Errno> {
    openat_read(dir, key.as_path())
}

pub fn open_or_create_dir_at(
    dir: Option<&OwnedFd>,
    path: impl rustix::path::Arg + Copy,
) -> Result<OwnedFd, Errno> {
    if let Some(dir) = dir {
        match rustix::fs::mkdirat(dir, path, Mode::from_bits_truncate(0o744)) {
            Ok(_) => Ok(()),
            Err(e) if e == Errno::EXIST => Ok(()),
            e => e,
        }?;
        rustix::fs::openat2(
            dir,
            path,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH,
        )
    } else {
        match rustix::fs::mkdir(path, Mode::from_bits_truncate(0o744)) {
            Ok(_) => Ok(()),
            Err(e) if e == Errno::EXIST => Ok(()),
            e => e,
        }?;
        rustix::fs::open(
            path,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }
}

fn openat_create_write(dir: &OwnedFd, key: &BlobKey) -> Result<std::fs::File, Errno> {
    let open = || {
        openat_key(
            dir,
            key,
            Mode::from_bits_truncate(0o644),
            OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
        )
    };
    match open() {
        Ok(f) => Ok(f),
        Err(e) if e == Errno::NOENT => {
            rustix::fs::mkdirat(dir, key.parts().0, Mode::from_bits_truncate(0o744))?;
            open()
        }
        e => e,
    }
}

fn openat_create_write_async(dir: &OwnedFd, key: &BlobKey) -> Result<tokio::fs::File, Errno> {
    let open = || {
        openat_key(
            dir,
            key,
            Mode::from_bits_truncate(0o644),
            OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC | OFlags::NONBLOCK,
        )
        .map(tokio::fs::File::from_std)
    };
    match open() {
        Ok(f) => Ok(f),
        Err(e) if e == Errno::NOENT => {
            rustix::fs::mkdirat(dir, key.parts().0, Mode::from_bits_truncate(0o744))?;
            open()
        }
        e => e,
    }
}

fn openat_key(
    dir: &OwnedFd,
    key: &BlobKey,
    mode: Mode,
    flags: OFlags,
) -> Result<std::fs::File, Errno> {
    let fd = rustix::fs::openat2(dir, key.as_path(), flags, mode, ResolveFlags::BENEATH)?;
    Ok(fd.into())
}

// wish there was unlinkat2 with BENEATH
fn unlinkat(dir: &OwnedFd, key: &BlobKey) -> Result<(), Errno> {
    rustix::fs::unlinkat(dir, key.as_path(), AtFlags::empty())
}

pub fn atomic_inc(x: &AtomicU64) {
    x.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn atomic_take(x: &AtomicU64) -> u64 {
    x.swap(0, std::sync::atomic::Ordering::Relaxed)
}
