use std::io::Write;
use std::io::Read;
use std::path::Path;
use std::fs::File;
use std::io;
use walkdir::WalkDir;

pub struct ArchiveWriter<O: Write> {
    out: O
}

#[derive(Debug)]
pub enum Error {
    IoError,
    StripPrefixError,
    WalkdirError,
}

impl From<std::io::Error> for Error { fn from(_e: std::io::Error) -> Error { Error::IoError } }
impl From<std::path::StripPrefixError> for Error { fn from(_e: std::path::StripPrefixError) -> Error { Error::StripPrefixError } }
impl From<walkdir::Error> for Error { fn from(_e: walkdir::Error) -> Error { Error::WalkdirError } }

impl<O: Write> ArchiveWriter<O> {
    fn write_bytes(&mut self, data: &[u8]) -> Result<(), Error> {
        write!(self.out, "{}:", data.len())?;
        self.out.write(data)?;
        Ok(())
    }

    fn write_reader<R: Read>(&mut self, size: u64, data: &mut R) -> Result<(), Error> {
        write!(self.out, "{}:", size)?;
        io::copy(data, &mut self.out)?;
        Ok(())
    }

    pub fn add_bytes<B: AsRef<[u8]>, C: AsRef<[u8]>>(&mut self, name: B, data: C) -> Result<(), Error> {
        self.write_bytes(name.as_ref())?;
        self.write_bytes(data.as_ref())?;
        Ok(())
    }

    pub fn add_file<B: AsRef<[u8]>>(&mut self, name: B, size: u64, file: &mut File) -> Result<(), Error> {
        self.write_bytes(name.as_ref())?;
        self.write_reader(size, file)?;
        Ok(())
    }
}

pub fn archive_path<P: AsRef<Path>, O: Write>(root: &P, out: &mut O) -> Result<(), Error> {
    let mut writer = ArchiveWriter { out: out };
    let iter = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file());
    for e in iter {
        let name = e.path().strip_prefix(root)?.as_os_str().as_encoded_bytes();
        let len = e.metadata()?.len();
        let mut file = File::open(e.path())?;
        writer.add_file(name, len, &mut file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::distributions::{Alphanumeric, DistString};
    use std::ffi::OsString;

    struct TempDir {
        name: OsString
    }

    impl TempDir {
        fn new() -> Self {
            let string = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
            let ret = Self { name: format!("/tmp/{string}").into() };
            std::fs::create_dir(&ret).unwrap();
            ret
        }
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            return Path::new(&self.name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(self).unwrap()
        }
    }

    fn write_file<P: AsRef<Path>>(p: &P, name: &str, data: &[u8]) {
        let path = p.as_ref().join(name);
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let mut f = File::create(path).unwrap();
        f.write_all(data).unwrap();
    }

    #[test]
    fn test_basic_out() {
        let mut writer = ArchiveWriter { out: vec![] };
        writer.add_bytes("file1.txt", b"data").unwrap();
        writer.add_bytes("file2.txt", b"jjjj").unwrap();
        assert_eq!(writer.out, b"9:file1.txt4:data9:file2.txt4:jjjj");
    }

    #[test]
    fn test_basic_dir() {
        let td = TempDir::new();
        write_file(&td, "file1.txt", b"data");
        write_file(&td, "file2.txt", b"jjjj");
        write_file(&td, "b/file3.txt", b"ffff");
        let mut out = vec![];
        archive_path(&td, &mut out).unwrap();
        // let sout = std::str::from_utf8(&out).unwrap();
        // println!("{sout}");
        assert_eq!(out, b"11:b/file3.txt4:ffff9:file1.txt4:data9:file2.txt4:jjjj");
    }
}
