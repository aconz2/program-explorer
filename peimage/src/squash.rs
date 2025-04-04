use std::collections::BTreeSet;
use std::io;
use std::io::{Read,Write};
use std::path::{PathBuf,Path};
use tar::{Archive,Entry,EntryType};

#[derive(Debug)]
pub enum SquashError {
    Io(io::Error),
    OpaqueWhiteoutNoParent,
}

impl From<io::Error> for SquashError {
    fn from(e: io::Error) -> Self {
        SquashError::Io(e)
    }
}

#[derive(PartialEq,Debug)]
enum Whiteout {
    File(PathBuf),
    Opaque(PathBuf),
}

pub fn squash<W, R>(layers: &mut [Archive<R>], _out: &mut W)
    -> Result<(), SquashError>
    where W: Write,
          R: Sized + Read,
{

    let mut deleted_files = BTreeSet::new();
    let mut deleted_opaques = BTreeSet::new();

    for layer in layers.iter_mut().rev() {
        for entry in layer.entries()? {
            let entry = entry?;

            //let name = entry.path()?.file_name()
            match whiteout(&entry)? {
                Some(Whiteout::File(path)) => {
                    deleted_files.insert(path);
                    continue;
                }
                Some(Whiteout::Opaque(path)) => {
                    deleted_opaques.insert(path);
                    continue;
                }
                _ => {}
            }

            let path = &entry.path()?;
            if deleted_files.contains(path.as_ref()) {
                println!("yo this file got deleted {:?}", path);
            }

            if opaque_deleted(&deleted_opaques, path) {
                println!("yo this file got deleted because of an opaque {:?}", path);
            }

            drop(entry);
        }
    }
    for path in deleted_files.iter() {
        println!("deleted file {:?}", path);
    }
    for path in deleted_opaques.iter() {
        println!("deleted opaques {:?}", path);
    }
    Ok(())
}

fn opaque_deleted<P: AsRef<Path>>(opaques: &BTreeSet<PathBuf>, path: P) -> bool {
    todo!()
}

fn whiteout<R: Read>(entry: &Entry<R>) -> Result<Option<Whiteout>, SquashError> {
    // this should be true but idk if universal
    //if entry.header.entry_type() != EntryType::Regular {
    //    return Ok(None)
    //}
    let path = entry.path()?; // can fail if not unicode
    let name = {
        if let Some(name) = path.file_name().and_then(|x| x.to_str()) {
            name
        } else {
            return Ok(None)
        }
    };
    // is starts_with correct or should it be exact comparison for opaques
    if name.starts_with(".wh..wh..opq") {
        if let Some(parent) = path.parent() {
            return Ok(Some(Whiteout::Opaque(parent.into())));
        } else {
            // I don't think this should happend
            return Err(SquashError::OpaqueWhiteoutNoParent);
        }
    }
    if let Some(trimmed) = name.strip_prefix(".wh.") {
        let hidden = path.with_file_name(trimmed);
        return Ok(Some(Whiteout::File(hidden)));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    use tar::{Builder,Header};
    use std::io::{Seek,SeekFrom,Cursor};

    fn make_entry<P, F, B>(path: P, data: &[u8], f: F) -> B
        where P: AsRef<Path>,
              F: Fn(Entry<'_, Cursor<Vec<u8>>>) -> B {
        let mut h = Header::new_ustar();
        h.set_path(path).unwrap();
        h.set_entry_type(EntryType::Regular);
        h.set_uid(1000);
        h.set_gid(1000);
        h.set_size(data.len() as u64);
        h.set_cksum();

        let mut b = Builder::new(io::Cursor::new(vec![]));
        b.append(&h, data).unwrap();
        let mut buf = b.into_inner().unwrap();
        buf.seek(SeekFrom::Start(0)).unwrap();

        {
        use std::fs::File;
        let mut f = File::create("/tmp/myarchive.tar").unwrap();
        std::io::copy(&mut buf, &mut f).unwrap();
        }

        buf.seek(SeekFrom::Start(0)).unwrap();
        //println!("{:?}", buf);
        {
        let mut ar = Archive::new(buf.clone());
        println!("got count {}", ar.entries().unwrap().count());
        }
        let mut ar = Archive::new(buf);
        let entry = ar.entries()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        f(entry)
    }

    #[test]
    fn test_opaque() {
        make_entry("foo", &b"foooo"[..], |e| {
            assert!(whiteout(&e).unwrap().is_none());
        });

    }
}
