use flate2::read::GzDecoder;
use std::borrow::Borrow;
use std::cmp::Ord;
use std::collections::BTreeSet;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use tar::{Archive, Builder as ArchiveBuilder, Entry, EntryType};

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

#[derive(PartialEq, Debug)]
enum Whiteout {
    File(PathBuf),
    Opaque(PathBuf),
}

// important notes about the OCI spec
// "Extracting a layer with hardlink references to files outside of the layer may fail."
//
// so GzDecoder doesn't have an option for Read+Seek,. What we really want is a trait for
// SeekStart since we don't expect to be able to randomly seek in a compressed stream but we can
// easily restart from the beginning. Instead take the underlying reader directly.
// That does mean we are not agnostic to the compression which is a bit annoying
// but in practice I think everything is (unfortunately) tgz TODO not true, tar+{,gzip,zstd}
// this does make testing slightly more annoying since we first compress our layers. I
// considered either dynamically checking or something like that but gets complicated
//
// compared to sylabs https://github.com/sylabs/oci-tools/blob/main/pkg/mutate/squash.go
// we do not store the contents of every file in memory, but we do have to have a seekable
// stream since we take a second pass. Using libz-ng, a second pass through is less of a time
// concern, but it does still mean you can't stream in. This also implements a better check of
// opaque deleted/shadowed files in my opinion because they check each path component of each file
// against a map from string to (bool, bool) which is a huge number of lookups. Here we can use
// a btree and then check the prefix (cry for a trie, but idk if it would really be
// worth it here)
// Hardlinks are annoying means that on our first pass through TODO
//
// Total memory usage is something like the sum of path lengths from all entries, since we store
// deleted ones, opaque ones, and non-deleted ones since these then get shown as, minus the size of
// those paths lengths from the first layer. This is actually pretty good for containers with a
// big first layer and some smaller layers after that.
// note we don't have to store deletions on the first (last iteration) layer since we wouldn't
// check them

