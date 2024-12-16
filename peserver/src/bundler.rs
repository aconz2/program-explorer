use std::io;
use std::io::{Read};
use std::path::{Path,PathBuf};
use std::fs;
use std::fs::{DirEntry,File};

use clap::{Parser};
use rmp_serde::Serializer;
use serde::ser::Serialize;
use sha2::{Sha256,Digest};
use base64::prelude::{BASE64_STANDARD,Engine};

use peserver::staticfiles::{StaticFileEntry};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg()]
    dir: String,
}

fn sha2_hex(buf: &[u8]) -> String {
    let hash = Sha256::digest(&buf);
    BASE64_STANDARD.encode(hash)
}

fn read_to_vec<P: AsRef<Path>>(path: P) -> Vec<u8> {
    let mut buf = vec![];
    File::open(path).unwrap().read_to_end(&mut buf).unwrap();
    buf
}

fn content_type_for(name: &str) -> &str {
    if name.ends_with(".html") { return "text/html"; }
    if name.ends_with(".js") { return "text/javascript"; }
    if name.ends_with(".css") { return "text/css"; }
    if name.ends_with(".svg") { return "image/svg+xml"; }
    todo!("content_type_for {}", name);
}

fn main() {
    let args = Args::parse();
    let base_path = PathBuf::from(args.dir).canonicalize().unwrap();
    let mut entries = vec![];
    let mut total_size = 0;
    walkdir_files(&base_path, &mut |entry: &DirEntry| {
        if ! entry.file_type().unwrap().is_file() { return; }
        let path = entry.path();
        let name = path.strip_prefix(&base_path).unwrap().to_str().unwrap().to_string();
        assert!(!name.starts_with("api/"));
        let data = read_to_vec(entry.path());
        total_size += data.len();

        let path = if name == "index.html" {
            "/".to_string()
        } else {
            format!("/{}", name)
        };
        let content_type = content_type_for(&name);
        let headers = {
            let mut acc = vec![];
            acc.push(("Content-type".into(), content_type.into()));
            acc.push(("Content-length".into(), format!("{}", data.len())));
            acc
        };
        let etag = format!("\"{}\"", sha2_hex(&data));
        let sf = StaticFileEntry {
            etag: Some(etag),
            path,
            headers,
            data,
            gzip: None,
        };
        eprintln!("{:?}", entry.path());
        eprintln!("path={:?}", sf.path);
        eprintln!("etag={:?}", sf.etag);
        for (h, v) in &sf.headers {
            eprintln!("header= {h} {v}");
        }
        eprintln!("--");
        entries.push(sf);
    }).unwrap();

    let mut outfile = File::create("/tmp/foo").unwrap();
    entries.serialize(&mut Serializer::new(&mut outfile)).unwrap();
    let outlen = outfile.metadata().unwrap().len();
    println!("insize  {:8} bytes", total_size);
    println!("outsize {:8} bytes", outlen);
}

// https://doc.rust-lang.org/std/fs/fn.read_dir.html
// fn walkdir_files(dir: &Path, cb: &dyn Fn(&DirEntry)) -> io::Result<()> {
fn walkdir_files<F: FnMut(&DirEntry) -> ()>(dir: &Path, cb: &mut F) -> io::Result<()> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walkdir_files(&path, cb)?;
            } else {
                cb(&entry);
            }
        }
    }
    Ok(())
}
