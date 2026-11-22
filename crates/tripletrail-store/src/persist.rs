//! The on-disk form of a store: numbered checkpoints plus a write-ahead log.
//!
//! ```text
//! dir/CURRENT            the number n of the current checkpoint
//! dir/checkpoint-n/      dict.bin (dict-1.bin, ...) and one .seg file per quad permutation
//! dir/wal-n.log          transactions committed since checkpoint n
//! ```
//!
//! A checkpoint is written next to the current one and becomes current when `CURRENT` is
//! replaced (write to a temporary file, then rename), so a crash leaves either the old or
//! the new one. The log holds one line per added (`+`) or removed (`-`) quad in N-Quads
//! syntax, and a `C` line closing each transaction; it is synced before a commit returns.
//! On open, a transaction without its `C` line is dropped.
//!
//! A checkpoint also holds `checksums`: the length and CRC-32 of each of its files, written
//! last. Opening a checkpoint checks them, so a damaged file is reported as corrupt instead
//! of being read as wrong data. (A checkpoint without the file, from before it existed, is
//! read unchecked.)
//!
//! The dictionary only grows, so a checkpoint writes only the keys added since the last one,
//! as the next piece (`dict-1.bin`, `dict-2.bin`, ...), and hard-links the earlier pieces.
//! After [`DICT_PIECES`] pieces, the pieces after the first are written again as one (the
//! whole dictionary, when the first holds less than half of it). Each piece has its
//! saved hash index next to it (`dict.idx`, `dict-1.idx`, ...), so that opening a store
//! hashes no keys: an index that is missing, damaged or from another hash function is built
//! again on the heap, and written by the next checkpoint (#218). And the geometry literals of
//! each piece are saved in binary form next to it (`geoms.bin`, `geoms-1.bin`, ...), so a
//! checkpoint encodes only the geometries added since the last one.

use crate::StoreError;
use crate::buf::{Buf, Pod};
use crate::dict::{Dictionary, SavedIndex, index_check, index_slots};
use crate::geobin::{Saved, SavedBoxes, SavedPart};
use crate::index::{PERM_NAMES, QuadIndex};
/// Quad permutations in checkpoints written before PSOG and GPSO.
const OLD_PERMS: usize = 6;
use crate::segment::{Key, Segment};
use memmap2::Mmap;
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Quad;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The log is folded into a new checkpoint once it is this large.
pub(crate) const CHECKPOINT_LOG_SIZE: u64 = 256 << 20;

/// Dictionary files before `ends` was padded to an 8-byte boundary.
const DICT_MAGIC_V1: &[u8; 8] = b"OXGSDIC1";
const DICT_MAGIC_V2: &[u8; 8] = b"OXGSDIC2";
/// As v2; plain decimal `xsd:double` and `xsd:decimal` literals and most `xsd:date` and
/// `xsd:dateTime` literals have inline ids (issue #125).
const DICT_MAGIC: &[u8; 8] = b"OXGSDIC3";
/// As v3, in a piece that may hold compact WKT keys (#370): written with WKT literals in binary
/// form, or after it by a store that may still hold such keys. A binary from before them refuses
/// such a piece, as it would misread its keys (#428).
const DICT_MAGIC_COMPACT: &[u8; 8] = b"OXGSDIC4";
/// As v4, in a piece whose compact WKT keys may hold packed records (#152, see
/// [`crate::geobin::PACKED`]): what a piece that may hold compact keys is written as now. A
/// binary from before packed records refuses such a piece, as it could not read those keys.
const DICT_MAGIC_PACKED: &[u8; 8] = b"OXGSDIC5";
/// As v3, in a piece that may hold compact IRI keys (#152, see [`crate::dict::Namespaces`]):
/// written by a store that keeps namespaces. A binary from before them refuses such a piece.
const DICT_MAGIC_IRIS: &[u8; 8] = b"OXGSDIC6";
/// As v5 (compact WKT keys, packed records), in a piece that may also hold compact IRI keys.
const DICT_MAGIC_IRIS_PACKED: &[u8; 8] = b"OXGSDIC7";
/// The dictionary formats read, oldest first: their position is the version.
const DICT_MAGICS: [&[u8; 8]; 7] = [
    DICT_MAGIC_V1,
    DICT_MAGIC_V2,
    DICT_MAGIC,
    DICT_MAGIC_COMPACT,
    DICT_MAGIC_PACKED,
    DICT_MAGIC_IRIS,
    DICT_MAGIC_IRIS_PACKED,
];

/// Whether a piece of dictionary version `version` may hold compact WKT keys.
fn may_hold_compact_wkt(version: usize) -> bool {
    matches!(version, 3 | 4 | 6)
}

/// The magic a dictionary piece of `dict` is written with: whether it may hold compact WKT
/// keys (`compact`), and compact IRI keys (when `dict` keeps namespaces).
fn dict_magic(dict: &Dictionary, compact: bool) -> &'static [u8; 8] {
    match (dict.namespaces().len() > 0, compact) {
        (false, false) => DICT_MAGIC,
        (false, true) => DICT_MAGIC_PACKED,
        (true, false) => DICT_MAGIC_IRIS,
        (true, true) => DICT_MAGIC_IRIS_PACKED,
    }
}
/// The namespaces of compact IRI keys, in a checkpoint whose dictionary has any: the count,
/// the bytes' length, the ends (count + 1), then the bytes.
const NS_MAGIC: &[u8; 8] = b"OXGSNSP1";
const NAMESPACES: &str = "namespaces.bin";
/// A saved index of a dictionary piece: keys, slots and [`index_check`], then the slots.
const INDEX_MAGIC: &[u8; 8] = b"OXGSDIX1";
const INDEX_HEADER: usize = 32;
const SEG_MAGIC: &[u8; 8] = b"OXGSSEG1";
/// Segment files with blocks of varying size: the quads per block follow the offsets.
const SEG_MAGIC2: &[u8; 8] = b"OXGSSEG2";
/// Segment files with column blocks (#169): converted from the formats above when read.
const SEG_MAGIC3: &[u8; 8] = b"OXGSSEG3";
/// As format 3, with the blocks first and the sizes last, so that a segment can be written
/// front to back while it is built (#218).
const SEG_MAGIC4: &[u8; 8] = b"OXGSSEG4";
/// The sizes at the end of a format 4 segment file: quads, firsts, offsets, starts, bytes.
const SEG_TRAILER: usize = 5 * 8;
/// The geometries of the whole dictionary: count and bytes, ordinals, offsets, then records.
const GEO_MAGIC: &[u8; 8] = b"OXGSGEO1";
/// Those of one dictionary piece, written front to back: the records, padded to 8 bytes, the
/// ordinals (padded), the offsets, then the count and the records' length (#218).
const GEO_MAGIC2: &[u8; 8] = b"OXGSGEO2";
/// Format 2 with the boxes of the larger geometries after the offsets (#754): their ordinals
/// (padded), their corners (4 x f64), their CRS tags (padded), then the count, the records'
/// length and the boxes' count. See [`crate::geobin::saved_box`].
const GEO_MAGIC3: &[u8; 8] = b"OXGSGEO3";
/// Format 2 adds `square1` per group (#210); format 1 files are recomputed on first use.
const STATS_MAGIC: &[u8; 8] = b"OXGSSTA2";
const CHECKSUMS: &str = "checksums";
/// A checkpoint's dictionary is written as one file again once it has this many pieces.
const DICT_PIECES: usize = 8;

/// A file of a checkpoint's dictionary: its name, how many keys it holds and its sum.
#[derive(Clone, Debug)]
pub(crate) struct DictPiece {
    file: String,
    keys: usize,
    sum: Sum,
    /// The sum of its saved index, when it has one.
    index: Option<Sum>,
    /// The sum of its saved geometries, when it has them.
    geoms: Option<Sum>,
}

/// The file of the saved geometries of the dictionary piece in `file`.
fn geoms_name(file: &str) -> String {
    file.replacen("dict", "geoms", 1)
}

/// Writes the binary form of the geometry literal with this ordinal to the buffer; false
/// when it is not valid (nothing is saved for it then).
pub(crate) type EncodeGeometry<'a> = &'a (dyn Fn(u32, &mut Vec<u8>) -> bool + Sync);

/// The file of the saved index of the dictionary piece in `file`.
fn index_name(file: &str) -> String {
    format!("{}.idx", file.strip_suffix(".bin").unwrap_or(file))
}

fn dict_piece_name(n: usize) -> String {
    if n == 0 {
        "dict.bin".to_owned()
    } else {
        format!("dict-{n}.bin")
    }
}

/// Length and CRC-32 of a written file.
type Sum = (u64, u32);

/// A writer that keeps the length and CRC-32 of what goes through it.
struct Summing<W> {
    inner: W,
    len: u64,
    crc: crc32fast::Hasher,
}

impl<W: Write> Summing<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            len: 0,
            crc: crc32fast::Hasher::new(),
        }
    }

    fn finish(self) -> (W, Sum) {
        (self.inner, (self.len, self.crc.finalize()))
    }

    /// The writer, the bytes written and their checksum, still open to combine with another.
    fn finish_open(self) -> (W, u64, crc32fast::Hasher) {
        (self.inner, self.len, self.crc)
    }
}

impl<W: Write> Write for Summing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.len += n as u64;
        self.crc.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Opens `path` for a checkpoint file.
fn summing(path: &Path) -> io::Result<Summing<BufWriter<File>>> {
    Ok(Summing::new(BufWriter::new(File::create(path)?)))
}

/// Flushes and syncs a checkpoint file, and gives its sum.
fn finish(w: Summing<BufWriter<File>>) -> io::Result<Sum> {
    let (w, sum) = w.finish();
    w.into_inner()?.sync_all()?;
    Ok(sum)
}

fn sum_of(bytes: &[u8]) -> Sum {
    let mut crc = crc32fast::Hasher::new();
    // In parallel pieces for large files, combined in order.
    const PIECE: usize = 64 << 20;
    if bytes.len() > PIECE {
        let parts: Vec<crc32fast::Hasher> = bytes
            .par_chunks(PIECE)
            .map(|c| {
                let mut h = crc32fast::Hasher::new();
                h.update(c);
                h
            })
            .collect();
        for p in parts {
            crc.combine(&p);
        }
    } else {
        crc.update(bytes);
    }
    (bytes.len() as u64, crc.finalize())
}

/// The sums in a checkpoint's `checksums` file, by file name; `None` if it has none (an
/// older checkpoint).
fn read_sums(dir: &Path) -> Result<Option<std::collections::HashMap<String, Sum>>, StoreError> {
    let path = dir.join(CHECKSUMS);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut sums = std::collections::HashMap::new();
    for line in text.lines() {
        let mut parts = line.split(' ');
        let (Some(name), Some(len), Some(crc), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(corrupt(&path));
        };
        let len = len.parse().map_err(|_| corrupt(&path))?;
        let crc = u32::from_str_radix(crc, 16).map_err(|_| corrupt(&path))?;
        sums.insert(name.to_owned(), (len, crc));
    }
    Ok(Some(sums))
}

/// How much of a checkpoint is checked as it is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    /// The file lengths against the checksums, and the headers: what opening a mapped store
    /// does, without reading its data (#436). [`verify_checkpoint`] then checks the sums.
    Lengths,
    /// Also every file against its sum, and the dictionary keys and segment blocks.
    Full,
}

/// Whether a mapped checkpoint file has the length of its sum (when there is one).
fn check_len(
    sums: Option<&std::collections::HashMap<String, Sum>>,
    file: &Mapped,
) -> Result<(), StoreError> {
    let Some(sums) = sums else { return Ok(()) };
    let name = file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match sums.get(&name) {
        Some(&(len, _)) if len == file.map.len() as u64 => Ok(()),
        _ => Err(corrupt(&file.path)),
    }
}

thread_local! {
    /// Checkpoints larger than this are checked in the background (see [`Check::Lengths`]).
    static BACKGROUND_ABOVE: std::cell::Cell<u64> = const { std::cell::Cell::new(1 << 30) };
}

/// Sets, for this thread, the size above which a mapped checkpoint is checked in the
/// background rather than as it opens (for tests).
pub(crate) fn set_background_above(bytes: u64) {
    BACKGROUND_ABOVE.set(bytes);
}

/// Whether the checkpoint in `dir` is checked in the background: it is larger than
/// [`BACKGROUND_ABOVE`] (1 GiB), so that checking it would hold up opening (#436). A smaller
/// one is checked whole as it opens, as before.
pub(crate) fn checks_in_background(dir: &Path) -> bool {
    let Ok(Some(sums)) = read_sums(dir) else {
        return false;
    };
    sums.values().map(|s| s.0).sum::<u64>() > BACKGROUND_ABOVE.get()
}

/// The files of a checkpoint to check in the background, opened when the store opens: a
/// later checkpoint replaces the directory and links the files it keeps unchanged, but these
/// handles still read the very files the store mapped, so the check covers all of them
/// whatever is written meanwhile (#436).
pub(crate) struct FilesToCheck(Vec<(PathBuf, File, Sum)>);

