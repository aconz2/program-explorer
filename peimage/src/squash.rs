use std::borrow::Borrow;
use std::cmp::Ord;
use std::collections::{BTreeSet,BTreeMap};
use std::ffi::OsStr;
use std::io;
use std::io::{Read, Seek, Write};
use std::ops::Bound;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use tar::{Archive, Builder as ArchiveBuilder, Entry, EntryType};

#[derive(Debug)]
pub enum SquashError {
    Io(io::Error),
    OpaqueWhiteoutNoParent,
    HardlinkNoLink,
    Finish,
    Utf8Error,
}

impl From<io::Error> for SquashError {
    fn from(e: io::Error) -> Self {
        SquashError::Io(e)
    }
}

#[derive(PartialEq, Debug)]
enum Whiteout {
    Single(PathBuf),
    Opaque(PathBuf),
}

#[derive(Debug, PartialEq)]
enum DeletionState {
    Whiteout,
    Opaque,
    Shadowed,
}

#[derive(PartialEq, Debug)]
enum DeletionReason {
    Single,
    SingleDir,
    Opaque,
    Shadowed,
}

//                 Match Type
//               |   Exact  |   Child   |
// DeletionState |----------------------|
//      Whiteout |  Single  | SingleDir |
//        Opaque |    -     |  Opaque   |
//      Shadowed | Shadowed |     -     |

#[derive(Default)]
struct Deletions {
    //singles: BTreeSet<PathBuf>,
    //opaques: BTreeSet<PathBuf>,
    //seen: BTreeSet<PathBuf>,
    map: BTreeMap<PathBuf, DeletionState>,

