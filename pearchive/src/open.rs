use crate::{Error, FILE_MODE, MKDIR_MODE};
use std::ffi::CStr;

use rustix::{
    fd::{AsFd, OwnedFd},
    fs::{Mode, OFlags, ResolveFlags},
};

pub(crate) fn openat<Fd: AsFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    rustix::fs::openat2(
        fd,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH,
    )
    .map_err(|_| Error::OpenAt)
}

pub(crate) fn openat_w<Fd: AsFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    rustix::fs::openat2(
        fd,
        name,
        OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from_bits_truncate(FILE_MODE),
        ResolveFlags::BENEATH,
    )
    .map_err(|_| Error::OpenAt)
}

pub(crate) fn opendir(name: &CStr) -> Result<OwnedFd, Error> {
    rustix::fs::open(
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| Error::OpenAt)
}

pub(crate) fn opendirat<Fd: AsFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    rustix::fs::openat2(
        fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH,
    )
    .map_err(|_| Error::OpenAt)
}

pub(crate) fn opendirat_cwd(name: &CStr) -> Result<OwnedFd, Error> {
    opendirat(&rustix::fs::CWD, name)
}

pub(crate) fn openpathat<Fd: AsFd>(fd: &Fd, name: &CStr) -> Result<OwnedFd, Error> {
    rustix::fs::openat2(
        fd,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH,
    )
    .map_err(|_| Error::OpenAt)
}

pub(crate) fn mkdirat<Fd: AsFd>(fd: &Fd, name: &CStr) -> Result<(), Error> {
    rustix::fs::mkdirat(fd, name, Mode::from_bits_truncate(MKDIR_MODE)).map_err(|_| Error::MkdirAt)
}
