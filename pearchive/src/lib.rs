use std::os::fd::{AsRawFd,OwnedFd};
use std::fs;
use std::fs::File;
use std::path::Path;
use std::ffi::{CStr,CString};
use std::io::{Write,BufWriter};

use rustix::fs::{RawDir,FileType};
use memmap2::MmapOptions;

mod open;
use open::{openat,opendirat_cwd,openat_w,opendirat,openpathat};

const MAX_DIR_DEPTH: usize = 32;
const DIRENT_BUF_SIZE: usize = 2048;
const MKDIR_MODE: u32 = 0o744;
const FILE_MODE: u32 = 0o644;
const MAX_NAME_LEN: usize = 255; // max len on tmpfs

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
    Create,
    OpenAt,
    Getdents,
    DirTooDeep,
    MkdirAt,
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
    BadTag,
    ArchiveTruncated,
    Chdir,
    Chroot,
    Unshare,
    Mmap,
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

fn unshare_user() -> Result<(), Error> {
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    unsafe {
        let ret = libc::unshare(libc::CLONE_NEWUSER);
        if ret < 0 { return Err(Error::Unshare); }
    }
    fs::write("/proc/self/uid_map", format!("0 {} 1", uid).as_bytes())
        .map_err(|_| Error::Write)?;
    fs::write("/proc/self/setgroups", b"deny")
        .map_err(|_| Error::Write)?;
    fs::write("/proc/self/gid_map", format!("0 {} 1", gid).as_bytes())
        .map_err(|_| Error::Write)?;
    Ok(())
}

fn chroot(dir: &Path) -> Result<(), Error> {
    use std::os::unix::fs;
    fs::chroot(dir).map_err(|_| Error::Chroot)?;
    std::env::set_current_dir("/").map_err(|_| Error::Chdir)?;
    Ok(())
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
    // we don't care about an empty name, will fail on open
    for i in 0..=MAX_NAME_LEN {
        if input[i] == 0 {
            *input = &input[i+1..];
            return Ok(());
        }
    }
    return Err(Error::BadName);
}

fn read_cstr<'a>(input: &mut &'a [u8]) -> Result<&'a CStr, Error> {
    let ret = unsafe { CStr::from_bytes_with_nul_unchecked(input) };
    munch_cstr(input)?;
    Ok(ret)
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

    fn into_file(self) -> Result<File, Error> {
        self.writer.into_inner().map_err(|_| Error::Write)
    }
}

impl Visitor for PackToFileVisitor {
    fn on_file(&mut self, name: &CStr, size: u64, mut fd: OwnedFd) -> Result<(), Error> {
        // println!("UNPACK file {name:?} {size}");
        let size_u32: u32 = size.try_into().map_err(|_| Error::Write)?;
        self.writer.write_all(&[ArchiveFormat1Tag::File as u8]).map_err(|_| Error::Write)?;
        self.writer.write_all(name.to_bytes_with_nul()).map_err(|_| Error::Write)?;
        self.writer.write_all(&size_u32.to_le_bytes()).map_err(|_| Error::Write)?;
        self.writer.flush().map_err(|_| Error::Flush)?;
        sendfile_all(&mut fd, self.writer.get_mut(), size)?;
        Ok(())
    }

    fn on_dir(&mut self, name: &CStr) -> Result<(), Error> {
        // println!("UNPACK dir {name:?}");
        self.writer.write_all(&[ArchiveFormat1Tag::Dir as u8]).map_err(|_| Error::Write)?;
        self.writer.write_all(name.to_bytes_with_nul()).map_err(|_| Error::Write)?;
        Ok(())
    }

    fn leave_dir(&mut self) -> Result<(), Error> {
        // println!("UNPACK pop");
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
                //v.on_file(name, size, fd).map_err(|_| Error::OnFile)?;
                v.on_file(name, size, fd)?;
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
    visitor.into_file()
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
                let name = read_cstr(&mut cur)?;
                let len = read_le_u32(&mut cur)? as usize;
                if len > cur.len() { return Err(Error::ArchiveTruncated); }
                let mut file: File = openat_w(parent, name)?.into();
                file.write_all(&cur[..len]).map_err(|_| Error::Write)?;
                cur = &cur[len..];
            },
            Some(Ok(ArchiveFormat1Tag::Dir)) => {
                cur = &cur[1..];
                let parent = stack.last().unwrap();
                let name = read_cstr(&mut cur)?;
                mkdirat(parent, name).unwrap();
                match cur.get(0).map(|x| x.try_into()) {
                    Some(Ok(ArchiveFormat1Tag::Pop)) => {
                        // fast path for empty dir, never open the dir or push it
                        // advance past Pop
                        cur = &cur[1..];
                    },
                    Some(Ok(_)) => {
                        stack.push(openpathat(parent, name)?);
                    }
                    _ => {
                        // handled in outer match next loop
                    }
                }
            },
            Some(Ok(ArchiveFormat1Tag::Pop)) => {
                cur = &cur[1..];
                stack.pop().ok_or(Error::EmptyStack)?;
            },
            Some(Err(_)) => {
                return Err(Error::BadTag);
            },
            None => {
                // idk if I like this
                return (stack.len() == 1).then_some(()).ok_or(Error::ArchiveTruncated);
                // if stack.len() != 1 {
                //     return Err(Error::ArchiveTruncated);
                // } else {
                //     return Ok(())
                // }
            }
        }
    }
}