    whiteout_q: Vec<PathBuf>,
    opaque_q: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Stats {
    deletions: usize,
    deletion_dirs: usize,
    opaques: usize,
    shadowed: usize,
    deletions_map_size: usize,
    //singles_size: usize,
    //opaques_size: usize,
    //seen_size: usize,
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
//
// Ran into some weirdness with unicode paths, in index.docker.io/library/gcc:13.3.0 there is a
// file "etc/ssl/certs/NetLock_Arany_=Class_Gold=_Főtanúsítvány.pem" whose path name is 62 bytes
// long, but for some reason the tar layer stored this utf8 path name in a pax extension header
// (key = "path") and in the header itself is an asciify "etc/ssl/certs/NetLock_Arany_=Class_Gold=_Ftanstvny.pem"
// and I wasn't writing out the pax extensions so the output tar was getting the ascii version
//
// GzDecoder will create a BufReader internally if it doesn't get a BufRead, so no need to pass in
// a BufRead

pub fn squash<W, R>(layer_readers: &mut [R], out: &mut W) -> Result<Stats, SquashError>
where
    W: Write,
    R: Read + Seek,
{
    let mut deletions = Deletions::default();
    //let mut hardlinks: BTreeSet<PathBuf> = BTreeSet::new();

    let mut aw = ArchiveBuilder::new(out);
    let mut stats = Stats::default();

    // we do enumerate, then rev, so i==0 is layer_readers[0] and is our last layer to be processed
    // where we can skip storing deletions
    for (i, reader) in layer_readers.iter_mut().enumerate().rev() {
        // TODO handle more archive types
        let mut layer = Archive::new(GzDecoder::new(&mut *reader));
        for entry in layer.entries()? {
            let mut entry = entry?;

            match whiteout(&entry)? {
                Some(Whiteout::Single(path)) => {
                    if i != 0 {
                        deletions.push_whiteout(path);
                    }
                    continue;
                }
                Some(Whiteout::Opaque(path)) => {
                    if i != 0 {
                        deletions.push_opaque(path);
                    }
                    continue;
                }
                _ => {}
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

            //if let Some(_) = entry.header().as_ustar() {
            //    eprintln!("entry is ustar");
            //} else if let Some(_) = entry.header().as_gnu() {
            //    eprintln!("entry is gnu");
            //}

            match deletions.is_deleted(&entry.path()?) {
                Some(DeletionReason::Single) => {
                    stats.deletions += 1;
                    continue;
                }
                Some(DeletionReason::SingleDir) => {
                    stats.deletion_dirs += 1;
                    continue;
                }
                Some(DeletionReason::Opaque) => {
                    stats.opaques += 1;
                    continue;
                }
                Some(DeletionReason::Shadowed) => {
                    stats.shadowed += 1;
                    continue;
                }
                _ => {}
            }

            // TODO should / do we need to exclude "path" and "link" extensions since
            // the append_{link,data} calls will emit those for us
            // and just doing the pax extension alone didn't seem to be enough to make long paths
            // work
            if let Some(extensions) = entry.pax_extensions()? {
                // even though PaxExtensions implements IntoIter, it has the wrong type, first
                // because its element type is Result<PaxExtension> and not (&str, &[u8]) but
                // also because the key() returns a Result<&str> because it may not be valid
                // utf8. maybe there is a fancy way of adapting the iterator but doesn't matter
                let mut acc = vec![];
                for extension in extensions.into_iter() {
                    let extension = extension?;
                    let key = extension.key().map_err(|_| SquashError::Utf8Error)?;
                    let value = extension.value_bytes();
                    acc.push((key, value));
                }
                aw.append_pax_extensions(acc)?;
            }

            // annoying we have to clone the header since we have to borrow the entry to read
            // from. same with owning the path
            let mut header = entry.header().clone();
            match entry.header().entry_type() {
                EntryType::Link | EntryType::Symlink => {
                    let path = entry.path()?;
                    let link = entry.link_name()?.ok_or(SquashError::HardlinkNoLink)?;
                    aw.append_link(&mut header, path, link)?;
                }
                _ => {
                    let path = entry.path()?.into_owned();
                    aw.append_data(&mut header, path, &mut entry)?;
                }
            }

            // once we write a file, we mark it as deleted so we do not write it again
            // on the last layer there is no point storing the deletion
            if i != 0 {
                deletions.insert_seen(entry.path()?.into());
            }
        }

        if i != 0 {
            deletions.end_of_layer();
        }
    }

    aw.finish().map_err(|_| SquashError::Finish)?;

    stats.deletions_map_size = deletions.map.len();
    //stats.seen_size = deletions.seen.len();
    //stats.whiteout_size = deletions..len();
    //stats.opaque_size = deletions.opaques.len();

    Ok(stats)
}

impl Deletions {
    fn push_whiteout(&mut self, p: PathBuf) {
        self.whiteout_q.push(p);
    }
    fn push_opaque(&mut self, p: PathBuf) {
        self.opaque_q.push(p);
    }
    fn insert_seen(&mut self, p: PathBuf) {
        //self.seen.insert(p);
        self.insert(p, DeletionState::Shadowed);
    }
    fn is_deleted<P: AsRef<Path>>(&mut self, p: P) -> Option<DeletionReason> {
        //let p = p.as_ref();
        //if self.seen.contains(p) {
        //    return Some(DeletionReason::Shadowed);
        //} else if let Some(x) = lower_bound_inclusive(&self.singles, p) {
        //    if p == x {
        //        return Some(DeletionReason::Single);
        //    } else if p.starts_with(x) {
        //        return Some(DeletionReason::SingleDir);
        //    }
        //} else if let Some(x) = lower_bound_exclusive(&self.opaques, p) {
        //    if p.starts_with(x) {
        //        return Some(DeletionReason::Opaque);
        //    }
        //}
        //None
        let p = p.as_ref();
        //eprintln!("is_deleted {:?}", p);
        let mut iter = self.map.range::<Path, _>((Bound::Unbounded, Bound::Included(p)));
        let (key, state) = iter.next_back()?;
        //eprintln!("is_deleted {:?} bound included {:?} {:?}", p, key, state);
        //if key == p { // exact match on the key we are searching for
        //    match state {
        //        DeletionState::Shadowed => {
        //            return Some(DeletionReason::Shadowed);
        //        }
        //        DeletionState::Whiteout => {
        //            return Some(DeletionReason::Single);
        //        }
        //        _ => {} // Opaque exact match doesn't do anything
        //    }
        //} else if *state == DeletionState::Whiteout && p.starts_with(key) {
        //    return Some(DeletionReason::SingleDir);
        //} else if *state == DeletionState::Opaque && p.starts_with(key) {
        //    return Some(DeletionReason::Opaque);
        //}
        match state {
            DeletionState::Shadowed if key == p => {
                return Some(DeletionReason::Shadowed);
            }
            DeletionState::Whiteout if key == p => {
                return Some(DeletionReason::Single);
            }
            DeletionState::Whiteout if p.starts_with(key) => {
                return Some(DeletionReason::SingleDir);
            }
            DeletionState::Opaque if key !=p && p.starts_with(key) => {
                return Some(DeletionReason::Opaque);
            }
            _ => {}
        }

        // not an exact match
        let (key, state) = iter.next_back()?;
        if *state == DeletionState::Opaque && p.starts_with(key) {
            return Some(DeletionReason::Opaque);
        }
        None
    }
    fn insert(&mut self, path: PathBuf, reason: DeletionState) {
        use DeletionState::*;
        if let Some(state) = self.map.get_mut(&path) {
            *state = reason;
            //     old  ,  new
            //match (&state, reason) {
            //    (Whiteout, Whiteout) |
            //    (Opaque, Opaque) |
            //    (Shadowed, Shadowed)  => {
            //        // kinda weird duplicate but okay
            //    }
            //    (Whiteout, Opaque) => { }
            //    (Whiteout, Shadowed) => { }
            //    (Opaque, Whiteout) => {}
            //    (Opaque, Shadowed) => {}
            //    // if something is already in the map b/c it is shadowed (already seen), and then a
            //    // lower layer whiteouts/opaques it, update to reflect that
            //    (Shadowed, Whiteout) => {
            //        *state = Whiteout;
            //    }
            //    (Shadowed, Opaque) => {
            //        *state = Opaque;
            //    }
            //}
        } else {
            self.map.insert(path, reason);
        }
    }
    fn end_of_layer(&mut self) {
        //self.map.extend(
        //    self.whiteout_q.drain(..)
        //    .map(|x| (x, DeletionState::Whiteout))
        //);
        //self.map.extend(
        //    self.opaque_q.drain(..)
        //    .map(|x| (x, DeletionState::Opaque))
        //);
        // have to take to not have a double borrow
        for x in std::mem::take(&mut self.whiteout_q).into_iter() {
            self.insert(x, DeletionState::Whiteout);
        }
        for x in std::mem::take(&mut self.opaque_q).into_iter() {
            self.insert(x, DeletionState::Opaque);
        }
        //for x in self.opaque_q.drain(..) {
        //    self.insert(DeletionState::Opaque, x);
        //}
        //self.singles.extend(self.singles_q.drain(..));
        //self.opaques.extend(self.opaques_q.drain(..));
    }
}

fn lower_bound_exclusive<'a, K, T>(set: &'a BTreeSet<T>, key: &K) -> Option<&'a T>
where
    T: Borrow<K> + Ord,
    K: Ord + ?Sized,
{
    let mut iter = set.range((Bound::Unbounded, Bound::Excluded(key)));
    iter.next_back()
}

fn lower_bound_inclusive<'a, K, T>(set: &'a BTreeSet<T>, key: &K) -> Option<&'a T>
where
    T: Borrow<K> + Ord,
    K: Ord + ?Sized,
{
    let mut iter = set.range((Bound::Unbounded, Bound::Included(key)));
    iter.next_back()
}

fn whiteout<R: Read>(entry: &Entry<R>) -> Result<Option<Whiteout>, SquashError> {
    // this should be true but idk if universal
    //if entry.header.entry_type() != EntryType::Regular {
    //    return Ok(None)
    //}
    let path = entry.path()?; // can fail if not unicode on Windows (so should never fail)
                              // TODO bad bad do prefix check against bytes, not string
    let name = {
        if let Some(name) = path.file_name() {
            name.as_encoded_bytes()
        } else {
            return Ok(None);
        }
    };
    // is starts_with correct or should it be exact comparison for opaques
    // like dir/.wh..wh..opqSUFFIX is some edge case
    if name.starts_with(b".wh..wh..opq") {
        if let Some(parent) = path.parent() {
            return Ok(Some(Whiteout::Opaque(parent.into())));
        } else {
            // I don't think this should happend
            return Err(SquashError::OpaqueWhiteoutNoParent);
        }
    }
    if let Some(trimmed) = name.strip_prefix(b".wh.") {
        let hidden = path.with_file_name(OsStr::from_bytes(trimmed));
        return Ok(Some(Whiteout::Single(hidden)));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::error;
    use std::io::{Cursor, Seek, SeekFrom};
    use std::process::{Command, Stdio};

    use flate2::write::GzEncoder;
    use tar::{Builder, Header};

    use crate::podman::build_with_podman;

    // sorted list of (key,value) bytes
    type Ext = Vec<(String, Vec<u8>)>;

    #[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    enum EntryTyp {
        #[default]
        File,
        Dir,
        Link,
        Symlink,
        Fifo,
    }

    // E is a standalone redux Entry
    #[derive(Default, Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
    struct E {
        typ: EntryTyp,
        path: PathBuf,
        data: Option<Vec<u8>>,
        ext: Ext,
        link: Option<PathBuf>,
        mtime: u64,
        uid: u64,
        gid: u64,
        mode: u32,
        // mode,uid,gid ...
    }

    impl E {
        fn file<P: Into<PathBuf>>(path: P, data: &[u8]) -> Self {
            Self {
                typ: EntryTyp::File,
                path: path.into(),
                data: Some(Vec::from(data)),
                mode: 0o744,
                ..Default::default()
            }
        }
        fn dir<P: Into<PathBuf>>(path: P) -> Self {
            Self {
                typ: EntryTyp::Dir,
                path: path.into(),
                ..Default::default()
            }
        }
        fn link<P1: Into<PathBuf>, P2: Into<PathBuf>>(path: P1, link: P2) -> Self {
            Self {
                typ: EntryTyp::Link,
                path: path.into(),
                link: Some(link.into()),
                ..Default::default()
            }
        }
        fn symlink<P1: Into<PathBuf>, P2: Into<PathBuf>>(path: P1, link: P2) -> Self {
            Self {
                typ: EntryTyp::Symlink,
                path: path.into(),
                link: Some(link.into()),
                ..Default::default()
            }
        }
        fn fifo<P: Into<PathBuf>>(path: P) -> Self {
            Self {
                typ: EntryTyp::Fifo,
                path: path.into(),
                ..Default::default()
            }
        }
    }

    type EList = BTreeSet<E>;

    fn as_pax<'a>(ext: &'a Ext) -> impl IntoIterator<Item = (&'a str, &'a [u8])> {
        ext.iter().map(|(x, y)| (x.as_ref(), y.as_ref()))
    }

