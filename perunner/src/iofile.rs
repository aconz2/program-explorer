use std::fs::File;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

use rustix::fd::AsFd;
use rustix::fs::{fcntl_add_seals, fstat, ftruncate, memfd_create, MemfdFlags, SealFlags};

const PMEM_ALIGN_SIZE: u64 = 0x20_0000; // 2 MB

pub struct IoFile {
    file: File,
}

pub struct IoFileBuilder {
    file: File,
}

impl IoFileBuilder {
    pub fn new() -> rustix::io::Result<Self> {
        let fd = memfd_create(
            "peiofile",
            MemfdFlags::ALLOW_SEALING | MemfdFlags::NOEXEC_SEAL | MemfdFlags::CLOEXEC,
        )?;
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

impl AsFd for IoFileBuilder {
    fn as_fd(&self) -> BorrowedFd {
        self.file.as_fd()
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

impl AsFd for IoFile {
    fn as_fd(&self) -> BorrowedFd {
        self.file.as_fd()
    }
}

impl AsRawFd for IoFile {
    fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

impl From<IoFile> for OwnedFd {
    fn from(io_file: IoFile) -> OwnedFd {
        io_file.file.into()
    }
}

fn round_up_to<const N: u64>(x: u64) -> u64 {
    if x == 0 {
        return N;
    }
    x.div_ceil(N) * N
}

pub fn round_up_file_to_pmem_size<F: AsFd>(f: F) -> rustix::io::Result<u64> {
    let stat = fstat(&f)?;
    let cur = stat.st_size.try_into().unwrap_or(0);
    let newlen = round_up_to::<PMEM_ALIGN_SIZE>(cur);
    if cur != newlen {
        ftruncate(f, newlen)?;
    }
    Ok(newlen)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_iofile() {
        let mut io_file = {
            let mut builder = IoFileBuilder::new().unwrap();
            builder.write_all(b"hello world").unwrap();
            builder.finish().unwrap().into_inner()
        };
        let len = io_file.metadata().unwrap().len();
        assert_eq!(len, PMEM_ALIGN_SIZE);

        io_file.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 11];
        assert_eq!(11, io_file.read(&mut buf).unwrap());
        assert_eq!(&buf, b"hello world");

        // can write 2MB of stuff
        io_file.seek(SeekFrom::Start(0)).unwrap();
        let data = &[0xff].repeat(PMEM_ALIGN_SIZE as usize);
        io_file.write_all(&data).unwrap();

        // but can't write 1 byte more
        assert!(io_file.write_all(&[0xff]).is_err());

        // can't shrink
        assert!(io_file.set_len(1024).is_err());
    }

    #[test]
    fn test_round_up_to() {
        assert_eq!(PMEM_ALIGN_SIZE, round_up_to::<PMEM_ALIGN_SIZE>(0));
        assert_eq!(
            PMEM_ALIGN_SIZE,
            round_up_to::<PMEM_ALIGN_SIZE>(PMEM_ALIGN_SIZE - 1)
        );
        assert_eq!(
            PMEM_ALIGN_SIZE,
            round_up_to::<PMEM_ALIGN_SIZE>(PMEM_ALIGN_SIZE)
        );
        assert_eq!(
            2 * PMEM_ALIGN_SIZE,
            round_up_to::<PMEM_ALIGN_SIZE>(PMEM_ALIGN_SIZE + 1)
        );
    }
}
