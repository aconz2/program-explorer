use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::io::{BufReader, Read, Seek, Write};
use std::ops::Bound;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[cfg(feature = "nocrc")]
use flate2::bufread::DeflateDecoder;
use flate2::bufread::GzDecoder;
use rustix::fs::Mode;
use tar::{Archive, Builder as ArchiveBuilder, Entry, EntryType};
use zstd::stream::Decoder as ZstdDecoder;

use peoci::Compression;

use peerofs::build::{
    Builder as ErofsBuilder, Error as ErofsError, Meta as ErofsMeta, Stats as ErofsStats, XattrMap,
};

#[derive(Debug)]
pub enum SquashError {
    Io(io::Error),
    OpaqueWhiteoutNoParent,
    HardlinkNoLink,
    Finish,
    Utf8Error,
    MkfsFailed,
    Mkfifo,
    FifoOpen,
    UidTooBig,
    GidTooBig,
    UnhandledEntryType(EntryType),
    Erofs(ErofsError),
}

impl From<io::Error> for SquashError {
    fn from(e: io::Error) -> Self {
        SquashError::Io(e)
    }
}

impl From<ErofsError> for SquashError {
    fn from(e: ErofsError) -> Self {
        SquashError::Erofs(e)
    }
}

#[derive(PartialEq, Debug)]
enum Whiteout {
    Whiteout(PathBuf),
    Opaque(PathBuf),
}

#[derive(Debug, PartialEq, Clone)]
enum DeletionState {
    Whiteout,
    Opaque,
    Shadowed,
}

#[derive(PartialEq, Debug)]
enum DeletionReason {
    Whiteout,
    WhiteoutDir,
    Opaque,
    Shadowed,
}

// Table giving the DeletionReason for each combination of DeletionState and the Match Type.
// DeletionState is the state of a stored path in whatever impl Deletions. Match Type is whether
// the path matches that thing exactly or is a child.
//                        Match Type
//               |    Exact   |    Child    |
// DeletionState |--------------------------|
//      Whiteout |  Whiteout  | WhiteoutDir |
//        Opaque |     -      |   Opaque    |
//      Shadowed |  Shadowed  |      -      |

trait Deletions {
    fn push_whiteout(&mut self, x: PathBuf);
    fn push_opaque(&mut self, x: PathBuf);
    fn is_deleted<P: AsRef<Path>>(&mut self, x: P) -> Option<DeletionReason>;
    fn insert_seen(&mut self, p: PathBuf);
    fn end_of_layer(&mut self);
}

#[derive(Default)]
struct DeletionsOsString {
    map: BTreeMap<OsString, DeletionState>,
    whiteout_q: Vec<OsString>,
    opaque_q: Vec<OsString>,
}

#[allow(dead_code)]
#[derive(Default)]
struct DeletionsPathBuf {
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
}