/// Holds every check in the background before it reads a byte while set (for tests).
static HOLD_CHECKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn hold_checks(on: bool) {
    HOLD_CHECKS.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Opens every file of the checkpoint in `dir` that has a sum, to check with
/// [`verify_files`].
pub(crate) fn files_to_check(dir: &Path) -> Result<FilesToCheck, StoreError> {
    let Some(sums) = read_sums(dir)? else {
        return Ok(FilesToCheck(Vec::new()));
    };
    let mut names: Vec<(String, Sum)> = sums.into_iter().collect();
    names.sort();
    let mut files = Vec::with_capacity(names.len());
    for (name, sum) in names {
        let path = dir.join(name);
        let file = File::open(&path).map_err(|_| corrupt(&path))?;
        files.push((path, file, sum));
    }
    Ok(FilesToCheck(files))
}

/// Checks every file of `files` against its sum, reading it front to back (not mapped, so
/// the process does not keep its pages). Run in the background after a store opens (#436):
/// `Ok(false)` when `stop` was set (the store closed).
///
/// It keeps out of the way of the store's own reads (#437): it waits while `busy` says the
/// store is being written, rests after each read as long as the read took, so that it takes
/// at most about half of what the disk can read, and lets the kernel drop the pages it read,
/// which would otherwise push out of memory the pages the store's lookups use.
pub(crate) fn verify_files(
    files: FilesToCheck,
    stop: &std::sync::atomic::AtomicBool,
    busy: &dyn Fn() -> bool,
) -> Result<bool, StoreError> {
    use std::io::Read;
    use std::sync::atomic::Ordering::{Relaxed, SeqCst};
    while HOLD_CHECKS.load(SeqCst) {
        if stop.load(Relaxed) {
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut buf = vec![0u8; 8 << 20];
    for (path, mut file, sum) in files.0 {
        let (mut len, mut crc) = (0u64, crc32fast::Hasher::new());
        loop {
            while busy() {
                if stop.load(Relaxed) {
                    return Ok(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if stop.load(Relaxed) {
                return Ok(false);
            }
            let started = std::time::Instant::now();
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            drop_cached(&file, len, n);
            len += n as u64;
            std::thread::sleep(started.elapsed().min(std::time::Duration::from_millis(200)));
        }
        if (len, crc.finalize()) != sum {
            return Err(corrupt(&path));
        }
    }
    Ok(true)
}

/// Tells the kernel that the `n` bytes of `file` at `offset`, just read, are not needed again
/// (Linux). Pages a mapping of the store uses stay: the kernel keeps mapped pages.
fn drop_cached(file: &File, offset: u64, n: usize) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: a plain system call on an open file; it only advises the kernel.
        unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                offset as libc::off_t,
                n as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (file, offset, n);
}

/// Whether a mapped checkpoint file matches its sum (when there is one).
fn check_sum(
    sums: Option<&std::collections::HashMap<String, Sum>>,
    file: &Mapped,
) -> Result<(), StoreError> {
    let Some(sums) = sums else { return Ok(()) };
    let name = file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match sums.get(&name) {
        Some(&expected) if sum_of(&file.map) == expected => Ok(()),
        _ => Err(corrupt(&file.path)),
    }
}

pub(crate) struct Disk {
    /// The exclusive lock on the directory's `LOCK` file, held while the store is open.
    _lock: File,
    dir: PathBuf,
    generation: u64,
    wal: File,
    log_size: u64,
    /// Syncs of the log, shared by the transactions waiting for one (group commit, #709).
    pub(crate) durable: Arc<Durable>,
    /// The log is folded into a new checkpoint once it is larger than this.
    pub(crate) checkpoint_at: u64,
    /// The dictionary files of the current checkpoint; empty when the next one must write
    /// the dictionary whole (no checkpoint yet, or one without checksums or in an older
    /// format).
    dict_pieces: Vec<DictPiece>,
    /// Where folds write the segments they build.
    pub(crate) spill: Arc<Spill>,
    /// Where each log goes when a checkpoint retires it (#222), if anywhere.
    pub(crate) archive: Option<PathBuf>,
    /// The transactions committed to the current log.
    pub(crate) transactions: u64,
    /// How many retired logs a checkpoint leaves in the store directory, for followers that
    /// have not read them to the end yet (#217).
    pub(crate) keep_logs: u64,
    /// This store's timeline (#222): a random id, new for every store directory (a copy or a
    /// restore starts its own), so that the logs of different incarnations of a dataset
    /// never mix in an archive.
    pub(crate) timeline: String,
}

/// Group commit (#709): transactions append to the log under the writer lock but sync it
/// after letting go of the lock, so that the transactions that queued meanwhile append too
/// and one `fdatasync` covers them all. The first to wait syncs for everyone appended so
/// far; the others wait for it, and sync again only if they appended after it started.
#[derive(Debug)]
pub(crate) struct Durable {
    state: std::sync::Mutex<Synced>,
    done: std::sync::Condvar,
}

#[derive(Debug)]
struct Synced {
    /// The log being written, as a handle of its own, to sync without the writer lock.
    file: Arc<File>,
    /// Tickets: transactions appended so far, and those known to be on disk.
    written: u64,
    synced: u64,
    /// Whether a sync is running.
    syncing: bool,
    /// The generation of the log being written, its length as of the last append, and its
    /// length known to be on disk: where a follower may read up to (#217).
    generation: u64,
    len: u64,
    synced_len: u64,
    /// The error of a failed sync. It sticks: the kernel may have dropped the pages that
    /// failed, so a later sync that succeeds proves nothing about them.
    failed: Option<(io::ErrorKind, String)>,
}

impl Durable {
    fn new(file: File, generation: u64, len: u64) -> Self {
        Self {
            state: std::sync::Mutex::new(Synced {
                file: Arc::new(file),
                written: 0,
                synced: 0,
                syncing: false,
                failed: None,
                generation,
                len,
                synced_len: len,
            }),
            done: std::sync::Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Synced> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The error of a failed sync, if there was one.
    fn check(&self) -> io::Result<()> {
        match &self.lock().failed {
            Some((kind, e)) => Err(io::Error::new(
                *kind,
                format!("an earlier sync of the write-ahead log failed: {e}"),
            )),
            None => Ok(()),
        }
    }

    /// Notes an append, after which the log is `len` bytes long, and returns its ticket. The
    /// caller holds the writer lock.
    fn written(&self, len: u64) -> u64 {
        let mut s = self.lock();
        s.written += 1;
        s.len = len;
        s.written
    }

    /// Notes that the log is now `file`, of `generation`, and that a checkpoint made what was
    /// appended to the old one durable. The caller holds the writer lock.
    fn switch(&self, file: File, generation: u64) {
        let mut s = self.lock();
        s.file = Arc::new(file);
        s.synced = s.written;
        s.generation = generation;
        s.len = 0;
        s.synced_len = 0;
        drop(s);
        self.done.notify_all();
    }

    /// The generation of the log being written and its length known to be on disk, which
    /// ends with a complete entry.
    pub(crate) fn synced_position(&self) -> (u64, u64) {
        let s = self.lock();
        (s.generation, s.synced_len)
    }

    /// Returns once the transaction of `ticket` is on disk.
    pub(crate) fn wait(&self, ticket: u64) -> io::Result<()> {
        let mut s = self.lock();
        loop {
            if s.synced >= ticket {
                return Ok(());
            }
            if let Some((kind, e)) = &s.failed {
                return Err(io::Error::new(*kind, e.clone()));
            }
            if s.syncing {
                s = self.done.wait(s).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            s.syncing = true;
            let (target, target_len) = (s.written, s.len);
            let file = Arc::clone(&s.file);
            drop(s);
            let result = file.sync_data();
            s = self.lock();
            s.syncing = false;
            match result {
                // A checkpoint that switched logs meanwhile marked everything synced: the
                // length is then that of the new log.
                Ok(()) if target > s.synced => {
                    s.synced = target;
                    s.synced_len = target_len;
                }
                Ok(()) => {}
                Err(e) => {
                    if s.failed.is_none() {
                        // Commits applied since the last good sync stay readable until the
                        // store is reopened, though their callers got an error.
                        eprintln!(
                            "tripletrail-store: syncing the write-ahead log failed ({e}): \
                             refusing writes until the store is reopened; commits since the \
                             last good sync may be lost"
                        );
                    }
                    s.failed = Some((e.kind(), e.to_string()));
                }
            }
            self.done.notify_all();
        }
    }
}

/// What a follower reads of a store's logs (#217), without the writer lock: a commit holds
/// that lock while it writes, a large one for seconds.
#[derive(Debug)]
pub(crate) struct Shipping {
    durable: Arc<Durable>,
    dir: PathBuf,
    pub(crate) timeline: String,
    /// The timeline's archive directory, as [`Disk::archive`].
    pub(crate) archive: std::sync::Mutex<Option<PathBuf>>,
}

/// What [`Shipping::read`] found.
pub(crate) enum Shipped {
    /// The store no longer has that log, or not that far.
    Gone,
    /// Bytes from the offset asked for; how long that log is (known to be on disk); and
    /// whether it is retired, so that no more is written to it.
    Read(Vec<u8>, u64, bool),
}

impl Shipping {
    /// The generation of the log being written and its length on disk.
    pub(crate) fn position(&self) -> (u64, u64) {
        self.durable.synced_position()
    }

    /// Up to `max` bytes of generation `generation`'s log from `offset`, up to what is on disk.
    pub(crate) fn read(&self, generation: u64, offset: u64, max: usize) -> io::Result<Shipped> {
        use std::io::{Read, Seek};
        let (current, synced) = self.position();
        if generation > current {
            return Ok(Shipped::Gone);
        }
        let name = wal_file_name(generation);
        let archive = self
            .archive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut file = None;
        for path in [Some(self.dir.join(&name)), archive.map(|a| a.join(&name))]
            .into_iter()
            .flatten()
        {
            match File::open(&path) {
                Ok(f) => {
                    file = Some(f);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        let Some(mut file) = file else {
            // Deleted, but read to its end already: the next log follows.
            return Ok(match retired_len(&self.dir, generation) {
                Some(len) if len == offset => Shipped::Read(Vec::new(), len, true),
                _ => Shipped::Gone,
            });
        };
        // The log being written is read only as far as it is synced; a retired one (complete
        // once its checkpoint is in place) whole.
        let end = if generation == current {
            synced
        } else {
            file.metadata()?.len()
        };
        if offset > end {
            return Ok(Shipped::Gone);
        }
        let len = (end - offset).min(max as u64);
        file.seek(io::SeekFrom::Start(offset))?;
        let mut bytes = Vec::with_capacity(len as usize);
        file.take(len).read_to_end(&mut bytes)?;
        Ok(Shipped::Read(bytes, end, generation < current))
    }
}

/// The file listing the lengths of the last logs retired, by generation (#217).
const RETIRED_FILE: &str = "RETIRED";

/// How many retired logs [`RETIRED_FILE`] lists.
const RETIRED_KEPT: usize = 64;

/// Notes in the store directory that the log of `generation`, `len` bytes long, is retired.
fn record_retired(dir: &Path, generation: u64, len: u64) -> io::Result<()> {
    let path = dir.join(RETIRED_FILE);
    let old = fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().collect();
    let new = format!("{generation} {len}");
    lines.push(&new);
    let from = lines.len().saturating_sub(RETIRED_KEPT);
    let tmp = dir.join("RETIRED.tmp");
    fs::write(&tmp, lines[from..].join("\n") + "\n")?;
    fs::rename(&tmp, &path)
}

/// The length of the retired log of `generation`, if [`RETIRED_FILE`] lists it.
fn retired_len(dir: &Path, generation: u64) -> Option<u64> {
    fs::read_to_string(dir.join(RETIRED_FILE))
        .ok()?
        .lines()
        .filter_map(|l| {
            let (g, len) = l.split_once(' ')?;
            Some((g.parse::<u64>().ok()?, len.parse::<u64>().ok()?))
        })
        .find(|&(g, _)| g == generation)
        .map(|(_, len)| len)
}

fn checkpoint_dir(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("checkpoint-{n}"))
}

fn wal_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("wal-{n}.log"))
}

/// The format of the store directories this build writes (#221), kept in their `FORMAT`
/// file. It goes up when a change of the files or the write-ahead log would be misread by an
/// older TripleTrail, which then refuses the store instead (see [`check_format`]). Files of
/// older formats are read as they are, and rewritten in the current one by the next
/// checkpoint.
pub const STORE_FORMAT: u32 = 1;

/// The file of a store directory that holds its format.
const FORMAT_FILE: &str = "FORMAT";

/// The format of the store directory `dir`: its `FORMAT` file, 1 when it has none (a store
/// written before the file existed, or none yet).
pub(crate) fn store_format(dir: &Path) -> Result<u64, StoreError> {
    let path = dir.join(FORMAT_FILE);
    match fs::read_to_string(&path) {
        Ok(t) => t.trim().parse().map_err(|_| corrupt(&path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(1),
        Err(e) => Err(e.into()),
    }
}

/// Refuses a store of a newer format than this build reads, before anything in it is read
/// or changed: its log could hold entries this build would take for a torn end and cut off.
/// With `write`, a store without a `FORMAT` file gets one.
pub(crate) fn check_format(dir: &Path, write: bool) -> Result<(), StoreError> {
    let format = store_format(dir)?;
    if format > u64::from(STORE_FORMAT) {
        return Err(newer(dir, format, STORE_FORMAT));
    }
    if write && !dir.join(FORMAT_FILE).exists() {
        write_format(dir)?;
    }
    Ok(())
}

/// Writes this build's format into the store directory `dir`.
fn write_format(dir: &Path) -> Result<(), StoreError> {
    let tmp = dir.join("FORMAT.tmp");
    {
        let mut f = File::create(&tmp)?;
        writeln!(f, "{STORE_FORMAT}")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, dir.join(FORMAT_FILE))?;
    sync_dir(dir)?;
    Ok(())
}

/// The error for `path`, of format `found`, written by a TripleTrail newer than this one, which
/// reads up to `supported`.
fn newer(
    path: &Path,
    found: impl std::fmt::Display,
    supported: impl std::fmt::Display,
) -> StoreError {
    StoreError::Unsupported(format!(
        "{} is in format {found}, written by a newer TripleTrail: this one ({}) reads up to format {supported}. Open it with a newer tripletrail (it was not changed)",
        path.display(),
        env!("CARGO_PKG_VERSION"),
    ))
}

fn corrupt(path: &Path) -> StoreError {
    StoreError::Corrupt(path.display().to_string())
}

/// How long [`lock`] waits for a conflicting lock to go.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Locks the store in `dir` until the returned file is closed: exclusively to write it
/// (creating `LOCK`), shared to read it. A store that has never been opened for writing has no
/// `LOCK` to share, and a file system without locks is not locked (`None`). Another process
/// holding a lock that conflicts is an error, [`StoreError::Locked`], after a short wait: a
/// child process another thread is starting holds a copy of every open file until it runs
/// its program, so a store just closed can look locked for a moment.
pub(crate) fn lock(dir: &Path, exclusive: bool) -> Result<Option<File>, StoreError> {
    let path = dir.join("LOCK");
    let file = if exclusive {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?
    } else {
        match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    };
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    let locked = loop {
        let locked = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match locked {
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => break other,
        }
    };
    match locked {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Err(StoreError::Locked(dir.display().to_string())),
        Err(fs::TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    // Directories can't be opened for syncing on every platform; it's best effort there.
    match File::open(dir) {
        Ok(d) => d.sync_all().or(Ok(())),
        Err(_) => Ok(()),
    }
}

/// Opens or creates the store in `dir`; returns its data and the logged transactions to
/// replay.
#[allow(clippy::type_complexity)]
pub(crate) fn open(
    dir: &Path,
) -> Result<
    (
        Dictionary,
        QuadIndex,
        Option<Saved>,
        Disk,
        Vec<(bool, Quad)>,
    ),
    StoreError,
> {
    fs::create_dir_all(dir)?;
    // Before reading anything: another writer could be replacing the checkpoint (#296).
    let lock = match lock(dir, true)? {
        Some(lock) => lock,
        // Not lockable here: kept open all the same, and nothing else changes.
        None => OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("LOCK"))?,
    };
    // Before anything is read, replayed or truncated: a newer TripleTrail's log or files.
    check_format(dir, true)?;
    let current = dir.join("CURRENT");
    let generation = match fs::read_to_string(&current) {
        Ok(s) => s.trim().parse::<u64>().map_err(|_| corrupt(&current))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    check_generations(dir, generation).map_err(|()| corrupt(&current))?;
    let timeline = timeline(dir)?;
    let (dict, quads, saved, dict_pieces) = if generation == 0 {
        (Dictionary::new(), QuadIndex::default(), None, Vec::new())
    } else {
        let dir = checkpoint_dir(dir, generation);
        // Mapped, as a read-only store is: the data stays in the page cache, which the
        // kernel can drop, instead of on the heap (#218).
        let (dict, quads, mut pieces) = read_checkpoint(&dir, true)?;
        // A cache of parsed geometries: if missing (an older checkpoint) or unreadable, the
        // literals are parsed from their text.
        let (geoms, linkable) = read_geometries(&dir, true);
        for piece in &mut pieces {
            piece.geoms = linkable.get(&geoms_name(&piece.file)).copied();
        }
        (dict, quads, geoms, pieces)
    };
    // Older logs stay: one whose archiving failed, or that a crash left before it was archived,
    // is archived (or, without an archive, deleted) by the next checkpoint.
    remove_stale(dir, generation, None, 0);
    let wal_file = wal_path(dir, generation);
    let (log, committed, transactions) = read_log(&wal_file)?;
    let wal = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&wal_file)?;
    // Drop an unfinished transaction at the end.
    wal.set_len(committed)?;
    let durable = Arc::new(Durable::new(wal.try_clone()?, generation, committed));
    let mut disk = Disk {
        _lock: lock,
        dir: dir.to_owned(),
        generation,
        wal,
        log_size: committed,
        durable,
        checkpoint_at: CHECKPOINT_LOG_SIZE,
        dict_pieces,
        spill: Arc::new(Spill::new(dir)?),
        archive: None,
        transactions,
        keep_logs: 0,
        timeline,
    };
    io::Seek::seek(&mut disk.wal, io::SeekFrom::End(0))?;
    Ok((dict, quads, saved, disk, log))
}

/// The directory of the current checkpoint of the store in `dir`, if it has one.
pub(crate) fn current_checkpoint(dir: &Path) -> Option<PathBuf> {
    let n: u64 = fs::read_to_string(dir.join("CURRENT"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (n > 0).then(|| checkpoint_dir(dir, n))
}

/// Checks that generation `current` (from `CURRENT`) fits the files in `dir`, before
/// [`remove_stale`] deletes the others. A crash leaves at most older generations and the next
/// one (a checkpoint written but not yet made current, while the current log still exists).
/// Anything else means `CURRENT` is damaged: going by it would delete the real data.
fn check_generations(dir: &Path, current: u64) -> Result<(), ()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    let current_log = wal_path(dir, current).exists();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let n = name.strip_prefix("checkpoint-").or_else(|| {
            name.strip_prefix("wal-")
                .and_then(|n| n.strip_suffix(".log"))
        });
        if let Some(n) = n.and_then(|n| n.parse::<u64>().ok())
            && n > current
            && (n > current + 1 || !current_log)
        {
            return Err(());
        }
    }
    Ok(())
}

/// Removes checkpoints and logs other than generation `keep` (left by a crash, or retired by a
/// checkpoint). With `archive`, the logs older than `keep` are archived there first; those of
/// generation `keep_from` on are left in place (archived too).
fn remove_stale(dir: &Path, keep: u64, archive: Option<&Path>, keep_from: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // Old checkpoints are renamed out of the way here and deleted on a thread of their own:
    // freeing the blocks of segments a merge replaced takes seconds at 1B quads, and this
    // runs in the commit that checkpoints (#218). A rename that fails (Windows, with files
    // still mapped) leaves the directory for a later checkpoint or the next open.
    let mut doomed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name.strip_prefix("checkpoint-")
            && n.parse::<u64>().is_ok_and(|n| n != keep)
        {
            let stale = dir.join(format!("{STALE}{name}"));
            if fs::rename(entry.path(), &stale).is_ok() {
                doomed.push(stale);
            } else {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        // Left by a deletion the process did not finish.
        if name.starts_with(STALE) {
            doomed.push(entry.path());
        }
        if let Some(n) = name
            .strip_prefix("wal-")
            .and_then(|n| n.strip_suffix(".log"))
            .and_then(|n| n.parse::<u64>().ok())
            && n != keep
        {
            if let Some(archive) = archive
                && n < keep
                && let Err(e) = archive_log(&entry.path(), &archive.join(name.as_ref()))
            {
                // Kept, to be archived at the next checkpoint.
                eprintln!(
                    "tripletrail-store: archiving {} to {}: {e}",
                    entry.path().display(),
                    archive.display()
                );
                continue;
            }
            if n >= keep_from && n < keep {
                continue;
            }
            let _ = fs::remove_file(entry.path());
        }
    }
    for d in doomed {
        remove_later(d);
    }
}

/// Deletes the directory `dir` on the one thread that deletes old checkpoints, unless it is
/// being deleted already (checkpoints can follow each other faster than a deletion).
fn remove_later(dir: PathBuf) {
    use std::sync::{Mutex, OnceLock, mpsc};
    static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static REMOVER: OnceLock<Option<mpsc::Sender<PathBuf>>> = OnceLock::new();
    let pending = || {
        PENDING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let remover = REMOVER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let spawned = std::thread::Builder::new()
            .name("tripletrail-remove".into())
            .spawn(move || {
                for dir in rx {
                    let _ = fs::remove_dir_all(&dir);
                    pending().retain(|d| *d != dir);
                }
            });
        match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                eprintln!(
                    "tripletrail-store: old checkpoints are deleted as they are replaced: {e}"
                );
                None
            }
        }
    });
    let Some(remover) = remover else {
        let _ = fs::remove_dir_all(dir);
        return;
    };
    {
        let mut pending = pending();
        if pending.contains(&dir) {
            return;
        }
        pending.push(dir.to_path_buf());
    }
    if remover.send(dir.to_path_buf()).is_err() {
        pending().retain(|d| *d != dir);
        let _ = fs::remove_dir_all(dir);
    }
}

/// The prefix of an old checkpoint's directory while it is deleted.
const STALE: &str = "stale-";

/// Copies a retired log into the archive, synced: a hard link where the archive is on the same
/// file system, a copy otherwise. A log archived already (same length) is left as it is.
fn archive_log(from: &Path, to: &Path) -> io::Result<()> {
    let len = fs::metadata(from)?.len();
    if fs::metadata(to).is_ok_and(|m| m.len() == len) {
        return Ok(());
    }
    // The timeline's directory may have been pruned while it held no log anyone needed.
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir)?;
    }
    let _ = fs::remove_file(to);
    if fs::hard_link(from, to).is_err() {
        let tmp = to.with_extension("log.part");
        fs::copy(from, &tmp)?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, to)?;
    }
    if let Some(dir) = to.parent() {
        sync_dir(dir)?;
    }
    Ok(())
}

/// What a write-ahead log holds, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEntry {
    /// A committed transaction, with its commit time (milliseconds since 1970) when the log
    /// records one.
    Transaction(Option<u64>, Vec<(bool, Quad)>),
    /// A bulk load that went into the next checkpoint instead of the log, at that time.
    Load(Option<u64>),
}

/// The complete transactions of a log (their changes in order), the length of the log up to
/// the last one, and how many there are.
type ReadLog = (Vec<(bool, Quad)>, u64, u64);

fn read_log(path: &Path) -> Result<ReadLog, StoreError> {
    let mut log = Vec::new();
    let mut transactions = 0;
    let committed = scan_log(path, |entry| {
        if let LogEntry::Transaction(_, mut changes) = entry {
            log.append(&mut changes);
            transactions += 1;
        }
        true
    })?;
    Ok((log, committed, transactions))
}

/// Calls `f` with each complete entry of a log, until it returns `false`; returns the length
/// of the log up to the last complete entry read. A log has one `+ <quad> .` or
/// `- <quad> .` line per change and a `C [millis]` line ending each transaction; an
/// `L [millis]` line marks a bulk load that bypassed the log.
pub(crate) fn scan_log(path: &Path, f: impl FnMut(LogEntry) -> bool) -> Result<u64, StoreError> {
    let text = match fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    scan_log_bytes(&text, path, f)
}

/// [`scan_log`] of a log's bytes (`path` names it in errors).
pub(crate) fn scan_log_bytes(
    text: &[u8],
    path: &Path,
    mut f: impl FnMut(LogEntry) -> bool,
) -> Result<u64, StoreError> {
    let time = |line: &[u8]| -> Result<Option<u64>, StoreError> {
        match line.get(1..) {
            None | Some([]) => Ok(None),
            Some(rest) => std::str::from_utf8(rest)
                .ok()
                .and_then(|r| r.trim().parse().ok())
                .map(Some)
                .ok_or_else(|| corrupt(path)),
        }
    };
    let mut pending = Vec::new();
    let mut committed = 0;
    let mut pos = 0;
    while pos < text.len() {
        let Some(len) = text[pos..].iter().position(|&b| b == b'\n') else {
            break; // a line cut short by a crash
        };
        let line = &text[pos..pos + len];
        pos += len + 1;
        match line.first() {
            Some(b'C') => {
                let entry = LogEntry::Transaction(time(line)?, std::mem::take(&mut pending));
                committed = pos as u64;
                if !f(entry) {
                    return Ok(committed);
                }
            }
            Some(b'L') => {
                committed = pos as u64;
                if !f(LogEntry::Load(time(line)?)) {
                    return Ok(committed);
                }
            }
            Some(&op @ (b'+' | b'-')) => {
                let quad = RdfParser::from_format(RdfFormat::NQuads)
                    .for_slice(line.get(2..).ok_or_else(|| corrupt(path))?)
                    .next()
                    .ok_or_else(|| corrupt(path))?
                    .map_err(|_| corrupt(path))?;
                pending.push((op == b'+', quad));
            }
            _ => return Err(corrupt(path)),
        }
    }
    Ok(committed)
}

/// The name of generation `n`'s log.
/// The oldest log after generation `n` in any of `dirs`, if there is one.
pub(crate) fn later_log(dirs: &[PathBuf], n: u64) -> Option<u64> {
    dirs.iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flat_map(|entries| entries.flatten())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("wal-")?
                .strip_suffix(".log")?
                .parse::<u64>()
                .ok()
        })
        .filter(|&m| m > n)
        .min()
}

pub(crate) fn wal_file_name(n: u64) -> String {
    format!("wal-{n}.log")
}

/// The file of a base backup that says where it starts in the chain of logs.
const BASE_FILE: &str = "BASE";

/// The file holding a store directory's timeline id.
const TIMELINE_FILE: &str = "TIMELINE";

/// The timeline of the store in `dir`, created (random) when the directory has none yet.
fn timeline(dir: &Path) -> Result<String, StoreError> {
    let path = dir.join(TIMELINE_FILE);
    match fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_owned()),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let id = new_timeline();
    let tmp = dir.join("TIMELINE.tmp");
    let mut f = File::create(&tmp)?;
    writeln!(f, "{id}")?;
    f.sync_all()?;
    fs::rename(&tmp, &path)?;
    sync_dir(dir)?;
    Ok(id)
}

/// The timeline id of the store directory `dir`, if it has one (without creating it).
pub(crate) fn timeline_of(dir: &Path) -> Option<String> {
    let t = fs::read_to_string(dir.join(TIMELINE_FILE)).ok()?;
    Some(t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// 128 random bits as hex, from the standard library's randomly seeded hasher keys.
fn new_timeline() -> String {
    use std::hash::{BuildHasher, Hasher};
    let half = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(now_millis());
        h.write_u64(salt ^ u64::from(std::process::id()));
        h.finish()
    };
    format!("{:016x}{:016x}", half(1), half(2))
}

/// Where a base backup starts in the chain of logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Base {
    /// The timeline of the store it was copied from.
    pub(crate) timeline: String,
    /// The generation of that store it holds.
    pub(crate) generation: u64,
    /// How many transactions of that generation's log it holds.
    pub(crate) transactions: u64,
    /// When it was taken, in milliseconds since 1970.
    pub(crate) time: u64,
}

/// Records in the store directory `dir` (a copy) where it starts in the chain of logs.
pub(crate) fn write_base(dir: &Path, base: &Base) -> Result<(), StoreError> {
    let path = dir.join(BASE_FILE);
    let mut f = File::create(&path)?;
    writeln!(
        f,
        "timeline {}\ngeneration {}\ntransactions {}\ntime {}",
        base.timeline, base.generation, base.transactions, base.time
    )?;
    f.sync_all()?;
    sync_dir(dir)?;
    Ok(())
}

/// A base backup's `BASE` file, if it has one.
pub(crate) fn read_base(dir: &Path) -> Result<Option<Base>, StoreError> {
    let path = dir.join(BASE_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let text_field = |name: &str| {
        text.lines()
            .find_map(|l| Some(l.strip_prefix(name)?.trim().to_owned()))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| corrupt(&path))
    };
    let field = |name: &str| -> Result<u64, StoreError> {
        text_field(name)?.parse().map_err(|_| corrupt(&path))
    };
    Ok(Some(Base {
        timeline: text_field("timeline ")?,
        generation: field("generation ")?,
        transactions: field("transactions ")?,
        time: field("time ")?,
    }))
}

/// Copies the store directory `from` to the new directory `to` (its files copied, never
/// linked, leaving out the lock, the base file, the timeline and old checkpoints being
/// deleted: the copy starts a timeline of
/// its own when it is opened), synced; written under a temporary name and renamed at the end.
pub(crate) fn copy_store_dir(from: &Path, to: &Path) -> Result<(), StoreError> {
    fn copy(from: &Path, to: &Path) -> io::Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let name = entry.file_name();
            // An old checkpoint being deleted is no part of the store.
            if name == "LOCK"
                || name == BASE_FILE
                || name == TIMELINE_FILE
                || name == RETIRED_FILE
                || name.to_string_lossy().starts_with(STALE)
            {
                continue;
            }
            let target = to.join(&name);
            if entry.file_type()?.is_dir() {
                copy(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), &target)?;
                File::open(&target)?.sync_all()?;
            }
        }
        sync_dir(to)
    }
    let parent = match to.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_owned(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent)?;
    let name = to.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.copying-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let copied = copy(from, &tmp).and_then(|()| rename_dir(&tmp, to));
    if copied.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    copied?;
    sync_dir(&parent)?;
    Ok(())
}

