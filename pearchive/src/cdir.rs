
// this is remnants of using libc to write a "better" dir iterator that could use openat then
// fdopendir instead of passing in a whole path everythime, but fdopendir makes 3 extra syscalls!
// too many

// fn dirent_name_ptr(dirent: &*const libc::dirent) -> *const i8 {
//     use std::mem;
//     const offset: isize = mem::offset_of!(libc::dirent, d_name) as isize;
//     unsafe { dirent.byte_offset(offset) as *const i8 }
// }
// 
// fn dirent_name_osstr(dirent: &*const libc::dirent) -> &OsStr {
//     let cname = unsafe { CStr::from_ptr(dirent_name_ptr(dirent)) };
//     unsafe { OsStr::from_encoded_bytes_unchecked(cname.to_bytes()) }
// }
// 
// fn dirent_name_cstr(dirent: &*const libc::dirent) -> &CStr {
//     unsafe { CStr::from_ptr(dirent_name_ptr(dirent)) }
// }
// 
// struct DIR {
//     // TODO dir takes over the file so long as we close with closedir it will close the fd and free
//     // it
//     dirp: *mut libc::DIR,
//     file: File,
// }
// 
// fn fdopendir(file: &File) -> Result<*mut libc::DIR, Error> {
//     let p = unsafe {
//         libc::fdopendir(file.as_raw_fd())
//     };
//     if p.is_null() { return Err(Error::FdOpenDir); }
//     Ok(p)
// }
// 
// impl DIR {
//     fn open(path: &Path) -> Result<Self, Error> {
//         // TODO open this with O_DIRECTORY ?
//         let file = File::open(path).map_err(|_| Error::Open)?;
//         // this calls fcntl F_GETFD to make sure the fd isn't opened with O_PATH
//         // then it unconditionally calls fcntl F_SETFD O_CLOEXEC
//         // and it calls stat, so 3 syscalls :(
//         let dirp = fdopendir(&file)?;
//         Ok(Self { dirp: dirp, file: file })
//     }
// 
//     fn readdir(&mut self) -> Option<*const libc::dirent> {
//         let ret = unsafe { libc::readdir(self.dirp) };
//         if ret.is_null() { return None; }
//         Some(ret)
//     }
// 
//     fn openat(&self, dirent: *const libc::dirent) -> Result<Self, Error> {
//         let file = unsafe {
//             let ret = libc::openat(self.file.as_raw_fd(), dirent_name_ptr(&dirent), libc::O_RDONLY | libc::O_CLOEXEC);
//             if ret < 0 { return Err(Error::OpenAt) }
//             File::from_raw_fd(ret)
//         };
//         let dirp = fdopendir(&file)?;
//         Ok(Self { dirp: dirp, file: file })
//     }
// }
// 
// fn list_dir_c_rec(curpath: &mut PathBuf, dirp: &mut DIR, dirs: &mut Vec::<OsString>, files: &mut Vec::<OsString>, depth: usize) -> Result<(), Error> {
//     if depth > MAX_DIR_DEPTH { return Err(Error::DirTooDeep); }
// 
//     while let Some(dirent) = dirp.readdir() {
//         let d_type = unsafe { (*dirent).d_type };
//         match d_type {
//             libc::DT_REG => {
//                 // TODO is this zero copy?
//                 files.push(curpath.join(dirent_name_osstr(&dirent)).into());
//             },
//             libc::DT_DIR => {
//                 let cstr = dirent_name_cstr(&dirent).to_bytes();
//                 if cstr == b"." || cstr == b".." {
//                     continue;
//                 }
//                 dirs.push(curpath.join(dirent_name_osstr(&dirent)).into());
//                 curpath.push(dirent_name_osstr(&dirent));
//                 let mut newdir = dirp.openat(dirent)?;
//                 list_dir_c_rec(curpath, &mut newdir, dirs, files, depth + 1)?;
//                 curpath.pop();
//             }
//             // TODO apparently DT_UNKNOWN is possible on some fs's like xfs and you have to do a
//             // stat call
//             _ => {}
//         }
//     }
//     Ok(())
// }
// 
// // struct DIR {
// //     dir: *libc::DIR
// // }
// 
// // impl From<File: AsRawFd> for DIR {
// //     type Error = Error;
// //     fn try_from(file: &File) -> Result<Self, Error> {
// //         let p = libc::fopendir(dirfile.as_raw_fd());
// //         if p == 0 { return Err(Error::OpenDir); }
// //         Ok(Self { dir: p })
// //     }
// // }
// 
// pub fn list_dir_c(dir: &Path) -> Result<(Vec<OsString>, Vec<OsString>), Error> {
//     use std::ffi::CString;
//     // let dirfile = File::open(dir).map_err(|_| Error::Open)?;
//     // //let dirp: DIR = dirfile
//     // let dirp = unsafe {
//     //     let p = libc::fdopendir(dirfile.as_raw_fd());
//     //     if p.is_null() { return Err(Error::FdOpenDir); }
//     //     p
//     // };
//     let mut dirp = DIR::open(dir)?;
//     let mut dirs: Vec::<OsString> = vec![];
//     let mut files: Vec::<OsString> = vec![];
//     let mut curpath = PathBuf::new();
//     list_dir_c_rec(&mut curpath, &mut dirp, &mut dirs, &mut files, 0)?;
//     Ok((dirs, files))
// }
// 