pub trait EntryCallback {
    fn on_entry<R: Read>(&mut self, entry: &mut Entry<'_, R>) -> Result<(), SquashError>;
}

// Important notes about the OCI spec
// "Extracting a layer with hardlink references to files outside of the layer may fail."
//
// Compared to sylabs https://github.com/sylabs/oci-tools/blob/main/pkg/mutate/squash.go
// they buffer files per layer in memory to deal with hardlinks. I haven't (yet) run into or
// understand fully the situations they are handling, but for now we are not buffering file data in
// memory or handling hardlinks specially at all. If we do handle hardlinks, I think doing a second
// pass is a better option, though it does eliminate the possibility of streaming from network, but
// I think that is okay. I intend to wait for all layers to be done until processing.
// I also think the whiteout/opaque handling is better here because we do a single BTree lookup
// that handles exact match and prefix handling without doing a hash lookup of every path and all
// its ancestors.
//
// Total memory usage is something like the sum of path lengths from all entries
// minus the size of those paths lengths from the first layer.
// This is actually pretty good for containers with a big first layer and some smaller layers after that.
//
// Ran into some weirdness with unicode paths, in index.docker.io/library/gcc:13.3.0 there is a
// file "etc/ssl/certs/NetLock_Arany_=Class_Gold=_Főtanúsítvány.pem" whose path name is 62 bytes
// long, but for some reason the tar layer stored this utf8 path name in a pax extension header
// (key = "path") and in the header itself is an asciify "etc/ssl/certs/NetLock_Arany_=Class_Gold=_Ftanstvny.pem"
// and I wasn't writing out the pax extensions so the output tar was getting the ascii version
// That was fixed by using ArchiveWriter.append_data which takes care of writing the path out,
// though I thought it would be sufficient to just make sure to write any pax extensions
//
// DeletionsOsString is slightly (~3-5%) faster on gcc:13.3.0 map size grows to 23k, this is
// beacuse (I assume) equality and starts_with tests are directly on bytes and not iterator
// comparisons of Components. But, I'm undecided which to go with, so leaving as a trait with both
// impls for right now
//
// This all got a bit ugly when we supported multiple compression types None/Gzip/Zstd and wanting
// to support a callback version as well. Currently, we take the layers as pairs of (Compression,
// Read) where they all have to have the same Read impl (ie all File or all Vec but not mixed).
// The layer Read impl is NOT the decompressor Read b/c we want to support mixed types.
// We might have been able to take a list of dyn impl Read layers, BUT with a callback API, the
// Entry is specific to the Reader, so I don't think that works (maybe you could downcast or
// something?). So there are two places we have to support being parametric over the reader which
// is
//  1) squash_layer which processes a single layer of Archive<R>
//  2) EntryCallback which gets a callback on_entry which is generic over Entry<'_, R>
//
// We currently

struct SquashToTar<W: Write> {
    archive: ArchiveBuilder<W>,
}

impl<W: Write> EntryCallback for SquashToTar<W> {
    fn on_entry<R: Read>(&mut self, entry: &mut Entry<'_, R>) -> Result<(), SquashError> {
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
            self.archive.append_pax_extensions(acc)?;
        }

        // annoying we have to clone the header since we have to borrow the entry to read
        // from. same with owning the path
        let mut header = entry.header().clone();
        match entry.header().entry_type() {
            EntryType::Link | EntryType::Symlink => {
                let path = entry.path()?;
                let link = entry.link_name()?.ok_or(SquashError::HardlinkNoLink)?;
                self.archive.append_link(&mut header, path, link)?;
            }
            _ => {
                let path = entry.path()?.into_owned();
                self.archive.append_data(&mut header, path, entry)?;
            }
        }
        Ok(())
    }
}

pub fn squash_to_tar<W, R>(
    layer_readers: &mut [(Compression, R)],
    out: &mut W,
) -> Result<Stats, SquashError>
where
    W: Write,
    R: Read,
{
    let mut helper = SquashToTar {
        archive: ArchiveBuilder::new(out),
    };
    let stats = squash_cb(layer_readers, &mut helper)?;
    helper.archive.finish().map_err(|_| SquashError::Finish)?;

    Ok(stats)
}

struct SquashToErofs<W: Write + Seek> {
    builder: ErofsBuilder<W>,
}

fn header_to_meta(header: &tar::Header, xattrs: XattrMap) -> Result<ErofsMeta, SquashError> {
    let meta = ErofsMeta {
        uid: header
            .uid()?
            .try_into()
            .map_err(|_| SquashError::UidTooBig)?,
        gid: header
            .gid()?
            .try_into()
            .map_err(|_| SquashError::GidTooBig)?,
        mtime: header.mtime()?,
        mode: Mode::from_raw_mode(header.mode()?),
        xattrs,
    };
    Ok(meta)
}

