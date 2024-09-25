use std::collections::HashSet;
use std::path::Path;
use std::io;
use std::io::{stdin,BufRead,Write,BufWriter,Seek,SeekFrom};
use std::ffi::OsString;
use std::os::unix::prelude::OsStrExt;
use std::os::fd::{FromRawFd,AsRawFd,IntoRawFd};
use std::fs::File;
use memmap::MmapOptions;
use std::fs;
use std::path::PathBuf;
use std::ffi::{CStr,OsStr};

/// archive format
/// num_dirs: u32le 
/// num_files: u32le
/// dirnames_size: u32le
/// filenames_size: u32le
/// <dirnames with null bytes> of length dirnames_size bytes
/// <filenames with null bytes> of length filenames_size bytes
/// 0-3 padding bytes to align file_sizes up to 4 byte alignment
/// <num_files x u32le file sizes> of length num_files * 4 bytes
/// <data>

// pkg pearchive;

// default fd table size is 64, we 3 + 1 open by default but we don't want to go to fd 257 because
// that would trigger a realloc and then we waste, so this should always be 4 less than a power of
// 2. Seems like diminishing returns
const NUM_OPEN_FDS: i32 = 256 - 4;
const MAX_DIR_DEPTH: usize = 1024;

#[derive(Debug)]
pub enum Error {
    Align,
    Open,
    Write,
    Stat,
    Seek,
    FileSizeMismatch,
    FileSizeTooBig,
    Slice,
    NotADir,
    ReadDir,
    Entry,
    FileType,
    DirTooDeep,
    DirEntName,
    FdOpenDir,
    OpenAt,
}

fn as_slice<T>(data: &[u8]) -> Option<&[T]> {
    let len = data.len();
    let ptr = data.as_ptr();
    let align = std::mem::align_of::<T>();
    if len % align != 0 { return None; }
    if (ptr as usize) % align != 0 { return None; }
    unsafe {
        let ptr = ptr as *const T;
        Some(std::slice::from_raw_parts(ptr, len / align))
    }
}