/// Milliseconds since 1970, for the log's commit lines.
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

impl Disk {
    /// The dictionary pieces and segments of the current checkpoint, mapped, unchecked: for
    /// a store to let go of its copies on the heap right after writing them.
    pub(crate) fn mapped(&self) -> Result<(DictPieces, SavedSegments), StoreError> {
        let parts = read_parts(
            &checkpoint_dir(&self.dir, self.generation),
            true,
            Check::Lengths,
        )?;
        Ok((
            parts.dict,
            parts.segments.into_iter().zip(parts.runs).collect(),
        ))
    }

    /// The directory of the current checkpoint, if there is one.
    pub(crate) fn checkpoint_path(&self) -> Option<PathBuf> {
        (self.generation > 0).then(|| checkpoint_dir(&self.dir, self.generation))
    }

    /// The store directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn log_size(&self) -> u64 {
        self.log_size
    }

    /// Appends one transaction to the log and syncs it: its changes as ids (`true`: added),
    /// written as text through `decode`.
    pub(crate) fn append(
        &mut self,
        log: &[([u64; 4], bool)],
        decode: impl Fn([u64; 4]) -> Quad + Sync,
    ) -> io::Result<()> {
        let ticket = self.append_unsynced(log, decode)?;
        self.durable.wait(ticket)
    }

    /// Appends one transaction to the log without syncing it, and returns the ticket to pass
    /// to [`Durable::wait`], which returns once the transaction is on disk.
    pub(crate) fn append_unsynced(
        &mut self,
        log: &[([u64; 4], bool)],
        decode: impl Fn([u64; 4]) -> Quad + Sync,
    ) -> io::Result<u64> {
        // After a failed sync, what was written since may not be on disk, and later syncs
        // can report success all the same: no more writes until the store is reopened.
        self.durable.check()?;
        use rayon::prelude::*;
        use std::fmt::Write as _;
        // Large transactions are written out in parallel, in pieces (#292), a group of pieces
        // at a time so that the text of a large one is never held whole (#382).
        const PIECE: usize = 16_384;
        let group = PIECE * rayon::current_num_threads().max(1) * 2;
        for quads in log.chunks(group) {
            let parts: Vec<String> = quads
                .par_chunks(PIECE)
                .map(|chunk| {
                    let mut text = String::new();
                    for &(q, insert) in chunk {
                        let sign = if insert { '+' } else { '-' };
                        writeln!(text, "{sign} {} .", decode(q)).expect("writing to a string");
                    }
                    text
                })
                .collect();
            for part in &parts {
                self.wal.write_all(part.as_bytes())?;
                self.log_size += part.len() as u64;
            }
        }
        let commit = format!("C {}\n", now_millis());
        self.wal.write_all(commit.as_bytes())?;
        self.log_size += commit.len() as u64;
        self.transactions += 1;
        Ok(self.durable.written(self.log_size))
    }

    /// Records in the log that a bulk load goes into the next checkpoint instead: a restore
    /// from archived logs cannot replay past it.
    pub(crate) fn mark_load(&mut self) -> io::Result<()> {
        self.durable.check()?;
        let line = format!("L {}\n", now_millis());
        self.wal.write_all(line.as_bytes())?;
        self.log_size += line.len() as u64;
        let ticket = self.durable.written(self.log_size);
        self.durable.wait(ticket)
    }

    /// What followers read of the logs (#217).
    pub(crate) fn shipping(&self) -> Shipping {
        Shipping {
            durable: Arc::clone(&self.durable),
            dir: self.dir.to_path_buf(),
            timeline: self.timeline.clone(),
            archive: std::sync::Mutex::new(self.archive.clone()),
        }
    }

    /// Where a base backup taken now starts in the chain of logs: the store's timeline, its
    /// generation and the transactions committed to its current log.
    pub(crate) fn position(&self) -> Base {
        Base {
            timeline: self.timeline.clone(),
            generation: self.generation,
            transactions: self.transactions,
            time: now_millis() as u64,
        }
    }

    /// Writes `dict` and `quads` (compacted) as the next checkpoint and starts a new log.
    pub(crate) fn checkpoint(
        &mut self,
        dict: &Dictionary,
        quads: &QuadIndex,
        geometries: EncodeGeometry<'_>,
        render_wkt: bool,
    ) -> Result<(), StoreError> {
        let next = self.generation + 1;
        let target = checkpoint_dir(&self.dir, next);
        let _ = fs::remove_dir_all(&target);
        let previous = checkpoint_dir(&self.dir, self.generation);
        let pieces = write_checkpoint(
            &target,
            dict,
            quads,
            geometries,
            Some((&previous, &self.dict_pieces)),
            render_wkt,
        )?;
        File::create(wal_path(&self.dir, next))?.sync_all()?;
        // For followers that read the old log to its end before it is deleted (#217).
        record_retired(&self.dir, self.generation, self.log_size)?;
        let tmp = self.dir.join("CURRENT.tmp");
        {
            let mut f = File::create(&tmp)?;
            writeln!(f, "{next}")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, self.dir.join("CURRENT"))?;
        sync_dir(&self.dir)?;
        self.wal = OpenOptions::new()
            .append(true)
            .open(wal_path(&self.dir, next))?;
        // The checkpoint holds every transaction of the old log, synced or not.
        self.durable.switch(self.wal.try_clone()?, next);
        self.log_size = 0;
        self.transactions = 0;
        self.dict_pieces = pieces;
        self.generation = next;
        // Every older generation: on Windows, files still mapped (by the store, or by
        // snapshots) cannot be deleted yet, so what is left of them goes at a later
        // checkpoint, once they are let go, or at the next open.
        remove_stale(
            &self.dir,
            next,
            self.archive.as_deref(),
            next.saturating_sub(self.keep_logs),
        );
        Ok(())
    }
}