// TODO the error handling here is subpar b/c everything gets funneled into SquashError
impl<W: Write + Seek> EntryCallback for SquashToErofs<W> {
    fn on_entry<R: Read>(&mut self, entry: &mut Entry<'_, R>) -> Result<(), SquashError> {
        let mut xattrs = XattrMap::new();
        if let Some(extensions) = entry.pax_extensions()? {
            for extension in extensions.into_iter() {
                let extension = extension?;
                // maybe we should check this is valid but idk
                let key = extension.key_bytes();
                let value = extension.value_bytes();
                xattrs.insert(key.into(), value.into());
            }
        }

        // annoying we have to clone the header since we have to borrow the entry to read
        // from. same with owning the path
        let header = entry.header().clone();
        let meta = header_to_meta(&header, xattrs)?;
        match entry.header().entry_type() {
            EntryType::Regular => {
                let path = entry.path()?.into_owned();
                self.builder
                    .add_file(path, meta, header.size()? as usize, entry)?;
            }
            EntryType::Directory => {
                let path = entry.path()?.into_owned();
                self.builder.upsert_dir(path, meta)?;
            }
            EntryType::Symlink => {
                let path = entry.path()?;
                let link = entry.link_name()?.ok_or(SquashError::HardlinkNoLink)?;
                self.builder.add_symlink(path, link, meta)?;
            }
            EntryType::Link => {
                let path = entry.path()?;
                let link = entry.link_name()?.ok_or(SquashError::HardlinkNoLink)?;
                self.builder.add_link(path, link, meta)?;
            }
            t => {
                return Err(SquashError::UnhandledEntryType(t));
            }
        }
        Ok(())
    }
}

pub fn squash_to_erofs<W, R>(
    layer_readers: &mut [(Compression, R)],
    out: &mut W,
) -> Result<(Stats, ErofsStats), SquashError>
where
    W: Write + Seek,
    R: Read,
{
    let mut helper = SquashToErofs {
        builder: ErofsBuilder::new(out)?,
    };
    let squash_stats = squash_cb(layer_readers, &mut helper)?;
    let (erofs_stats, _) = helper.builder.into_inner()?;

    Ok((squash_stats, erofs_stats))
}