pub fn squash<W, R>(layer_readers: &mut [R], out: &mut W) -> Result<(), SquashError>
where
    W: Write,
    R: Read + Seek,
{
    let mut deletions = Deletions::default();
    //let mut hardlinks: BTreeSet<PathBuf> = BTreeSet::new();

    let mut aw = ArchiveBuilder::new(out);

    // we do enumerate, then rev, so i==0 is layer_readers[0] and is our last layer to be processed
    // where we can skip storing deletions
    for (i, reader) in layer_readers.iter_mut().enumerate().rev() {
        {
            // pass 1: gather all whiteouts and hard links
            // &mut * creates a fresh borrow
            let mut layer = Archive::new(GzDecoder::new(&mut *reader));
            for entry in layer.entries()? {
                let mut entry = entry?;

                // only have to store deletions
                if i != 0 {
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
                }

                //if entry.header().entry_type() == EntryType::Link
                //    && !deletions.is_deleted(&entry.path()?)
                //{
                //    if let Some(link) = entry.link_name()? {
                //        hardlinks.insert(link.into());
                //    } else {
                //        return Err(SquashError::HardlinkNoLink);
                //    }
                //}

                if deletions.is_deleted(entry.path()?) {
                    continue;
                }

                // annoying we have to clone the header
                aw.append(&entry.header().clone(), &mut entry)?;

                // once we write a file, we mark it as deleted so we do not write it again
                if i != 0 {
                    deletions.push_file(entry.path()?.into());
                }
            }
        }

        //reader.seek(SeekFrom::Start(0))?;

        //{
        //    // pass 2:
        //    let mut layer = Archive::new(GzDecoder::new(&mut *reader));
        //    for entry in layer.entries()? {
        //        let mut entry = entry?;
        //
        //        // skip whiteouts
        //        match whiteout(&entry)? {
        //            Some(Whiteout::File(_)) | Some(Whiteout::Opaque(_)) => {
        //                continue;
        //            }
        //            _ => {}
        //        }
        //
        //        if deletions.is_deleted(entry.path()?) {
        //            continue;
        //        }
        //
        //        // annoying we have to clone the header
        //        aw.append(&entry.header().clone(), &mut entry)?;
        //
        //        // once we write a file, we mark it as deleted so we do not write it again
        //        deletions.push_file(entry.path()?.into());
        //    }
        //}

        // end of layer, updated deleted_{files,opaques}
        if i != 0 {
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
where
    T: Borrow<K> + Ord,
    K: Ord + ?Sized,
{
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
            return Ok(None);
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

    use flate2::write::GzEncoder;
    use std::io::{Cursor, Seek, SeekFrom};
    use tar::{Builder, Header};

    // E is a standalone redux Entry
    #[derive(Debug, PartialOrd, Ord, PartialEq, Eq)]
    enum E {
        File { path: PathBuf, data: Vec<u8> },
        Dir { path: PathBuf },
        Link { path: PathBuf, link: PathBuf },
    }

    impl E {
        fn file(path: &str, data: &[u8]) -> Self {
            Self::File {
                path: path.into(),
                data: Vec::from(data),
            }
        }
        fn dir(path: &str) -> Self {
            Self::Dir { path: path.into() }
        }
        fn link(path: &str, link: &str) -> Self {
            Self::Link {
                path: path.into(),
                link: link.into(),
            }
        }
    }

    type EList = BTreeSet<E>;

    fn serialize_to_writer<W: Write>(entries: &[E], out: &mut W) {
        let mut writer = ArchiveBuilder::new(out);
        for entry in entries {
            let mut h = Header::new_ustar();
            match entry {
                E::File { path, data } => {
                    h.set_path(path.clone()).unwrap();
                    h.set_entry_type(EntryType::Regular);
                    h.set_size(data.len() as u64);
                    h.set_cksum();
                    writer.append(&h, Cursor::new(&data)).unwrap();
                }
                E::Dir { path } => {
                    h.set_path(path.clone()).unwrap();
                    h.set_entry_type(EntryType::Directory);
                    h.set_size(0);
                    h.set_cksum();
                    writer.append(&h, &b""[..]).unwrap();
                }
                E::Link { path, link } => {
                    h.set_path(path.clone()).unwrap();
                    h.set_entry_type(EntryType::Link);
                    h.set_link_name(link).unwrap();
                    h.set_size(0);
                    h.set_cksum();
                    writer.append(&h, &b""[..]).unwrap();
                }
            }
        }
    }

    fn serialize(entries: &[E]) -> Vec<u8> {
        let mut buf = io::Cursor::new(vec![]);
        serialize_to_writer(entries, &mut buf);
        buf.into_inner()
    }

    fn serialize_gz(entries: &[E]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        serialize_to_writer(entries, &mut encoder);
        encoder.finish().unwrap()
    }

    fn deserialize(data: &[u8]) -> EList {
        let mut reader = Archive::new(Cursor::new(data));
        reader
            .entries()
            .unwrap()
            .map(|x| x.unwrap())
            .map(|mut x| {
                let path = x.path().unwrap().into();
                match x.header().entry_type() {
                    EntryType::Regular => {
                        let mut data = vec![];
                        x.read_to_end(&mut data).unwrap();
                        E::File { path, data }
                    }
                    EntryType::Directory => E::Dir { path },
                    EntryType::Link => {
                        let link = x.link_name().unwrap().unwrap().into();
                        E::Link { path, link }
                    }
                    x => {
                        panic!("unhandled entry type {x:?}");
                    }
                }
            })
            .collect()
    }

    fn make_entry<P, F, B>(path: P, data: &[u8], f: F) -> B
    where
        P: AsRef<Path>,
        F: Fn(Entry<'_, Cursor<Vec<u8>>>) -> B,
    {
        let mut h = Header::new_ustar();
        h.set_path(path).unwrap();
        h.set_entry_type(EntryType::Regular);
        h.set_size(data.len() as u64);
        h.set_cksum();

        let buf = {
            let mut b = Builder::new(Cursor::new(vec![]));
            b.append(&h, data).unwrap();
            let mut buf = b.into_inner().unwrap();
            buf.seek(SeekFrom::Start(0)).unwrap();
            buf
        };

        let mut ar = Archive::new(buf);
        let entry = ar.entries().unwrap().next().unwrap().unwrap();
        f(entry)
    }

    #[rustfmt::skip]
    #[test]
    fn test_opaque() {
        make_entry("foo", &b"data"[..], |e| {
            assert_eq!(whiteout(&e).unwrap(), None);
        });

        let files = vec![
            (".wh.foo", "foo"),
            ("dir/.wh.foo", "dir/foo"),
        ];

        let dirs = vec![
            ("dir/.wh..wh..opq", "dir"),
            ("dir1/dir2/.wh..wh..opq", "dir1/dir2"),
        ];

        for (x, y) in files.into_iter() {
            make_entry(x, &b"data"[..], |e| {
                assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::File(y.into())));
            });
        }

        for (x, y) in dirs.into_iter() {
            make_entry(x, &b""[..], |e| {
                assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::Opaque(y.into())));
            });
        }
    }

    #[test]
    fn test_lower_bound() {
        // ascii / is l
        let set: BTreeSet<_> = vec!["dir1/", "dir1!", "dir2/dir3/"].into_iter().collect();
        assert_eq!(lower_bound(&set, "dir1/file"), Some("dir1/").as_ref());
        assert_eq!(lower_bound(&set, "dir0/file"), None);
        assert_eq!(lower_bound(&set, "dir2/file"), Some("dir2/dir3/").as_ref());
    }

    #[test]
    fn test_opaque_deleted() {
        let set: BTreeSet<PathBuf> = vec!["dir1/", "dir1!", "dir2/dir3/"]
            .into_iter()
            .map(|x| x.into())
            .collect();
        assert!(opaque_deleted(&set, "dir1/file"));
        assert!(opaque_deleted(&set, "dir2/dir3/file"));
        assert!(!opaque_deleted(&set, "dir1file"));
        assert!(!opaque_deleted(&set, "dir0/file"));
        assert!(!opaque_deleted(&set, "dir2/file"));
    }

    #[test]
    fn test_serde() {
        let entries = vec![E::file("x", b"hi"), E::link("y", "x")];
        let buf = serialize(&entries);
        assert_eq!(entries.into_iter().collect::<EList>(), deserialize(&buf));
    }

    fn squash_layers_vec(layers: Vec<Vec<E>>) -> EList {
        let mut readers: Vec<Cursor<Vec<u8>>> = layers
            .into_iter()
            .map(|x| Cursor::new(serialize_gz(&x)))
            .collect();
        let mut buf = Cursor::new(vec![]);
        squash(&mut readers, &mut buf).unwrap();
        deserialize(&buf.into_inner())
    }

    macro_rules! check_squash {
        ($layers:expr, $expected:expr) => {{
            assert_eq!(
                squash_layers_vec($layers),
                $expected.into_iter().collect::<EList>()
            );
        }};
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_file_overwrite() {
        // overwrite of a file
        check_squash!(
            vec![
                vec![E::file("x", b"hi")],
                vec![E::file("x", b"bye")],
            ],
            vec![E::file("x", b"bye")]
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_file_whiteout() {
        // whiteout of a file
        check_squash!(
            vec![
                vec![E::file("x", b"hi")],
                vec![E::file(".wh.x", b"")],
            ],
            vec![]
        );

        // whiteout of a non-matching file
        check_squash!(
            vec![
                vec![E::file("x", b"hi")],
                vec![E::file(".wh.xyz", b"")],
            ],
            vec![E::file("x", b"hi")]
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_opaque_whiteout() {
        check_squash!(
            vec![
                vec![E::dir("x"), E::file("x/f", b"hi"), E::file("xfile", b"hello")],
                vec![E::file("x/.wh..wh..opq", b"")],
            ],
            vec![E::dir("x"), E::file("xfile", b"hello")]
        );

        check_squash!(
            vec![
                vec![E::dir("x"), E::file("x/f", b"hi")],
                vec![E::file("x/.wh..wh..opq", b""), E::file("x/g", b"bye")],
            ],
            vec![E::dir("x"), E::file("x/g", b"bye")]
        );
    }
}