pub fn unpack_file_to_dir_with_unshare_chroot(file: File, dir: &Path) -> Result<(), Error> {
    let mmap = unsafe { MmapOptions::new().map(&file).map_err(|_| Error::Mmap)? };
    unpack_data_to_dir_with_unshare_chroot(mmap.as_ref(), dir)
}

pub fn unpack_data_to_dir_with_unshare_chroot(data: &[u8], dir: &Path) -> Result<(), Error> {
    unshare_user()?;
    chroot(&dir)?;

    let starting_dir = opendirat_cwd(c".")?;

    unsafe { unpack_to_cwd(data, starting_dir) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::ffi::OsString;
    use std::os::fd::FromRawFd;
    use std::fs;
    //use std::thread;
    use std::process::Command;

    use rand;
    use rand::distributions::DistString;

    struct TempDir { name: OsString }

    impl TempDir {
        fn new() -> Self {
            let rng = rand::distributions::Alphanumeric.sample_string(&mut rand::thread_rng(), 8);
            let ret = Self { name: format!("/tmp/charchive-{rng}").into() };
            std::fs::create_dir(&ret.name).unwrap();
            ret
        }

        fn join<O: AsRef<Path>>(&self, other: O) -> PathBuf { self.as_ref().join(other) }

        fn file(self, name: &str, data: &[u8]) -> Self {
            File::create(&self.join(name)).unwrap().write_all(data).unwrap();
            self
        }

        fn dir(self, name: &str) -> Self {
            fs::create_dir(self.join(name)).unwrap();
            self
        }

        #[allow(dead_code)]
        fn digest(&self) -> String {
            let output = Command::new("sh")
                .current_dir(self)
                .arg("-c")
                .arg("cat <(find -type f -exec sha256sum '{}' '+' | sort) <(find -type d | sort) | sha256sum")
                .output();
            String::from_utf8(output.unwrap().stdout).unwrap()
        }
    }

    impl AsRef<Path> for TempDir { fn as_ref(&self) -> &Path { return Path::new(&self.name) } }
    impl Drop for TempDir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(self); } }

    fn tempfile() -> File {
        unsafe {
            let ret = libc::open(c"/tmp".as_ptr(), libc::O_TMPFILE | libc::O_RDWR, 0o600);
            assert!(ret > 0);
            File::from_raw_fd(ret)
        }
    }

    #[test]
    fn basic_pack() {
        let td1 = TempDir::new()
            .file("file-1", b"hello world")
            .file("file-2", b"yooo")
            .dir("adir")
            .file("adir/another-file", b"some data");
        // let td2 = TempDir::new().unwrap();

        let f = pack_dir_to_file(td1.as_ref(), tempfile()).unwrap();
        assert!(f.metadata().unwrap().len() > 0);

        // can shell out to actual program
        // but then annoyingly we have to link the tempfile
        // println!("{}", std::env::current_exe().unwrap().display());
        // TODO we can't use CLONE_NEWUSER in a threaded program;
        // thread::scope(|s| {
        //     s.spawn(|| {
        //         unpack_file_to_dir_with_unshare_chroot(f, td2.as_ref()).unwrap();
        //     });
        // });

        // assert_eq!(td1.digest(), td2.digest());
    }

    #[test]
    fn pack_name_max_length_ok() {
        let name255 = String::from_utf8(vec![97u8; 255]).unwrap();
        let td1 = TempDir::new().file(&name255, b"hello world");
        assert!(pack_dir_to_file(td1.as_ref(), tempfile()).is_ok());
    }
    #[test]
    #[should_panic]
    fn pack_name_max_length_too_long() {
        let name256 = String::from_utf8(vec![97u8; 256]).unwrap();
        let _ = TempDir::new().file(&name256, b"hello world");
        // fail at creation of file
        // assert!(pack_dir_to_file(td1.as_ref(), tempfile()).is_err());
    }
}
