use flate2::read::GzDecoder;
use std::borrow::Borrow;
use std::cmp::Ord;
use std::collections::BTreeSet;
use std::io;
use std::io::{Read, Seek, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
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

enum WhiteoutKind {
    Single,
    Opaque,
}

#[derive(Debug,Default)]
pub struct Stats {
    deletions: usize,
    opaques: usize,
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
                        deletions.push_file(path);
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

            match deletions.is_deleted(entry.path()?) {
                Some(WhiteoutKind::Single) => {
                    stats.deletions += 1;
                    continue;
                }
                Some(WhiteoutKind::Opaque) => {
                    stats.opaques += 1;
                    continue;
                }
                _ => {}
            }

            if let Some(extensions) = entry.pax_extensions()? {
                // even though PaxExtensions implements IntoIter, it has the wrong type, first
                // because its element type is Result<PaxExtension> and not (&str, &[u8]) but
                // also because the key() returns a Result<&str> because it may not be valid
                // utf8
                let mut acc = vec![];
                for extension in extensions.into_iter() {
                    let extension = extension?;
                    let key = extension.key().map_err(|_| SquashError::Utf8Error)?;
                    let value = extension.value_bytes();
                    eprintln!("writing  extension {:?} {:?}", key, value);
                    acc.push((key, value));
                }
                aw.append_pax_extensions(acc)?;
            }
            //eprintln!("entry has path{:?} len={}", entry.path().unwrap(), entry.path().unwrap().as_os_str().as_encoded_bytes().len());
            //
            // annoying we have to clone the header since we have to borrow the entry to read
            // from. same with owning the path
            // would it be right to use .append_data(entry.header(), path, entry)?
            // would it double write an extension? b/c we already write the extension above
            //aw.append(&entry.header().clone(), &mut entry)?;
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
                deletions.push_file(entry.path()?.into());
            }
        }

        // end of layer, updated deleted_{files,opaques}
        if i != 0 {
            deletions.end_of_layer();
        }
    }

    aw.finish().map_err(|_| SquashError::Finish)?;

    Ok(stats)
}

#[derive(Default)]
struct Deletions {
    singles: BTreeSet<PathBuf>,
    opaques: BTreeSet<PathBuf>,

    singles_q: Vec<PathBuf>,
    opaques_q: Vec<PathBuf>,
}