/// Writes the files of a checkpoint (dictionary, saved geometries, one segment per quad
/// permutation) into the new directory `target`, in parallel, and syncs them; returns its
/// dictionary pieces. With `previous`, the directory and dictionary pieces of the current
/// checkpoint, those pieces are linked and only the keys added since written.
fn write_checkpoint(
    target: &Path,
    dict: &Dictionary,
    quads: &QuadIndex,
    geometries: EncodeGeometry<'_>,
    previous: Option<(&Path, &[DictPiece])>,
    render_wkt: bool,
) -> io::Result<Vec<DictPiece>> {
    fs::create_dir_all(target)?;
    // Segment files are linked only into a checkpoint of the store itself; a copy elsewhere
    // (`save_to`, a backup) shares no file with the store, so that damage to one does not
    // reach the other.
    let link = previous.is_some();
    let mut pieces = previous
        .map(|(dir, pieces)| link_dict_pieces(dir, pieces, target, dict.len()))
        .unwrap_or_default();
    let saved_keys: usize = pieces.iter().map(|p| p.keys).sum();
    let new_piece = (pieces.is_empty() || saved_keys < dict.len()).then(|| {
        let file = dict_piece_name(pieces.len());
        (file, saved_keys, dict.len() - saved_keys)
    });
    // The indexes and geometries to write: of the new piece, and of the pieces linked
    // without them.
    let (mut indexes, mut geoms) = (Vec::new(), Vec::new());
    let mut from = 0;
    for piece in &pieces {
        if piece.index.is_none() {
            indexes.push((index_name(&piece.file), from, piece.keys));
        }
        if piece.geoms.is_none() {
            geoms.push((geoms_name(&piece.file), from, piece.keys));
        }
        from += piece.keys;
    }
    if let Some((file, from, keys)) = &new_piece {
        indexes.push((index_name(file), *from, *keys));
        // With WKT written from the geometries, the new piece's geometries are written with
        // its keys.
        if !render_wkt {
            geoms.push((geoms_name(file), *from, *keys));
        }
    }
    // The sum of the geometries written with the new piece's keys.
    let new_geoms = std::sync::OnceLock::new();
    type Job<'a> = Box<dyn Fn() -> io::Result<Sum> + Send + Sync + 'a>;
    let jobs: Vec<(String, Job)> = new_piece
        .iter()
        .map(|(file, from, keys)| {
            let path = target.join(file);
            let job: Job = if render_wkt {
                let (geoms_path, new_geoms) = (target.join(geoms_name(file)), &new_geoms);
                Box::new(move || {
                    let (dict_sum, geoms_sum) =
                        write_dict_compact(&path, &geoms_path, dict, *from, *keys, geometries)?;
                    let _ = new_geoms.set(geoms_sum);
                    Ok(dict_sum)
                })
            } else {
                Box::new(move || write_dict(&path, dict, *from, *keys))
            };
            (file.clone(), job)
        })
        .chain(indexes.iter().map(|(file, from, keys)| {
            let path = target.join(file);
            (
                file.clone(),
                Box::new(move || write_index(&path, dict, *from, *keys)) as Job,
            )
        }))
        .chain(geoms.iter().map(|(file, from, keys)| {
            let path = target.join(file);
            (
                file.clone(),
                Box::new(move || write_geoms(&path, dict, *from, *keys, geometries)) as Job,
            )
        }))
        .chain([(
            "stats.bin".to_owned(),
            Box::new(|| write_stats(&target.join("stats.bin"), quads)) as Job,
        )])
        .chain((dict.namespaces().len() > 0).then(|| {
            (
                NAMESPACES.to_owned(),
                Box::new(|| write_namespaces(&target.join(NAMESPACES), dict)) as Job,
            )
        }))
        .chain(
            PERM_NAMES
                .iter()
                .zip(&quads.perms)
                .flat_map(|(name, perm)| {
                    let runs = perm.runs.iter().enumerate().flat_map(move |(i, run)| {
                        [
                            (run_name(name, i, true), &run.added),
                            (run_name(name, i, false), &run.removed),
                        ]
                    });
                    std::iter::once((format!("{name}.seg"), &perm.segment))
                        .chain(runs)
                        .map(|(file, segment)| {
                            let path = target.join(&file);
                            (
                                file,
                                Box::new(move || write_segment(&path, segment, link)) as Job,
                            )
                        })
                }),
        )
        .collect();
    let mut sums = jobs
        .par_iter()
        .map(|(name, job)| Ok((name, job()?)))
        .collect::<io::Result<Vec<_>>>()?;
    let new_geoms_name = new_piece.as_ref().map(|(file, ..)| geoms_name(file));
    if let (Some(name), Some(&sum)) = (&new_geoms_name, new_geoms.get()) {
        sums.push((name, sum));
    }
    let mut text = String::new();
    for piece in &pieces {
        let (len, crc) = piece.sum;
        text += &format!("{} {len} {crc:08x}\n", piece.file);
        if let Some((len, crc)) = piece.index {
            text += &format!("{} {len} {crc:08x}\n", index_name(&piece.file));
        }
        if let Some((len, crc)) = piece.geoms {
            text += &format!("{} {len} {crc:08x}\n", geoms_name(&piece.file));
        }
    }
    for &(name, sum) in &sums {
        if let Some((file, _, keys)) = &new_piece
            && name == file
        {
            pieces.push(DictPiece {
                file: file.clone(),
                keys: *keys,
                sum,
                index: None,
                geoms: None,
            });
        }
        let (len, crc) = sum;
        text += &format!("{name} {len} {crc:08x}\n");
    }
    for piece in &mut pieces {
        let sum_of = |name: String| sums.iter().find(|(n, _)| **n == name).map(|&(_, s)| s);
        if let Some(sum) = sum_of(index_name(&piece.file)) {
            piece.index = Some(sum);
        }
        if let Some(sum) = sum_of(geoms_name(&piece.file)) {
            piece.geoms = Some(sum);
        }
    }
    // A checkpoint whose dictionary may hold compact keys says so, for the lookups of the
    // dictionary read from it (see [`crate::dict::Dictionary::may_hold_compact`]).
    if render_wkt || dict.may_hold_compact() {
        File::create(target.join(COMPACT_KEYS))?.sync_all()?;
    }
    // Last: a checkpoint with its checksums file has all its files complete.
    let mut f = File::create(target.join(CHECKSUMS))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    sync_dir(target)?;
    Ok(pieces)
}

/// Links the dictionary pieces of the checkpoint in `dir` into `target`, when the new
/// checkpoint can keep them: they hold at most `keys` keys and are fewer than
/// [`DICT_PIECES`] (else only the first). Returns the pieces linked: none when the
/// dictionary is to be written whole.
fn link_dict_pieces(
    dir: &Path,
    pieces: &[DictPiece],
    target: &Path,
    keys: usize,
) -> Vec<DictPiece> {
    let saved: usize = pieces.iter().map(|p| p.keys).sum();
    if pieces.is_empty() || saved > keys {
        return Vec::new();
    }
    // At the limit, the pieces after the first are written again as one, when the first
    // holds at least half the keys: a rewrite in proportion to what came since the first
    // was written, not to the whole dictionary, which at 1B quads stalls the write that
    // checkpoints for minutes (#218).
    let pieces = if pieces.len() < DICT_PIECES {
        pieces
    } else if pieces[0].keys.saturating_mul(2) >= keys {
        &pieces[..1]
    } else {
        return Vec::new();
    };
    for piece in pieces {
        // A file system without hard links: written whole.
        if fs::hard_link(dir.join(&piece.file), target.join(&piece.file)).is_err() {
            for piece in pieces {
                let _ = fs::remove_file(target.join(&piece.file));
            }
            return Vec::new();
        }
    }
    let mut pieces = pieces.to_vec();
    for piece in &mut pieces {
        // An index or geometries not linked are written again.
        let index = index_name(&piece.file);
        if piece.index.is_some() && fs::hard_link(dir.join(&index), target.join(&index)).is_err() {
            piece.index = None;
        }
        let geoms = geoms_name(&piece.file);
        if piece.geoms.is_some() && fs::hard_link(dir.join(&geoms), target.join(&geoms)).is_err() {
            piece.geoms = None;
        }
    }
    pieces
}

/// Writes a store directory that [`open`] and [`open_read_only`] read as it is: checkpoint 1
/// with these parts and an empty log. It is written next to `path` under a temporary name and
/// renamed at the end, so `path` never holds a half-written store. `path` must not exist yet,
/// or be an empty directory.
pub(crate) fn save(
    path: &Path,
    dict: &Dictionary,
    quads: &QuadIndex,
    geometries: EncodeGeometry<'_>,
    render_wkt: bool,
) -> Result<(), StoreError> {
    match fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(StoreError::Unsupported(format!(
                    "{} already exists and is not empty",
                    path.display()
                )));
            }
            fs::remove_dir(path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let name = path
        .file_name()
        .ok_or_else(|| StoreError::Unsupported(format!("{} has no name", path.display())))?
        .to_string_lossy()
        .into_owned();
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_owned(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent)?;
    let tmp = parent.join(format!(".{name}.saving-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let written = (|| -> Result<(), StoreError> {
        fs::create_dir_all(&tmp)?;
        write_checkpoint(
            &checkpoint_dir(&tmp, 1),
            dict,
            quads,
            geometries,
            None,
            render_wkt,
        )?;
        if render_wkt {
            mark_render_wkt(&tmp, true)?;
        }
        File::create(wal_path(&tmp, 1))?.sync_all()?;
        // Bases, store backups and saved releases carry their format (#221).
        write_format(&tmp)?;
        {
            // Closed before the rename: Windows refuses to rename a directory with a file
            // open inside it.
            let mut f = File::create(tmp.join("CURRENT"))?;
            writeln!(f, "1")?;
            f.sync_all()?;
        }
        sync_dir(&tmp)?;
        rename_dir(&tmp, path)?;
        sync_dir(&parent)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    written
}

/// Renames directory `from` to `to`. On Windows, a virus scanner or the search indexer may
/// still hold a file just written inside `from`, which makes the rename fail with "access
/// denied" for a moment: it is retried for up to about two seconds.
fn rename_dir(from: &Path, to: &Path) -> io::Result<()> {
    let mut wait = std::time::Duration::from_millis(20);
    for _ in 0..7 {
        match fs::rename(from, to) {
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => {
                std::thread::sleep(wait);
                wait *= 2;
            }
            other => return other,
        }
    }
    fs::rename(from, to)
}

/// Reads the store in `dir` without changing anything there: its checkpoint (memory-mapped)
/// and the logged transactions to apply in memory.
#[allow(clippy::type_complexity)]
pub(crate) fn open_read_only(
    dir: &Path,
) -> Result<
    (
        Dictionary,
        QuadIndex,
        Option<Saved>,
        Vec<(bool, Quad)>,
        Option<File>,
    ),
    StoreError,
> {
    // A shared lock: other readers are fine, a writer is not (#296).
    let lock = lock(dir, false)?;
    check_format(dir, false)?;
    let current = dir.join("CURRENT");
    let generation = match fs::read_to_string(&current) {
        Ok(s) => s.trim().parse::<u64>().map_err(|_| corrupt(&current))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if dir.is_dir() {
                0
            } else {
                return Err(StoreError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no store at {}", dir.display()),
                )));
            }
        }
        Err(e) => return Err(e.into()),
    };
    let (dict, quads, saved) = if generation == 0 {
        (Dictionary::new(), QuadIndex::default(), None)
    } else {
        let cp = checkpoint_dir(dir, generation);
        let (dict, quads, _) = read_checkpoint(&cp, true)?;
        (dict, quads, read_geometries(&cp, true).0)
    };
    let (log, _, _) = read_log(&wal_path(dir, generation))?;
    Ok((dict, quads, saved, log, lock))
}
fn write_u64s(w: &mut impl Write, values: &[u64]) -> io::Result<()> {
    for chunk in values.chunks(8192) {
        let bytes: Vec<u8> = chunk.iter().flat_map(|v| v.to_le_bytes()).collect();
        w.write_all(&bytes)?;
    }
    Ok(())
}