    fn serialize_to_writer<W: Write>(entries: &[E], out: &mut W) {
        use EntryTyp::*;
        let mut writer = ArchiveBuilder::new(out);
        for entry in entries {
            writer.append_pax_extensions(as_pax(&entry.ext)).unwrap();
            let mut h = Header::new_ustar();
            h.set_uid(entry.uid);
            h.set_gid(entry.gid);
            h.set_mtime(entry.mtime);
            h.set_mode(entry.mode);
            match entry.typ {
                File => {
                    let data = entry.data.as_ref().unwrap();
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(data.len() as u64);
                    writer
                        .append_data(&mut h, &entry.path, Cursor::new(data))
                        .unwrap();
                }
                Dir => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    writer.append_data(&mut h, &entry.path, &b""[..]).unwrap();
                }
                Link => {
                    h.set_entry_type(tar::EntryType::Link);
                    h.set_size(0);
                    writer
                        .append_link(&mut h, &entry.path, entry.link.as_ref().unwrap())
                        .unwrap();
                }
                Symlink => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    writer
                        .append_link(&mut h, &entry.path, entry.link.as_ref().unwrap())
                        .unwrap();
                }
                Fifo => {
                    h.set_entry_type(tar::EntryType::Fifo);
                    h.set_size(0);
                    writer.append_data(&mut h, &entry.path, &b""[..]).unwrap();
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
                let path: PathBuf = x.path().unwrap().into();
                let ext = {
                    if let Some(ext) = x.pax_extensions().unwrap() {
                        ext.into_iter()
                            .map(|x| x.unwrap())
                            .map(|x| (x.key().unwrap().to_string(), Vec::from(x.value_bytes())))
                            .collect()
                    } else {
                        vec![]
                    }
                };
                let header = x.header();
                let uid = header.uid().unwrap();
                let gid = header.gid().unwrap();
                let mode = header.mode().unwrap();
                let mtime = header.mtime().unwrap();
                let entry_type = header.entry_type();

                let typ = match entry_type {
                    tar::EntryType::Regular => EntryTyp::File,
                    tar::EntryType::Directory => EntryTyp::Dir,
                    tar::EntryType::Link => EntryTyp::Link,
                    tar::EntryType::Symlink => EntryTyp::Symlink,
                    tar::EntryType::Fifo => EntryTyp::Fifo,
                    x => {
                        panic!("unhandled entry type {x:?}");
                    }
                };

                let link = match entry_type {
                    tar::EntryType::Link | tar::EntryType::Symlink => {
                        Some(x.link_name().unwrap().unwrap().into())
                    }
                    _ => None,
                };

                let data = match entry_type {
                    tar::EntryType::Regular => {
                        let mut data = vec![];
                        x.read_to_end(&mut data).unwrap();
                        Some(data)
                    }
                    _ => None,
                };

                E {
                    typ,
                    path,
                    link,
                    ext,
                    data,
                    uid,
                    gid,
                    mode,
                    mtime,
                }
            })
            .collect()
    }

    fn make_entry<P, F, B>(path: P, f: F) -> B
    where
        P: AsRef<Path>,
        F: Fn(Entry<'_, Cursor<Vec<u8>>>) -> B,
    {
        let mut h = Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(0);

        let buf = {
            let mut b = Builder::new(Cursor::new(vec![]));
            b.append_data(&mut h, path, &b""[..]).unwrap();
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
    fn test_whiteout_recognition() {
        make_entry("foo", |e| {
            assert_eq!(whiteout(&e).unwrap(), None);
        });

        let files = vec![
            (OsStr::from_bytes(b".wh.foo"),     OsStr::from_bytes(b"foo")),
            (OsStr::from_bytes(b"dir/.wh.foo"), OsStr::from_bytes(b"dir/foo")),
            (OsStr::from_bytes(b".wh.abc\xff"), OsStr::from_bytes(b"abc\xff")),
        ];

        let dirs = vec![
            (OsStr::from_bytes(b"dir/.wh..wh..opq"),       OsStr::from_bytes(b"dir")),
            (OsStr::from_bytes(b"dir1/dir2/.wh..wh..opq"), OsStr::from_bytes(b"dir1/dir2")),
            (OsStr::from_bytes(b"dir1\xff/.wh..wh..opq"),  OsStr::from_bytes(b"dir1\xff")),
        ];

        for (x, y) in files.into_iter() {
            make_entry(x, |e| {
                assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::Single(y.into())));
            });
        }

        for (x, y) in dirs.into_iter() {
            make_entry(x, |e| {
                assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::Opaque(y.into())));
            });
        }
    }

    #[rustfmt::skip]
    #[test]
    fn test_deletions() {
        // deletions should be the same whether the tar stream has trailing slashes on the dirs
        // this is because we use Path == and Path.starts_with which is smarter than String == and
        // String.starts_with
        for trailing_slash in vec![false].into_iter() {
            let mut d = Deletions::default();
            assert_eq!(d.is_deleted("foo"), None);
            // TODO I tried making push_whiteout take P: Into<PathBuf> but wasn't working great
            d.push_whiteout((if trailing_slash {"x/"} else {"x"}).into());
            d.end_of_layer();
            assert_eq!(d.is_deleted("x"), Some(DeletionReason::Single));
            assert_eq!(d.is_deleted("x/"), Some(DeletionReason::Single));
            assert_eq!(d.is_deleted("x/file"), Some(DeletionReason::SingleDir));
            assert_eq!(d.is_deleted("xfile"), None);

            d.push_opaque((if trailing_slash {"a/b/"} else {"a/b"}).into());
            d.end_of_layer();
            assert_eq!(d.is_deleted("a/b"), None);
            assert_eq!(d.is_deleted("a/b/"), None);
            assert_eq!(d.is_deleted("a/b/foo"), Some(DeletionReason::Opaque));
            assert_eq!(d.is_deleted("a/b/foo/"), Some(DeletionReason::Opaque));

            d.insert_seen((if trailing_slash {"q/"} else {"q"}).into());
            assert_eq!(d.is_deleted("q"), Some(DeletionReason::Shadowed), "trailing_slash={}", trailing_slash);
            assert_eq!(d.is_deleted("q/"), Some(DeletionReason::Shadowed));
            assert_eq!(d.is_deleted("q/file"), None);
            assert_eq!(d.is_deleted("qfile"), None);
        }
    }

    #[test]
    fn test_lower_bound_exclusive() {
        let set: BTreeSet<_> = vec!["dir1/", "dir1!", "dir2/dir3/"].into_iter().collect();
        assert_eq!(lower_bound_exclusive(&set, "dir1/"), Some("dir1!").as_ref());
        assert_eq!(
            lower_bound_exclusive(&set, "dir1/file"),
            Some("dir1/").as_ref()
        );
        assert_eq!(lower_bound_exclusive(&set, "dir0/file"), None);
        assert_eq!(
            lower_bound_exclusive(&set, "dir2/file"),
            Some("dir2/dir3/").as_ref()
        );
    }

    #[test]
    fn test_lower_bound_inclusive() {
        let set: BTreeSet<_> = vec!["dir1/", "dir1!", "dir2/dir3/"].into_iter().collect();
        assert_eq!(
            lower_bound_inclusive(&set, "dir1/file"),
            Some("dir1/").as_ref()
        );
        assert_eq!(lower_bound_inclusive(&set, "dir1/"), Some("dir1/").as_ref());
    }

    #[test]
    fn test_serde() {
        let long_name = "a".repeat(101);
        let with_attrs = {
            let mut entry = E::file("attrs", b"hii");
            entry.uid = 1000;
            entry.gid = 1000;
            entry.mtime = 1024;
            entry.mode = 0o755;
            entry
        };
        let entries = vec![
            E::file("x", b"hi"),
            E::link("y", "x"),
            E::file(&long_name, b"foo"),
            E::link("z", &long_name),
            E::symlink("s", &long_name),
            E::fifo("fifo"),
            with_attrs,
        ];
        let buf = serialize(&entries);
        assert_eq!(entries.into_iter().collect::<EList>(), deserialize(&buf));
    }

    fn squash_layers_vec(layers: Vec<Vec<E>>) -> Result<EList, SquashError> {
        let mut readers: Vec<Cursor<Vec<u8>>> = layers
            .into_iter()
            .map(|x| Cursor::new(serialize_gz(&x)))
            .collect();
        let mut buf = Cursor::new(vec![]);
        let _ = squash(&mut readers, &mut buf)?;
        Ok(deserialize(&buf.into_inner()))
    }

    macro_rules! check_squash {
        ($layers:expr, $expected:expr) => {{
            assert_eq!(
                squash_layers_vec($layers).unwrap(),
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
    fn test_squash_file_shared_prefix() {
        // this checks a tricky condition where the final layer stores x as a "deletion" so it
        // won't get emitted twice, but we don't want that to behave the same as .wh.x since that
        // would delete x/y in a lower dir
        check_squash!(
            vec![
                vec![E::file("xy", b"bye")],
                vec![E::file("x/y", b"bye")],
                vec![E::file("x", b"hi")],
            ],
            vec![E::file("x", b"hi"), E::file("xy", b"bye"), E::file("x/y", b"bye")]
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_file_whiteout() {
        // whiteout of a file
        check_squash!(
            vec![
                vec![E::file("x", b"hi")],
                vec![E::file(".wh.x", b""), E::file("y", b"hi")],
                vec![E::file(".wh.y", b"")],
            ],
            vec![]
        );

        // whiteout a dir, the dir itself and all files below should be wiped
        // but a file that shares a prefix should not
        check_squash!(
            vec![
                vec![E::dir("x"), E::dir("x/y"), E::file("x/foo", b""), E::file("x/y/bar", b""), E::file("x!ile", b"")],
                vec![E::file(".wh.x", b"")],
            ],
            vec![E::file("x!ile", b"")]
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

    #[rustfmt::skip]
    #[test]
    fn test_squash_chained_deletions() {
        check_squash!(
            vec![
                vec![E::dir("x"), E::file("x/a", b"hi")],
                vec![E::file(".wh.x", b"")],
                vec![E::file("x", b"bye")],
            ],
            vec![E::file("x", b"bye")]
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_unicode() {
        check_squash!(
            vec![
                vec![E::file("hellö", b"foo")],
            ],
            vec![E::file("hellö", b"foo")]
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_byte_paths() {
        let non_utf8_a = OsStr::from_bytes(b"abc\xff");
        let non_utf8_b = OsStr::from_bytes(b"def\xff");
        let non_utf8_b_whiteout = OsStr::from_bytes(b".wh.def\xff");
        // TODO add opaque whiteout
        check_squash!(
            vec![
                vec![E::file(non_utf8_a, b"foo"), E::link("link", non_utf8_a), E::file(non_utf8_b, b"bar")],
                vec![E::file(non_utf8_b_whiteout, b"")],
            ],
            vec![E::file(non_utf8_a, b"foo"), E::link("link", non_utf8_a)]
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_long_paths() {
        // 100 is the max length without extensions
        let long_name = "a".repeat(101);
        check_squash!(
            vec![
                vec![E::file(&long_name, b"foo"), E::link("link", &long_name), E::symlink("slink", &long_name)],
            ],
            vec![E::file(&long_name, b"foo"), E::link("link", &long_name), E::symlink("slink", &long_name)]
        );
    }

    /// returns the difference (podman - squash) of podman exporting a flat image vs us squashing
    /// the layers produced when running the containerfile
    fn podman_squash_diff(containerfile: &str) -> Result<(EList, EList), Box<dyn error::Error>> {
        let rootfs = build_with_podman(containerfile)?;
        let mut layers: Vec<_> = rootfs.layers.into_iter().map(Cursor::new).collect();

        let mut buf = Cursor::new(vec![]);
        squash(&mut layers, &mut buf).unwrap();
        let our_combined = deserialize(&buf.into_inner());
        let podman_combined = deserialize(&rootfs.combined);
        Ok((our_combined, podman_combined))
    }

    macro_rules! check_podman {
        ($containerfile:expr, $expected_ours_minus_podman:expr, $expected_podman_minus_ours:expr) => {{
            if let Err(_) = Command::new("podman")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                eprintln!("podman missing");
            } else {
                let (ours, podman) = podman_squash_diff($containerfile).unwrap();

                // doing two of these sequentially is actually annoying because when the first
                // fails may want to see the other to diagnose the problem..
                assert_eq!(
                    ours.difference(&podman).cloned().collect::<EList>(),
                    $expected_ours_minus_podman.into_iter().collect::<EList>(),
                    "ours minus podman"
                );

                assert_eq!(
                    podman.difference(&ours).cloned().collect::<EList>(),
                    $expected_podman_minus_ours.into_iter().collect::<EList>(),
                    "podman minus ours"
                );
            }
        }};
    }

    // currently we output a dir for . since that is from the busybox layer, but podman doesn't for
    // some reason. That does seem important if you need to set the permissions on / or whatever.
    // and so when we check, we have to check against the exact mtime etc so this is that dir
    fn busybox_root_dir() -> E {
        E { typ: EntryTyp::Dir, path: "./".into(), data: None, ext: vec![], link: None, mtime: 1727386302, uid: 0, gid: 0, mode: 0o755 }
    }

    #[rustfmt::skip]
    #[test]
    fn test_podman_1() {
        check_podman!(r#"
FROM docker.io/library/busybox@sha256:22f27168517de1f58dae0ad51eacf1527e7e7ccc47512d3946f56bdbe913f564
RUN echo hi > x && ln x y && mkfifo fifo
        "#,
        vec![busybox_root_dir()], // ours minus podman
        vec![]  // podman minus ours
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_podman_2() {
        check_podman!(r#"
FROM docker.io/library/busybox@sha256:22f27168517de1f58dae0ad51eacf1527e7e7ccc47512d3946f56bdbe913f564
RUN echo hi > x
RUN ln x y
RUN mkfifo fifo
        "#,
        vec![busybox_root_dir()], // ours minus podman
        vec![]  // podman minus ours
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_podman_3() {
        check_podman!(r#"
FROM docker.io/library/busybox@sha256:22f27168517de1f58dae0ad51eacf1527e7e7ccc47512d3946f56bdbe913f564
RUN mkdir -p x/y && touch xfile x/file x/y/file
RUN rm -rf x
        "#,
        vec![busybox_root_dir()], // ours minus podman
        vec![]  // podman minus ours
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_podman_4() {
        check_podman!(r#"
FROM docker.io/library/busybox@sha256:22f27168517de1f58dae0ad51eacf1527e7e7ccc47512d3946f56bdbe913f564
RUN touch x && setfattr -n user.MYATTR -v foo x
RUN chmod 444 x
        "#,
        vec![busybox_root_dir()], // ours minus podman
        vec![]  // podman minus ours
        );
    }
}