fn squash_layer<R, D, F>(
    cb: &mut F,
    i: usize,
    stats: &mut Stats,
    deletions: &mut D,
    mut layer: Archive<R>,
) -> Result<(), SquashError>
where
    R: Read,
    D: Deletions,
    F: EntryCallback,
{
    for entry in layer.entries()? {
        let mut entry = entry?;

        match whiteout(&entry)? {
            Some(Whiteout::Whiteout(path)) => {
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

        match deletions.is_deleted(entry.path()?) {
            Some(DeletionReason::Whiteout) => {
                stats.deletions += 1;
                continue;
            }
            Some(DeletionReason::WhiteoutDir) => {
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

        cb.on_entry(&mut entry)?;

        // on the last layer there is no point storing the deletion
        if i != 0 {
            deletions.insert_seen(entry.path()?.as_os_str().into());
        }
    }

    // apply whiteouts/opques except on the last layer
    if i != 0 {
        deletions.end_of_layer();
    }
    Ok(())
}

pub fn squash_cb<R, F>(
    layer_readers: &mut [(Compression, R)],
    cb: &mut F,
) -> Result<Stats, SquashError>
where
    R: Read,
    F: EntryCallback,
{
    let mut deletions = DeletionsOsString::default();
    //let mut deletions = DeletionsPathBuf::default();

    //let mut hardlinks: BTreeSet<PathBuf> = BTreeSet::new();

    let mut stats = Stats::default();

    // we do enumerate, then rev, so i==0 is layer_readers[0] and is our last layer to be processed
    // where we can skip storing deletions
    for (i, (compression, reader)) in layer_readers.iter_mut().enumerate().rev() {
        match compression {
            Compression::None => {
                squash_layer(
                    cb,
                    i,
                    &mut stats,
                    &mut deletions,
                    Archive::new(BufReader::with_capacity(32 * 1024, &mut *reader)),
                )?;
            }
            Compression::Gzip => {
                #[cfg(feature = "nocrc")]
                let reader = {
                    let reader = GzDecoder::new(BufReader::new(&mut *reader));
                    let _ = reader
                        .header()
                        .expect("only way this can be none is if reader EWOULDBLOCK");
                    Archive::new(DeflateDecoder::new(reader.into_inner()))
                };
                #[cfg(not(feature = "nocrc"))]
                let reader = Archive::new(GzDecoder::new(BufReader::with_capacity(
                    32 * 1024,
                    &mut *reader,
                )));
                squash_layer(cb, i, &mut stats, &mut deletions, reader)?;
            }
            Compression::Zstd => {
                squash_layer(
                    cb,
                    i,
                    &mut stats,
                    &mut deletions,
                    Archive::new(ZstdDecoder::new(&mut *reader)?),
                )?;
            }
        }
    }

    stats.deletions_map_size = deletions.map.len();

    Ok(stats)
}

fn without_trailing_slash(x: OsString) -> OsString {
    let b = x.as_os_str().as_bytes();
    if b.ends_with(b"/") {
        // can you do this without allocating?
        let mut ret = OsString::new();
        ret.push(OsStr::from_bytes(&b[..b.len() - 1]));
        ret
    } else {
        x
    }
}

// checks if key starts_with prefix where both should have no trailing slash
/// assert!(!os_str_starts_with(OsStr::new("xfile"), OsStr::new("x")));
/// assert!(os_str_starts_with(OsStr::new("x/file"), OsStr::new("x")));
fn os_str_starts_with(x: &OsStr, prefix: &OsStr) -> bool {
    let x = x.as_bytes();
    let prefix = prefix.as_bytes();
    if let Some(rem) = x.strip_prefix(prefix) {
        rem.starts_with(b"/")
    } else {
        false
    }
}

impl DeletionsOsString {
    fn insert(&mut self, path: OsString, reason: DeletionState) {
        use DeletionState::*;
        self.map
            .entry(path)
            .and_modify(|state|
            //     old  ,  new
            match (&state, reason.clone()) {
                (Whiteout, Whiteout) | (Opaque, Opaque) | (Shadowed, Shadowed) => {
                    // kinda weird duplicate but okay
                }
                (Whiteout, Opaque) | (Whiteout, Shadowed) => {
                    // no change, stays as whiteout
                }
                // _ + Whiteout = Whiteout
                (Opaque, Whiteout) | (Shadowed, Whiteout) => {
                    *state = Whiteout;
                }
                // Shadowed + Opaque = Whiteout
                (Shadowed, Opaque) | (Opaque, Shadowed) => {
                    *state = Whiteout;
                }
            })
            .or_insert(reason);
    }
}

impl Deletions for DeletionsOsString {
    // push_* normally take output from fn whiteout which will always return results without a
    // trailing / but in testing and/or just to safeguard the logic with OsString, we check
    fn push_whiteout(&mut self, p: PathBuf) {
        self.whiteout_q.push(without_trailing_slash(p.into()));
    }
    fn push_opaque(&mut self, p: PathBuf) {
        self.opaque_q.push(without_trailing_slash(p.into()));
    }
    fn insert_seen(&mut self, p: PathBuf) {
        self.insert(p.into(), DeletionState::Shadowed);
    }
    fn is_deleted<P: AsRef<Path>>(&mut self, p: P) -> Option<DeletionReason> {
        let p = {
            let p = p.as_ref().as_os_str();
            let b = p.as_bytes();
            if b.ends_with(b"/") {
                OsStr::from_bytes(&b[..b.len() - 1])
            } else {
                p
            }
        };
        // Query for an exact match of the path and anything less than it,
        // the first element of iter could be an exact match
        let (key, state) = self
            .map
            .range::<OsStr, _>((Bound::Unbounded, Bound::Included(p)))
            .next_back()?;

        // it would be nice if the iter already told us if we matched
        // and/or a starts_with_or_eq which just ran the components iter once and returned a
        // tristate
        // looking at Components more, I'm wondering if we should be normalizing paths (/ suffix for dirs) and storing
        // as OsString since the iter logic on every compare might be adding up

        match state {
            DeletionState::Shadowed if key == p => Some(DeletionReason::Shadowed),
            DeletionState::Whiteout if key == p => Some(DeletionReason::Whiteout),
            DeletionState::Whiteout if os_str_starts_with(p, key) => {
                Some(DeletionReason::WhiteoutDir)
            }
            DeletionState::Opaque if key != p && os_str_starts_with(p, key) => {
                Some(DeletionReason::Opaque)
            }
            _ => None,
        }
    }

    fn end_of_layer(&mut self) {
        // have to take to not have a double borrow
        for x in std::mem::take(&mut self.whiteout_q).into_iter() {
            self.insert(x, DeletionState::Whiteout);
        }
        for x in std::mem::take(&mut self.opaque_q).into_iter() {
            self.insert(x, DeletionState::Opaque);
        }
    }
}

impl DeletionsPathBuf {
    fn insert(&mut self, path: PathBuf, reason: DeletionState) {
        use DeletionState::*;
        // TODO use try_insert when stable

        if let Some(state) = self.map.get_mut(&path) {
            //     old  ,  new
            match (&state, reason) {
                (Whiteout, Whiteout) | (Opaque, Opaque) | (Shadowed, Shadowed) => {
                    // kinda weird duplicate but okay
                }
                (Whiteout, Opaque) | (Whiteout, Shadowed) => {
                    // no change, stays as whiteout
                }
                // _ + Whiteout = Whiteout
                (Opaque, Whiteout) | (Shadowed, Whiteout) => {
                    *state = Whiteout;
                }
                // Shadowed + Opaque = Whiteout
                (Shadowed, Opaque) | (Opaque, Shadowed) => {
                    *state = Whiteout;
                }
            }
        } else {
            self.map.insert(path, reason);
        }
    }
}

impl Deletions for DeletionsPathBuf {
    fn push_whiteout(&mut self, p: PathBuf) {
        self.whiteout_q.push(p);
    }
    fn push_opaque(&mut self, p: PathBuf) {
        self.opaque_q.push(p);
    }
    fn insert_seen(&mut self, p: PathBuf) {
        self.insert(p, DeletionState::Shadowed);
    }
    fn is_deleted<P: AsRef<Path>>(&mut self, p: P) -> Option<DeletionReason> {
        let p = p.as_ref();

        // Query for an exact match of the path and anything less than it
        // the first element of iter could be an exact match
        let (key, state) = self
            .map
            .range::<Path, _>((Bound::Unbounded, Bound::Included(p)))
            .next_back()?;

        // it would be nice if the iter already told us if we matched
        // and/or a starts_with_or_eq which just ran the components iter once and returned a
        // tristate
        // looking at Components more, I'm wondering if we should be normalizing paths (/ suffix for dirs) and storing
        // as OsString since the iter logic on every compare might be adding up

        match state {
            DeletionState::Shadowed if key == p => Some(DeletionReason::Shadowed),
            DeletionState::Whiteout if key == p => Some(DeletionReason::Whiteout),
            DeletionState::Whiteout if p.starts_with(key) => Some(DeletionReason::WhiteoutDir),
            DeletionState::Opaque if key != p && p.starts_with(key) => Some(DeletionReason::Opaque),
            _ => None,
        }
    }

    fn end_of_layer(&mut self) {
        // have to take to not have a double borrow
        for x in std::mem::take(&mut self.whiteout_q).into_iter() {
            self.insert(x, DeletionState::Whiteout);
        }
        for x in std::mem::take(&mut self.opaque_q).into_iter() {
            self.insert(x, DeletionState::Opaque);
        }
    }
}

fn whiteout<R: Read>(entry: &Entry<R>) -> Result<Option<Whiteout>, SquashError> {
    // this should be true but idk if universal
    //if entry.header.entry_type() != EntryType::Regular {
    //    return Ok(None)
    //}
    let path = entry.path()?; // can fail if not unicode on Windows (so should never fail)
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
        return Ok(Some(Whiteout::Whiteout(hidden)));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::io::{Cursor, Seek, SeekFrom};
    use std::process::{Command, Stdio};

    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use zstd::stream::Encoder as ZstdEncoder;

    use crate::podman;
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
        fn with_uid(mut self: Self, uid: u64) -> Self {
            self.uid = uid;
            self
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
                        let mut vec: Vec<_> = ext
                            .into_iter()
                            .map(|x| x.unwrap())
                            .map(|x| (x.key().unwrap().to_string(), Vec::from(x.value_bytes())))
                            .collect();
                        vec.sort();
                        vec
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
                assert_eq!(whiteout(&e).unwrap(), Some(Whiteout::Whiteout(y.into())));
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
        // TODO this doesn't report the type and whether trailing slash was being used
        fn test<D: Deletions + Default>() {
            for trailing_slash in vec![false].into_iter() {
                let mut d = D::default();
                assert_eq!(d.is_deleted("foo"), None);

                d.push_whiteout((if trailing_slash {"x/"} else {"x"}).into());
                d.end_of_layer();
                assert_eq!(d.is_deleted("x"), Some(DeletionReason::Whiteout));
                assert_eq!(d.is_deleted("x/"), Some(DeletionReason::Whiteout));
                assert_eq!(d.is_deleted("x/file"), Some(DeletionReason::WhiteoutDir));
                assert_eq!(d.is_deleted("xfile"), None);

                d.push_opaque((if trailing_slash {"a/b/"} else {"a/b"}).into());
                d.end_of_layer();
                assert_eq!(d.is_deleted("a/b"), None);
                assert_eq!(d.is_deleted("a/b/"), None);
                assert_eq!(d.is_deleted("a/b/foo"), Some(DeletionReason::Opaque));
                assert_eq!(d.is_deleted("a/b/foo/"), Some(DeletionReason::Opaque));

                d.insert_seen((if trailing_slash {"q/"} else {"q"}).into());
                assert_eq!(d.is_deleted("q"), Some(DeletionReason::Shadowed));
                assert_eq!(d.is_deleted("q/"), Some(DeletionReason::Shadowed));
                assert_eq!(d.is_deleted("q/file"), None);
                assert_eq!(d.is_deleted("qfile"), None);
            }
        }

        test::<DeletionsOsString>();
        test::<DeletionsPathBuf>();
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
        let mut readers: Vec<_> = layers
            .into_iter()
            .map(|x| (Compression::Gzip, Cursor::new(serialize_gz(&x))))
            .collect();
        let mut buf = Cursor::new(vec![]);
        let _ = squash_to_tar(&mut readers, &mut buf)?;
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
    fn test_squash_deletion_state_update() {
        // test states `<from> then <to>` where the <from> state is encountered in a later layer
        // and then <to> is encountered in an earlier layer

        // shadowed then whiteout
        check_squash!(
            vec![
                vec![E::dir("x"), E::file("x/a", b"hi")],
                vec![E::file(".wh.x", b"")],
                vec![E::file("x", b"bye")],
            ],
            vec![E::file("x", b"bye")]
        );

        // shadowed then opaque
        check_squash!(
            vec![
                vec![E::dir("x").with_uid(1), E::file("x/a", b"hi")],
                vec![E::file("x/.wh..wh..opq", b"")],
                vec![E::dir("x").with_uid(2)],
            ],
            vec![E::dir("x").with_uid(2)]
        );

        // whiteout then opaque
        check_squash!(
            vec![
                vec![E::dir("x"), E::file("x/a", b"hi")],
                vec![E::file("x/.wh..wh..opq", b"")],
                vec![E::file(".wh.x", b"")],
            ],
            vec![]
        );

        // whiteout then shadowed
        check_squash!(
            vec![
                vec![E::file("x", b"hello")],
                vec![E::file("x", b"hi")],
                vec![E::file(".wh.x", b"")],
            ],
            vec![]
        );

        // opaque then shadowed
        check_squash!(
            vec![
                vec![E::dir("x").with_uid(1)],
                vec![E::dir("x").with_uid(2), E::file("x/a", b"hi")],
                vec![E::file("x/.wh..wh..opq", b"")],
            ],
            vec![E::dir("x").with_uid(2)]
        );

        // opaque then whiteout
        // this is not realistic
        check_squash!(
            vec![
                vec![E::dir("x"), E::file("x/a", b"hi")],
                vec![E::file(".wh.x", b"")],
                vec![E::file("x/.wh..wh..opq", b"")],
            ],
            vec![]
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

    // TODO unhandled
    #[rustfmt::skip]
    #[test]
    #[ignore]
    fn test_squash_root() {
        check_squash!(
            vec![
                vec![E::dir(".").with_uid(10)],
            ],
            vec![E::dir(".").with_uid(10)]
        );

        check_squash!(
            vec![
                vec![E::dir("./").with_uid(10), E::file("foo", b""), E::file("./foo", b"")],
                vec![E::file("./.wh..wh..opq", b"").with_uid(10)],
            ],
            vec![E::dir(".").with_uid(10)]
        );
    }

    /// returns the difference (podman - squash) of podman exporting a flat image vs us squashing
    /// the layers produced when running the containerfile
    fn podman_squash_diff(containerfile: &str) -> Result<(EList, EList), podman::Error> {
        let rootfs = build_with_podman(containerfile)?;
        let mut layers: Vec<_> = rootfs
            .layers
            .into_iter()
            .map(|(c, b)| (c, Cursor::new(b)))
            .collect();

        let mut buf = Cursor::new(vec![]);
        squash_to_tar(&mut layers, &mut buf).unwrap();
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
                // TODO is there a nicer way to signal test skipped?
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
        E {
            typ: EntryTyp::Dir,
            path: "./".into(),
            data: None,
            ext: vec![],
            link: None,
            mtime: 1727386302,
            uid: 0,
            gid: 0,
            mode: 0o755,
        }
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

    #[test]
    fn test_mixed_compression() {
        let f1 = E::file("none", b"none");
        let f2 = E::file("gz", b"gz");
        let f3 = E::file("zstd", b"zstd");

        let layer1 = {
            let mut encoder = Cursor::new(vec![]);
            serialize_to_writer(&[f1.clone()], &mut encoder);
            (Compression::None, Cursor::new(encoder.into_inner()))
        };
        // GzEncoder takes the vec directly whereas
        let layer2 = {
            let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
            serialize_to_writer(&[f2.clone()], &mut encoder);
            (Compression::Gzip, Cursor::new(encoder.finish().unwrap()))
        };
        let layer3 = {
            let mut encoder = ZstdEncoder::new(Vec::new(), 3).unwrap();
            serialize_to_writer(&[f3.clone()], &mut encoder);
            (Compression::Zstd, Cursor::new(encoder.finish().unwrap()))
        };
        let mut layers = vec![layer1, layer2, layer3];

        let mut buf = Cursor::new(vec![]);
        let _ = squash_to_tar(&mut layers, &mut buf).unwrap();
        let output = deserialize(&buf.into_inner());
        assert_eq!(vec![f1, f2, f3].into_iter().collect::<EList>(), output,)
    }
}
