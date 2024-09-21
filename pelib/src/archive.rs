use std::io::{Write,Read};
use std::path::Path;
use std::fs::File;
use std::io;
use walkdir::WalkDir;
use std::os::fd::{RawFd,AsRawFd,FromRawFd};
use std::ffi::{OsString,CString};
use std::os::unix::ffi::{OsStrExt,OsStringExt};
use std::ptr;
use std::fs;

use libc;

use openat2::{openat2_cstr,OpenHow,ResolveFlags};

pub struct ArchiveWriter<O: Write> {
    out: O
}

// pub struct ArchiveReader<I: Read + AsRawFd<I>> {
//     inp: Take<I>,
//     path_buf: Vec<u8>,
// }

#[derive(Debug)]
pub enum Error {
    IoError,
    StripPrefixError,
    WalkdirError,
    SizeError,
    ReadError,
    Todo,
    NonAsciiSize,
    NoColon,
    BadColon,
    StrError,
    ParseError,
    DataSizeTooBig,
    NotADir,
    Open,
    Path,
    CopyFileRange,
    CopyFileRangeZero,
    CopyFileRangeRetTooBig,
    MkdirP,
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

struct DirFd {
    fd: RawFd,
}

impl DirFd {
    fn new<P: AsRef<Path>>(path: &P) -> Result<Self, Error> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Err(Error::NotADir);
        }
        let fd = unsafe {
            let pathz = CString::new(path.as_os_str().as_bytes()).map_err(|_|Error::Path)?;
            let ret = libc::open(pathz.as_ptr(), libc::O_PATH);
            if ret < 0 {
                return Err(Error::Open);
            }
            ret
        };
        Ok(Self { fd: fd })
    }
}

impl Drop for DirFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl AsRawFd for DirFd {
    fn as_raw_fd(&self) -> i32 { return self.fd }
}

fn copy_file_range(fd_in: RawFd, len: usize, file_out: File) -> Result<(), Error> {
    let fd_out = file_out.as_raw_fd();
    let mut len = len;
    while len > 0 {
        let ret = unsafe {
            libc::copy_file_range(fd_in, ptr::null_mut(), fd_out, ptr::null_mut(), len, 0)
        };
        if ret < 0 { return Err(Error::CopyFileRange); }
        if ret == 0 { return Err(Error::CopyFileRangeZero); }
        let ret = ret as usize;
        if ret > len { return Err(Error::CopyFileRangeRetTooBig); }
        len -= ret;
    }
    Ok(())
}

pub fn unpack_archive<P: AsRef<Path>>(root: &P, file: &P) -> Result<(), Error> {
    let mut infile = File::open(file).map_err(|_|Error::ReadError)?;
    let dirfd = DirFd::new(root)?;
    let fd_in = infile.as_raw_fd();

    let content_length = {
        let mut buf = [0; 4];
        infile.read_exact(&mut buf).map_err(|_|Error::ReadError)?;
        u32::from_le_bytes(buf) as usize
    };

    let mut inp = infile.take(content_length as u64);

    let mut path_buf: Vec<u8> = vec![];

    while inp.limit() > 0 {

        let path_size = {
            let mut buf = [0; 2];
            inp.read_exact(&mut buf).map_err(|_|Error::ReadError)?;
            u16::from_le_bytes(buf) as usize
        };

        // read 4 extra for the size of the data buffer
        path_buf.resize(path_size + 4, 0);
        inp.read_exact(&mut path_buf[..]).map_err(|_|Error::ReadError)?;

        let data_size = {
            let mut buf: [u8; 4] = [0; 4];
            buf.copy_from_slice(&path_buf[path_size..]);
            u32::from_le_bytes(buf) as usize
        };
        if data_size > inp.limit() as usize {
            return Err(Error::DataSizeTooBig);
        }

        path_buf.resize(path_size, 0);
        // TODO want to not copy path_buf and just 
        //path_buf.push(b'\0');
        
        {
            // TODO get rid of copy / don't create dirs
            let osstring = OsString::from_vec(path_buf.clone());
            let p = Path::new(&osstring);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).map_err(|_|Error::MkdirP)?;
            }
        }
        let file_out = unsafe {
            let how = {
                let mut x = OpenHow::new(libc::O_WRONLY | libc::O_CREAT, 0o777);
                x.resolve |= ResolveFlags::IN_ROOT;
                x
            };

            // TODO get rid of copy!
            let path_bufz = CString::new(path_buf.as_slice()).map_err(|_|Error::Path)?;
            let fd = openat2_cstr(Some(dirfd.as_raw_fd()), path_bufz.as_c_str(), &how)?;
            File::from_raw_fd(fd)
        };
        copy_file_range(fd_in, data_size, file_out)?;

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
            fs::create_dir(&ret).unwrap();
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
            fs::remove_dir_all(self).unwrap()
        }
    }

    fn write_file<P: AsRef<Path>>(p: &P, name: &str, data: &[u8]) {
        let path = p.as_ref().join(name);
        if let Some(p) = path.parent() {
            let _ = fs::create_dir_all(p);
        }
        let mut f = File::create(path).unwrap();
        f.write_all(data).unwrap();
    }

    #[test]
    fn test_writer_basic_out() {
        let mut writer = ArchiveWriter { out: vec![] };
        writer.add_bytes("file1.txt", b"data").unwrap();
        writer.add_bytes("file2.txt", b"jjjj").unwrap();
        assert_eq!(writer.out, b"9:file1.txt4:data9:file2.txt4:jjjj");
    }

    #[test]
    fn test_archive_path_basic_dir() {
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

    // #[test]
    // fn test_unpack_basic() {
    //     let td = TempDir::new();

    // }
}
