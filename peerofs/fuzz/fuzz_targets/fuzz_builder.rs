#![no_main]

use std::io::Cursor;
use std::path::PathBuf;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use rustix::fs::Mode;

use peerofs::build::{Builder, BuilderConfig, Meta, XattrMap};

#[derive(Arbitrary, Debug)]
struct ArbMeta {
    uid: u32,
    gid: u32,
    mtime: u64,
    mode: u16,
    xattrs: XattrMap,
}

impl From<ArbMeta> for Meta {
    fn from(val: ArbMeta) -> Self {
        Meta {
            uid: val.uid,
            gid: val.gid,
            mtime: val.mtime,
            mode: Mode::from_bits_truncate(val.mode.into()),
            xattrs: val.xattrs,
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Op {
    File {
        path: PathBuf,
        meta: ArbMeta,
        data: Vec<u8>,
    },
    Dir {
        path: PathBuf,
        meta: ArbMeta,
    },
    Symlink {
        path: PathBuf,
        target: PathBuf,
        meta: ArbMeta,
    },
    Link {
        path: PathBuf,
        target: PathBuf,
        meta: ArbMeta,
    },
}

fuzz_target!(|ops: Vec<Op>| {
    let mut builder = Builder::new(Cursor::new(vec![]), BuilderConfig::default()).unwrap();
    for op in ops {
        match op {
            Op::File { path, meta, data } => {
                let _ = builder.add_file(path, meta.into(), data.len(), &mut Cursor::new(data));
            }
            Op::Dir { path, meta } => {
                let _ = builder.upsert_dir(path, meta.into());
            }
            Op::Symlink { path, target, meta } => {
                let _ = builder.add_symlink(path, target, meta.into());
            }
            Op::Link { path, target, meta } => {
                let _ = builder.add_link(path, target, meta.into());
            }
        }
    }
    let _ = builder.into_inner();
});
