use std::io;
use std::io::{Write,Seek, SeekFrom, Read};
use std::fs::File;
use std::os::fd::AsRawFd;

use rustix::fs::{memfd_create, MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, AtFlags, statat};
use rustix::fd::AsFd;


const PMEM_ALIGN_SIZE: u64 = 0x20_0000; // 2 MB

pub struct IoFile {
    file: File,
}

pub struct IoFileBuilder {
    file: File,
}

impl IoFileBuilder {
    pub fn new() -> rustix::io::Result<Self> {
        // NOTE: CLOEXEC is NOT set
        let fd = memfd_create("peiofile", MemfdFlags::ALLOW_SEALING | MemfdFlags::NOEXEC_SEAL)?;
        Ok(Self { file: fd.into() })
    }

    pub fn finish(mut self) -> rustix::io::Result<IoFile> {
        let _ = round_up_file_to_pmem_size(&mut self.file)?;
        fcntl_add_seals(&self.file, SealFlags::SHRINK | SealFlags::GROW)?;
        fcntl_add_seals(&self.file, SealFlags::SEAL)?;
        Ok(IoFile { file: self.file })
    }
}

impl AsRawFd for IoFileBuilder {
    fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

impl Write for IoFileBuilder {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.file.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for IoFileBuilder {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.file.seek(from)
    }
}

impl IoFile {
    pub fn into_inner(self) -> File {
        self.file
    }
}

impl Read for IoFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for IoFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.file.seek(from)
    }
}

impl AsRawFd for IoFile {
    fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

fn round_up_to<const N: u64>(x: u64) -> u64 {
    if x == 0 { return N; }
    ((x + (N - 1)) / N) * N
}

fn round_up_file_to_pmem_size<F: AsFd>(f: F) -> rustix::io::Result<u64> {
    let stat = statat(&f, "", AtFlags::EMPTY_PATH)?;
    let cur = stat.st_size as u64;
    let newlen = round_up_to::<PMEM_ALIGN_SIZE>(cur);
    if cur != newlen {
        ftruncate(f, newlen)?;
    }
    Ok(newlen)
}
