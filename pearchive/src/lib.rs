use std::os::fd::{FromRawFd,AsRawFd,OwnedFd};
use std::fs::File;
use std::path::Path;
use std::ffi::{CStr,CString};
use std::io::{Write,BufWriter};

use rustix::fs::{RawDir,FileType};

const MAX_DIR_DEPTH: usize = 32;
const DIRENT_BUF_SIZE: usize = 2048;

/// v1 archive format
/// message+
/// message =
///   | file: <tag> <name zero term> <u32le> <blob>
///   | dir:  <tag> <name zero term>
///   | pop:  <tag>
///
/// alternate format would be to buffer the names and sizes and just dump
/// the blob data so, this avoids the write per message but requires buffering
/// <blob size> <blob data> <message+>
/// message =
///   | file: <tag> <name zero term> <u32le>
///   | dir:  <tag> <name zero term>
///   | pop:  <tag>
///


#[derive(Debug)]
pub enum Error {
    Entry,
    ReadDir,
    FileType,
    OpenAt,
    Getdents,
    DirTooDeep,
    NotADir,
    FdOpenDir,
    Fstat,
    OnFile,
    OnDir,
    OnPop,
    Write,
    SendFile,
}

pub enum ArchiveFormat1Tag {
    File = 1,
    Dir = 2,
    Pop = 3,
}

pub trait Visitor {
    fn on_file(&mut self, name: &CStr, size: u64, fd: OwnedFd) -> Result<(), ()>;
    fn on_dir(&mut self, name: &CStr) -> Result<(), ()>;
    fn leave_dir(&mut self) -> Result<(), ()>;
}

fn openat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn opendirat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn opendirat_cwd(name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(libc::AT_FDCWD, name.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl TryFrom<&u8> for ArchiveFormat1Tag {
    type Error = ();
    fn try_from(x: &u8) -> Result<ArchiveFormat1Tag, ()> {
        match x {
            // TODO what is the right way to do this?
            1 => Ok(ArchiveFormat1Tag::File),
            2 => Ok(ArchiveFormat1Tag::Dir),
            3 => Ok(ArchiveFormat1Tag::Pop),
            _ => Err(()),
        }
    }
}

fn read_le_u32(input: &mut &[u8]) -> u32 {
    let (int_bytes, rest) = input.split_at(std::mem::size_of::<u32>());
    *input = rest;
    u32::from_le_bytes(int_bytes.try_into().unwrap())
}

fn file_size<Fd: AsRawFd>(fd: &Fd) -> Result<u64, Error> {
    use std::mem;
    let size = unsafe {
        let mut buf: libc::stat = mem::zeroed();
        let ret = libc::fstat(
            fd.as_raw_fd(),
            &mut buf as *mut _
        );
        if ret < 0 { return Err(Error::Fstat); }
        buf.st_size
    };
    // dude st_size is signed here and unsigned in statx
    size.try_into().map_err(|_| Error::Fstat)
}

fn sendfile_all<Fd1: AsRawFd, Fd2: AsRawFd>(fd_in: &mut Fd1, fd_out: &mut Fd2, len: u64) -> Result<(), Error> {
    use std::ptr;
    let mut len = len;
    while len > 0 {
        let ret = unsafe {
            libc::sendfile(fd_out.as_raw_fd(), fd_in.as_raw_fd(), ptr::null_mut(), len as usize)
        };
        if ret <= 0 { return Err(Error::SendFile); }
        let ret = ret as u64;
        if ret > len { return Err(Error::SendFile); }
        len -= ret;
    }
    Ok(())
}

struct PackToFileVisitor {
    writer: BufWriter::<File>,
}

impl PackToFileVisitor {
    fn new(out: File) -> Self {
        Self { writer: BufWriter::new(out) }
    }

    fn into_file(self) -> File {
        self.writer.into_inner().map_err(|_| Error::Write).unwrap()
    }
}

impl Visitor for PackToFileVisitor {
    fn on_file(&mut self, name: &CStr, size: u64, mut fd: OwnedFd) -> Result<(), ()> {
        self.writer.write_all(&[ArchiveFormat1Tag::File as u8]).map_err(|_| ())?;
        self.writer.write_all(name.to_bytes_with_nul()).map_err(|_| ())?;
        self.writer.write_all(&(size as u32).to_le_bytes()).map_err(|_| ())?;
        self.writer.flush().map_err(|_| ())?;
        // let outfile = self.writer.get_mut();
        sendfile_all(&mut fd, self.writer.get_mut(), size).map_err(|_| ())?;
        Ok(())
    }

    fn on_dir(&mut self, name: &CStr) -> Result<(), ()> {
        self.writer.write_all(&[ArchiveFormat1Tag::Dir as u8]).map_err(|_| ())?;
        self.writer.write_all(name.to_bytes_with_nul()).map_err(|_| ())?;
        Ok(())
    }

    fn leave_dir(&mut self) -> Result<(), ()> {
        self.writer.write_all(&[ArchiveFormat1Tag::Pop as u8]).map_err(|_| ())?;
        Ok(())
    }
}

// would love to know how this looks as an iterator at some point
// and what the right error handling is, here I throw away the visitor's error
fn visit_dirc_rec<V: Visitor>(curdir: &OwnedFd, v: &mut V, depth: usize) -> Result<(), Error> {
    if depth > MAX_DIR_DEPTH { return Err(Error::DirTooDeep); }

    let mut buf = Vec::with_capacity(DIRENT_BUF_SIZE);
    let mut iter = RawDir::new(&curdir, buf.spare_capacity_mut());

    while let Some(entry) = iter.next() {
        let entry = entry.map_err(|_| Error::Getdents)?;
        match entry.file_type() {
            FileType::RegularFile => {
                let name = entry.file_name();
                let fd = openat(curdir, name)?;
                let size = file_size(&fd)?;
                v.on_file(name, size, fd).map_err(|_| Error::OnFile)?;
            },
            FileType::Directory => {
                if entry.file_name() == c"." || entry.file_name() == c".." {
                    continue;
                }
                let newdirfd = opendirat(curdir, entry.file_name())?;
                let curname = entry.file_name();

                v.on_dir(curname).map_err(|_| Error::OnDir)?;
                visit_dirc_rec(&newdirfd, v, depth + 1)?;
                v.leave_dir().map_err(|_| Error::OnDir)?;
            },
            _ => {}
        }
    }

    Ok(())
}

fn visit_dirc<V: Visitor>(dir: &CStr, v: &mut V) -> Result<(), Error> {
    let dirfd = opendirat_cwd(dir)?;
    visit_dirc_rec(&dirfd, v, 0)?;
    Ok(())
}

pub fn visit_dir<V: Visitor>(dir: &Path, v: &mut V) -> Result<(), Error> {
    let cstr = CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    visit_dirc(&cstr, v)
}

pub fn pack_dir_to_file(dir: &Path, file: File) -> Result<File, Error> {
    let mut visitor = PackToFileVisitor::new(file);
    visit_dir(dir, &mut visitor).unwrap();
    Ok(visitor.into_file())
}