/// Writes `keys` keys of the dictionary, those after the first `from`, as a dictionary file
/// of their own (its key ends counted from its first byte).
fn write_dict(path: &Path, dict: &Dictionary, from: usize, keys: usize) -> io::Result<Sum> {
    let mut len = 0;
    dict.for_each_run(from, keys, |_, ends| len += ends[ends.len() - 1] - ends[0]);
    let mut w = summing(path)?;
    // Keys copied from a dictionary that may hold compact ones may be compact.
    w.write_all(dict_magic(dict, dict.may_hold_compact()))?;
    write_u64s(&mut w, &[len, keys as u64 + 1])?;
    let mut result = Ok(());
    dict.for_each_run(from, keys, |bytes, ends| {
        if result.is_ok() {
            result = w.write_all(&bytes[ends[0] as usize..]);
        }
    });
    std::mem::replace(&mut result, Ok(()))?;
    // Padded so that `ends` starts on an 8-byte boundary and can be mapped as it is.
    let pad = (len.next_multiple_of(8) - len) as usize;
    w.write_all(&[0; 8][..pad])?;
    write_u64s(&mut w, &[0])?;
    let mut at = 0;
    dict.for_each_run(from, keys, |_, ends| {
        if result.is_ok() {
            let shifted: Vec<u64> = ends[1..].iter().map(|e| e - ends[0] + at).collect();
            at = shifted.last().copied().unwrap_or(at);
            result = write_u64s(&mut w, &shifted);
        }
    });
    result?;
    finish(w)
}

/// Writes the namespaces of the dictionary's compact IRI keys (see [`NS_MAGIC`]). Small, so
/// written whole by every checkpoint.
fn write_namespaces(path: &Path, dict: &Dictionary) -> io::Result<Sum> {
    let (bytes, ends) = dict.namespaces().saved();
    let mut w = summing(path)?;
    w.write_all(NS_MAGIC)?;
    write_u64s(&mut w, &[ends.len() as u64 - 1, bytes.len() as u64])?;
    write_u64s(&mut w, ends)?;
    w.write_all(bytes)?;
    finish(w)
}

/// Reads the namespaces of a checkpoint (none when it has no namespaces file).
fn read_namespaces(
    dir: &Path,
    sums: Option<&HashMap<String, Sum>>,
) -> Result<crate::dict::Namespaces, StoreError> {
    let path = dir.join(NAMESPACES);
    if !path.exists() {
        return Ok(crate::dict::Namespaces::default());
    }
    let file = Mapped::open(path.clone())?;
    file.refuse_newer(&[NS_MAGIC])?;
    // Small: always checked against its sum, and copied to the heap.
    check_sum(sums, &file)?;
    let (_, h) = file.header(&[NS_MAGIC], 2)?;
    let count = usize::try_from(h[0]).map_err(|_| corrupt(&path))?;
    let len = usize::try_from(h[1]).map_err(|_| corrupt(&path))?;
    let at = 24;
    let ends_len = count
        .checked_add(1)
        .and_then(|n| n.checked_mul(8))
        .ok_or_else(|| corrupt(&path))?;
    if at + ends_len + len != file.map.len() {
        return Err(corrupt(&path));
    }
    let ends: Vec<u64> = file.map[at..at + ends_len]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect();
    let bytes = file.map[at + ends_len..].to_vec();
    crate::dict::Namespaces::from_saved(bytes, ends).ok_or_else(|| corrupt(&path))
}

/// The file in a store directory that marks it as keeping WKT literals that its writer
/// writes back exactly only in binary form (see [`crate::Store::set_render_wkt`]).
const RENDER_WKT: &str = "render-wkt";

/// In a checkpoint directory: its dictionary may hold compact keys (#370).
const COMPACT_KEYS: &str = "compact-keys";

/// Whether the dictionary of the checkpoint in `dir` may hold compact keys.
pub(crate) fn has_compact_keys(dir: &Path) -> bool {
    dir.join(COMPACT_KEYS).is_file()
}

/// Whether the store directory `dir` keeps WKT literals in binary form.
pub(crate) fn render_wkt(dir: &Path) -> bool {
    dir.join(RENDER_WKT).is_file()
}

/// Marks the store directory `dir` as keeping WKT literals in binary form, or not.
pub(crate) fn mark_render_wkt(dir: &Path, on: bool) -> io::Result<()> {
    let path = dir.join(RENDER_WKT);
    if on {
        File::create(&path)?.sync_all()?;
    } else if let Err(e) = fs::remove_file(&path)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e);
    }
    sync_dir(dir)
}

/// [`write_dict`] for a store that keeps WKT literals in binary form (#370): each WKT key
/// whose text [`crate::wkt`] writes back exactly from its geometry is written compact, with the
/// geometry in it (see [`crate::dict::is_compact`]); the geometries of the other keys go to
/// the geometries file at `geoms_path`, as [`write_geoms`] writes them. Gives the sums of both
/// files.
fn write_dict_compact(
    path: &Path,
    geoms_path: &Path,
    dict: &Dictionary,
    from: usize,
    keys: usize,
    encode: EncodeGeometry<'_>,
) -> io::Result<(Sum, Sum)> {
    use crate::id::Kind;
    use std::io::{Seek, SeekFrom};
    const CHUNK: usize = 16_384;
    const HEADER: usize = 24;
    // The header holds the length of the keys, known at the end: written then, and summed
    // apart.
    let mut file = File::create(path)?;
    file.write_all(&[0; HEADER])?;
    let mut w = Summing::new(BufWriter::new(file));
    let mut g = summing(geoms_path)?;
    g.write_all(GEO_MAGIC3)?;
    let (mut ordinals, mut offsets) = (Vec::new(), vec![0u64]);
    let mut boxes = BoxSpill::new(geoms_path)?;
    let (mut len, mut geoms_len) = (0u64, 0u64);
    // The keys written compact, with their length: the others keep theirs.
    let mut compacted: Vec<(u32, u32)> = Vec::new();
    let first = from as u32 + 1;
    let end = (from + keys) as u32 + 1;
    let starts: Vec<u32> = (first..end).step_by(CHUNK).collect();
    struct Part {
        keys: Vec<u8>,
        compacted: Vec<(u32, u32)>,
        ordinals: Vec<u32>,
        ends: Vec<u64>,
        records: Vec<u8>,
        boxes: GeomBoxes,
    }
    for batch in starts.chunks(4 * rayon::current_num_threads()) {
        let parts: Vec<Part> = batch
            .par_iter()
            .map(|&start| {
                let mut p = Part {
                    keys: Vec::new(),
                    compacted: Vec::new(),
                    ordinals: Vec::new(),
                    ends: Vec::new(),
                    records: Vec::new(),
                    boxes: GeomBoxes::default(),
                };
                let mut record = Vec::new();
                for o in start..(start + CHUNK as u32).min(end) {
                    let key = dict.raw_key(o);
                    if key[0] != Kind::Geometry as u8 {
                        p.keys.extend_from_slice(key);
                        continue;
                    }
                    if let Some(record) = crate::dict::compact_record(key) {
                        p.boxes.push(o, record);
                        p.keys.extend_from_slice(key);
                        continue;
                    }
                    record.clear();
                    if !encode(o, &mut record) {
                        p.keys.extend_from_slice(key);
                        continue;
                    }
                    p.boxes.push(o, &record);
                    let at = p.keys.len();
                    if crate::dict::compact(key, &record, &mut p.keys) {
                        p.compacted.push((o, (p.keys.len() - at) as u32));
                    } else {
                        p.keys.extend_from_slice(key);
                        p.records.extend_from_slice(&record);
                        p.ordinals.push(o);
                        p.ends.push(p.records.len() as u64);
                    }
                }
                p
            })
            .collect();
        for p in parts {
            w.write_all(&p.keys)?;
            len += p.keys.len() as u64;
            compacted.extend_from_slice(&p.compacted);
            g.write_all(&p.records)?;
            ordinals.extend_from_slice(&p.ordinals);
            offsets.extend(p.ends.iter().map(|e| e + geoms_len));
            geoms_len += p.records.len() as u64;
            boxes.append(&p.boxes)?;
        }
    }
    let pad = |n: u64| (n.next_multiple_of(8) - n) as usize;
    // The geometries file, as `write_geoms` ends it.
    let geoms_sum = finish_geoms(g, &ordinals, &offsets, geoms_len, boxes)?;
    // The key ends: those of the dictionary, but for the keys written compact.
    w.write_all(&[0; 8][..pad(len)])?;
    write_u64s(&mut w, &[0])?;
    let (mut at, mut next) = (0u64, compacted.iter().peekable());
    let mut o = first;
    let mut result = Ok(());
    dict.for_each_run(from, keys, |_, ends| {
        if result.is_err() {
            return;
        }
        let shifted: Vec<u64> = ends
            .windows(2)
            .map(|e| {
                let n = match next.peek() {
                    Some(&&(c, n)) if c == o => {
                        next.next();
                        u64::from(n)
                    }
                    _ => e[1] - e[0],
                };
                o += 1;
                at += n;
                at
            })
            .collect();
        result = write_u64s(&mut w, &shifted);
    });
    result?;
    debug_assert_eq!(at, len);
    let (buffered, body_len, body_crc) = w.finish_open();
    let mut file = buffered
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    let mut header = Vec::with_capacity(HEADER);
    header.extend_from_slice(dict_magic(dict, true));
    header.extend_from_slice(&len.to_le_bytes());
    header.extend_from_slice(&(keys as u64 + 1).to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.sync_all()?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&header);
    crc.combine(&body_crc);
    Ok(((HEADER as u64 + body_len, crc.finalize()), geoms_sum))
}

/// Writes the saved index of the `keys` keys of the dictionary after the first `from`. It is
/// filled in place in the mapped file, at random, so a large index takes no heap.
fn write_index(path: &Path, dict: &Dictionary, from: usize, keys: usize) -> io::Result<Sum> {
    let slots = index_slots(keys);
    let len = INDEX_HEADER + 8 * slots;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    // Zeros: every slot starts empty. Written, not made with `set_len`: a sparse file has no
    // blocks yet, and a write to its map on a full disk kills the process (SIGBUS) where
    // writing the zeros fails with an error (#218).
    allocate_zeros(&file, len)?;
    // SAFETY: the file was just created by this checkpoint and nothing else maps it yet.
    let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    map[..8].copy_from_slice(INDEX_MAGIC);
    for (i, v) in [keys as u64, slots as u64, index_check()]
        .iter()
        .enumerate()
    {
        map[8 + 8 * i..16 + 8 * i].copy_from_slice(&v.to_le_bytes());
    }
    let body = &mut map[INDEX_HEADER..];
    // SAFETY: a mapping starts on a page boundary, so the slots after the 32-byte header are
    // 8-byte aligned, and `body` holds exactly `slots` of them.
    let body = unsafe { std::slice::from_raw_parts_mut(body.as_mut_ptr().cast::<u64>(), slots) };
    dict.fill_index(from, keys, body);
    if cfg!(target_endian = "big") {
        for s in body.iter_mut() {
            *s = s.to_le();
        }
    }
    map.flush()?;
    let sum = sum_of(&map);
    drop(map);
    file.sync_all()?;
    Ok(sum)
}

/// Writes `len` zero bytes to the empty `file`, so that its blocks are allocated on disk.
fn allocate_zeros(file: &File, len: usize) -> io::Result<()> {
    const CHUNK: usize = 1 << 20;
    let zeros = vec![0u8; CHUNK.min(len)];
    let mut w = file;
    let mut left = len;
    while left > 0 {
        let n = left.min(CHUNK);
        w.write_all(&zeros[..n])?;
        left -= n;
    }
    Ok(())
}

/// The saved index `file` of a dictionary piece of `keys` keys, if it is listed in the
/// checksums and matches them (with `verify`), its header and this build's hash function.
fn read_index(
    dir: &Path,
    file: &str,
    keys: usize,
    sums: Option<&std::collections::HashMap<String, Sum>>,
    verify: bool,
) -> Option<(SavedIndex, Sum)> {
    let sum = *sums?.get(file)?;
    let mapped = Mapped::open(dir.join(file)).ok()?;
    if verify {
        check_sum(sums, &mapped).ok()?;
    } else {
        check_len(sums, &mapped).ok()?;
    }
    let (_, h) = mapped.header(&[INDEX_MAGIC], 3).ok()?;
    let slots = index_slots(keys);
    if h != [keys as u64, slots as u64, index_check()]
        || mapped.map.len() != INDEX_HEADER + 8 * slots
    {
        return None;
    }
    let slots = mapped.buf(INDEX_HEADER, slots as u64, true).ok()?;
    prefetch(&mapped.map);
    Some((SavedIndex(slots), sum))
}

/// Reads a mapped file into memory on a thread of its own, a page at a time, so that the
/// lookups of the first writes after a store opens do not each wait on a random read of the
/// disk (#437: on OSM Europe 1B, the first commit of 10,000 triples spent 7 s looking up
/// terms in a cold 7 GB dictionary index, the 20th 90 ms). The pages are touched through
/// the mapping, so the background check of the files (which drops what it read from the page
/// cache) keeps them. Left out when the file would take more than a quarter of the memory
/// (or of `--memory-limit`), or with `TRIPLETRAIL_PREFETCH=0`; stops when the store lets go of
/// the file.
fn prefetch(map: &Arc<Mmap>) {
    let len = map.len();
    let budget = crate::index::memory_limit()
        .or_else(physical_memory)
        .map_or(0, |m| m / 4);
    if len < (1 << 20)
        || len > budget
        || std::env::var("TRIPLETRAIL_PREFETCH").is_ok_and(|v| v == "0")
    {
        return;
    }
    let map = Arc::clone(map);
    let _ = std::thread::Builder::new()
        .name("tripletrail-prefetch".into())
        .spawn(move || {
            #[cfg(unix)]
            let _ = map.advise(memmap2::Advice::WillNeed);
            let mut sum = 0u8;
            for (i, at) in (0..map.len()).step_by(4096).enumerate() {
                // Every 64 MB: the store may have moved on to another checkpoint.
                if i % 16384 == 0 && Arc::strong_count(&map) == 1 {
                    break;
                }
                sum = sum.wrapping_add(map[at]);
            }
            std::hint::black_box(sum);
        });
}

