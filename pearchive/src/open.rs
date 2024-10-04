use std::os::fd::{FromRawFd,AsRawFd,OwnedFd};
use std::ffi::CStr;
use crate::{FILE_MODE,Error};

pub fn openat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn openat_w<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC, FILE_MODE);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn opendirat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn opendirat_cwd(name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(libc::AT_FDCWD, name.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn openpathat<Fd: AsRawFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    let fd = unsafe {
        let ret = libc::openat(fd.as_raw_fd(), name.as_ptr(), libc::O_DIRECTORY | libc::O_PATH | libc::O_CLOEXEC);
        if ret < 0 { return Err(Error::OpenAt); }
        ret
    };
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