impl Deletions {
    fn push_file(&mut self, p: PathBuf) {
        self.singles_q.push(p);
    }
    fn push_opaque(&mut self, p: PathBuf) {
        self.opaques_q.push(p);
    }
    fn is_deleted<P: AsRef<Path>>(&mut self, p: P) -> Option<WhiteoutKind> {
        if self.singles.contains(p.as_ref()) { Some(WhiteoutKind::Single) }
        else if opaque_deleted(&self.opaques, p) { Some(WhiteoutKind::Opaque) }
        else { None }
    }
    fn end_of_layer(&mut self) {
        self.singles.extend(self.singles_q.drain(..));
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
    // TODO bad bad do prefix check against bytes, not string
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
        return Ok(Some(Whiteout::Single(hidden)));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    use flate2::write::GzEncoder;
    use std::io::{Cursor, Seek, SeekFrom};
    use tar::{Builder, Header, EntryType};
    use std::error;

    use crate::podman::build_with_podman;

    // E is a standalone redux Entry
    #[derive(Debug, PartialOrd, Ord, PartialEq, Eq)]
    enum E {
        File { path: PathBuf, data: Vec<u8> },
        Dir { path: PathBuf },
        Link { path: PathBuf, link: PathBuf },
        Symlink { path: PathBuf, link: PathBuf },
    }

    impl E {
        fn file<P: Into<PathBuf>>(path: P, data: &[u8]) -> Self {
            Self::File {
                path: path.into(),
                data: Vec::from(data),
            }
        }
        fn dir<P: Into<PathBuf>>(path: P) -> Self {
            Self::Dir { path: path.into() }
        }
        fn link<P1: Into<PathBuf>, P2: Into<PathBuf>>(path: P1, link: P2) -> Self {
            Self::Link {
                path: path.into(),
                link: link.into(),
            }
        }
        fn symlink<P1: Into<PathBuf>, P2: Into<PathBuf>>(path: P1, link: P2) -> Self {
            Self::Symlink {
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
                    h.set_entry_type(EntryType::Regular);
                    h.set_size(data.len() as u64);
                    writer.append_data(&mut h, path, Cursor::new(&data)).unwrap();
                }
                E::Dir { path } => {
                    h.set_entry_type(EntryType::Directory);
                    h.set_size(0);
                    writer.append_data(&mut h, path, &b""[..]).unwrap();
                }
                E::Link { path, link } => {
                    h.set_entry_type(EntryType::Link);
                    h.set_size(0);
                    writer.append_link(&mut h, path, link).unwrap();
                }
                E::Symlink { path, link } => {
                    h.set_entry_type(EntryType::Symlink);
                    h.set_size(0);
                    writer.append_link(&mut h, path, link).unwrap();
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
                    EntryType::Symlink => {
                        let link = x.link_name().unwrap().unwrap().into();
                        E::Symlink { path, link }
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
                assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::Single(y.into())));
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
        let mut long_name = String::new();
        for _ in 0..101 {
            long_name.push('a');
        }
        let entries = vec![
            E::file("x", b"hi"),
            E::link("y", "x"),
            E::file(&long_name, b"foo"),
            E::link("z", &long_name),
            E::symlink("s", &long_name),
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
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let non_utf8 = OsStr::from_bytes(b"abc\xfffoo");
        check_squash!(
            vec![
                vec![E::file(non_utf8, b"foo"), E::link("link", non_utf8)],
            ],
            vec![E::file(non_utf8, b"foo"), E::link("link", non_utf8)]
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_squash_long_paths() {
        // 100 is the max length without extensions
        let mut long_name = String::new();
        for _ in 0..101 {
            long_name.push('a');
        }
        check_squash!(
            vec![
                vec![E::file(&long_name, b"foo"), E::link("link", &long_name), E::symlink("slink", &long_name)],
            ],
            vec![E::file(&long_name, b"foo"), E::link("link", &long_name), E::symlink("slink", &long_name)]
        );
    }

    // NOTE this always skips the base layer and you should probably rm -rf /bin
    // and there are the annoying proc,run,sys dirs that are always there
    fn squash_containerfile(containerfile: &str) -> Result<EList, Box<dyn error::Error>> {
        let mut layers: Vec<_> = build_with_podman(containerfile)
            .unwrap()
            .into_iter()
            .skip(1) // skip the base layer
            .map(Cursor::new)
            .collect();

        let mut buf = Cursor::new(vec![]);
        squash(&mut layers, &mut buf).unwrap();
        Ok(deserialize(&buf.into_inner()))
    }

    macro_rules! check_podman {
        ($containerfile:expr, $expected:expr) => {{
            assert_eq!(
                squash_containerfile($containerfile).unwrap(),
                $expected.into_iter().collect::<EList>()
            );
        }};
    }

    #[rustfmt::skip]
    #[test]
    fn test_podman_cross_layer_link() {
        // cross layer link
        check_podman!(r#"
FROM docker.io/library/busybox@sha256:22f27168517de1f58dae0ad51eacf1527e7e7ccc47512d3946f56bdbe913f564
RUN echo hi > x
RUN ln x y
RUN rm -rf /bin
            "#,
            vec![
                E::file("x", b"hi\n"), E::link("y", "x"),
                // these are annoyingly always present in podman's layers
                // also note the trailing slash
                E::dir("proc/"), E::dir("run/"), E::dir("sys/"),
            ]
        );
    }
}