/// The machine's memory in bytes, where it can be asked.
fn physical_memory() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain queries of system constants.
        let (pages, size) = unsafe {
            (
                libc::sysconf(libc::_SC_PHYS_PAGES),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        if pages > 0 && size > 0 {
            return usize::try_from(pages)
                .ok()?
                .checked_mul(usize::try_from(size).ok()?);
        }
    }
    None
}

/// The boxes of a batch of records, gathered as they are encoded.
#[derive(Default)]
struct GeomBoxes {
    ordinals: Vec<u32>,
    corners: Vec<[u64; 4]>,
    tags: Vec<u8>,
}

impl GeomBoxes {
    /// The box of ordinal `o`'s record, if [`crate::geobin::saved_box`] keeps one.
    fn push(&mut self, o: u32, record: &[u8]) {
        if let Some((tag, corners)) = crate::geobin::saved_box(record) {
            self.ordinals.push(o);
            self.corners.push(corners);
            self.tags.push(tag);
        }
    }
}

/// The boxes a geometries file of format 3 saves, kept in a file beside it until the
/// records are written: a store's boxes take gigabytes (37 bytes per larger geometry), which
/// a checkpoint should not hold on the heap (#218).
struct BoxSpill {
    path: PathBuf,
    w: BufWriter<File>,
    count: u64,
}

/// The bytes of one box in a [`BoxSpill`]: ordinal, corners, tag.
const SPILLED_BOX: usize = 4 + 32 + 1;

impl BoxSpill {
    fn new(beside: &Path) -> io::Result<Self> {
        let mut name = beside.as_os_str().to_owned();
        name.push(".boxes");
        let path = PathBuf::from(name);
        let w = BufWriter::new(File::create(&path)?);
        Ok(Self { path, w, count: 0 })
    }

    fn append(&mut self, boxes: &GeomBoxes) -> io::Result<()> {
        for ((o, c), t) in boxes.ordinals.iter().zip(&boxes.corners).zip(&boxes.tags) {
            self.w.write_all(&o.to_le_bytes())?;
            for x in c {
                self.w.write_all(&x.to_le_bytes())?;
            }
            self.w.write_all(&[*t])?;
        }
        self.count += boxes.ordinals.len() as u64;
        Ok(())
    }

    /// Copies one field of every box to `out`, in order: `bytes` bytes at `at` of each.
    fn copy_field(&self, at: usize, bytes: usize, out: &mut impl Write) -> io::Result<()> {
        use std::io::Read;
        let mut r = io::BufReader::with_capacity(1 << 20, File::open(&self.path)?);
        let mut entry = [0u8; SPILLED_BOX];
        for _ in 0..self.count {
            r.read_exact(&mut entry)?;
            out.write_all(&entry[at..at + bytes])?;
        }
        Ok(())
    }
}

impl Drop for BoxSpill {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Ends a geometries file of format 3 whose `len` bytes of records are written: the
/// ordinals, the offsets, the boxes and the sizes.
fn finish_geoms(
    mut w: Summing<BufWriter<File>>,
    ordinals: &[u32],
    offsets: &[u64],
    len: u64,
    mut boxes: BoxSpill,
) -> io::Result<Sum> {
    let pad = |n: u64| (n.next_multiple_of(8) - n) as usize;
    w.write_all(&[0; 8][..pad(len)])?;
    let raw: Vec<u8> = ordinals.iter().flat_map(|o| o.to_le_bytes()).collect();
    w.write_all(&raw)?;
    w.write_all(&[0; 8][..pad(raw.len() as u64)])?;
    write_u64s(&mut w, offsets)?;
    boxes.w.flush()?;
    let m = boxes.count;
    boxes.copy_field(0, 4, &mut w)?;
    w.write_all(&[0; 8][..pad(4 * m)])?;
    boxes.copy_field(4, 32, &mut w)?;
    boxes.copy_field(36, 1, &mut w)?;
    w.write_all(&[0; 8][..pad(m)])?;
    write_u64s(&mut w, &[ordinals.len() as u64, len, m])?;
    finish(w)
}

/// Writes the binary form of the geometry literals among the `keys` keys of the dictionary
/// after the first `from`, front to back: encoded in parallel, a chunk of ordinals at a time,
/// so only the ordinals and offsets stay on the heap while it runs.
fn write_geoms(
    path: &Path,
    dict: &Dictionary,
    from: usize,
    keys: usize,
    encode: EncodeGeometry<'_>,
) -> io::Result<Sum> {
    use crate::id::Kind;
    const CHUNK: usize = 16_384;
    let mut w = summing(path)?;
    w.write_all(GEO_MAGIC3)?;
    let (mut ordinals, mut offsets) = (Vec::new(), vec![0u64]);
    let mut boxes = BoxSpill::new(path)?;
    let mut len = 0u64;
    let first = from as u32 + 1;
    let end = (from + keys) as u32 + 1;
    let starts: Vec<u32> = (first..end).step_by(CHUNK).collect();
    // A few chunks per thread at a time: their records are on the heap until written.
    for batch in starts.chunks(4 * rayon::current_num_threads()) {
        type Part = (Vec<u32>, Vec<u64>, Vec<u8>, GeomBoxes);
        let parts: Vec<Part> = batch
            .par_iter()
            .map(|&start| {
                let (mut os, mut ends, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
                let mut boxes = GeomBoxes::default();
                for o in start..(start + CHUNK as u32).min(end) {
                    let key = dict.raw_key(o);
                    if key[0] != Kind::Geometry as u8 {
                        continue;
                    }
                    // Compact keys hold their geometry: only its box is saved here.
                    if let Some(record) = crate::dict::compact_record(key) {
                        boxes.push(o, record);
                        continue;
                    }
                    let at = bytes.len();
                    if encode(o, &mut bytes) {
                        boxes.push(o, &bytes[at..]);
                        os.push(o);
                        ends.push(bytes.len() as u64);
                    } else {
                        bytes.truncate(at);
                    }
                }
                (os, ends, bytes, boxes)
            })
            .collect();
        for (os, ends, bytes, part_boxes) in parts {
            w.write_all(&bytes)?;
            ordinals.extend_from_slice(&os);
            offsets.extend(ends.iter().map(|e| e + len));
            len += bytes.len() as u64;
            boxes.append(&part_boxes)?;
        }
    }
    finish_geoms(w, &ordinals, &offsets, len, boxes)
}

/// The file of a permutation's run `i`: its added quads or its removed ones.
fn run_name(perm: &str, i: usize, added: bool) -> String {
    format!("{perm}.{i}{}.seg", if added { "a" } else { "r" })
}

/// The segments of each permutation as saved: the segment and its runs (added, removed).
pub(crate) type SavedSegments = Vec<(Segment, Vec<(Segment, Segment)>)>;

/// The counts of the permutations with their runs ([`crate::stats`]), one after another.
fn write_stats(path: &Path, quads: &QuadIndex) -> io::Result<Sum> {
    let mut values = Vec::new();
    for perm in 0..quads.perms.len() {
        quads.stored_stats(perm).encode(&mut values);
    }
    let mut w = summing(path)?;
    w.write_all(STATS_MAGIC)?;
    write_u64s(&mut w, &[values.len() as u64])?;
    write_u64s(&mut w, &values)?;
    finish(w)
}

/// Sets the counts of the segments from the checkpoint in `dir`: a cache, so nothing happens
/// when the file is missing (older checkpoints), damaged or for other segments.
fn read_stats(dir: &Path, quads: &QuadIndex) {
    let read = || -> Option<Vec<crate::stats::SegStats>> {
        let file = Mapped::open(dir.join("stats.bin")).ok()?;
        check_sum(read_sums(dir).ok()?.as_ref(), &file).ok()?;
        let (_, h) = file.header(&[STATS_MAGIC], 1).ok()?;
        let values: Buf<u64> = file.buf(16, h[0], false).ok()?;
        let mut at = 0;
        // As many as were saved: six in checkpoints from before PSOG and GPSO, whose counts
        // are then built on first use.
        let mut all = Vec::new();
        while at < values.len() && all.len() < quads.perms.len() {
            all.push(crate::stats::SegStats::decode(&values, &mut at)?);
        }
        (at == values.len() && all.len() >= OLD_PERMS).then_some(all)
    };
    let Some(all) = read() else { return };
    // Counts that don't match their permutation's length belong to other data.
    if all
        .iter()
        .enumerate()
        .any(|(perm, s)| s.rows != quads.stored_len(perm) as u64)
    {
        return;
    }
    for (perm, s) in all.into_iter().enumerate() {
        quads.set_stored_stats(perm, s);
    }
}

fn write_segment(path: &Path, s: &Segment, link: bool) -> io::Result<Sum> {
    // A segment read from a file (a fold's, or a checkpoint's) is that file: linked into a
    // checkpoint of the store, copied elsewhere.
    if let Some(run) = &s.file {
        if link && fs::hard_link(&run.path, path).is_ok() {
            // Opened for writing: on Windows, flushing a file needs a handle with write
            // access.
            fs::OpenOptions::new().write(true).open(path)?.sync_all()?;
            return Ok(run.sum);
        }
        if !link {
            // Checked on the way: a damaged file is not copied into a backup under a new sum.
            let sum = copy_file(&run.path, path)?;
            if sum != run.sum {
                let _ = fs::remove_file(path);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} does not match its checksum", run.path.display()),
                ));
            }
            return Ok(sum);
        }
    }
    let mut w = summing(path)?;
    w.write_all(SEG_MAGIC4)?;
    w.write_all(&s.bytes)?;
    write_segment_end(
        &mut w,
        s.len,
        &s.firsts,
        &s.offsets,
        &s.starts,
        s.bytes.len(),
    )?;
    finish(w)
}

/// Copies `from` to `to` byte by byte, and syncs it: not `fs::copy`, which may share the
/// blocks of the two files on file systems that can (reflinks), so that damage to one would
/// be damage to the other. Returns the sum of what was written.
fn copy_file(from: &Path, to: &Path) -> io::Result<Sum> {
    use std::io::Read;
    let mut r = File::open(from)?;
    let mut w = summing(to)?;
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
    }
    finish(w)
}

/// What follows the blocks in a format 4 segment file.
fn write_segment_end(
    w: &mut impl Write,
    len: usize,
    firsts: &[Key],
    offsets: &[u64],
    starts: &[u64],
    bytes: usize,
) -> io::Result<()> {
    // Padded so that the arrays start on an 8-byte boundary (the magic number is 8 bytes).
    w.write_all(&[0; 8][..bytes.next_multiple_of(8) - bytes])?;
    let flat: Vec<u64> = firsts.iter().flatten().copied().collect();
    write_u64s(w, &flat)?;
    write_u64s(w, offsets)?;
    write_u64s(w, starts)?;
    write_u64s(
        w,
        &[
            len as u64,
            firsts.len() as u64,
            offsets.len() as u64,
            starts.len() as u64,
            bytes as u64,
        ],
    )
}

/// The segment files that folds of a store on disk write (#218), numbered, in `runs/`: a
/// fold then holds a block at a time on the heap, not the segments it builds. A file is
/// deleted when the last segment read from it is dropped; a checkpoint links the files of
/// the segments it saves.
pub(crate) struct Spill {
    dir: PathBuf,
    next: std::sync::atomic::AtomicU64,
}

impl Spill {
    /// Spills into `store/runs`, emptied first: the files of earlier runs are no segment's.
    pub(crate) fn new(store: &Path) -> io::Result<Self> {
        let dir = store.join("runs");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            next: Default::default(),
        })
    }

    /// Writes dictionary keys (`bytes` and their `ends`) to a new file and maps them; the
    /// file is removed at once where the system allows it (elsewhere, when the store next
    /// opens).
    pub(crate) fn map_keys(&self, bytes: &[u8], ends: &[u64]) -> io::Result<(Buf<u8>, Buf<u64>)> {
        let (path, file) = loop {
            let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = self.dir.join(format!("{n}.keys"));
            match File::options().write(true).create_new(true).open(&path) {
                Ok(f) => break (path, f),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        };
        let at = bytes.len().next_multiple_of(8);
        let mut w = BufWriter::new(file);
        w.write_all(bytes)?;
        w.write_all(&[0; 8][..at - bytes.len()])?;
        for e in ends {
            w.write_all(&e.to_le_bytes())?;
        }
        w.into_inner().map_err(io::IntoInnerError::into_error)?;
        let mapped = Mapped::open(path.clone()).map_err(io::Error::other)?;
        let _ = fs::remove_file(&path);
        let bad = || io::Error::other("dictionary keys written short");
        Ok((
            Buf::mapped(&mapped.map, 0, bytes.len()).ok_or_else(bad)?,
            Buf::mapped(&mapped.map, at, ends.len()).ok_or_else(bad)?,
        ))
    }

    /// A new segment file.
    pub(crate) fn writer(&self) -> io::Result<RunWriter> {
        // A new name: a merge still running for an earlier opening of the store may hold
        // some (#218).
        let (path, file) = loop {
            let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = self.dir.join(format!("{n}.seg"));
            match File::options().write(true).create_new(true).open(&path) {
                Ok(f) => break (path, f),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        };
        let mut w = Summing::new(BufWriter::new(file));
        w.write_all(SEG_MAGIC4)?;
        Ok(RunWriter {
            path,
            w: Some(w),
            bytes: 0,
        })
    }
}

/// A segment file being written: its blocks, then [`RunWriter::finish`].
pub(crate) struct RunWriter {
    path: PathBuf,
    /// `None` once finished.
    w: Option<Summing<BufWriter<File>>>,
    bytes: usize,
}

impl RunWriter {
    /// Appends block bytes.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.w.as_mut().expect("not finished").write_all(bytes)?;
        self.bytes += bytes.len();
        Ok(())
    }

    /// The block bytes written so far, read back (when the file cannot be finished).
    pub(crate) fn read_back(&self) -> io::Result<Vec<u8>> {
        let mut bytes = fs::read(&self.path)?;
        bytes.drain(..8);
        bytes.truncate(self.bytes);
        Ok(bytes)
    }

    /// Ends the file and maps it as a segment; the block bytes, [`crate::segment::PAD`]
    /// included, are written. On an error, gives the block bytes back as read from the file.
    pub(crate) fn finish(
        mut self,
        len: usize,
        firsts: &[Key],
        offsets: &[u64],
        starts: &[u64],
    ) -> Result<Segment, (StoreError, Vec<u8>)> {
        let written = (|| -> Result<Segment, StoreError> {
            let bytes = self.bytes;
            let mut w = self.w.take().expect("not finished");
            write_segment_end(&mut w, len, firsts, offsets, starts, bytes)?;
            let (w, sum) = w.finish();
            // Not synced: the write-ahead log keeps the writes; a checkpoint syncs the files
            // it links.
            w.into_inner().map_err(io::IntoInnerError::into_error)?;
            let file = Mapped::open(self.path.clone())?;
            let mut segment = read_segment_v4(&file, true, true)?;
            segment.file = Some(Arc::new(RunFile {
                path: self.path.clone(),
                sum,
                owned: true,
            }));
            Ok(segment)
        })();
        match written {
            Ok(segment) => {
                // The file is the segment's now.
                self.path = PathBuf::new();
                Ok(segment)
            }
            Err(e) => {
                let bytes = self
                    .read_back()
                    .expect("a segment file just written can be read back");
                Err((e, bytes))
            }
        }
    }
}

