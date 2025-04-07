use std::collections::BTreeSet;
use std::io;
use std::io::{Read,Write,Seek,SeekFrom};
use std::path::{PathBuf,Path};
use tar::{Archive,Entry,EntryType,Builder as ArchiveBuilder};
use std::ops::Bound;
use std::cmp::Ord;
use std::borrow::Borrow;
use flate2::read::GzDecoder;

#[derive(Debug)]
pub enum SquashError {
    Io(io::Error),
    OpaqueWhiteoutNoParent,
    HardlinkNoLink,
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

// so GzDecoder doesn't have an option for R+Seek, so we instead take the underlying
// reader directly. That does mean we are not agnostic to the compression which is a bit annoying
// but in practice I think everything is (unfortunately) tgz

pub fn squash<W, R>(layer_readers: &mut [R], out: &mut W)
    -> Result<(), SquashError>
    where W: Write,
          R: Read + Seek,
{

    let mut deletions = Deletions::default();
    let mut hardlinks: BTreeSet<PathBuf> = BTreeSet::new();

    let mut aw = ArchiveBuilder::new(out);

    for reader in layer_readers.iter_mut().rev() {
        { // pass 1
            // &mut * creates a fresh borrow
            let mut layer = Archive::new(GzDecoder::new(&mut *reader));
            for entry in layer.entries()? {
                let entry = entry?;

                match whiteout(&entry)? {
                    Some(Whiteout::File(path)) => {
                        deletions.push_file(path);
                        continue;
                    }
                    Some(Whiteout::Opaque(path)) => {
                        deletions.push_opaque(path);
                        continue;
                    }
                    _ => {}
                }

                if entry.header().entry_type() == EntryType::Link && !deletions.is_deleted(&entry.path()?) {
                    if let Some(link) = entry.link_name()? {
                        hardlinks.insert(link.into());
                    } else {
                        return Err(SquashError::HardlinkNoLink);
                    }
                }
            }
        }

        reader.seek(SeekFrom::Start(0))?;

        { // pass 2
            let mut layer = Archive::new(GzDecoder::new(&mut *reader));
            for entry in layer.entries()? {
                let mut entry = entry?;

                // skip whiteouts
                match whiteout(&entry)? {
                    Some(Whiteout::File(_)) |
                    Some(Whiteout::Opaque(_)) => {
                        continue;
                    }
                    _ => {}
                }

                if deletions.is_deleted(entry.path()?) {

                }

                // annoying we have to clone the header
                aw.append(&entry.header().clone(), &mut entry)?;

            }

        }

        { // end of layer, updated deleted_{files,opaques}
            deletions.end_of_layer();
        }
    }

    Ok(())
}

#[derive(Default)]
struct Deletions {
    files: BTreeSet<PathBuf>,
    opaques: BTreeSet<PathBuf>,

    files_q: Vec<PathBuf>,
    opaques_q: Vec<PathBuf>,
}


impl Deletions {
    fn push_file(&mut self, p: PathBuf) {
        self.files_q.push(p);
    }
    fn push_opaque(&mut self, p: PathBuf) {
        self.opaques_q.push(p.into());
    }

    fn is_deleted<P: AsRef<Path>>(&mut self, p: P) -> bool {
        self.files.contains(p.as_ref()) || opaque_deleted(&self.opaques, p)
    }

    fn end_of_layer(&mut self) {
        self.files.extend(self.files_q.drain(..));
        self.opaques.extend(self.opaques_q.drain(..));
    }
}


fn opaque_deleted<P: AsRef<Path>>(opaques: &BTreeSet<PathBuf>, path: P) -> bool {
    if let Some(prefix) = lower_bound(opaques, path.as_ref()) {
        path.as_ref().starts_with(prefix)
    } else {
        false
    }
}

fn lower_bound<'a, K, T>(set: &'a BTreeSet<T>, key: &K) -> Option<&'a T>
    where T: Borrow<K> + Ord,
          K: Ord + ?Sized {
    let mut iter = set.range((Bound::Unbounded, Bound::Excluded(key)));
    iter.next_back()
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

        let buf = {
            let mut b = Builder::new(io::Cursor::new(vec![]));
            b.append(&h, data).unwrap();
            let mut buf = b.into_inner().unwrap();
            buf.seek(SeekFrom::Start(0)).unwrap();
            buf
        };

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
        make_entry("foo", &b"data"[..], |e| {
            assert_eq!(whiteout(&e).unwrap(), None);
        });

        make_entry(".wh.foo", &b"data"[..], |e| {
            assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::File("foo".into())));
        });

        make_entry("dir/.wh..wh..opq", &b""[..], |e| {
            assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::Opaque("dir".into())));
        });

    }

    #[test]
    fn test_lower_bound() {
        let set: BTreeSet<_> = vec!["dir1/", "dir2/dir3/"].into_iter().collect();
        assert_eq!(lower_bound(&set, "dir1/file"), Some("dir1/").as_ref());
        assert_eq!(lower_bound(&set, "dir0/file"), None);
        assert_eq!(lower_bound(&set, "dir2/file"), Some("dir2/dir3/").as_ref());
    }

    #[test]
    fn test_opaque_deleted() {
        let set: BTreeSet<PathBuf> = vec!["dir1/", "dir2/dir3/"].into_iter().map(|x| x.into()).collect();
        assert!(opaque_deleted(&set, "dir1/file"));
        assert!(opaque_deleted(&set, "dir2/dir3/file"));
        assert!(!opaque_deleted(&set, "dir0/file"));
        assert!(!opaque_deleted(&set, "dir2/file"));
    }
}
