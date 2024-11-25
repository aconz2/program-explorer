use std::fs::File;
use std::io::{Read,Write,Seek,SeekFrom};
use std::time::Duration;
use std::path::Path;

use byteorder::{WriteBytesExt,ReadBytesExt,LE};
use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub enum RootfsKind {
    Sqfs,
    Erofs,
}

// TODO if we want to support both erofs and sqfs and check their magic
// they are erofs: 0xE0F5E1E2 at 1024
//       squashfs: 0x73717368 at    0
impl RootfsKind {
    pub fn try_from_path_name<P: AsRef<Path>>(p: P) -> Option<Self> {
        match p.as_ref().extension() {
            Some(e) => {
                match e.to_str() {
                    Some("sqfs") => Some(RootfsKind::Sqfs),
                    Some("erofs") => Some(RootfsKind::Erofs),
                    _ => None
                }
            }
            None => None
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub enum ResponseFormat {
    PeArchiveV1,
    JsonV1,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    // https://github.com/opencontainers/runtime-spec/blob/main/config.md
    // fully filled in config.json ready to pass to crun
    pub oci_runtime_config: String,
    pub timeout: Duration,
    pub stdin: Option<String>,  // name of file in user's archive, not contents
    pub strace: bool,
    pub crun_debug: bool,
    pub rootfs_dir: String,
    pub rootfs_kind: RootfsKind, // this isn't really viable since we need to know
    pub response_format: ResponseFormat,
    pub kernel_inspect: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok {
        siginfo : SigInfoRedux,
        rusage  : Rusage,
        #[serde(skip_serializing_if = "Option::is_none")]
        stdout  : Option<String>,  // not included in ResponseFormat::PeArchiveV1
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr  : Option<String>,  // not included in ResponseFormat::PeArchiveV1
    },
    Overtime {
        siginfo : SigInfoRedux,
        rusage  : Rusage,
        #[serde(skip_serializing_if = "Option::is_none")]
        stdout  : Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr  : Option<String>,
    },
    Panic {
        message : String,
    }
}

//#[derive(Debug, Serialize, Deserialize, Clone)]
//pub enum ExitKind {
//    Ok,
//    Panic,
//    Overtime,
//    Abnormal,
//}

#[derive(Debug, Serialize, Deserialize)]
pub struct TimeVal {
    pub sec: i64,
    pub usec: i64, // this is susec_t which is signed for some reason
}

// this is a portion of siginfo interpreted from waitid(2)
#[derive(Debug, Serialize, Deserialize)]
pub enum SigInfoRedux {
    Exited(i32),
    Killed(i32),
    Dumped(i32),
    Stopped(i32),
    Trapped(i32),
    Continued(i32),
    Unk{status: i32, code: i32},
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Rusage {
    pub ru_utime    : TimeVal,     /* user CPU time used */
    pub ru_stime    : TimeVal,     /* system CPU time used */
    pub ru_maxrss   : i64,         /* maximum resident set size */
    pub ru_ixrss    : i64,         /* integral shared memory size */
    pub ru_idrss    : i64,         /* integral unshared data size */
    pub ru_isrss    : i64,         /* integral unshared stack size */
    pub ru_minflt   : i64,         /* page reclaims (soft page faults) */
    pub ru_majflt   : i64,         /* page faults (hard page faults) */
    pub ru_nswap    : i64,         /* swaps */
    pub ru_inblock  : i64,         /* block input operations */
    pub ru_oublock  : i64,         /* block output operations */
    pub ru_msgsnd   : i64,         /* IPC messages sent */
    pub ru_msgrcv   : i64,         /* IPC messages received */
    pub ru_nsignals : i64,         /* signals received */
    pub ru_nvcsw    : i64,         /* voluntary context switches */
    pub ru_nivcsw   : i64,         /* involuntary context switches */
}

// impl From<libc::c_int> for Status {
//     fn from(status: libc::c_int) -> Self {
//         Self {
//             status: if libc::WIFEXITED(status) { Some(libc::WEXITSTATUS(status) as u8) } else { None },
//             signal: if libc::WIFSIGNALED(status) { Some(libc::WTERMSIG(status)) } else { None },
//         }
//     }
// }

impl From<libc::siginfo_t> for SigInfoRedux {
    fn from(siginfo: libc::siginfo_t) -> Self {
        let status = unsafe { siginfo.si_status() }; // why is this unsafe?
        match siginfo.si_code {
            libc::CLD_EXITED => SigInfoRedux::Exited(status),
            libc::CLD_KILLED => SigInfoRedux::Killed(status),
            libc::CLD_DUMPED => SigInfoRedux::Dumped(status),
            libc::CLD_TRAPPED => SigInfoRedux::Trapped(status),
            libc::CLD_CONTINUED => SigInfoRedux::Continued(status),
            _ => SigInfoRedux::Unk{code: siginfo.si_code, status: status},
        }
    }
}

impl From<libc::timeval> for TimeVal {
    fn from(tv: libc::timeval) -> Self {
        TimeVal {
            sec:  tv.tv_sec,
            usec: tv.tv_usec,
        }
    }
}

impl From<libc::rusage> for Rusage {
    fn from(u: libc::rusage) -> Self {
        Rusage {
            ru_utime    : u.ru_utime.into(),
            ru_stime    : u.ru_stime.into(),
            ru_maxrss   : u.ru_maxrss,
            ru_ixrss    : u.ru_ixrss,
            ru_idrss    : u.ru_idrss,
            ru_isrss    : u.ru_isrss,
            ru_minflt   : u.ru_minflt,
            ru_majflt   : u.ru_majflt,
            ru_nswap    : u.ru_nswap,
            ru_inblock  : u.ru_inblock,
            ru_oublock  : u.ru_oublock,
            ru_msgsnd   : u.ru_msgsnd,
            ru_msgrcv   : u.ru_msgrcv,
            ru_nsignals : u.ru_nsignals,
            ru_nvcsw    : u.ru_nvcsw,
            ru_nivcsw   : u.ru_nivcsw,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io,
    Ser,
}

// todo use a single write
fn write_u32_le_slice(file: &mut File, xs: &[u32]) -> std::io::Result<()> {
    for x in xs {
        file.write_u32::<LE>(*x)?;
    }
    Ok(())
}

fn read_u32_le_slice(file: &mut File, xs: &mut [u32]) -> std::io::Result<()> {
    file.read_u32_into::<LE>(xs)
}

fn read_u32_le_pair(file: &mut File) -> std::io::Result<(u32, u32)> {
    let mut buf = [0; 2];
    read_u32_le_slice(file, &mut buf)?;
    Ok((buf[0], buf[1]))
}

// going into the guest, we have
// <u32: archive size> <u32: config size> <config> <archive>
// config is always in bincode format
// file is left with cursor at beginning of archive but you then must
// seek back to 0 to write the archive size
// file should be at 0, but we don't seek it so
pub fn write_io_file_config(file: &mut File, config: &Config, archive_size: u32) -> Result<(), Error> {
    let config_bytes = bincode::serialize(&config).map_err(|_| Error::Ser)?;
    let config_size: u32 = config_bytes.len().try_into().unwrap();
    write_u32_le_slice(file, &[archive_size, config_size]).map_err(|_| Error::Io)?;
    file.write_all(&config_bytes).map_err(|_| Error::Io)?;
    Ok(())
}

pub fn read_io_file_config(file: &mut File) -> Result<(u32, Config), Error> {
    let (archive_size, response_size) = read_u32_le_pair(file).map_err(|_| Error::Io)?;
    let mut buf = vec![0; response_size as usize];
    file.read_exact(&mut buf).map_err(|_| Error::Io)?;
    let config = bincode::deserialize(&buf).map_err(|_| Error::Ser)?;
    Ok((archive_size, config))
}

// coming out of the guest, we have
// <u32: archive end (absolute)> <u32: response size> <response> <archive>
// response is always in json format and archive_size may be 0
pub fn write_io_file_response(file: &mut File, response: &Response) -> Result<(), Error> {
    let response_bytes = serde_json::to_vec(&response).map_err(|_| Error::Ser)?;
    let response_size: u32 = response_bytes.len().try_into().unwrap();
    write_u32_le_slice(file, &[0, response_size]).map_err(|_| Error::Io)?;
    file.write_all(&response_bytes).map_err(|_| Error::Io)?;
    Ok(())
}

// coming out of the guest, we have
// <u32: archive end (absolute)> <u32: response size> <response> <archive>
// response is always in json format and archive_size may be 0
// we return the archive size and bytes of the response json
// file cursor is left at beginning of archive
pub fn read_io_file_response_bytes(file: &mut File) -> Result<(u32, Vec<u8>), Error> {
    file.seek(SeekFrom::Start(0)).map_err(|_| Error::Io)?;
    let (archive_size, response_size) = read_u32_le_pair(file).map_err(|_| Error::Io)?;
    let mut ret = vec![0; response_size as usize];
    file.read_exact(&mut ret).map_err(|_| Error::Io)?;
    Ok((archive_size, ret))
}

pub fn read_io_file_response_archive_bytes(file: &mut File) -> Result<Vec<u8>, Error> {
    file.seek(SeekFrom::Start(0)).map_err(|_| Error::Io)?;
    let (archive_size, response_size) = read_u32_le_pair(file).map_err(|_| Error::Io)?;
    // could also truncate to archive_end and read_to_end to avoid the zero initialize
    let mut ret = vec![0u8; (4 + archive_size + response_size) as usize];
    // ret[0..4] = &response_size.to_le_bytes(); // wish this would work
    {
        let b = response_size.to_le_bytes();
        for i in 0..4 { ret[i] = b[i]; }
    }
    file.read_exact(&mut ret[4..]).map_err(|_| Error::Io)?;
    Ok(ret)
}

pub fn read_io_file_response(file: &mut File) -> Result<(u32, Response), Error> {
    let (archive_size, response_bytes) = read_io_file_response_bytes(file)?;
    let response = serde_json::from_slice(&response_bytes).map_err(|_| Error::Ser)?;
    Ok((archive_size, response))
}
