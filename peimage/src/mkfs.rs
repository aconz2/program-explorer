use std::process::Command;
use std::io::{Read, Seek, BufWriter};
use std::env;
use std::path::{Path, PathBuf};
use std::fs::{OpenOptions, remove_file};

use rustix::fs::{mknodat,FileType, Mode, open, OFlags};

use crate::squash::{squash,SquashError,Stats};

// TODO allow passing more args into mkfs.erofs, wait with timeout

pub fn squash_erofs<R, P>(layer_readers: &mut [R], outfile: P) -> Result<Stats, SquashError>
where
    R: Read + Seek,
    P: AsRef<Path>,
{
    let fifo = mkfifo().map_err(|_| SquashError::Mkfifo)?;

    let mut child = Command::new("mkfs.erofs")
        .arg(outfile.as_ref().as_os_str())
        .arg(fifo.clone())
        .spawn()?;

    // Linux fifo size is 16 pages, should we match that?
    let fifo_file = OpenOptions::new().write(true).open(&fifo).map_err(|_| SquashError::FifoOpen)?;
    let _fifo_file_remover = UnlinkFile {path: fifo.clone()};

    let mut out = BufWriter::with_capacity(4096 * 8, fifo_file);

    let stats = squash(layer_readers, &mut out)?;
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