impl Drop for RunWriter {
    fn drop(&mut self) {
        // Finished, the file belongs to its segment (and the path is empty here); otherwise
        // it is nobody's.
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The file a segment was read from, which a checkpoint links rather than writing the
/// segment again: one a fold wrote ([`Spill`]), deleted when the segment goes, or a
/// checkpoint's file (#444), which stays.
pub(crate) struct RunFile {
    path: PathBuf,
    sum: Sum,
    /// Written by a fold, so deleted with the segment.
    owned: bool,
}

impl RunFile {
    /// Flushes the file to disk. A merge in the background calls it on what it wrote, so
    /// that the checkpoint linking the file, on the write path, finds nothing left to flush
    /// (#218: at 1B quads, gigabytes).
    pub(crate) fn sync(&self) -> io::Result<()> {
        // Opened for writing: on Windows, flushing a file needs a handle with write access.
        fs::OpenOptions::new()
            .write(true)
            .open(&self.path)?
            .sync_all()
    }
}

impl Drop for RunFile {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A format 4 segment file.
fn read_segment_v4(file: &Mapped, keep_mapped: bool, verify: bool) -> Result<Segment, StoreError> {
    let path = &file.path;
    let n = file.map.len();
    if n < 8 + SEG_TRAILER {
        return Err(corrupt(path));
    }
    let h: Vec<u64> = file.map[n - SEG_TRAILER..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect();
    let len = usize::try_from(h[0]).map_err(|_| corrupt(path))?;
    let bytes: Buf<u8> = file.buf(8, h[4], keep_mapped)?;
    let at = (8 + bytes.len()).next_multiple_of(8);
    let firsts: Buf<Key> = file.buf(at, h[1], keep_mapped)?;
    let at = at + byte_len::<Key>(h[1], path)?;
    let offsets: Buf<u64> = file.buf(at, h[2], keep_mapped)?;
    let at = at + byte_len::<u64>(h[2], path)?;
    let starts: Buf<u64> = file.buf(at, h[3], keep_mapped)?;
    let at = at + byte_len::<u64>(h[3], path)?;
    if at + SEG_TRAILER != n {
        return Err(corrupt(path));
    }
    let segment = Segment {
        len,
        firsts,
        offsets,
        starts,
        bytes,
        stats: Default::default(),
        file: None,
    };
    if verify && !segment.is_consistent() {
        return Err(corrupt(path));
    }
    Ok(segment)
}

/// A checkpoint file, memory-mapped.
struct Mapped {
    path: PathBuf,
    map: Arc<Mmap>,
}

impl Mapped {
    fn open(path: PathBuf) -> Result<Self, StoreError> {
        let file = File::open(&path)?;
        // SAFETY: checkpoint files are written once, before the checkpoint becomes current,
        // and never changed afterwards (a later checkpoint goes to a new directory).
        let map = unsafe { Mmap::map(&file)? };
        Ok(Self {
            path,
            map: Arc::new(map),
        })
    }

    /// The error for a file that starts with none of `magics`: a newer version of the same
    /// kind of file (the same seven bytes, a higher last digit) was written by a newer
    /// TripleTrail; anything else is damage.
    fn unknown_magic(&self, magic: &[u8], magics: &[&[u8; 8]]) -> StoreError {
        self.newer_magic(magic, magics)
            .unwrap_or_else(|| corrupt(&self.path))
    }

    /// The error for a file whose `magic` is a newer version of one of `magics` (#221).
    fn newer_magic(&self, magic: &[u8], magics: &[&[u8; 8]]) -> Option<StoreError> {
        let latest = magics
            .iter()
            .filter(|m| magic.len() == 8 && m[..7] == magic[..7])
            .map(|m| m[7])
            .max()?;
        (magic[7].is_ascii_digit() && magic[7] > latest).then(|| {
            newer(
                &self.path,
                String::from_utf8_lossy(magic),
                String::from_utf8_lossy(&[&magic[..7], &[latest][..]].concat()),
            )
        })
    }

    /// Refuses a file written in a newer version of the formats `magics`, before its sum is
    /// checked: it is not damaged, only too new for this TripleTrail.
    fn refuse_newer(&self, magics: &[&[u8; 8]]) -> Result<(), StoreError> {
        match self.map.get(..8).and_then(|m| self.newer_magic(m, magics)) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The header fields after the magic number, if the file starts with one of `magics`
    /// (and which one).
    fn header(&self, magics: &[&[u8; 8]], fields: usize) -> Result<(usize, Vec<u64>), StoreError> {
        let magic = self.map.get(..8).ok_or_else(|| corrupt(&self.path))?;
        let which = magics
            .iter()
            .position(|m| m.as_slice() == magic)
            .ok_or_else(|| self.unknown_magic(magic, magics))?;
        let h = self
            .map
            .get(8..8 + 8 * fields)
            .ok_or_else(|| corrupt(&self.path))?;
        Ok((
            which,
            h.as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect(),
        ))
    }

    /// `len` values at byte `offset`, kept mapped or copied out; an error if the file is too
    /// short (sizes in a damaged header must not make us read or allocate past it).
    fn buf<T: Pod>(&self, offset: usize, len: u64, keep: bool) -> Result<Buf<T>, StoreError> {
        let len = usize::try_from(len).map_err(|_| corrupt(&self.path))?;
        let buf = Buf::mapped(&self.map, offset, len).ok_or_else(|| corrupt(&self.path))?;
        Ok(if keep { buf } else { Buf::Owned(buf.to_vec()) })
    }
}

fn byte_len<T>(n: u64, path: &Path) -> Result<usize, StoreError> {
    usize::try_from(n)
        .ok()
        .and_then(|n| n.checked_mul(std::mem::size_of::<T>()))
        .ok_or_else(|| corrupt(path))
}

/// The saved geometries of the checkpoint in `dir`, with the sums of the files of those that
/// a next checkpoint can link. A cache: a file that is missing, damaged or does not match its
/// checksum is left out (its literals are parsed again from their text).
fn read_geometries(dir: &Path, keep_mapped: bool) -> (Option<Saved>, HashMap<String, Sum>) {
    let background = keep_mapped && checks_in_background(dir);
    let Ok(Some(sums)) = read_sums(dir) else {
        return (None, HashMap::new());
    };
    let names: Vec<String> = (0..)
        .map(|n| geoms_name(&dict_piece_name(n)))
        .take_while(|n| sums.contains_key(n))
        .collect();
    let read = |name: &String| -> Option<(SavedPart, bool)> {
        let path = dir.join(name);
        let file = Mapped::open(path.clone()).ok()?;
        // Of a large checkpoint, only the length: the sum is checked in the background (#436).
        if background {
            check_len(Some(&sums), &file).ok()?;
        } else {
            check_sum(Some(&sums), &file).ok()?;
        }
        if file.map.get(..8) == Some(GEO_MAGIC.as_slice()) {
            // Before #218: one file for the whole dictionary, not linked by the next checkpoint.
            let (_, h) = file.header(&[GEO_MAGIC], 2).ok()?;
            let n = h[0];
            let at = 24;
            let ordinals: Buf<u32> = file.buf(at, n, false).ok()?;
            let at = at + byte_len::<u32>(n, &path).ok()?;
            let offsets: Buf<u64> = file.buf(at, n.checked_add(1)?, false).ok()?;
            let at = at + byte_len::<u64>(n + 1, &path).ok()?;
            let bytes: Buf<u8> = file.buf(at, h[1], keep_mapped).ok()?;
            return Some((SavedPart::new(ordinals, offsets, bytes)?, false));
        }
        let magic = file.map.get(..8)?;
        let tail_len = if magic == GEO_MAGIC3.as_slice() {
            24
        } else if magic == GEO_MAGIC2.as_slice() {
            16
        } else {
            return None;
        };
        let tail = file.map.len().checked_sub(tail_len).filter(|&t| t >= 8)?;
        let word = |at: usize| u64::from_le_bytes(file.map[at..at + 8].try_into().unwrap());
        let (n, len) = (word(tail), word(tail + 8));
        let bytes: Buf<u8> = file.buf(8, len, keep_mapped).ok()?;
        let at = (8 + bytes.len()).next_multiple_of(8);
        let ordinals: Buf<u32> = file.buf(at, n, keep_mapped).ok()?;
        let at = (at + byte_len::<u32>(n, &path).ok()?).next_multiple_of(8);
        let offsets: Buf<u64> = file.buf(at, n.checked_add(1)?, keep_mapped).ok()?;
        let mut at = at + byte_len::<u64>(n + 1, &path).ok()?;
        let mut boxes = SavedBoxes::default();
        if tail_len == 24 {
            let m = word(tail + 16);
            let box_ordinals: Buf<u32> = file.buf(at, m, keep_mapped).ok()?;
            at = (at + byte_len::<u32>(m, &path).ok()?).next_multiple_of(8);
            let corners: Buf<[u64; 4]> = file.buf(at, m, keep_mapped).ok()?;
            at += byte_len::<[u64; 4]>(m, &path).ok()?;
            let tags: Buf<u8> = file.buf(at, m, keep_mapped).ok()?;
            at = (at + byte_len::<u8>(m, &path).ok()?).next_multiple_of(8);
            boxes = SavedBoxes::new(box_ordinals, corners, tags)?;
        }
        if at != tail {
            return None;
        }
        Some((
            SavedPart::new(ordinals, offsets, bytes)?.with_boxes(boxes),
            true,
        ))
    };
    let mut parts = Vec::new();
    let mut linkable = HashMap::new();
    for name in &names {
        if let Some((part, piece)) = read(name) {
            parts.push(part);
            if piece {
                linkable.insert(name.clone(), sums[name]);
            }
        }
    }
    match Saved::from_parts(parts) {
        Some(saved) => (Some(saved), linkable),
        None => (None, HashMap::new()),
    }
}

/// Reads a checkpoint. With `keep_mapped`, the dictionary and segments stay views into the
/// mapped files; otherwise they are copied to the heap and the files closed.
fn read_checkpoint(
    dir: &Path,
    keep_mapped: bool,
) -> Result<(Dictionary, QuadIndex, Vec<DictPiece>), StoreError> {
    // A large mapped checkpoint is not read at open: [`verify_checkpoint`] checks it in the
    // background.
    let check = if keep_mapped && checks_in_background(dir) {
        Check::Lengths
    } else {
        Check::Full
    };
    let parts = read_parts(dir, keep_mapped, check)?;
    let mut dict = Dictionary::from_saved(parts.dict, parts.namespaces);
    dict.note_compact(parts.compact || has_compact_keys(dir));
    let mut quads = QuadIndex::from_segments(parts.segments);
    quads.set_runs(parts.runs);
    if parts.version < 2 {
        let (dict, quads) = inline_values(dict, quads);
        return Ok((dict, quads, Vec::new()));
    }
    read_stats(dir, &quads);
    Ok((dict, quads, parts.pieces))
}

/// A dictionary's pieces, as key bytes and key ends.
pub(crate) type DictPieces = Vec<crate::dict::SavedPiece>;

/// The files of a checkpoint, read.
struct Parts {
    /// The dictionary's pieces.
    dict: DictPieces,
    /// The namespaces of its compact IRI keys.
    namespaces: crate::dict::Namespaces,
    /// The dictionary's format.
    version: usize,
    /// Some piece of the dictionary may hold compact WKT keys ([`DICT_MAGIC_COMPACT`]).
    compact: bool,
    /// The dictionary pieces the next checkpoint can keep.
    pieces: Vec<DictPiece>,
    segments: Vec<Segment>,
    /// The runs of each permutation read, oldest first: added, removed.
    runs: Vec<Vec<(Segment, Segment)>>,
}

/// Reads the dictionary pieces and segments of the checkpoint in `dir`; with `verify`, checks
/// the files against their sums and the dictionary's keys (a checkpoint just written needs
/// neither).
fn read_parts(dir: &Path, keep_mapped: bool, check: Check) -> Result<Parts, StoreError> {
    let verify = check == Check::Full;
    let sums = read_sums(dir)?;
    let sums = sums.as_ref();
    let namespaces = read_namespaces(dir, sums)?;
    // One dictionary file: its key bytes and ends, and its format.
    let read_piece = |file: &str, keep_mapped: bool| -> Result<_, StoreError> {
        let path = dir.join(file);
        let file = Mapped::open(path.clone())?;
        // Before the sum: a newer file is not a damaged one.
        file.refuse_newer(&DICT_MAGICS)?;
        if verify {
            check_sum(sums, &file)?;
        } else {
            check_len(sums, &file)?;
        }
        let (version, h) = file.header(&DICT_MAGICS, 2)?;
        let at = 24;
        let bytes: Buf<u8> = file.buf(at, h[0], keep_mapped)?;
        let mut at = at + bytes.len();
        if version > 0
        /* from OXGSDIC2 on: padded */
        {
            at = at.next_multiple_of(8);
        }
        let ends: Buf<u64> = file.buf(at, h[1], keep_mapped)?;
        // The header's lengths must add up to the file's.
        if at + 8 * ends.len() != file.map.len()
            || ends.first() != Some(&0)
            || ends.last() != Some(&(bytes.len() as u64))
            || (verify && !Dictionary::valid_parts(&bytes, &ends, &namespaces))
        {
            return Err(corrupt(&path));
        }
        Ok((bytes, ends, version))
    };
    let read_dict = || -> Result<(DictPieces, usize, bool, Vec<DictPiece>), StoreError> {
        // The pieces after `dict.bin` are listed in the checksums (they are newer).
        let names: Vec<String> = (0..)
            .map(dict_piece_name)
            .take_while(|n| n == "dict.bin" || sums.is_some_and(|s| s.contains_key(n)))
            .collect();
        let pieces = names
            .par_iter()
            .map(|n| read_piece(n, keep_mapped))
            .collect::<Result<Vec<_>, _>>()?;
        let version = pieces[0].2;
        let current = pieces.iter().all(|p| p.2 >= 2);
        let compact = pieces.iter().any(|p| may_hold_compact_wkt(p.2));
        // Saved indexes are used when the keys stay mapped (copied to the heap, the table
        // is smaller).
        let indexes: Vec<Option<(SavedIndex, Sum)>> = names
            .par_iter()
            .zip(&pieces)
            .map(|(n, p)| {
                (current && keep_mapped)
                    .then(|| read_index(dir, &index_name(n), p.1.len() - 1, sums, verify))
                    .flatten()
            })
            .collect();
        let info = |n: &String, ends: &Buf<u64>, index: &Option<(SavedIndex, Sum)>| {
            Some(DictPiece {
                file: n.clone(),
                keys: ends.len() - 1,
                sum: *sums?.get(n)?,
                index: index.as_ref().map(|i| i.1),
                geoms: None,
            })
        };
        // Only pieces in the current format, with sums, are kept by the next checkpoint.
        let info: Vec<DictPiece> = if current {
            names
                .iter()
                .zip(&pieces)
                .zip(&indexes)
                .map(|((n, p), i)| info(n, &p.1, i))
                .collect::<Option<_>>()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let pieces = pieces
            .into_iter()
            .zip(indexes)
            .map(|((b, e, _), i)| (b, e, i.map(|i| i.0)))
            .collect();
        Ok((pieces, version, compact, info))
    };
    let read_segment = |name: &str| -> Result<Segment, StoreError> {
        let path = dir.join(format!("{name}.seg"));
        let file = Mapped::open(path.clone())?;
        file.refuse_newer(&[SEG_MAGIC, SEG_MAGIC2, SEG_MAGIC3, SEG_MAGIC4])?;
        if verify {
            check_sum(sums, &file)?;
        } else {
            check_len(sums, &file)?;
        }
        if file.map.get(..8) == Some(SEG_MAGIC4.as_slice()) {
            let mut segment = read_segment_v4(&file, keep_mapped, verify)?;
            // The next checkpoint links this file rather than writing the segment again
            // (#444): it stays the checkpoint's, and is not deleted with the segment.
            if let Some(&sum) = sums.and_then(|s| s.get(&format!("{name}.seg"))) {
                segment.file = Some(Arc::new(RunFile {
                    path,
                    sum,
                    owned: false,
                }));
            }
            return Ok(segment);
        }
        let magics = [SEG_MAGIC, SEG_MAGIC2, SEG_MAGIC3];
        let (version, _) = file.header(&magics, 0)?;
        let fields = [4, 5, 5][version];
        let (_, h) = file.header(&magics, fields)?;
        let at = 8 + 8 * h.len();
        let len = usize::try_from(h[0]).map_err(|_| corrupt(&path))?;
        let firsts: Buf<[u64; 4]> = file.buf(at, h[1], keep_mapped)?;
        let at = at + byte_len::<[u64; 4]>(h[1], &path)?;
        let offsets: Buf<u64> = file.buf(at, h[2], keep_mapped)?;
        let at = at + byte_len::<u64>(h[2], &path)?;
        if version < 2 {
            // Older block formats: v2 also holds the quads per block (from partial
            // compaction). Converted, so the next checkpoint writes the current format.
            let (starts, at) = if version == 1 {
                let starts: Buf<u64> = file.buf(at, h[4], keep_mapped)?;
                (Some(starts), at + byte_len::<u64>(h[4], &path)?)
            } else {
                (None, at)
            };
            let bytes: Buf<u8> = file.buf(at, h[3], keep_mapped)?;
            if at + bytes.len() != file.map.len() {
                return Err(corrupt(&path));
            }
            return Segment::from_legacy(len, firsts, offsets, starts, bytes)
                .ok_or_else(|| corrupt(&path));
        }
        let starts: Buf<u64> = file.buf(at, h[3], keep_mapped)?;
        let at = at + byte_len::<u64>(h[3], &path)?;
        let bytes: Buf<u8> = file.buf(at, h[4], keep_mapped)?;
        if at + bytes.len() != file.map.len() {
            return Err(corrupt(&path));
        }
        let segment = Segment {
            len,
            firsts,
            offsets,
            starts,
            bytes,
            stats: Default::default(),
            file: None,
        };
        if verify && !segment.is_consistent() {
            return Err(corrupt(&path));
        }
        Ok(segment)
    };
    // Checkpoints written before PSOG and GPSO existed have only the first six segments
    // (neither the file nor its checksum): the others are then built from SPOG.
    let saved = |name: &&str| {
        let file = format!("{name}.seg");
        dir.join(&file).exists() || sums.is_some_and(|s| s.contains_key(&file))
    };
    let names: Vec<&str> = PERM_NAMES
        .iter()
        .enumerate()
        .take_while(|(i, n)| *i < OLD_PERMS || saved(n))
        .map(|(_, n)| *n)
        .collect();
    let run_names: Vec<Vec<(String, String)>> = names
        .iter()
        .map(|n| {
            (0..)
                .map(|i| (run_name(n, i, true), run_name(n, i, false)))
                .take_while(|(a, _)| match sums {
                    Some(sums) => sums.contains_key(a),
                    None => dir.join(a).exists(),
                })
                .collect()
        })
        .collect();
    let (dict, (segments, runs)) = rayon::join(read_dict, || {
        rayon::join(
            || {
                names
                    .par_iter()
                    .map(|n| read_segment(n))
                    .collect::<Result<Vec<_>, _>>()
            },
            || {
                run_names
                    .par_iter()
                    .map(|runs| {
                        runs.iter()
                            .map(|(a, r)| {
                                let strip = |n: &str| n.trim_end_matches(".seg").to_owned();
                                Ok((read_segment(&strip(a))?, read_segment(&strip(r))?))
                            })
                            .collect::<Result<Vec<_>, StoreError>>()
                    })
                    .collect::<Result<Vec<_>, _>>()
            },
        )
    });
    let (dict, version, compact, pieces) = dict?;
    Ok(Parts {
        dict,
        namespaces,
        version,
        compact,
        pieces,
        segments: segments?,
        runs: runs?,
    })
}

/// A checkpoint from before issue #125 (OXGSDIC1 and 2) has dictionary ids for the literals
/// that now have inline ids: their quads get the inline ids, so the terms are found again.
/// Their dictionary entries stay, unused. The counts are rebuilt on first use.
fn inline_values(dict: Dictionary, quads: QuadIndex) -> (Dictionary, QuadIndex) {
    use crate::dict::{Encoded, decode, encode};
    use crate::id::{Kind, TermId};
    let mut key = Vec::new();
    let mut ids: rustc_hash::FxHashMap<u64, u64> = Default::default();
    for o in 1..=dict.len() as u32 {
        let bytes = dict.raw_key(o);
        if bytes.first() != Some(&(Kind::Typed as u8)) {
            continue;
        }
        if let Encoded::Inline(id) = encode(decode(bytes).as_ref(), &mut key) {
            ids.insert(TermId::new(Kind::Typed, u64::from(o)).0, id.0);
        }
    }
    if ids.is_empty() {
        return (dict, quads);
    }
    let all = crate::index::Pattern {
        s: None,
        p: None,
        o: None,
        g: None,
        all_graphs: true,
    };
    let migrated = quads
        .scan(all)
        .map(|[s, p, o, g]| [s, p, ids.get(&o).copied().unwrap_or(o), g])
        .collect();
    (dict, QuadIndex::from_quads(migrated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Kind, TermId};
    use crate::index::Pattern;

    #[test]
    fn the_check_in_the_background_waits_while_the_store_is_written() {
        let dir = tripletrail_testdir::tempdir("verify-wait");
        let bytes = vec![7u8; 3 << 20];
        let path = dir.join("data");
        std::fs::write(&path, &bytes).unwrap();
        let sum = (bytes.len() as u64, crc32fast::hash(&bytes));
        let files = || FilesToCheck(vec![(path.clone(), File::open(&path).unwrap(), sum)]);
        let stop = std::sync::atomic::AtomicBool::new(false);
        // Busy at first: nothing is read until the writes stop.
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let busy = || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 5;
        assert!(verify_files(files(), &stop, &busy).unwrap());
        assert!(asked.load(std::sync::atomic::Ordering::SeqCst) > 5);
        // Busy until the store closes: it stops without a verdict.
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!verify_files(files(), &stop, &|| true).unwrap());
        // A damaged file is still found.
        stop.store(false, std::sync::atomic::Ordering::SeqCst);
        let wrong = FilesToCheck(vec![(
            path.clone(),
            File::open(&path).unwrap(),
            (sum.0, sum.1 ^ 1),
        )]);
        assert!(verify_files(wrong, &stop, &|| false).is_err());
    }

    #[test]
    fn geometries_files_save_the_boxes_of_larger_geometries() {
        use oxigraph::model::{GraphNameRef, Literal, NamedNodeRef, QuadRef};
        let dir = tripletrail_testdir::tempdir("geo-boxes");
        let wkt = |text: String| {
            Literal::new_typed_literal(
                text,
                NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
            )
        };
        let literals = [
            // Saved with a box: CRS84, EPSG:4326 (its axes as written) and an empty polygon.
            wkt("POLYGON((3 50, 4 50, 4 51.5, 3 51, 3 50))".into()),
            wkt("<http://www.opengis.net/def/crs/EPSG/0/4326> LINESTRING(50 3, 51 4, 52 4.5, 52.5 4)"
                .into()),
            wkt("MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), EMPTY)".into()),
            // Without one: a short record, and another CRS (its IRI is in the record).
            wkt("POINT(4.35 50.85)".into()),
            wkt("<http://www.opengis.net/def/crs/EPSG/0/31370> POLYGON((150000 170000, 150100 170000, 150100 170100, 150000 170000))"
                .into()),
        ];
        let saved_box = [true, true, true, false, false];
        let store = crate::Store::open(&dir).unwrap();
        for (i, l) in literals.iter().enumerate() {
            store
                .insert(QuadRef::new(
                    NamedNodeRef::new_unchecked(&format!("http://example.com/g{i}")),
                    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT"),
                    l,
                    GraphNameRef::DefaultGraph,
                ))
                .unwrap();
        }
        store.checkpoint().unwrap();
        let ordinals: Vec<u32> = literals
            .iter()
            .map(|l| store.term_id(l.as_ref().into()).unwrap().payload() as u32)
            .collect();
        let expected: Vec<_> = literals
            .iter()
            .map(|l| {
                let g = store.geometry(l.as_ref().into()).unwrap();
                let b = geo::BoundingRect::bounding_rect(&g.geometry)
                    .map(|r| [r.min().x, r.min().y, r.max().x, r.max().y]);
                (g.crs.clone(), b)
            })
            .collect();
        drop(store);
        let cp = current_checkpoint(&dir).unwrap();
        let (saved, _) = read_geometries(&cp, false);
        let saved = saved.unwrap();
        for (i, &o) in ordinals.iter().enumerate() {
            let from_box = saved.bbox(o);
            assert_eq!(from_box.is_some(), saved_box[i], "geometry {i}");
            if let Some(b) = from_box {
                assert_eq!(b, expected[i], "geometry {i}");
            }
            let record = saved.record(o).and_then(crate::geobin::bbox);
            assert_eq!(record.as_ref(), Some(&expected[i]), "geometry {i}");
        }
        // The same file in format 2 (no boxes, as written before #754) is still read: the
        // boxes come from the records then.
        let path = cp.join("geoms.bin");
        let file = fs::read(&path).unwrap();
        assert_eq!(&file[..8], GEO_MAGIC3);
        let word = |at: usize| u64::from_le_bytes(file[at..at + 8].try_into().unwrap()) as usize;
        let (n, len) = (word(file.len() - 24), word(file.len() - 16));
        let end = (8 + len).next_multiple_of(8) + (4 * n).next_multiple_of(8) + 8 * (n + 1);
        let mut old = GEO_MAGIC2.to_vec();
        old.extend_from_slice(&file[8..end]);
        old.extend_from_slice(&file[file.len() - 24..file.len() - 8]);
        fs::write(&path, &old).unwrap();
        let sums = fs::read_to_string(cp.join(CHECKSUMS)).unwrap();
        let sums: String = sums
            .lines()
            .map(|line| match line.split(' ').next() {
                Some("geoms.bin") => {
                    format!("geoms.bin {} {:08x}\n", old.len(), crc32fast::hash(&old))
                }
                _ => format!("{line}\n"),
            })
            .collect();
        fs::write(cp.join(CHECKSUMS), sums).unwrap();
        let saved = read_geometries(&cp, false).0.unwrap();
        for (i, &o) in ordinals.iter().enumerate() {
            assert!(saved.bbox(o).is_none());
            let record = saved.record(o).and_then(crate::geobin::bbox);
            assert_eq!(record.as_ref(), Some(&expected[i]), "geometry {i}");
        }
        let store = crate::Store::open(&dir).unwrap();
        for (i, l) in literals.iter().enumerate() {
            let id = store.term_id(l.as_ref().into()).unwrap();
            assert_eq!(
                store.geometry_bbox(id),
                Some(expected[i].clone()),
                "geometry {i}"
            );
        }
        drop(store);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_checkpoints_get_inline_values() {
        // As a checkpoint from before #125 had them: "0.340"^^xsd:double and a date in the
        // dictionary, next to a double that stays there.
        let mut dict = Dictionary::new();
        let mut id = |kind: Kind, rest: &str| {
            let mut k = vec![kind as u8];
            k.extend_from_slice(rest.as_bytes());
            TermId::new(kind, u64::from(dict.get_or_insert(&k).0))
        };
        let s = id(Kind::Iri, "http://e/s");
        let p = id(Kind::Iri, "http://e/p");
        let km = id(
            Kind::Typed,
            "http://www.w3.org/2001/XMLSchema#double\x000.340",
        );
        let day = id(
            Kind::Typed,
            "http://www.w3.org/2001/XMLSchema#date\x002024-05-01",
        );
        let other = id(
            Kind::Typed,
            "http://www.w3.org/2001/XMLSchema#double\x003.4E-1",
        );
        let quads = QuadIndex::from_quads(vec![
            [s.0, p.0, km.0, 0],
            [s.0, p.0, day.0, 0],
            [s.0, p.0, other.0, 0],
        ]);
        let (_, quads) = inline_values(dict, quads);
        let all = Pattern {
            s: None,
            p: None,
            o: None,
            g: None,
            all_graphs: true,
        };
        let mut objects: Vec<u64> = quads.scan(all).map(|q| q[2]).collect();
        objects.sort_unstable();
        let mut expected = vec![
            TermId::number(Kind::Double, "0.340").unwrap().0,
            TermId::date_time("2024-05-01", true).unwrap().0,
            other.0,
        ];
        expected.sort_unstable();
        assert_eq!(objects, expected);
    }

    #[test]
    fn logs_with_and_without_commit_times() {
        let dir = tripletrail_testdir::tempdir("log");
        let path = dir.join("wal-0.log");
        // An older log (bare `C`), a timed commit, a load mark, and a transaction cut short.
        fs::write(
            &path,
            "+ <http://e/a> <http://e/p> <http://e/o> .\nC\n\
             - <http://e/a> <http://e/p> <http://e/o> .\nC 1700000000000\n\
             L 1700000000500\n\
             + <http://e/b> <http://e/p> <http://e/o> .\n",
        )
        .unwrap();
        let mut seen = Vec::new();
        let committed = scan_log(&path, |e| {
            seen.push(match e {
                LogEntry::Transaction(t, changes) => format!("T {t:?} {}", changes.len()),
                LogEntry::Load(t) => format!("L {t:?}"),
            });
            true
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                "T None 1",
                "T Some(1700000000000) 1",
                "L Some(1700000000500)"
            ]
        );
        let (log, len, transactions) = read_log(&path).unwrap();
        assert_eq!((log.len(), len, transactions), (2, committed, 2));
        fs::write(&path, "C soon\n").unwrap();
        assert!(read_log(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
