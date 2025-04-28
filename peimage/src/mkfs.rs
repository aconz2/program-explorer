use std::env;
use std::fs::{remove_file, OpenOptions};
use std::io::{BufWriter, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::fs::{mknodat, open, FileType, Mode, OFlags};

use crate::squash::{squash, SquashError, Stats};

// TODO allow passing more args into mkfs.erofs, wait with timeout
//
// notes on mkfs.erofs
// without multithreading uses these syscalls
//   access
//   arch_prctl
//   brk
//   close
//   copy_file_range
//   dup
//   exit_group
//   fallocate
//   fstat
//   fstatfs
//   ftruncate
//   getcwd
//   getpid
//   getppid
//   getrandom
//   gettid
//   getuid
//   ioctl
//   lseek
//   mmap
//   mprotect
//   munmap
//   openat
//   pread64
//   prlimit64
//   pwrite64
//   read
//   readlink
//   rseq
//   rt_sigprocmask
//   set_robust_list
//   set_tid_address
//   write
//
// it first truncates the dest file to 1Tib (2**40), then copies each file's data portion at 1Tib
// going forward and does so without compression. It does this by reading from the pipe, lseeking, then
// writing to the file in 32k chunks; I don't know why it doesn't use copy_file_range here.
// It then begins filling in the file by reading from the end of the file and copying the data to
// the beginning of the file. For files less than 4K, there is no compression (these could be
// written in place already I think). It does use copy_file_range in this phase but I'm not sure
// what for. Strangely uses a mix of pwrite64 on the write side and seek+read on the read side (all
// of the same file). I would think using pwritev would be useful here when writing larger files.
// It then writes loads of pwrite64 of size 64 which are the large inode size with a mix of things
// like symlinks but these are all sequential, so could also use pwritev (or buffer in mem then
// flush). I think some of these are also small files with inline data. Not sure yet what the dir
// ent writes are. I'm thinking about how to seccomp the binary with a patch I think, but also just
// thinking about writing my own builder.
//
// But good to keep in mind that building a erofs on tmpfs will consume at peak the sum of all file
// sizes uncompressed + the maps and stuff overhead in memory. Vs if you build on disk, then you
// are first writing out the sum of all file sizes, then reading them back and writing the sum of
// all compressed file sizes.
//
// it does a fallocate(4, FALLOC_FL_KEEP_SIZE|FALLOC_FL_PUNCH_HOLE, 51175371, 53)
//
// trying out a seccomp of mkfs.erofs gives about a 7% overhead, probably because of the high
// number of syscalls (387,772 on silkeh/clang:17. top 4:
//   209202 pwrite64
//    74073 read
//    55014 write
//    48811 lseek

pub fn squash_erofs<R, P>(layer_readers: &mut [R], outfile: P) -> Result<Stats, SquashError>
where
    R: Read + Seek,
    P: AsRef<Path>,
{
    let fifo = mkfifo().map_err(|_| SquashError::Mkfifo)?;

    let mut child = Command::new("mkfs.erofs")
        .arg("--quiet")
        .arg("--tar=f")
        .arg("-zlz4")
        .arg(outfile.as_ref().as_os_str())
        .arg(fifo.clone())
        .spawn()?;

    // Linux fifo size is 16 pages, should we match that?
    let fifo_file = OpenOptions::new()
        .write(true)
        .open(&fifo)
        .map_err(|_| SquashError::FifoOpen)?;
    let _fifo_file_remover = UnlinkFile { path: fifo.clone() };

    let mut out = BufWriter::with_capacity(4096 * 8, fifo_file);

    let stats = squash(layer_readers, &mut out)?;
    let _ = out.into_inner(); // close fifo
    let status = child.wait()?;

    if status.success() {
        Ok(stats)
    } else {
        Err(SquashError::MkfsFailed)
    }
}

fn mkfifo() -> rustix::io::Result<PathBuf> {
    use rand::distr::{Alphanumeric, SampleString};

    let rng = Alphanumeric.sample_string(&mut rand::rng(), 16);
    let temp_dir = env::temp_dir();
    let dir = open(&temp_dir, OFlags::DIRECTORY | OFlags::RDONLY, Mode::empty())?;
    let path = format!("pe-fifo-{rng}");

    // rustix doesn't have mkfifo https://github.com/bytecodealliance/rustix/issues/1391
    mknodat(dir, &path, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0)?;

    Ok(temp_dir.join(path))
}

struct UnlinkFile {
    path: PathBuf,
}

impl Drop for UnlinkFile {
    fn drop(&mut self) {
        let _ = remove_file(&self.path);
    }
}