fn u32_slice_as_u8_slice(data: &[u32]) -> Option<&[u8]> {
    let len = data.len().checked_mul(4)?;
    unsafe {
        let ptr = data.as_ptr() as *const u8;
        Some(std::slice::from_raw_parts(ptr, len))
    }
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

fn compute_dirs(files: &[OsString]) -> Result<Vec::<OsString>, Error> {
    let mut acc = HashSet::new();
    let empty = OsString::new();
    for file in files {
        let p = Path::new(&file);
        for parent in p.ancestors().skip(1) {
            if parent != empty {
                acc.insert(parent.to_owned().into_os_string());
            }
        }
    }
    let mut acc: Vec<_> = acc.drain().collect();
    acc.sort();
    Ok(acc)
}

fn align_to_4<W: Seek + Write>(writer: &mut W) -> Result<(), Error> {
    let pos = writer.stream_position().map_err(|_| Error::Align)?;
    let adj = 4 - (pos % 4);
    for _ in 0..adj { writer.write_all(&[0]).map_err(|_| Error::Align)?; }
    let pos = writer.stream_position().map_err(|_| Error::Align)?;
    //println!("wrote {} bytes of padding, pos now {}", adj, pos);
    assert!(pos % 4 == 0);
    Ok(())
}

fn write_zero_separated<'a, I: Iterator<Item = &'a [u8]>, W: Write>(xs: I, out: &mut W) -> Result<usize, Error> {
    let mut size = 0;
    for x in xs {
        size += 1 + x.len();
        out.write_all(x).map_err(|_| Error::Write)?;
        out.write_all(&[0]).map_err(|_| Error::Write)?;
    }
    return Ok(size);
}


fn dirent_name_ptr(dirent: &*const libc::dirent) -> *const i8 {
    use std::mem;
    const offset: isize = mem::offset_of!(libc::dirent, d_name) as isize;
    unsafe { dirent.byte_offset(offset) as *const i8 }
}

fn dirent_name_osstr(dirent: &*const libc::dirent) -> &OsStr {
    let cname = unsafe { CStr::from_ptr(dirent_name_ptr(dirent)) };
    unsafe { OsStr::from_encoded_bytes_unchecked(cname.to_bytes()) }
}

fn dirent_name_cstr(dirent: &*const libc::dirent) -> &CStr {
    unsafe { CStr::from_ptr(dirent_name_ptr(dirent)) }
}

struct DIR {
    dirp: *mut libc::DIR,
    file: File,
}

fn fdopendir(file: &File) -> Result<*mut libc::DIR, Error> {
    let p = unsafe {
        libc::fdopendir(file.as_raw_fd())
    };
    if p.is_null() { return Err(Error::FdOpenDir); }
    Ok(p)
}

impl DIR {
    fn open(path: &Path) -> Result<Self, Error> {
        // TODO open this with O_DIRECTORY
        let file = File::open(path).map_err(|_| Error::Open)?;
        let dirp = fdopendir(&file)?;
        Ok(Self { dirp: dirp, file: file })
    }

    fn readdir(&mut self) -> Option<*const libc::dirent> {
        // TODO this mofo is calling fcntl(_, F_GETFL) then fcntl(_, F_SETFD, O_CLOEXEC) all the
        // time
        let ret = unsafe { libc::readdir(self.dirp) };
        if ret.is_null() { return None; }
        Some(ret)
    }

    fn openat(&self, dirent: *const libc::dirent) -> Result<Self, Error> {
        let file = unsafe {
            let ret = libc::openat(self.file.as_raw_fd(), dirent_name_ptr(&dirent), libc::O_RDONLY | libc::O_CLOEXEC);
            if ret < 0 { return Err(Error::OpenAt) }
            File::from_raw_fd(ret)
        };
        let dirp = fdopendir(&file)?;
        Ok(Self { dirp: dirp, file: file })
    }
}

fn list_dir_c_rec(curpath: &mut PathBuf, dirp: &mut DIR, dirs: &mut Vec::<OsString>, files: &mut Vec::<OsString>, depth: usize) -> Result<(), Error> {
    if depth > MAX_DIR_DEPTH { return Err(Error::DirTooDeep); }

    while let Some(dirent) = dirp.readdir() {
        let d_type = unsafe { (*dirent).d_type };
        match d_type {
            libc::DT_REG => {
                // TODO is this zero copy?
                files.push(curpath.join(dirent_name_osstr(&dirent)).into());
            },
            libc::DT_DIR => {
                let cstr = dirent_name_cstr(&dirent).to_bytes();
                if cstr == b"." || cstr == b".." {
                    continue;
                }
                dirs.push(curpath.join(dirent_name_osstr(&dirent)).into());
                curpath.push(dirent_name_osstr(&dirent));
                let mut newdir = dirp.openat(dirent)?;
                list_dir_c_rec(curpath, &mut newdir, dirs, files, depth + 1)?;
                curpath.pop();
            }
            _ => {}
        }
    }
    Ok(())
}

// struct DIR {
//     dir: *libc::DIR
// }

// impl From<File: AsRawFd> for DIR {
//     type Error = Error;
//     fn try_from(file: &File) -> Result<Self, Error> {
//         let p = libc::fopendir(dirfile.as_raw_fd());
//         if p == 0 { return Err(Error::OpenDir); }
//         Ok(Self { dir: p })
//     }
// }

pub fn list_dir_c(dir: &Path) -> Result<(Vec<OsString>, Vec<OsString>), Error> {
    use std::ffi::CString;
    // let dirfile = File::open(dir).map_err(|_| Error::Open)?;
    // //let dirp: DIR = dirfile
    // let dirp = unsafe {
    //     let p = libc::fdopendir(dirfile.as_raw_fd());
    //     if p.is_null() { return Err(Error::FdOpenDir); }
    //     p
    // };
    let mut dirp = DIR::open(dir)?;
    let mut dirs: Vec::<OsString> = vec![];
    let mut files: Vec::<OsString> = vec![];
    let mut curpath = PathBuf::new();
    list_dir_c_rec(&mut curpath, &mut dirp, &mut dirs, &mut files, 0)?;
    Ok((dirs, files))
}

fn list_dir_rec(curpath: &mut PathBuf, dir: &Path, dirs: &mut Vec::<OsString>, files: &mut Vec::<OsString>, depth: usize) -> Result<(), Error> {
    if depth > MAX_DIR_DEPTH { return Err(Error::DirTooDeep); }
    // TODO it would be great to have a read_dir for a direntry so it could use openat
    for entry in dir.read_dir().map_err(|_| Error::ReadDir)? {
        let entry = entry.map_err(|_| Error::Entry)?;
        let ftype = entry.file_type().map_err(|_| Error::FileType)?;
        if ftype.is_file() {
            files.push(curpath.join(entry.file_name()).into());
        } else if ftype.is_dir() {
            curpath.push(entry.file_name());
            dirs.push(curpath.clone().into());
            list_dir_rec(curpath, entry.path().as_ref(), dirs, files, depth + 1)?;
        }
    }
    curpath.pop();
    files.sort();
    dirs.sort();
    Ok(())
}

pub fn list_dir(dir: &Path) -> Result<(Vec<OsString>, Vec<OsString>), Error> {
    if !dir.is_dir() { return Err(Error::NotADir); }
    let mut dirs: Vec::<OsString> = vec![];
    let mut files: Vec::<OsString> = vec![];
    let mut curpath = PathBuf::new();
    // let mut stack: Vec::<PathBuf> = Vec::with_capacity(32);
    // stack.push(dir.into());
    // while let Some(dir) = stack.pop() {
    //     for entry in dir.read_dir().map_err(|_| Error::ReadDir)? {
    //         let entry = entry.map_err(|_| Error::Entry)?;
    //         let ftype = entry.file_type().map_err(|_| Error::FileType)?;
    //         let mut haddir = false;
    //         if ftype.is_file() {
    //             files.push(curpath.join(entry.file_name()).into());
    //         } else if ftype.is_dir() {
    //             stack.push(entry.path().into());
    //             dirs.push(curpath.join(entry.file_name()).into());
    //             haddir = true;
    //         }
    //         if haddir {
    //             curpath = 
    //         } else {
    //         }
    //     }
    // }
    list_dir_rec(&mut curpath, dir, &mut dirs, &mut files, 0)?;
    files.sort();
    dirs.sort();
    Ok((dirs, files))
}

pub fn pack_dir(dir: &Path, outfile: &mut File) -> Result<(), Error> {
    if !dir.is_dir() { return Err(Error::NotADir); }
    Ok(())
}

pub fn pack_files(files: &[OsString], outfile: &mut File) -> Result<(), Error> {
    let dirs = compute_dirs(&files[..]).unwrap();
    pack_parts(dirs.as_slice(), files, outfile)
}

/// dirs: sorted 
/// files: sorted & all files
pub fn pack_parts(dirs: &[OsString], files: &[OsString], outfile: &mut File) -> Result<(), Error> {
    // TODO verify this uses the seek pos from the underlying file
    let mut outwriter = BufWriter::new(outfile);

    let starting_pos = outwriter.stream_position().map_err(|_| Error::Seek)?;

    // skip header sizes
    outwriter.seek(SeekFrom::Current(4 * 4)).map_err(|_| Error::Seek)?;

    // write dirs and files
    let dirsb_len = write_zero_separated(dirs.iter().map(|x| x.as_bytes()), &mut outwriter)?;
    let filesb_len = write_zero_separated(files.iter().map(|x| x.as_bytes()), &mut outwriter)?;

    // write header
    {
        outwriter.seek(SeekFrom::Start(starting_pos)).map_err(|_| Error::Seek)?;
        for i in [dirs.len(), files.len(), dirsb_len, filesb_len] {
            outwriter.write_all(&(i as u32).to_le_bytes()).map_err(|_| Error::Write)?;
        }
        outwriter.seek(SeekFrom::Start(starting_pos)).map_err(|_| Error::Seek)?;
    }
    
    // align & save position for file sizes table
    align_to_4(&mut outwriter)?;
    let sizes_pos = outwriter.stream_position().map_err(|_| Error::Seek)?;
    outwriter.seek(SeekFrom::Current((files.len() * 4).try_into().unwrap())).map_err(|_| Error::Seek)?;

    let mut sizes: Vec::<u32> = Vec::with_capacity(files.len());

    for file in files {
        let mut f = File::open(file).map_err(|_| Error::Open)?;
        let len = f.metadata().map_err(|_| Error::Stat)?.len();
        if len > u32::MAX as u64 { return Err(Error::FileSizeTooBig); }
        sizes.push(len as u32);
        io::copy(&mut f, &mut outwriter).map_err(|_| Error::Write)?;
    }

    outwriter.seek(SeekFrom::Start(sizes_pos)).map_err(|_| Error::Seek)?;
    outwriter.write_all(u32_slice_as_u8_slice(sizes.as_slice()).ok_or(Error::Slice)?).map_err(|_| Error::Write)?;

    Ok(())
}



// fn copy_file_range_all(filein: &mut File, fileout: &mut File, len: usize) -> Result<(), Error> {
//     let fd_in  = filein.as_raw_fd();
//     let fd_out = fileout.as_raw_fd();
//     let mut len = len;
//     while len > 0 {
//         let ret = unsafe {
//             libc::copy_file_range(fd_in, ptr::null_mut(), fd_out, ptr::null_mut(), len, 0)
//         };
//         if ret < 0 { return Err(Error::CopyFileRange); }
//         if ret == 0 { return Err(Error::CopyFileRange); }
//         let ret = ret as usize;
//         if ret > len { return Err(Error::CopyFileRange); }
//         len -= ret;
//     }
//     Ok(())
// }

// /// args <infile> <output dir> 
// ///   <output dir> should be empty
// fn unpack_v0(args: &[String]) {
//     let inname = args.get(0).ok_or(Error::NoOutfile).unwrap();
//     let outname = args.get(1).ok_or(Error::NoOutfile).unwrap();
//     let use_copy_file = { 
//         if let Some(s) = args.get(2) {
//             s == "copy_file_range"
//         } else {
//             false
//         }
//     };
//     println!("use_copy_file={}", use_copy_file);
//     let inpath = Path::new(&inname);
//     let outpath = Path::new(&outname);
//     assert!(inpath.is_file(), "{:?} should be a file", inpath);
//     assert!(outpath.is_dir(), "{:?} should be a dir", outpath);
//     let mut infile = File::open(inpath).unwrap();
//     let mmap = unsafe { MmapOptions::new().map(&infile).unwrap() };
//     let (num_dirs, num_files, dirnames_size, filenames_size) = {
//         (
//             u32::from_le_bytes(mmap[0..4].try_into().unwrap()) as usize,
//             u32::from_le_bytes(mmap[4..8].try_into().unwrap()) as usize,
//             u32::from_le_bytes(mmap[8..12].try_into().unwrap()) as usize,
//             u32::from_le_bytes(mmap[12..16].try_into().unwrap()) as usize,
//         )
//     };
//     let dirnames_start = 4 * 4;
//     let filenames_start = dirnames_start + dirnames_size;
//     let filesizes_start = {
//         let mut x = filenames_start + filenames_size;
//         if x % 4 != 0 {
//             let adj = 4 - (x % 4);
//             x += adj;
//         }
//         x
//     };
//     assert!(filesizes_start % 4 == 0, "filesizes_start={}", filesizes_start);
//     let data_start = filesizes_start + (4 * num_files);
// 
//     chroot(&outpath);
// 
//     {
//         let mut dirnames_cur = &mmap[dirnames_start..filenames_start];
//         for _ in 0..num_dirs {
//             unsafe {
//                 let ret = libc::mkdir(dirnames_cur.as_ptr() as *const i8, 0o755);
//                 assert!(ret == 0, "mkdir failed");
//             }
//             // idk is it better to do dirnames_buf.split(0)? 
//             let zbi = dirnames_cur.iter().position(|&x| x == 0).unwrap();
//             dirnames_cur = &dirnames_cur[zbi+1..];
//         }
//     }
// 
//     // kinda ugly
//     if use_copy_file {
//         let mut filenames_cur = &mmap[filenames_start..filesizes_start];
//         let filesizes = as_slice::<u32>(&mmap[filesizes_start..data_start]).unwrap();
//         assert!(filesizes.len() == num_files);
//         infile.seek(SeekFrom::Start(data_start as u64)).unwrap();
//         for size in filesizes {
//             let size = *size as usize;
//             let mut fileout = unsafe {
//                 let fd = libc::open(filenames_cur.as_ptr() as *const i8, libc::O_CREAT | libc::O_WRONLY, 0o755);
//                 assert!(fd > 0, "open failed");
//                 File::from_raw_fd(fd)
//             };
//             copy_file_range_all(&mut infile, &mut fileout, size).unwrap();
//             let zbi = filenames_cur.iter().position(|&x| x == 0).unwrap();
//             filenames_cur = &filenames_cur[zbi+1..];
//         };
// 
//     } else {
//         let mut filenames_cur = &mmap[filenames_start..filesizes_start];
//         let filesizes = as_slice::<u32>(&mmap[filesizes_start..data_start]).unwrap();
//         assert!(filesizes.len() == num_files);
//         let mut data_cur = &mmap[data_start..];
// 
//         let mut close_every: i32 = NUM_OPEN_FDS;
// 
//         for size in filesizes {
//             let size = *size as usize;
//             let mut fileout = unsafe {
//                 let fd = libc::open(filenames_cur.as_ptr() as *const i8, libc::O_CREAT | libc::O_WRONLY, 0o755);
//                 assert!(fd > 0, "open failed");
//                 File::from_raw_fd(fd)
//             };
//             let data = &data_cur[..size];
//             assert!(data.len() == size);
//             fileout.write_all(data).unwrap();
//             data_cur = &data_cur[size..];
// 
//             let _ = fileout.into_raw_fd();
//             close_every -= 1;
//             if close_every == 0 {
//                 unsafe {
//                     // TODO if this was in a lib we'd want to figure out our current fd that we'll
//                     // go into and/or verify there aren't any random fds above us but not sure you
//                     // can do that well so maybe this is only a go if we're a standalone exe
//                     libc::close_range(4, std::u32::MAX, 0);
//                 }
//                 close_every = NUM_OPEN_FDS;
//             }
// 
//             let zbi = filenames_cur.iter().position(|&x| x == 0).unwrap();
//             filenames_cur = &filenames_cur[zbi+1..];
//         }
//     }
// 
//     // TODO if this was in a lib we'd want to do another libc::close_range(4, std::u32::MAX, 0)
//     // here
// }
