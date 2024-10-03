use std::os::fd::{FromRawFd,AsRawFd,OwnedFd};
use std::fs::File;
use std::path::Path;
use std::ffi::{CStr,CString};
use std::io::{Write,BufWriter};

use rustix::fs::{RawDir,FileType};
use memmap::MmapOptions;

const MAX_DIR_DEPTH: usize = 32;
const DIRENT_BUF_SIZE: usize = 2048;
const MKDIR_MODE: u32 = 0o744;
const FILE_MODE: u32 = 0o644;

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
    // Entry,
    // ReadDir,
    // FileType,
    OpenAt,
    Getdents,
    DirTooDeep,
    MkdirAt,
    // NotADir,
    // FdOpenDir,
    Fstat,
    OnFile,
    OnDir,
    OnPop,
    Write,
    SendFile,
    Flush,
    BadName,
    BadSize,
    EmptyStack,
    // StackEmpty,
}

pub enum ArchiveFormat1Tag {
    File = 1,
    Dir = 2,
    Pop = 3,
}

pub trait Visitor {
    fn on_file(&mut self, name: &CStr, size: u64, fd: OwnedFd) -> Result<(), Error>;
    fn on_dir(&mut self, name: &CStr) -> Result<(), Error>;
    fn leave_dir(&mut self) -> Result<(), Error>;
}

fn chroot(dir: &Path) {
    use std::os::unix::fs;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    unsafe {
        let ret = libc::unshare(libc::CLONE_NEWUSER);
        assert!(ret == 0, "unshare fail");
    }
    File::create("/proc/self/uid_map").unwrap()
        .write_all(format!("0 {} 1", uid).as_bytes()).unwrap();
    File::create("/proc/self/setgroups").unwrap().write_all(b"deny").unwrap();
    File::create("/proc/self/gid_map").unwrap()
        .write_all(format!("0 {} 1", gid).as_bytes()).unwrap();
    fs::chroot(dir).unwrap();
    std::env::set_current_dir("/").unwrap();
}

fn openat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat_w<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC, FILE_MODE);
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

fn openpathat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_DIRECTORY | libc::O_PATH | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn mkdirat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<(), Error> {
    unsafe {
        let ret = libc::mkdirat(fd.as_raw_fd(), name.as_ptr(), MKDIR_MODE);
        if ret < 0 { return Err(Error::MkdirAt); }
        Ok(())
    }
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

fn read_le_u32(input: &mut &[u8]) -> Result<u32, Error> {
    let (int_bytes, rest) = input.split_at(std::mem::size_of::<u32>());
    *input = rest;
    Ok(u32::from_le_bytes(int_bytes.try_into().map_err(|_| Error::BadSize)?))
}

fn munch_cstr(input: &mut &[u8]) -> Result<(), Error> {
    // memchr ...
    let zbi = input.iter().position(|&x| x == 0).ok_or(Error::BadName)?;
    *input = &input[zbi+1..];
    Ok(())
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
    fn on_file(&mut self, name: &CStr, size: u64, mut fd: OwnedFd) -> Result<(), Error> {
        let size_u32: u32 = size.try_into().map_err(|_| Error::Write)?;
        self.writer.write_all(&[ArchiveFormat1Tag::File as u8]).map_err(|_| Error::Write)?;
        self.writer.write_all(name.to_bytes_with_nul()).map_err(|_| Error::Write)?;
        self.writer.write_all(&size_u32.to_le_bytes()).map_err(|_| Error::Write)?;
        self.writer.flush().map_err(|_| Error::Flush)?;
        sendfile_all(&mut fd, self.writer.get_mut(), size)?;
        Ok(())
    }

    fn on_dir(&mut self, name: &CStr) -> Result<(), Error> {
        self.writer.write_all(&[ArchiveFormat1Tag::Dir as u8]).map_err(|_| Error::Write)?;
        self.writer.write_all(name.to_bytes_with_nul()).map_err(|_| Error::Write)?;
        Ok(())
    }

    fn leave_dir(&mut self) -> Result<(), Error> {
        self.writer.write_all(&[ArchiveFormat1Tag::Pop as u8]).map_err(|_| Error::Write)?;
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

/// deemed unsafe because we unpack to cwd with no path traversal protection, caller should ensure
/// we are in a chroot or otherwise protected
unsafe fn unpack_to_cwd(data: &[u8], starting_dir: OwnedFd) -> Result<(), Error> {
    let mut stack: Vec<OwnedFd> = Vec::with_capacity(32);  // always non-empty
    stack.push(starting_dir);

    let mut cur = data;
    loop {
        match cur.get(0).map(|x| x.try_into()) {
            Some(Ok(ArchiveFormat1Tag::File)) => {
                cur = &cur[1..];
                let parent = stack.last().unwrap();
                let name = unsafe { CStr::from_bytes_with_nul_unchecked(cur) };
                let mut file: File = openat_w(parent, name)?.into();
                munch_cstr(&mut cur)?;
                let len = read_le_u32(&mut cur)? as usize;
                file.write_all(&cur[..len]).unwrap();
                cur = &cur[len..];
            },
            Some(Ok(ArchiveFormat1Tag::Dir)) => {
                cur = &cur[1..];
                let parent = stack.last().unwrap();
                let name = unsafe { CStr::from_bytes_with_nul_unchecked(cur) };
                mkdirat(parent, name).unwrap();
                munch_cstr(&mut cur)?;
                match cur.get(0).map(|x| x.try_into()) {
                    Some(Ok(ArchiveFormat1Tag::Pop)) => {
                        // fast path for empty dir, never open the dir and push it
                        // advance past Pop
                        cur = &cur[1..];
                    },
                    Some(Ok(_)) => {
                        stack.push(openpathat(parent, name)?);
                    }
                    _ => {
                        // will fail in outer
                    }
                }
            },
            Some(Ok(ArchiveFormat1Tag::Pop)) => {
                cur = &cur[1..];
                // always expected to be nonempty, todo handle gracefully for malicious archives
                stack.pop().ok_or(Error::EmptyStack)?;
            },
            Some(Err(_)) => {
                let b = cur[0];
                panic!("oh no got bad tag byte {b}");
            },
            None => {
                break;
            }
        }
    }
    Ok(())
}

pub fn unpack_file_to_dir_with_chroot(file: File, dir: &Path) -> Result<(), Error> {
    let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };

    chroot(&dir);

    let starting_dir = opendirat_cwd(c".")?;

    unsafe { unpack_to_cwd(mmap.as_ref(), starting_dir) }

}
