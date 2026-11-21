//! An RDF quad store built for spatial and routing workloads.
//!
//! - Terms get **64-bit ids** from a dictionary; canonical `xsd:integer` and `xsd:boolean`
//!   literals are stored inside the id. Ids are dense, so derived structures (spatial index,
//!   routing graphs) can index arrays by them.
//! - Quads are kept in **eight sorted, compressed permutations** (SPOG, POSG, OSPG, GSPO, GPOS,
//!   GOSP, and PSOG / GPSO for predicate scans sorted by subject), so every triple pattern is
//!   one range scan. Writes go to a small sorted delta that
//!   is folded into new segments now and then.
//! - **Geometry literals are parsed once**, when they are first stored, and kept by term id.
//! - [`Snapshot`] implements [`spareval::QueryableDataset`], so the same SPARQL evaluator,
//!   custom functions and SERVICE handlers run on it as on Oxigraph's store.
//! - On disk, a store is a checkpoint (dictionary plus segments) and a write-ahead log of
//!   the transactions since; see [`Store::open`].

mod bgp;
mod buf;
mod dataset;
mod dict;
pub mod entail;
mod estimate;
mod geobin;
mod geoms;
mod id;
mod index;
mod labels;
pub mod par;
mod persist;
mod ranks;
mod rows;
mod segment;
mod stats;
mod table;
mod update;
mod wkt;

pub use bgp::{
    BgpCount, CompiledBgp, DEFAULT_JOIN_MEMORY, IdRows, IdSlot, IdTriple, join_memory,
    memory_limit, set_join_memory, with_join_memory,
};
pub use dataset::{Dataset, DatasetError, QuadIter, RowView};
pub use labels::BlankNodes;
pub use persist::{LogEntry, STORE_FORMAT};

/// Where a store's write-ahead log stands (see [`Store::log_position`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPosition {
    /// The store's timeline: new for every store directory, so that positions of different
    /// incarnations of a dataset never mix.
    pub timeline: String,
    /// The generation of the log being written (bumped by every checkpoint).
    pub generation: u64,
    /// Its length on disk, in bytes, ending with a complete entry.
    pub length: u64,
}

/// What [`Store::read_log`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogRead {
    /// The store no longer has that log (retired and not kept), or not that far: a follower
    /// must start again from a base backup.
    Gone,
    /// Bytes of the log from the offset asked for.
    Bytes {
        bytes: Vec<u8>,
        /// The length of that log known to be on disk.
        end: u64,
        /// Whether the log is retired (a checkpoint followed it): nothing more is written to
        /// it, and the next generation's log follows.
        retired: bool,
    },
}

/// Calls `f` with each complete entry of write-ahead log bytes (as [`Store::read_log`] reads
/// them) until it returns `false`; returns how many bytes those entries take, which is where
/// the next read starts. An entry cut short at the end is left for the next read.
pub fn scan_log(bytes: &[u8], f: impl FnMut(LogEntry) -> bool) -> Result<u64, StoreError> {
    persist::scan_log_bytes(bytes, std::path::Path::new("a write-ahead log"), f)
}
pub use id::{DateTimeValue, Kind, Numeric, TermId};
pub use ranks::StringRanks;
pub use rows::{RowFilter, SharedHidden};
pub use table::{is_table_column, register_table, table_scope};
pub use update::{
    PrepareWhere, RowGuard, RowsOn, RunWhere, STORED_DEFAULT_GRAPH, UpdateError, WhereRead,
    WhereRows, read_stored_default_graph,
};

use dict::{Dictionary, Encoded};
use index::{Pattern, QuadIndex, Sorter};
use oxigraph::io::{RdfFormat, RdfParseError, RdfParser};
use oxigraph::model::{
    GraphName, GraphNameRef, NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Quad,
    QuadRef, Term, TermRef,
};
use rayon::iter::Either;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use spareval::{ExpressionTerm, InternalQuad, QueryableDataset};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tripletrail_geo::sync::{LockUnpoisoned, RwLockUnpoisoned};
use tripletrail_geo::{Geom, parse_wkt_literal_uncached};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Parse(#[from] RdfParseError),
    #[error("corrupt store file {0}")]
    Corrupt(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("the dataset is read-only")]
    ReadOnly,
    /// Another process has the store open for writing (or, to write, has it open at all).
    #[error("the store {0} is in use by another process")]
    Locked(String),
}

/// Quads parsed per batch handed to the interning thread (see [`Store::bulk_load_with`]).
#[cfg(not(test))]
const PARSE_BATCH: usize = 65_536;
/// Small in tests, so loads hand over several batches.
#[cfg(test)]
const PARSE_BATCH: usize = 3;
/// Writes are folded into the segments once the delta holds this many quads.
/// How much N-Triples / N-Quads text a bulk load reads and parses at a time.
#[cfg(not(test))]
const PARSE_BLOCK: usize = 64 << 20;
/// Small in tests, so loads cross block boundaries and lines longer than a block.
#[cfg(test)]
const PARSE_BLOCK: usize = 100;
const COMPACT_AT: usize = 65_536;
/// A transaction that takes longer than this logs where its time went (#437).
const SLOW_COMMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// What [`Store::commit`] did, for the log of slow commits.
#[derive(Default)]
struct Applied {
    /// The delta was folded into the segments (runs written): the time its lookups in the
    /// stored quads took.
    folded: Option<std::time::Duration>,
    /// Time spent writing a checkpoint, zero without one.
    checkpoint: std::time::Duration,
}
/// Bulk loads of at most this many quads into a store that holds data are applied like a
/// transaction (see [`Store::bulk_load`]).
const SMALL_LOAD: usize = 100_000;

/// What [`Store::restore`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// The logged transactions replayed onto the base backup.
    pub transactions: u64,
    /// The write-ahead logs read.
    pub logs: u64,
    /// The commit time of the last transaction replayed (milliseconds since 1970), when the
    /// log records it.
    pub last_commit: Option<u64>,
    /// The quads of the restored store.
    pub quads: usize,
}

/// A quad store. Cheap to clone; clones share the data.
#[derive(Clone)]
pub struct Store {
    /// Shared by the clones of this store: when the last one goes, so does the writer.
    /// Dropped first (fields drop in order), so [`Open`] still finds the store.
    _open: Arc<Open>,
    inner: Arc<Inner>,
}

/// Closes a store when the last of its clones is dropped, even while a merge in the
/// background still holds the store: the merge stops, its files are removed, and the writer
/// goes, and with it the directory's lock, so the store can be opened again at once and
/// nothing writes in its directory any more.
struct Open(std::sync::Weak<Inner>);

impl Drop for Open {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner
                .closed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // A merge that finishes meanwhile may start another before it sees `closed`:
            // joined in turn.
            loop {
                let threads = std::mem::take(&mut *inner.merge_threads.lock_unpoisoned());
                if threads.is_empty() {
                    break;
                }
                for thread in threads {
                    let _ = thread.join();
                }
            }
            inner.writer.lock_unpoisoned().take();
        }
    }
}

impl Store {
    fn with_inner(inner: Inner) -> Self {
        let inner = Arc::new(inner);
        Self {
            _open: Arc::new(Open(Arc::downgrade(&inner))),
            inner,
        }
    }
}

impl Inner {
    /// The parsed geometry of a stored geometry literal, parsing it on first use.
    fn geometry(&self, id: TermId) -> Option<Arc<Geom>> {
        if id.kind() != Kind::Geometry {
            return None;
        }
        let o = id.payload() as u32;
        if o == 0 {
            return None;
        }
        self.geometries.get_or_parse(o, || {
            // Saved with the checkpoint: decoded, not parsed.
            if let Some(g) = self.geometries.saved(o) {
                return Some(Some(Arc::new(g)));
            }
            // Decoded under the lock, parsed outside it.
            let term = {
                let dict = self.dict.read_unpoisoned();
                if o as usize > dict.len() {
                    return None;
                }
                // Kept in binary form in the dictionary.
                if let Some(g) = dict.geometry_record(o).and_then(geobin::decode) {
                    return Some(Some(Arc::new(g)));
                }
                dict.term(o)
            };
            Some(parse_wkt_literal_uncached(&term))
        })
    }

    /// The parsed geometry of a stored geometry literal, without keeping it: the one already
    /// parsed if there is one, else decoded or parsed for this caller only. A join that reads
    /// millions of polygons once each (#2) would otherwise keep them all in memory.
    fn geometry_transient(&self, id: TermId) -> Option<Arc<Geom>> {
        if id.kind() != Kind::Geometry {
            return None;
        }
        let o = id.payload() as u32;
        if o == 0 {
            return None;
        }
        if let Some(g) = self.geometries.peek(o) {
            return g;
        }
        if let Some(g) = self.geometries.saved(o) {
            return Some(Arc::new(g));
        }
        let term = {
            let dict = self.dict.read_unpoisoned();
            if o as usize > dict.len() {
                return None;
            }
            if let Some(g) = dict.geometry_record(o).and_then(geobin::decode) {
                return Some(Arc::new(g));
            }
            dict.term(o)
        };
        parse_wkt_literal_uncached(&term)
    }
}

/// What one commit changed (see [`Store::touched_between`]).
struct Touched {
    generation: u64,
    resources: Vec<u64>,
    predicates: Vec<u64>,
}

/// Commits larger than this are not kept for [`Store::touched_between`].
const TOUCHED_QUADS: usize = 1 << 16;

/// How many commits [`Store::touched_between`] can look back over.
const TOUCHED_COMMITS: usize = 64;

/// The most copies of triples in several graphs [`Snapshot::union_copies`] keeps (32 bytes and
/// a hash slot each); past them the union is left to the evaluator's merge.
pub const MAX_UNION_COPIES: usize = 4_000_000;

struct Inner {
    dict: RwLock<Dictionary>,
    quads: RwLock<QuadIndex>,
    /// Geometry literals parsed so far, by ordinal (`None`: not valid). Parsed on first use, so
    /// loads don't pay for geometries no query looks at.
    geometries: geoms::Geometries,
    /// Held by the one writer at a time; holds the write-ahead log of an on-disk store.
    writer: Mutex<Option<persist::Disk>>,
    /// What followers read of the write-ahead logs (#217), on a writable on-disk store.
    shipping: Option<persist::Shipping>,
    /// Opened with [`Store::open_read_only`]: every write fails.
    read_only: bool,
    /// The checkpoint files are memory-mapped rather than read.
    mapped: bool,
    /// Bumped by every change of the quads (see [`Store::generation`]).
    generation: std::sync::atomic::AtomicU64,
    /// What the last commits changed, by the generation each made (see
    /// [`Store::touched_between`]).
    touched: Mutex<std::collections::VecDeque<Touched>>,
    /// Built on first use (see [`Snapshot::string_ranks`]).
    ranks: Mutex<Option<Arc<StringRanks>>>,
    /// The copies of triples held by several graphs, for the generation they were found at
    /// (see [`Snapshot::union_copies`]).
    /// Held while they are found or brought up to date, so queries that need them at once
    /// wait for that one scan.
    union_copies: Mutex<Option<(u64, Option<Arc<RowFilter>>)>>,
    /// The shared lock of a store opened read-only (a writable store's is in its writer).
    _lock: Option<std::fs::File>,
    /// A merge of runs into the segments is running in the background (see
    /// [`Store::merge_in_background`]).
    merging: std::sync::atomic::AtomicBool,
    /// Off: runs are only folded in by compactions (for tests).
    background_merges: std::sync::atomic::AtomicBool,
    /// The graph kept holding the RDFS entailment of the others (#187), if any: every
    /// transaction and load brings it up to date (see [`Store::set_rdfs_entailment`]).
    rdfs_into: RwLock<Option<(NamedNode, entail::Regime)>>,
    /// The check of the checkpoint's files against their sums, run in the background after
    /// the store opens (#436).
    verification: Arc<VerifyState>,
    /// WKT literals that can be written back exactly are kept only in binary form by the
    /// next checkpoints (see [`Store::set_render_wkt`]).
    render_wkt: std::sync::atomic::AtomicBool,
    /// Every clone of the store was dropped (see [`Open`]): a merge running in the
    /// background stops.
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// The threads of merges in the background, joined when the store closes.
    merge_threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// When the quads last changed, in milliseconds since [`epoch`].
    last_write: std::sync::atomic::AtomicU64,
    /// Transactions running now: the check of the files in the background waits while there
    /// are any (#437).
    writing: std::sync::atomic::AtomicUsize,
    /// The thread that merges the runs left once the store is idle runs (#442).
    idle_watch: std::sync::atomic::AtomicBool,
    /// Commits and checkpoints since the store opened (see [`Store::write_stats`]).
    write_counts: WriteCounts,
}

/// See [`Store::write_stats`].
#[derive(Default)]
struct WriteCounts {
    commits: std::sync::atomic::AtomicU64,
    commit_nanos: std::sync::atomic::AtomicU64,
    slow_commits: std::sync::atomic::AtomicU64,
    checkpoints: std::sync::atomic::AtomicU64,
    checkpoint_nanos: std::sync::atomic::AtomicU64,
    last_checkpoint_nanos: std::sync::atomic::AtomicU64,
}

/// What a store wrote since it opened (#220: for metrics): transactions that changed
/// something, with the time they took to commit (durable included); those of a second or
/// more; and checkpoints written, with their time and the last one's.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WriteStats {
    pub commits: u64,
    pub commit_seconds: f64,
    pub slow_commits: u64,
    pub checkpoints: u64,
    pub checkpoint_seconds: f64,
    pub last_checkpoint_seconds: f64,
}

/// How long a store with runs must go without writes before they are merged into the
/// segments (#442), in milliseconds.
static IDLE_MERGE_AFTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30_000);

/// Counts a transaction in [`Inner::writing`] while it lives.
struct Writing<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> Writing<'a> {
    fn new(count: &'a std::sync::atomic::AtomicUsize) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// How long after a write the check of the files in the background stays paused, in
/// milliseconds (#437): commits come in bursts, and each pause ends at the first gap.
const VERIFY_PAUSE_AFTER_WRITE: u64 = 1000;

/// Milliseconds since the first call, for [`Inner::last_write`].
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Where the check of a store's files stands (see [`Store::verification`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The files are being read and checked against their sums.
    Checking,
    /// Every file matches its sum (or there was nothing to check).
    Verified,
    /// A file does not match its sum: the store answers queries, but refuses writes.
    Damaged(String),
    /// Not finished: the store closed first.
    Stopped,
}

#[derive(Default)]
struct VerifyState {
    state: Mutex<Option<Verification>>,
    done: std::sync::Condvar,
}

impl VerifyState {
    fn set(&self, v: Verification) {
        *self.state.lock_unpoisoned() = Some(v);
        self.done.notify_all();
    }

    fn get(&self) -> Verification {
        self.state
            .lock_unpoisoned()
            .clone()
            .unwrap_or(Verification::Verified)
    }

    fn damaged(&self) -> Result<(), StoreError> {
        match &*self.state.lock_unpoisoned() {
            Some(Verification::Damaged(file)) => Err(StoreError::Corrupt(file.clone())),
            _ => Ok(()),
        }
    }
}

/// Memory use of a store, in bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryUse {
    pub dictionary: usize,
    pub quads: usize,
    pub terms: usize,
    /// Geometry literals parsed so far (they are parsed on first use).
    pub geometries: usize,
    /// Roughly the heap bytes of those parsed geometries (#225).
    pub geometry_bytes: usize,
    /// The copies of triples in several graphs found for the union default graph (see
    /// [`Snapshot::union_copies`]).
    pub union_copies: usize,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// An empty in-memory store.
    pub fn new() -> Self {
        Self::from_parts(
            Dictionary::new(),
            QuadIndex::default(),
            None,
            None,
            false,
            false,
        )
    }

    fn from_parts(
        dict: Dictionary,
        quads: QuadIndex,
        disk: Option<persist::Disk>,
        saved: Option<geobin::Saved>,
        read_only: bool,
        mapped: bool,
    ) -> Self {
        Self::with_inner(Inner {
            dict: RwLock::new(dict),
            quads: RwLock::new(quads),
            geometries: geoms::Geometries::with_saved(saved),
            shipping: disk.as_ref().map(persist::Disk::shipping),
            writer: Mutex::new(disk),
            read_only,
            mapped,
            generation: Default::default(),
            touched: Default::default(),
            ranks: Default::default(),
            union_copies: Default::default(),
            _lock: None,
            merging: Default::default(),
            background_merges: std::sync::atomic::AtomicBool::new(true),
            rdfs_into: Default::default(),
            render_wkt: Default::default(),
            verification: Default::default(),
            closed: Default::default(),
            merge_threads: Default::default(),
            last_write: Default::default(),
            writing: Default::default(),
            idle_watch: Default::default(),
            write_counts: Default::default(),
        })
    }

    /// Opens the store kept in directory `path`, creating it if needed. Every committed
    /// transaction is in the write-ahead log before the commit returns; bulk loads write a
    /// new checkpoint instead. [`Store::checkpoint`] folds the log into a new checkpoint.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let (dict, quads, saved, disk, log) = persist::open(path.as_ref())?;
        let render_wkt = persist::render_wkt(path.as_ref());
        let checkpoint = disk.checkpoint_path();
        let store = Self::from_parts(dict, quads, Some(disk), saved, false, true);
        store
            .inner
            .render_wkt
            .store(render_wkt, std::sync::atomic::Ordering::Relaxed);
        // A store marked so before checkpoints said whether they hold compact keys.
        store.inner.dict.write_unpoisoned().note_compact(render_wkt);
        store.replay(log)?;
        store.verify_in_background(checkpoint);
        store.merge_in_background(&store.inner.writer.lock_unpoisoned());
        Ok(store)
    }

    /// Opens the store kept in directory `path` for reading only: nothing in the directory is
    /// changed (no log is opened, no stale file removed), the checkpoint files are
    /// memory-mapped rather than read (so several processes, or two versions of a dataset, share
    /// the page cache and the kernel can drop unused pages), and every write returns
    /// [`StoreError::ReadOnly`]. Transactions logged since the checkpoint are applied in
    /// memory.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let (dict, quads, saved, log, lock) = persist::open_read_only(path.as_ref())?;
        let store = Self::from_parts(dict, quads, None, saved, false, true);
        let render_wkt = persist::render_wkt(path.as_ref());
        store
            .inner
            .render_wkt
            .store(render_wkt, std::sync::atomic::Ordering::Relaxed);
        // A store marked so before checkpoints said whether they hold compact keys.
        store.inner.dict.write_unpoisoned().note_compact(render_wkt);
        store.replay(log)?;
        let Store { inner, _open } = store;
        // Its writer is none already.
        drop(_open);
        let inner = Arc::into_inner(inner).expect("a store nobody else holds yet");
        let store = Self::with_inner(Inner {
            read_only: true,
            mapped: true,
            shipping: None,
            generation: Default::default(),
            touched: Default::default(),
            _lock: lock,
            closed: Default::default(),
            ..inner
        });
        store.verify_in_background(persist::current_checkpoint(path.as_ref()));
        Ok(store)
    }

    /// Keeps WKT literals only in binary form, or not (#370). On, each checkpoint from now on
    /// keeps a WKT literal it writes as its geometry only, without its text, when the text
    /// written back from that geometry is exactly the literal's text (most WKT written by
    /// software is); results show the same text either way. That saves about the size of the
    /// text, at some cost each time a result shows it (about half a microsecond per
    /// geometry). Literals already written stay as they are until a checkpoint rewrites them.
    ///
    /// Kept with an on-disk store (a `render-wkt` file in its directory), so it holds when the
    /// store is opened again; [`Store::save_to`], [`Store::save_base`], [`Store::copy_dir`] and
    /// [`Store::restore`] carry it. Fails on a read-only store.
    pub fn set_render_wkt(&self, on: bool) -> Result<(), StoreError> {
        if self.inner.read_only {
            return Err(StoreError::ReadOnly);
        }
        let writer = self.inner.writer.lock_unpoisoned();
        if let Some(disk) = writer.as_ref() {
            persist::mark_render_wkt(disk.dir(), on)?;
        }
        self.inner
            .render_wkt
            .store(on, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Whether WKT literals are kept in binary form (see [`Store::set_render_wkt`]).
    pub fn render_wkt(&self) -> bool {
        self.inner
            .render_wkt
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Checks the files of the checkpoint `dir` against their sums in a thread of its own,
    /// so that opening a large store does not wait for its data to be read (#436). The files
    /// are opened here, before any write: the check reads them even after a checkpoint
    /// replaced the directory.
    fn verify_in_background(&self, dir: Option<PathBuf>) {
        let Some(dir) = dir.filter(|d| persist::checks_in_background(d)) else {
            return;
        };
        let state = Arc::clone(&self.inner.verification);
        let damaged = |e: StoreError| {
            eprintln!("tripletrail-store: {e}: the store refuses writes");
            Verification::Damaged(match e {
                StoreError::Corrupt(file) => file,
                e => e.to_string(),
            })
        };
        let files = match persist::files_to_check(&dir) {
            Ok(files) => files,
            Err(e) => return state.set(damaged(e)),
        };
        state.set(Verification::Checking);
        let closed = Arc::clone(&self.inner.closed);
        // Waits while the store is being written (#437): on a store larger than memory, the
        // check reads every file front to back, and the first commits after opening, whose
        // lookups read the disk, waited behind it.
        let inner = Arc::downgrade(&self.inner);
        let busy = move || {
            inner.upgrade().is_some_and(|inner| {
                use std::sync::atomic::Ordering::SeqCst;
                inner.writing.load(SeqCst) > 0
                    || now_ms().saturating_sub(inner.last_write.load(SeqCst))
                        < VERIFY_PAUSE_AFTER_WRITE
                        && inner.last_write.load(SeqCst) > 0
            })
        };
        let spawned = std::thread::Builder::new()
            .name("tripletrail-verify".into())
            .spawn(move || {
                let v = match persist::verify_files(files, &closed, &busy) {
                    Ok(true) => Verification::Verified,
                    Ok(false) => Verification::Stopped,
                    Err(e) => damaged(e),
                };
                state.set(v);
            });
        if spawned.is_err() {
            self.inner.verification.set(Verification::Stopped);
        }
    }

    /// Holds, for the whole process, every check of files in the background before it reads
    /// anything, while `on` (for tests of what happens during the check).
    #[doc(hidden)]
    pub fn hold_verification(on: bool) {
        persist::hold_checks(on);
    }

    /// Sets, for the calling thread, the checkpoint size (in bytes, 1 GiB by default) above
    /// which opening a store checks its files in the background (for tests).
    #[doc(hidden)]
    pub fn verify_in_background_above(bytes: u64) {
        persist::set_background_above(bytes);
    }

    /// Where the check of the store's files stands. Opening a store whose checkpoint is
    /// larger than 1 GiB maps it without reading it; its files are then read and checked
    /// against their sums in the background (#436), and a damaged file makes the store refuse
    /// writes. A smaller checkpoint is checked whole as it opens (a damaged file is an error
    /// then), and reads [`Verification::Verified`].
    pub fn verification(&self) -> Verification {
        self.inner.verification.get()
    }

    /// Waits until the check of the store's files is done (see [`Store::verification`]):
    /// an error names a damaged file.
    pub fn wait_verified(&self) -> Result<(), StoreError> {
        let state = &self.inner.verification;
        let mut v = state.state.lock_unpoisoned();
        while matches!(*v, Some(Verification::Checking)) {
            v = state.done.wait(v).unwrap_or_else(|e| e.into_inner());
        }
        drop(v);
        state.damaged()
    }

    /// Whether the store was opened with [`Store::open_read_only`].
    pub fn is_read_only(&self) -> bool {
        self.inner.read_only
    }

    /// Bytes of checkpoint files this store maps instead of holding on the heap.
    pub fn mapped_size(&self) -> usize {
        if !self.inner.mapped {
            return 0;
        }
        let dict = self.inner.dict.read_unpoisoned();
        dict.mapped_size() + self.inner.quads.read_unpoisoned().mapped_size()
    }

    /// The writer lock, or [`StoreError::ReadOnly`].
    fn lock_writer(&self) -> Result<std::sync::MutexGuard<'_, Option<persist::Disk>>, StoreError> {
        if self.inner.read_only {
            return Err(StoreError::ReadOnly);
        }
        self.inner.verification.damaged()?;
        Ok(self.inner.writer.lock_unpoisoned())
    }

    /// Applies logged transactions read at open.
    fn replay(&self, log: Vec<(bool, Quad)>) -> Result<(), StoreError> {
        let store = self;
        if !log.is_empty() {
            let mut writer = store.inner.writer.lock_unpoisoned();
            let mut txn = Transaction::new(store);
            for (insert, quad) in log {
                if insert {
                    txn.insert(quad.as_ref());
                } else {
                    txn.remove(quad.as_ref());
                }
            }
            let pending: Vec<([u64; 4], bool)> = txn.finish().into_iter().collect();
            store.commit(&pending, false, &mut writer)?;
        }
        Ok(())
    }

    /// Whether the store is kept on disk.
    pub fn is_persistent(&self) -> bool {
        self.inner.writer.lock_unpoisoned().is_some()
    }

    /// An in-memory copy of the store that changes apart from it, made without decoding a
    /// quad: the quad index is shared until either side writes (its versions are immutable),
    /// and the dictionary is copied, so term ids mean the same in both until either adds
    /// terms. Geometries are parsed again on first use. For [`Store::changes_from`].
    pub fn fork(&self) -> Self {
        let dict = self.inner.dict.read_unpoisoned().clone();
        let mut quads = self.inner.quads.read_unpoisoned().clone();
        // Folds of the copy build their segments in memory, not in this store's directory.
        quads.spill = None;
        let pinned = quads.pinned;
        let store = Self::from_parts(dict, quads, None, None, false, false);
        // A fork of an overlay is one too: no merge copies the base it shares.
        if pinned {
            store
                .inner
                .background_merges
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        store
    }

    /// Adds the triples of every named graph but `except` to the default graph, by id, without
    /// decoding a term: the default graph then holds what a union default graph reads, as a
    /// graph of its own. For a [`Store::fork`] that SHACL rules run on (#520). Returns the
    /// quads read.
    pub fn union_into_default_graph(&self, except: Option<TermId>) -> Result<usize, StoreError> {
        let mut writer = self.lock_writer()?;
        let mut sorter = Sorter::new(writer.as_ref().map(|d| Arc::clone(&d.spill)));
        let snapshot = self.snapshot();
        for [s, p, o, g] in snapshot.quad_ids_for_pattern(None, None, None, GraphSelector::AnyNamed)
        {
            if Some(g) != except {
                sorter.push([s.0, p.0, o.0, 0]);
            }
        }
        drop(snapshot);
        self.bulk_finish(sorter, &mut writer)
    }

    /// An overlay of the store (#406): a [`Store::fork`] that never copies the store's quads.
    /// Its writes stay in runs beside the shared segments, never folded into them (in the
    /// background or otherwise), so it takes memory in proportion to its changes. Reads see
    /// the store as it was when the overlay was made, with the overlay's changes; the store
    /// itself goes on changing apart from it. [`Store::changes_from`] gives the changes. A
    /// fork of an overlay is an overlay of the same base.
    pub fn overlay(&self) -> Self {
        let store = self.fork();
        store.inner.quads.write_unpoisoned().pinned = true;
        store
            .inner
            .background_merges
            .store(false, std::sync::atomic::Ordering::SeqCst);
        store
    }

    /// The quads kept apart from the segments, added and removed, counted once: an upper
    /// bound of an [`Store::overlay`]'s changes beyond those its base had in runs, cheap to
    /// read.
    pub fn unmerged_len(&self) -> usize {
        self.inner.quads.read_unpoisoned().unmerged_len()
    }

    /// The quads of this store that `before` lacks and those of `before` that it lacks, by id
    /// (`[s, p, o, g]`, `g` 0 for the default graph), in SPOG order. Ids are this store's, so
    /// it must be [`Store::fork`]ed from `before` (or `before` from it) and the terms of the
    /// removed quads are found with this store's [`Snapshot::term`].
    #[allow(clippy::type_complexity)]
    pub fn changes_from(&self, before: &Store) -> (Vec<[TermId; 4]>, Vec<[TermId; 4]>) {
        let after = self.inner.quads.read_unpoisoned().clone();
        let before = before.inner.quads.read_unpoisoned().clone();
        let (added, removed) = after.difference(&before);
        let ids = |v: Vec<[u64; 4]>| {
            v.into_iter()
                .map(|q| [TermId(q[0]), TermId(q[1]), TermId(q[2]), TermId(q[3])])
                .collect()
        };
        (ids(added), ids(removed))
    }

    /// Replaces the quads, with a new [`Store::generation`] under the same lock; returns it.
    fn set_quads(&self, quads: QuadIndex) -> u64 {
        let mut guard = self.inner.quads.write_unpoisoned();
        *guard = quads;
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Keeps what a commit that made `generation` changed, for [`Store::touched_between`]. A
    /// large commit is not kept: it leaves a gap, and whoever asks across it gets `None`.
    fn note_touched(&self, generation: u64, pending: &[([u64; 4], bool)]) {
        let mut touched = self.inner.touched.lock_unpoisoned();
        if pending.len() <= TOUCHED_QUADS {
            let mut resources: Vec<u64> = pending.iter().flat_map(|(q, _)| [q[0], q[2]]).collect();
            resources.sort_unstable();
            resources.dedup();
            let mut predicates: Vec<u64> = pending.iter().map(|(q, _)| q[1]).collect();
            predicates.sort_unstable();
            predicates.dedup();
            touched.push_back(Touched {
                generation,
                resources,
                predicates,
            });
        }
        while touched.len() > TOUCHED_COMMITS {
            touched.pop_front();
        }
    }

    /// The subjects and objects (`resources`) and the predicates of the quads that the commits
    /// after generation `from`, up to and with `to`, added or removed: `None` when that is not
    /// known for every one of them (another kind of change, a large commit, or one too long
    /// ago). For work derived from the data that can be brought up to date from what changed,
    /// such as the row filters of #202.
    pub fn touched_between(&self, from: u64, to: u64) -> Option<(Vec<u64>, Vec<u64>)> {
        Self::touched_between_inner(&self.inner, from, to)
    }

    fn touched_between_inner(inner: &Inner, from: u64, to: u64) -> Option<(Vec<u64>, Vec<u64>)> {
        if to <= from {
            return (to == from).then(Default::default);
        }
        let touched = inner.touched.lock_unpoisoned();
        let mut next = from + 1;
        let (mut resources, mut predicates) = (Vec::new(), Vec::new());
        for t in touched.iter() {
            if t.generation < next {
                continue;
            }
            if t.generation != next {
                return None;
            }
            resources.extend_from_slice(&t.resources);
            predicates.extend_from_slice(&t.predicates);
            if next == to {
                resources.sort_unstable();
                resources.dedup();
                predicates.sort_unstable();
                predicates.dedup();
                return Some((resources, predicates));
            }
            next += 1;
        }
        None
    }

    /// A consistent read-only view; later writes don't change it.
    pub fn snapshot(&self) -> Snapshot {
        let quads = self.inner.quads.read_unpoisoned();
        Snapshot {
            inner: Arc::clone(&self.inner),
            // Read under the quads' lock, which every change of the generation holds.
            generation: Some(
                self.inner
                    .generation
                    .load(std::sync::atomic::Ordering::SeqCst),
            ),
            quads: quads.clone(),
            ephemeral: Arc::default(),
            all_graphs: false,
            merged_default: MergedDefault::Plain,
            tables: table::pending(),
            rows: None,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.quads.read_unpoisoned().len()
    }

    /// A number that changes whenever the quads change (each commit and bulk load), for
    /// caches of what is derived from them: equal generations mean the same data.
    pub fn generation(&self) -> u64 {
        self.inner
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Commits and checkpoints since the store opened (see [`WriteStats`]).
    pub fn write_stats(&self) -> WriteStats {
        use std::sync::atomic::Ordering::Relaxed;
        let c = &self.inner.write_counts;
        let seconds = |n: &std::sync::atomic::AtomicU64| n.load(Relaxed) as f64 / 1e9;
        WriteStats {
            commits: c.commits.load(Relaxed),
            commit_seconds: seconds(&c.commit_nanos),
            slow_commits: c.slow_commits.load(Relaxed),
            checkpoints: c.checkpoints.load(Relaxed),
            checkpoint_seconds: seconds(&c.checkpoint_nanos),
            last_checkpoint_seconds: seconds(&c.last_checkpoint_nanos),
        }
    }

    pub fn memory_use(&self) -> MemoryUse {
        let dict = self.inner.dict.read_unpoisoned();
        MemoryUse {
            dictionary: dict.heap_size(),
            quads: self.inner.quads.read_unpoisoned().heap_size(),
            terms: dict.len(),
            geometries: self.inner.geometries.parsed(),
            geometry_bytes: self.inner.geometries.heap_size(),
            // Not waited for: a scan that is finding them holds the lock.
            union_copies: match self.inner.union_copies.try_lock() {
                Ok(c) => c
                    .as_ref()
                    .and_then(|(_, f)| f.as_ref())
                    .map_or(0, |f| f.quads_heap_size()),
                Err(_) => 0,
            },
        }
    }

    pub fn contains<'a>(&self, quad: impl Into<QuadRef<'a>>) -> bool {
        self.snapshot().contains(quad)
    }

    pub fn quads_for_pattern(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
    ) -> impl Iterator<Item = Quad> + use<> {
        self.snapshot()
            .quads_for_pattern(subject, predicate, object, graph_name)
    }

    /// How many quads match a pattern, without reading them; see [`Snapshot::estimate_len`].
    pub fn estimate_len(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
    ) -> usize {
        self.snapshot()
            .estimate_len(subject, predicate, object, graph_name)
    }

    /// Every quad.
    pub fn iter(&self) -> impl Iterator<Item = Quad> + use<> {
        self.quads_for_pattern(None, None, None, None)
    }

    /// Runs `f` as one transaction: it sees its own writes, and its writes become visible
    /// (and durable, for an on-disk store) together when it returns `Ok`.
    pub fn transaction<T, E: From<StoreError>>(
        &self,
        f: impl FnOnce(&mut Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        Ok(self.transaction_with_changes(f)?.0)
    }

    /// [`Store::transaction`], also returning what the transaction changed.
    pub fn transaction_with_changes<T, E: From<StoreError>>(
        &self,
        f: impl FnOnce(&mut Transaction<'_>) -> Result<T, E>,
    ) -> Result<(T, Changes), E> {
        let start = std::time::Instant::now();
        let _writing = Writing::new(&self.inner.writing);
        let mut writer = self.lock_writer()?;
        let locked = start.elapsed();
        let mut txn = Transaction::new(self);
        let result = f(&mut txn)?;
        if !txn.entailed
            && let Some((into, regime)) = self.kept_entailment()
        {
            txn.maintain(into.as_ref(), regime);
        }
        // As a list, without the map's spare capacity: it is the log, the commit and the
        // changes returned.
        let pending: Vec<([u64; 4], bool)> = txn.finish().into_iter().collect();
        let changed = start.elapsed();
        // Group commit (#709): the log is synced after the writer lock is let go, so that
        // the transactions queued behind this one share the sync. The transaction is applied
        // first, and becomes visible to readers up to one sync before it is durable; its
        // caller hears back only once it is.
        let mut durable = None;
        if let Some(disk) = writer.as_mut()
            && !pending.is_empty()
        {
            let dict = self.inner.dict.read_unpoisoned();
            let ticket = disk
                .append_unsynced(&pending, |q| decode_quad(&dict, q))
                .map_err(StoreError::from)?;
            durable = Some((Arc::clone(&disk.durable), ticket));
        }
        let logged = start.elapsed();
        let applied = self.commit(&pending, true, &mut writer)?;
        drop(writer);
        let applied_at = start.elapsed();
        if let Some((durable, ticket)) = durable {
            durable.wait(ticket).map_err(StoreError::from)?;
        }
        let total = start.elapsed();
        if !pending.is_empty() {
            use std::sync::atomic::Ordering::Relaxed;
            let c = &self.inner.write_counts;
            c.commits.fetch_add(1, Relaxed);
            c.commit_nanos
                .fetch_add(u64::try_from(total.as_nanos()).unwrap_or(u64::MAX), Relaxed);
            if total >= SLOW_COMMIT {
                c.slow_commits.fetch_add(1, Relaxed);
            }
        }
        if total >= SLOW_COMMIT {
            // Where the time of a slow commit went, for #437.
            eprintln!(
                "tripletrail-store: slow commit of {} changes: {} ms (waiting for the writer lock {} ms, \
                 changes and term lookups {} ms, write-ahead log {} ms, applying {} ms, \
                 syncing the log {} ms{}{}{})",
                pending.len(),
                total.as_millis(),
                locked.as_millis(),
                (changed - locked).as_millis(),
                (logged - changed).as_millis(),
                (applied_at - logged - applied.checkpoint).as_millis(),
                (total - applied_at).as_millis(),
                match applied.folded {
                    Some(looked_up) => format!(
                        " folding the delta in, of which {} ms looking its changes up",
                        looked_up.as_millis()
                    ),
                    None => String::new(),
                },
                if applied.checkpoint.is_zero() {
                    String::new()
                } else {
                    format!(", checkpoint {} ms", applied.checkpoint.as_millis())
                },
                if self.is_merging() {
                    ", a merge runs in the background"
                } else {
                    ""
                },
            );
        }
        Ok((
            result,
            Changes {
                quads: pending,
                inner: Some(Arc::clone(&self.inner)),
            },
        ))
    }

    /// Applies a transaction's writes. The caller holds the writer lock.
    fn commit(
        &self,
        pending: &[([u64; 4], bool)],
        may_checkpoint: bool,
        writer: &mut Option<persist::Disk>,
    ) -> Result<Applied, StoreError> {
        let mut applied = Applied::default();
        if pending.is_empty() {
            return Ok(applied);
        }
        self.note_write();
        if self.inner.quads.read_unpoisoned().delta_len() + pending.len() >= COMPACT_AT {
            // A write that fills the delta is folded into the segments with it in one pass,
            // outside the lock so readers aren't held up; only this writer changes quads.
            let mut quads = self.inner.quads.read_unpoisoned().clone();
            quads.spill = writer.as_ref().map(|d| Arc::clone(&d.spill));
            applied.folded = Some(quads.fold_in(pending));
            let generation = self.set_quads(quads);
            self.note_touched(generation, pending);
        } else {
            // In place: the deltas are only copied if a snapshot still holds them.
            let mut quads = self.inner.quads.write_unpoisoned();
            for &(q, present) in pending {
                if present {
                    quads.insert(q);
                } else {
                    quads.remove(q);
                }
            }
            // Under the lock, so a snapshot's generation is that of its quads.
            let generation = self
                .inner
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            drop(quads);
            self.note_touched(generation, pending);
        }
        // The transaction is durable once it is in the log: a checkpoint that fails now is
        // not its failure. The log stays as it is and the next commit tries again.
        if may_checkpoint
            && let Some(disk) = writer.as_mut()
            && disk.log_size() > disk.checkpoint_at
        {
            let start = std::time::Instant::now();
            if let Err(e) = self.checkpoint_with(disk, false) {
                eprintln!("tripletrail-store: checkpoint failed, keeping the write-ahead log: {e}");
            }
            applied.checkpoint = start.elapsed();
        }
        if may_checkpoint {
            self.merge_in_background(writer);
        }
        Ok(applied)
    }

    /// Starts folding the runs that compactions wrote into the segments, on a thread of its
    /// own (#218), unless one is running: reads merge every run in, so the fewer the better.
    /// The thread builds the new segments one permutation after another, then puts them in
    /// place under the writer lock; runs written meanwhile stay, and another merge starts
    /// for them. The caller holds the writer lock.
    fn merge_in_background(&self, writer: &Option<persist::Disk>) {
        use std::sync::atomic::Ordering::SeqCst;
        merge_in_background(&self.inner, writer, false);
        if writer.is_some()
            && !self.inner.read_only
            && self.inner.quads.read_unpoisoned().has_idle_runs()
            && !self.inner.idle_watch.swap(true, SeqCst)
        {
            watch_idle(&self.inner);
        }
    }

    /// Notes a write, which restarts the wait for idleness (#442). A merge started because
    /// the store was idle goes on: stopping it at every write made a store written once a
    /// minute start its rewrite over and over, without end. It is limited to segments of
    /// [`index::IDLE_MERGE_MAX`] quads, whose merge is short, and only its last moment, under
    /// the writer lock, holds up writes.
    fn note_write(&self) {
        self.inner
            .last_write
            .store(now_ms(), std::sync::atomic::Ordering::SeqCst);
    }

    /// Runs of every permutation, for tests.
    #[doc(hidden)]
    pub fn run_count(&self) -> usize {
        self.inner.quads.read_unpoisoned().run_count()
    }

    /// Sets, for the whole process, how long a store with runs goes without writes before
    /// they are merged (30 s by default). For tests.
    #[doc(hidden)]
    pub fn set_idle_merge_after(after: std::time::Duration) {
        IDLE_MERGE_AFTER.store(
            after.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// Turns merging runs into the segments in the background on or off, waiting for a
    /// merge that is running. For tests that look at the files of a checkpoint.
    #[doc(hidden)]
    pub fn set_background_merges(&self, on: bool) {
        self.inner
            .background_merges
            .store(on, std::sync::atomic::Ordering::SeqCst);
        self.wait_for_merges();
    }

    /// Sets, for the whole process, how much memory a bulk load into a store on disk may use
    /// to sort its quads, in bytes. A load of more quads than fit sorts them on disk (an
    /// external sort) and writes the indexes straight to files, so its memory stays within
    /// the limit apart from the dictionary. No limit by default: every quad is sorted in
    /// memory, which is fastest when they fit.
    pub fn set_memory_limit(bytes: usize) {
        index::set_memory_limit(bytes);
    }

    /// Sets, for the whole process, the number of quads from which a compaction writes runs
    /// rather than rewriting the permutations. For tests of runs on small stores.
    #[doc(hidden)]
    pub fn set_runs_from(quads: usize) {
        index::set_runs_from(quads);
    }

    /// Whether a merge of runs into the segments runs in the background.
    #[doc(hidden)]
    pub fn is_merging(&self) -> bool {
        self.inner.merging.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Waits until no merge runs in the background.
    #[doc(hidden)]
    pub fn wait_for_merges(&self) {
        while self.inner.merging.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Sets the size of the write-ahead log (in bytes) beyond which a commit folds it into a
    /// new checkpoint; 256 MB by default. Does nothing on an in-memory store.
    pub fn set_checkpoint_log_size(&self, bytes: u64) {
        if let Some(disk) = self.inner.writer.lock_unpoisoned().as_mut() {
            disk.checkpoint_at = bytes;
        }
    }

    pub fn insert<'a>(&self, quad: impl Into<QuadRef<'a>>) -> Result<bool, StoreError> {
        let quad = quad.into();
        self.transaction(|t| Ok(t.insert(quad)))
    }

    pub fn remove<'a>(&self, quad: impl Into<QuadRef<'a>>) -> Result<bool, StoreError> {
        let quad = quad.into();
        self.transaction(|t| Ok(t.remove(quad)))
    }

    /// Adds quads in one transaction.
    pub fn extend(&self, quads: impl IntoIterator<Item = Quad>) -> Result<(), StoreError> {
        self.transaction(|t| {
            for q in quads {
                t.insert(q.as_ref());
            }
            Ok(())
        })
    }

    /// Keeps graph `into` holding the RDFS entailment of the other graphs (#187), or stops
    /// (`None`). From then on every transaction brings it up to date before it commits, in
    /// the same commit: added data by deriving from the added triples only, while the schema
    /// (`rdfs:subClassOf`, `rdfs:subPropertyOf`, `rdfs:domain`, `rdfs:range`) stays as it
    /// was; anything else (a removal, a schema change) by entailing again
    /// ([`Transaction::entail_rdfs`]), which writes only the difference. A bulk load entails
    /// again after it. Writes into `into` itself are not kept: it is the reasoner's. Setting it
    /// does not entail; run [`Transaction::entail_rdfs`] once.
    pub fn set_rdfs_entailment(&self, into: Option<NamedNode>) {
        self.set_entailment(into, entail::Regime::Rdfs);
    }

    /// [`Store::set_rdfs_entailment`] under a regime. Under OWL 2 RL, a transaction that only
    /// adds triples is entailed from them; one that removes any, or adds schema (a triple
    /// with an RDFS or OWL term as predicate, or as the object of `rdf:type`), or adds
    /// `owl:sameAs` or a value of a functional or key property, entails again.
    pub fn set_entailment(&self, into: Option<NamedNode>, regime: entail::Regime) {
        *self.inner.rdfs_into.write_unpoisoned() = into.map(|g| (g, regime));
    }

    /// The graph the store keeps its entailment in, if any (see
    /// [`Store::set_rdfs_entailment`] and [`Store::set_entailment`]).
    pub fn rdfs_entailment(&self) -> Option<NamedNode> {
        self.kept_entailment().map(|(g, _)| g)
    }

    /// The graph the store keeps its entailment in and the rules, if any.
    pub fn kept_entailment(&self) -> Option<(NamedNode, entail::Regime)> {
        self.inner.rdfs_into.read_unpoisoned().clone()
    }

    /// After a bulk load: entails again when the store keeps an entailment.
    fn entail_after_load(&self) -> Result<(), StoreError> {
        if let Some((into, regime)) = self.kept_entailment() {
            self.transaction(|t| {
                t.entail(into.as_ref(), regime);
                Ok::<_, StoreError>(())
            })?;
        }
        Ok(())
    }

    /// Removes every quad.
    pub fn clear(&self) -> Result<(), StoreError> {
        let quads: Vec<Quad> = self.iter().collect();
        self.transaction(|t| {
            for q in &quads {
                t.remove(q.as_ref());
            }
            Ok(())
        })
    }

    /// Removes the quads of one graph.
    pub fn clear_graph<'a>(&self, graph: impl Into<GraphNameRef<'a>>) -> Result<(), StoreError> {
        let quads: Vec<Quad> = self
            .quads_for_pattern(None, None, None, Some(graph.into()))
            .collect();
        self.transaction(|t| {
            for q in &quads {
                t.remove(q.as_ref());
            }
            Ok(())
        })
    }

    pub fn named_graphs(&self) -> Vec<NamedOrBlankNode> {
        let snapshot = self.snapshot();
        snapshot
            .quads
            .graph_ids()
            .into_iter()
            .filter_map(|g| match snapshot.term(TermId(g)) {
                Ok(Term::NamedNode(n)) => Some(n.into()),
                Ok(Term::BlankNode(b)) => Some(b.into()),
                _ => None,
            })
            .collect()
    }

    /// Loads an RDF document. N-Triples and N-Quads are read in blocks of 64 MB, each parsed on
    /// every core. The data is
    /// added in bulk: one rebuild of the indexes, not one write per quad, and on disk a new
    /// checkpoint rather than a log entry. Not atomic: on a parse error, nothing is added.
    /// A load of at most 100,000 quads into a store that already holds data is applied like a
    /// transaction instead (one log entry on disk), which is much cheaper than a rebuild then.
    pub fn bulk_load(&self, format: RdfFormat, reader: impl Read) -> Result<usize, StoreError> {
        self.bulk_load_with(RdfParser::from_format(format), reader)
    }

    /// [`Store::bulk_load`] with a configured parser (base IRI, default graph...). Blank node
    /// labels are kept as written: see [`Store::bulk_load_with_blank_nodes`].
    pub fn bulk_load_with(
        &self,
        parser: RdfParser,
        reader: impl Read,
    ) -> Result<usize, StoreError> {
        self.bulk_load_with_blank_nodes(parser, reader, BlankNodes::Keep)
    }

    /// [`Store::bulk_load_with`], with the document's blank nodes its own or not (#840).
    /// Scoping is done here and not with [`RdfParser::rename_blank_nodes`]: N-Triples and
    /// N-Quads are parsed in parts, and each part would rename on its own.
    pub fn bulk_load_with_blank_nodes(
        &self,
        parser: RdfParser,
        mut reader: impl Read,
        blank_nodes: BlankNodes,
    ) -> Result<usize, StoreError> {
        let scope = match blank_nodes {
            BlankNodes::PerLoad => Some(labels::Scope::new()),
            BlankNodes::Keep => None,
        };
        let scope = scope.as_ref();
        let mut writer = self.lock_writer()?;
        let format = parser.format();
        let threads = rayon::current_num_threads();
        let sorter = Sorter::new(writer.as_ref().map(|d| Arc::clone(&d.spill)));
        let ids = if matches!(format, RdfFormat::NTriples | RdfFormat::NQuads) && threads > 1 {
            // Read a block at a time, so the text and the parsed terms of only a few blocks
            // are in memory at once. The parts of a block are parsed on every core, each into
            // a batch of dictionary keys, while another thread adds the keys of the blocks
            // parsed so far to the dictionary.
            self.intern_pipelined(sorter, |tx| {
                // The block grows as text comes in: a body of a few hundred bytes made a
                // zero-filled block of 64 MB, 40 ms under the writer lock (#702).
                let mut block: Vec<u8> = Vec::new();
                loop {
                    let start = block.len();
                    let size = PARSE_BLOCK.max(start * 2);
                    // A first piece, then, if there is more, room for the whole block at once.
                    let first = (64 << 10).min(size - start);
                    (&mut reader).take(first as u64).read_to_end(&mut block)?;
                    if block.len() == start + first {
                        block.reserve_exact(size - block.len());
                        (&mut reader)
                            .take((size - block.len()) as u64)
                            .read_to_end(&mut block)?;
                    }
                    let done = block.len() < size;
                    // Parse up to the last line end; the rest starts the next block.
                    let end = if done {
                        block.len()
                    } else {
                        match block.iter().rposition(|&b| b == b'\n') {
                            Some(i) => i + 1,
                            None => continue, // one line longer than the block: read more
                        }
                    };
                    let batches = parser
                        .clone()
                        .split_slice_for_parallel_parsing(&block[..end], threads * 4)
                        .into_par_iter()
                        .map(|p| {
                            let mut batch = KeyBatch::default();
                            let mut key = Vec::new();
                            for q in p {
                                batch.push(q?.as_ref(), scope, &mut key);
                            }
                            Ok(batch)
                        })
                        .collect::<Result<Vec<KeyBatch>, RdfParseError>>()?;
                    if tx.send(batches).is_err() {
                        return Ok(());
                    }
                    if done {
                        return Ok(());
                    }
                    block.drain(..end);
                }
            })?
        } else {
            // Turtle, RDF/XML...: this thread parses and writes each term's dictionary key,
            // while another adds the keys of the quads parsed so far to the dictionary. (The
            // quads are dropped on the thread that made them: freeing them on the other one is
            // slower than the whole hand-over gains.)
            self.intern_pipelined(sorter, |tx| {
                let mut batch = KeyBatch::default();
                let mut key = Vec::new();
                for q in parser.for_reader(reader) {
                    batch.push(q?.as_ref(), scope, &mut key);
                    if (batch.terms.len() >= 4 * PARSE_BATCH || batch.bytes.len() >= PARSE_BLOCK)
                        && tx.send(vec![std::mem::take(&mut batch)]).is_err()
                    {
                        return Ok(());
                    }
                }
                if !batch.terms.is_empty() {
                    let _ = tx.send(vec![batch]);
                }
                Ok(())
            })?
        };
        let added = self.bulk_finish(ids, &mut writer)?;
        drop(writer);
        self.entail_after_load()?;
        Ok(added)
    }

    /// Runs `produce`, which sends batches of parsed terms (a few at a time), while another thread adds their
    /// keys to the dictionary; returns the ids of the quads, in order.
    fn intern_pipelined(
        &self,
        mut ids: Sorter,
        produce: impl FnOnce(&std::sync::mpsc::SyncSender<Vec<KeyBatch>>) -> Result<(), StoreError>,
    ) -> Result<Sorter, StoreError> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<KeyBatch>>(2);
        let (produced, ids) = std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                for batch in rx.into_iter().flatten() {
                    let mut dict = self.inner.dict.write_unpoisoned();
                    let mut start = 0;
                    let mut id = |t: &BatchTerm| match *t {
                        BatchTerm::Id(id) => id,
                        BatchTerm::Key(kind, end) => {
                            let key = &batch.bytes[start..end as usize];
                            start = end as usize;
                            TermId::new(kind, u64::from(dict.get_or_insert(key).0)).0
                        }
                    };
                    for q in batch.terms.chunks_exact(4) {
                        ids.push([id(&q[0]), id(&q[1]), id(&q[2]), id(&q[3])]);
                    }
                    map_dict_tail(&mut dict, ids.spill());
                }
                ids
            });
            let produced = produce(&tx);
            drop(tx);
            (
                produced,
                worker.join().expect("the interning thread does not panic"),
            )
        });
        produced?;
        Ok(ids)
    }

    /// The ids of the quads of `batches`, adding their new terms to the dictionary: the terms
    /// of a batch are looked up on every core, then the new ones added in order.
    fn intern_batches(
        &self,
        batches: impl IntoIterator<Item = Vec<Quad>>,
        mut sorter: Sorter,
    ) -> Sorter {
        let mut ids: Vec<[u64; 4]> = Vec::new();
        for batch in batches {
            ids.clear();
            self.intern_into(&batch, &mut ids);
            for &q in &ids {
                sorter.push(q);
            }
            map_dict_tail(&mut self.inner.dict.write_unpoisoned(), sorter.spill());
        }
        sorter
    }

    /// Runs `f`, whose parallel iterators do lookups, on [`lookup_pool`] when the store maps
    /// checkpoint files, so that the cold reads of a store just opened wait side by side
    /// (#437); otherwise here.
    fn on_lookup_threads<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        if self.mapped_size() > 0 {
            lookup_pool().install(f)
        } else {
            f()
        }
    }

    /// Appends the ids of `batch` to `ids`, adding new terms to the dictionary: the terms are
    /// looked up in parallel (see [`Store::on_lookup_threads`]), then the new ones added in
    /// order.
    fn intern_into(&self, batch: &[Quad], ids: &mut Vec<[u64; 4]>) {
        let slotted: Vec<Vec<[Slot; 4]>> = self.on_lookup_threads(|| {
            let dict = self.inner.dict.read_unpoisoned();
            batch
                .par_chunks(LOOKUP_CHUNK)
                .map(|chunk| {
                    let mut key = Vec::new();
                    chunk
                        .iter()
                        .map(|q| slots(&dict, q.as_ref(), &mut key))
                        .collect()
                })
                .collect()
        });
        let mut dict = self.inner.dict.write_unpoisoned();
        for quad in slotted.into_iter().flatten() {
            ids.push(quad.map(|slot| match slot {
                Slot::Id(id) => id,
                Slot::New(kind, key) => TermId::new(kind, u64::from(dict.get_or_insert(&key).0)).0,
            }));
        }
    }

    /// Adds quads in bulk, like [`Store::bulk_load`].
    pub fn bulk_load_quads(&self, quads: impl IntoIterator<Item = Quad>) -> Result<(), StoreError> {
        let mut writer = self.lock_writer()?;
        let mut sorter = Sorter::new(writer.as_ref().map(|d| Arc::clone(&d.spill)));
        let mut interner = Interner::new(self);
        for q in quads {
            sorter.push(interner.quad(q.as_ref()));
        }
        drop(interner);
        self.bulk_finish(sorter, &mut writer)?;
        drop(writer);
        self.entail_after_load()?;
        Ok(())
    }

    /// Adds quads in bulk, a batch at a time: the terms of a batch are looked up in the
    /// dictionary in parallel, as N-Triples are by [`Store::bulk_load_with`].
    pub fn bulk_load_quad_batches(
        &self,
        batches: impl IntoIterator<Item = Vec<Quad>>,
    ) -> Result<usize, StoreError> {
        let mut writer = self.lock_writer()?;
        let sorter = Sorter::new(writer.as_ref().map(|d| Arc::clone(&d.spill)));
        let ids = self.intern_batches(batches, sorter);
        let added = self.bulk_finish(ids, &mut writer)?;
        drop(writer);
        self.entail_after_load()?;
        Ok(added)
    }

    /// Rebuilds the indexes with `ids` added. The caller holds the writer lock.
    ///
    /// A small load into a store that holds data is applied like a transaction instead: the
    /// quads go to the deltas and, on disk, to the write-ahead log, rather than rebuilding
    /// every permutation and writing a whole checkpoint for a few triples.
    fn bulk_finish(
        &self,
        sorter: Sorter,
        writer: &mut Option<persist::Disk>,
    ) -> Result<usize, StoreError> {
        let added = sorter.len();
        // A load that goes into a checkpoint rather than the log waits for the check of a
        // large store's files (#436) before it changes anything, so that a load refused for
        // damage leaves the store as it was.
        if writer.is_some() && (sorter.spilled() || added > SMALL_LOAD || self.is_empty()) {
            self.wait_verified()?;
        }
        self.note_write();
        if sorter.spilled() {
            // More quads than the memory limit lets us sort at once: every permutation is
            // rebuilt by an external sort, with the store's quads merged in.
            let mut current = self.inner.quads.read_unpoisoned().clone();
            current.spill = writer.as_ref().map(|d| Arc::clone(&d.spill));
            current.compact();
            let index = QuadIndex::from_sorter(sorter, Some(&current));
            drop(current);
            self.set_quads(index);
            if let Some(disk) = writer.as_mut() {
                disk.mark_load()?;
                self.checkpoint_with(disk, true)?;
            }
            return Ok(added);
        }
        let mut ids = sorter.into_vec();
        if added <= SMALL_LOAD && !self.is_empty() {
            ids.sort_unstable();
            ids.dedup();
            let snapshot = self.snapshot();
            ids.retain(|q| !snapshot.quads.contains(q));
            drop(snapshot);
            let pending: Vec<([u64; 4], bool)> = ids.into_iter().map(|q| (q, true)).collect();
            if let Some(disk) = writer.as_mut()
                && !pending.is_empty()
            {
                let dict = self.inner.dict.read_unpoisoned();
                disk.append(&pending, |q| decode_quad(&dict, q))?;
            }
            self.commit(&pending, true, writer)?;
            return Ok(added);
        }
        let mut current = self.inner.quads.read_unpoisoned().clone();
        current.spill = writer.as_ref().map(|d| Arc::clone(&d.spill));
        current.compact();
        // A load small next to the store goes into a run of each permutation (#218).
        if current.merges_in(added) {
            ids.par_sort_unstable();
            ids.dedup();
            ids.retain(|q| !current.contains(q));
            current.merge_in(&ids);
            self.set_quads(current);
            if let Some(disk) = writer.as_mut() {
                disk.mark_load()?;
                self.checkpoint_with(disk, true)?;
            }
            self.merge_in_background(writer);
            return Ok(added);
        }
        if current.len() > 0 {
            ids.extend(current.iter());
        }
        let index = QuadIndex::from_quads(ids);
        self.set_quads(index);
        if let Some(disk) = writer.as_mut() {
            disk.mark_load()?;
            self.checkpoint_with(disk, true)?;
        }
        Ok(added)
    }

    /// The parsed geometry of a geometry literal in the store, parsed on first use.
    pub fn geometry(&self, term: TermRef<'_>) -> Option<Arc<Geom>> {
        let id = match term {
            // The usual case, without copying the text into a key.
            TermRef::Literal(l)
                if l.language().is_none() && dict::is_geometry_datatype(l.datatype().as_str()) =>
            {
                let mut prefix = Vec::with_capacity(l.datatype().as_str().len() + 2);
                prefix.push(Kind::Geometry as u8);
                prefix.extend_from_slice(l.datatype().as_str().as_bytes());
                prefix.push(0);
                let o = self
                    .inner
                    .dict
                    .read_unpoisoned()
                    .get_split(&prefix, l.value().as_bytes())?;
                TermId::new(Kind::Geometry, u64::from(o))
            }
            _ => term_id(&self.inner.dict.read_unpoisoned(), term)?,
        };
        self.inner.geometry(id)
    }

    /// The parsed geometry of the geometry literal with id `id`, parsed (or decoded from the
    /// checkpoint) on first use.
    pub fn geometry_of(&self, id: TermId) -> Option<Arc<Geom>> {
        self.inner.geometry(id)
    }

    /// The id of a term in the store, if it is stored (or inline), without a snapshot.
    pub fn term_id(&self, term: TermRef<'_>) -> Option<TermId> {
        term_id(&self.inner.dict.read_unpoisoned(), term)
    }

    /// The literal of a dictionary id (a geometry literal, say), without a snapshot.
    pub fn dictionary_term(&self, id: TermId) -> Option<Term> {
        if !id.is_dictionary() {
            return dict::inline_term(id);
        }
        let dict = self.inner.dict.read_unpoisoned();
        let o = id.payload();
        (o != 0 && o as usize <= dict.len()).then(|| dict.term(o as u32))
    }

    /// The CRS and bounding box (`[min x, min y, max x, max y]`) of the geometry literal with
    /// id `id`; `Some((crs, None))` for an empty geometry, `None` for one that is not valid.
    /// Read from the box the checkpoint saved for a larger geometry, or from its saved record,
    /// when the geometry was not used yet, so nothing is decoded; otherwise from the parsed
    /// geometry, which is parsed if need be.
    pub fn geometry_bbox(&self, id: TermId) -> Option<(tripletrail_geo::Crs, Option<[f64; 4]>)> {
        use geo::BoundingRect;
        if id.kind() != Kind::Geometry {
            return None;
        }
        let o = id.payload() as u32;
        let parsed = match self.inner.geometries.peek(o) {
            Some(g) => g,
            // The box saved with the checkpoint: no record read (#754).
            None if let Some(b) = self.inner.geometries.saved_bbox(o) => return Some(b),
            None => match self.inner.geometries.saved_record(o).and_then(geobin::bbox) {
                Some(b) => return Some(b),
                None if let Some(b) = self
                    .inner
                    .dict
                    .read_unpoisoned()
                    .geometry_record(o)
                    .and_then(geobin::bbox) =>
                {
                    return Some(b);
                }
                // Not saved (a 3D geometry, say): parsed for its box only, and not kept, so
                // building a spatial index does not fill the cache with every geometry.
                None => {
                    let term = self.inner.dict.read_unpoisoned().term(o);
                    parse_wkt_literal_uncached(&term)
                }
            },
        }?;
        let b = parsed
            .geometry
            .bounding_rect()
            .map(|r| [r.min().x, r.min().y, r.max().x, r.max().y]);
        Some((parsed.crs.clone(), b))
    }

    /// Writes a new checkpoint and empties the write-ahead log. No-op for in-memory stores,
    /// and while a large store still checks its files after it opened (see
    /// [`Store::verification`]): the log is kept then. Fails on a damaged store.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let mut writer = self.inner.writer.lock_unpoisoned();
        if let Some(disk) = writer.as_mut() {
            self.checkpoint_with(disk, false)?;
            self.merge_in_background(&writer);
        }
        Ok(())
    }

    /// [`Store::checkpoint`], only when a transaction was committed to the log since the last
    /// checkpoint: a server that stops without having written anything leaves its
    /// checkpoint as it is (#440).
    pub fn checkpoint_if_changed(&self) -> Result<(), StoreError> {
        let mut writer = self.inner.writer.lock_unpoisoned();
        if let Some(disk) = writer.as_mut()
            && disk.transactions > 0
        {
            self.checkpoint_with(disk, false)?;
            self.merge_in_background(&writer);
        }
        Ok(())
    }

    /// Archives each write-ahead log into `dir` when a checkpoint retires it (#222), so that
    /// [`Store::restore`] can replay them onto a base backup ([`Store::save_base`]). `None`
    /// stops archiving. No-op for in-memory and read-only stores.
    pub fn set_wal_archive(&self, dir: Option<PathBuf>) -> Result<(), StoreError> {
        if let Some(dir) = &dir {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(disk) = self.inner.writer.lock_unpoisoned().as_mut() {
            // Each timeline has its own directory: the logs of an earlier incarnation of the
            // dataset (deleted and created again, or restored) are never overwritten.
            disk.archive = match dir {
                Some(dir) => {
                    let dir = dir.join(&disk.timeline);
                    std::fs::create_dir_all(&dir)?;
                    Some(dir)
                }
                None => None,
            };
            if let Some(shipping) = &self.inner.shipping {
                *shipping.archive.lock_unpoisoned() = disk.archive.clone();
            }
        }
        Ok(())
    }

    /// Where the write-ahead log stands (#217): the store's timeline, the generation of the
    /// log being written, and its length known to be on disk. `None` for in-memory and
    /// read-only stores. A follower that has applied a log up to this length holds every
    /// durable commit.
    pub fn log_position(&self) -> Option<LogPosition> {
        let shipping = self.inner.shipping.as_ref()?;
        let (generation, length) = shipping.position();
        Some(LogPosition {
            timeline: shipping.timeline.clone(),
            generation,
            length,
        })
    }

    /// Up to `max` bytes of the write-ahead log of `generation` from `offset` (#217), for a
    /// follower to apply with [`scan_log`]: the log being written only as far as it is on
    /// disk, a retired one (left in the store directory by [`Store::set_log_retention`], or
    /// archived by [`Store::set_wal_archive`]) whole. The bytes may end inside an entry when
    /// cut by `max`.
    pub fn read_log(
        &self,
        generation: u64,
        offset: u64,
        max: usize,
    ) -> Result<LogRead, StoreError> {
        let Some(shipping) = &self.inner.shipping else {
            return Err(StoreError::Unsupported(
                "only a writable on-disk store has a write-ahead log".to_owned(),
            ));
        };
        Ok(match shipping.read(generation, offset, max)? {
            persist::Shipped::Gone => LogRead::Gone,
            persist::Shipped::Read(bytes, end, retired) => LogRead::Bytes {
                bytes,
                end,
                retired,
            },
        })
    }

    /// Where a base backup ([`Store::save_base`]) starts in the chain of write-ahead logs:
    /// the timeline of the store it was taken of, the generation whose log continues it, and
    /// how many transactions of that log it holds already. `None` without a `BASE` file.
    pub fn base_position(path: impl AsRef<Path>) -> Result<Option<(String, u64, u64)>, StoreError> {
        Ok(persist::read_base(path.as_ref())?.map(|b| (b.timeline, b.generation, b.transactions)))
    }

    /// How many retired write-ahead logs a checkpoint leaves in the store directory (#217),
    /// newest first, for followers to read to their end; none by default. No-op for in-memory
    /// and read-only stores.
    pub fn set_log_retention(&self, logs: u64) {
        if let Some(disk) = self.inner.writer.lock_unpoisoned().as_mut() {
            disk.keep_logs = logs;
        }
    }

    /// Deletes the archived logs (in `archive`, as [`Store::set_wal_archive`] keeps them, a
    /// directory per timeline) that no base backup in `bases` needs: in each timeline, the
    /// logs older than the oldest base of that timeline, and every log of a timeline with no
    /// base. Directories in `bases` without a `BASE` file are ignored. Returns how many logs
    /// were deleted.
    pub fn prune_wal_archive(
        archive: impl AsRef<Path>,
        bases: &[PathBuf],
    ) -> Result<usize, StoreError> {
        let mut oldest: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for base in bases {
            if let Some(b) = persist::read_base(base)? {
                let g = oldest.entry(b.timeline).or_insert(b.generation);
                *g = (*g).min(b.generation);
            }
        }
        let entries = match std::fs::read_dir(archive.as_ref()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut removed = 0;
        for timeline in entries.flatten() {
            if !timeline.file_type()?.is_dir() {
                continue;
            }
            let keep_from = timeline
                .file_name()
                .to_str()
                .and_then(|t| oldest.get(t).copied())
                .unwrap_or(u64::MAX);
            for log in std::fs::read_dir(timeline.path())?.flatten() {
                let generation = log.file_name().to_str().and_then(|n| {
                    n.strip_prefix("wal-")?
                        .strip_suffix(".log")?
                        .parse::<u64>()
                        .ok()
                });
                if generation.is_some_and(|g| g < keep_from) {
                    std::fs::remove_file(log.path())?;
                    removed += 1;
                }
            }
            if keep_from == u64::MAX {
                // Empty now, unless something else was put there.
                let _ = std::fs::remove_dir(timeline.path());
            }
        }
        Ok(removed)
    }

    /// Copies the store directory `from` (a closed store, such as a store backup) to the new
    /// directory `to`: every file copied and synced, under a temporary name renamed at the
    /// end. The copy leaves out the lock and backup files, and starts a timeline of its own.
    pub fn copy_dir(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), StoreError> {
        // A newer TripleTrail's backup is refused before it is copied (#221).
        persist::check_format(from.as_ref(), false)?;
        persist::copy_store_dir(from.as_ref(), to.as_ref())
    }

    /// [`Store::save_to`], also recording where the copy starts in the chain of write-ahead
    /// logs (a `BASE` file: the generation and the transactions of its log already in the
    /// copy), so that [`Store::restore`] can replay the logs written since onto it. On an
    /// in-memory store it is a plain [`Store::save_to`].
    pub fn save_base(&self, path: impl AsRef<Path>) -> Result<(), StoreError> {
        // A backup of data not checked yet would sum it anew (#436).
        self.wait_verified()?;
        let writer = self.inner.writer.lock_unpoisoned();
        let position = writer.as_ref().map(persist::Disk::position);
        self.save_locked(path.as_ref())?;
        if let Some(position) = position {
            persist::write_base(path.as_ref(), &position)?;
        }
        Ok(())
    }

    /// Builds a new store at `target` from a base backup (a directory [`Store::save_base`]
    /// wrote) and the write-ahead logs written since, found in `logs` (archive directories,
    /// or the live store's own directory for its current log), in order. Transactions
    /// committed after `until` are left out; without it, every logged transaction is
    /// replayed; the replay stops at the first transaction committed after `until`, so if the
    /// clock stepped back, later commits with earlier times are left out too. `target` must
    /// not exist. Fails where a bulk load (which goes into a checkpoint, not the log) lies
    /// before the cut-off, as the logs cannot replay it, and where a log is missing from the
    /// chain while a later one exists.
    pub fn restore(
        base: impl AsRef<Path>,
        logs: &[PathBuf],
        until: Option<std::time::SystemTime>,
        target: impl AsRef<Path>,
    ) -> Result<Restored, StoreError> {
        let (base, target) = (base.as_ref(), target.as_ref());
        // Before its logs are scanned: a newer TripleTrail's entries would read as a torn end.
        persist::check_format(base, false)?;
        let Some(persist::Base {
            timeline,
            mut generation,
            transactions: skip,
            time: base_time,
        }) = persist::read_base(base)?
        else {
            return Err(StoreError::Unsupported(format!(
                "{} is not a base backup (no BASE file): take one with a store backup of an on-disk dataset",
                base.display()
            )));
        };
        if target.exists() {
            return Err(StoreError::Unsupported(format!(
                "{} exists already",
                target.display()
            )));
        }
        let until = until.map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64)
        });
        if until.is_some_and(|u| u < base_time) {
            return Err(StoreError::Unsupported(format!(
                "{} was taken after that time: restore from an older base backup",
                base.display()
            )));
        }
        // Only the logs of the base's timeline: an archive directory's subdirectory for it, a
        // live store directory of that timeline, or a directory named after it.
        let logs: Vec<PathBuf> = logs
            .iter()
            .flat_map(|d| {
                let sub = d.join(&timeline);
                let own = persist::timeline_of(d).map_or_else(
                    || d.file_name().is_some_and(|n| *n == *timeline),
                    |t| t == timeline,
                );
                [sub.is_dir().then_some(sub), own.then(|| d.clone())]
            })
            .flatten()
            .collect();
        let logs = logs.as_slice();
        persist::copy_store_dir(base, target)?;
        // Removes the half-built target when the restore stops with an error.
        struct Cleanup<'a>(Option<&'a Path>);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                if let Some(target) = self.0 {
                    let _ = std::fs::remove_dir_all(target);
                }
            }
        }
        let mut cleanup = Cleanup(Some(target));
        let store = Store::open(target)?;
        let mut restored = Restored::default();
        let mut skip = skip;
        'chain: loop {
            let name = persist::wal_file_name(generation);
            let Some(log) = logs.iter().map(|d| d.join(&name)).find(|p| p.is_file()) else {
                if let Some(later) = persist::later_log(logs, generation) {
                    return Err(StoreError::Unsupported(format!(
                        "{name} is missing but {} exists: the archive has a gap",
                        persist::wal_file_name(later)
                    )));
                }
                break;
            };
            let mut failed = None;
            let mut stop = false;
            persist::scan_log(&log, |entry| {
                match entry {
                    persist::LogEntry::Transaction(time, changes) => {
                        if skip > 0 {
                            skip -= 1;
                            return true;
                        }
                        if let (Some(until), Some(time)) = (until, time)
                            && time > until
                        {
                            stop = true;
                            return false;
                        }
                        let applied = store.transaction(|t| {
                            for (insert, quad) in &changes {
                                if *insert {
                                    t.insert(quad.as_ref());
                                } else {
                                    t.remove(quad.as_ref());
                                }
                            }
                            Ok::<_, StoreError>(())
                        });
                        if let Err(e) = applied {
                            failed = Some(e);
                            return false;
                        }
                        restored.transactions += 1;
                        restored.last_commit = time.or(restored.last_commit);
                    }
                    persist::LogEntry::Load(time) => {
                        if until.is_some_and(|u| time.is_some_and(|t| t > u)) {
                            stop = true;
                            return false;
                        }
                        failed = Some(StoreError::Unsupported(format!(
                            "a bulk load{} went into a checkpoint, not the write-ahead log: \
                             restore from a base backup taken after it, or to a time before it",
                            time.map(|t| format!(" at {t} ms")).unwrap_or_default()
                        )));
                        return false;
                    }
                }
                true
            })?;
            if let Some(e) = failed {
                drop(store);
                return Err(e);
            }
            restored.logs += 1;
            if stop {
                break 'chain;
            }
            generation += 1;
            skip = 0;
        }
        store.checkpoint()?;
        restored.quads = store.len();
        cleanup.0 = None;
        Ok(restored)
    }

    /// Writes the store, as it is now, to a new store directory at `path` that
    /// [`Store::open`] and [`Store::open_read_only`] open without loading anything: one
    /// checkpoint (with the parsed geometries in binary form) and an empty log, synced to disk.
    /// It is written under a temporary name next to `path` and renamed at the end, so `path`
    /// never holds a half-written store; `path` must not exist, or be an empty directory.
    ///
    /// Writes wait until it is done, so it holds exactly the transactions committed when it
    /// started. Works for in-memory and on-disk stores.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), StoreError> {
        self.wait_verified()?;
        let _writer = self.inner.writer.lock_unpoisoned();
        self.save_locked(path.as_ref())
    }

    /// [`Store::save_to`], with the writer lock held by the caller.
    fn save_locked(&self, path: &Path) -> Result<(), StoreError> {
        let mut quads = self.inner.quads.read_unpoisoned().clone();
        quads.compact();
        let dict = self.inner.dict.read_unpoisoned();
        let encode = |o: u32, out: &mut Vec<u8>| self.inner.encode_geometry(&dict, o, out);
        persist::save(path, &dict, &quads, &encode, self.render_wkt())
    }

    /// Writes a checkpoint. While the files of a large store are still checked after it
    /// opened (#436), none is written: it would rewrite data from files not checked yet and
    /// sum it anew, so that damage in it could no longer be found. A commit then keeps its
    /// log, which the next one tries again; with `wait` (a bulk load, which is not in the
    /// log), it waits for the check. Damaged data is never written into a checkpoint.
    fn checkpoint_with(&self, disk: &mut persist::Disk, wait: bool) -> Result<(), StoreError> {
        if wait {
            self.wait_verified()?;
        } else if self.verification() == Verification::Checking {
            return Ok(());
        }
        self.inner.verification.damaged()?;
        let started = std::time::Instant::now();
        let mut quads = self.inner.quads.read_unpoisoned().clone();
        quads.spill = Some(Arc::clone(&disk.spill));
        quads.compact();
        *self.inner.quads.write_unpoisoned() = quads.clone();
        {
            let dict = self.inner.dict.read_unpoisoned();
            let encode = |o: u32, out: &mut Vec<u8>| self.inner.encode_geometry(&dict, o, out);
            disk.checkpoint(&dict, &quads, &encode, self.render_wkt())?;
        }
        // The data just written takes the place of its copies on the heap: mapped, it can
        // stay in the page cache or leave it (#218). The writer lock keeps it unchanged.
        match disk.mapped() {
            Ok((pieces, segments)) => {
                let mut dict = self.inner.dict.write_unpoisoned();
                dict.replace_saved(pieces);
                dict.note_compact(self.render_wkt());
                self.inner.quads.write_unpoisoned().use_saved(segments);
            }
            Err(e) => eprintln!("tripletrail-store: checkpoint written but not mapped: {e}"),
        }
        use std::sync::atomic::Ordering::Relaxed;
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let c = &self.inner.write_counts;
        c.checkpoints.fetch_add(1, Relaxed);
        c.checkpoint_nanos.fetch_add(nanos, Relaxed);
        c.last_checkpoint_nanos.store(nanos, Relaxed);
        Ok(())
    }
}

/// See [`Store::merge_in_background`].
/// During a bulk load with a memory limit, moves the dictionary keys added on the heap to a
/// mapped file once they take an eighth of the limit, so the keys of a load
/// larger than memory stay in the page cache rather than on the heap (#218).
fn map_dict_tail(dict: &mut Dictionary, spill: Option<&Arc<persist::Spill>>) {
    let (Some(limit), Some(spill)) = (index::memory_limit(), spill) else {
        return;
    };
    if dict.tail_bytes() < (limit / 8).max(4096) {
        return;
    }
    if let Err(e) = dict.map_tail(|bytes, ends| spill.map_keys(bytes, ends)) {
        eprintln!("tripletrail-store: keeping new dictionary keys on the heap: {e}");
    }
}

/// Starts a merge in the background (see [`Store::merge_in_background`]). With `idle`, of
/// the runs of segments up to 256M quads that hold 1/1024 of them (#442). The caller holds the writer lock.
fn merge_in_background(inner: &Arc<Inner>, writer: &Option<persist::Disk>, idle: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    if inner.read_only
        || inner.closed.load(SeqCst)
        || !inner.background_merges.load(SeqCst)
        || !(if idle {
            inner.quads.read_unpoisoned().has_idle_runs()
        } else {
            inner.quads.read_unpoisoned().has_unmerged_runs()
        })
        || inner.merging.swap(true, SeqCst)
    {
        return;
    }
    let mut index = if idle {
        inner.quads.write_unpoisoned().start_merge_idle()
    } else {
        inner.quads.write_unpoisoned().start_merge()
    };
    index.spill = writer.as_ref().map(|d| Arc::clone(&d.spill));
    let weak = Arc::downgrade(inner);
    let stop = Arc::clone(&inner.closed);
    let spawned = std::thread::Builder::new()
        .name("tripletrail-merge".into())
        .spawn(move || {
            // `None` when stopped: what was written is removed with `index`.
            let merged = index.merge_runs(&stop);
            drop(index);
            // Flushed here, off the write path: the checkpoint that links the new segments
            // would otherwise flush them under the writer lock.
            for m in merged.iter().flatten() {
                m.sync();
            }
            let Some(inner) = weak.upgrade() else { return };
            let writer = inner.writer.lock_unpoisoned();
            inner
                .quads
                .write_unpoisoned()
                .finish_merge(merged.unwrap_or_default());
            inner.merging.store(false, SeqCst);
            merge_in_background(&inner, &writer, false);
        });
    match spawned {
        Ok(thread) => {
            let mut threads = inner.merge_threads.lock_unpoisoned();
            threads.retain(|t| !t.is_finished());
            threads.push(thread);
        }
        Err(_) => {
            inner.quads.write_unpoisoned().finish_merge(Vec::new());
            inner.merging.store(false, SeqCst);
        }
    }
}

/// Starts the thread that merges the runs a store has left once it has gone
/// [`IDLE_MERGE_AFTER`] without writes (#442): a read-mostly phase after a batch of updates
/// then reads the segments alone. Such a merge goes on through later writes (see
/// [`Store::note_write`]). The thread ends when the store closes.
fn watch_idle(inner: &Arc<Inner>) {
    use std::sync::atomic::Ordering::SeqCst;
    let weak = Arc::downgrade(inner);
    let closed = Arc::clone(&inner.closed);
    let spawned = std::thread::Builder::new()
        .name("tripletrail-idle".into())
        .spawn(move || {
            loop {
                // In short steps, so that a store closes without waiting for this thread.
                let wait = IDLE_MERGE_AFTER.load(SeqCst).clamp(10, 1000);
                for _ in 0..wait.div_ceil(20) {
                    std::thread::sleep(std::time::Duration::from_millis(wait.min(20)));
                    if closed.load(SeqCst) {
                        return;
                    }
                }
                let Some(inner) = weak.upgrade() else { return };
                if inner.merging.load(SeqCst)
                    || now_ms().saturating_sub(inner.last_write.load(SeqCst))
                        < IDLE_MERGE_AFTER.load(SeqCst)
                {
                    continue;
                }
                // Under the writer lock, so no write comes between the check and the start.
                let Ok(writer) = inner.writer.try_lock() else {
                    continue;
                };
                if !inner.quads.read_unpoisoned().has_idle_runs() {
                    // Nothing left: the next write that leaves runs starts a new watch.
                    inner.idle_watch.store(false, SeqCst);
                    return;
                }
                merge_in_background(&inner, &writer, true);
            }
        });
    match spawned {
        Ok(thread) => {
            let mut threads = inner.merge_threads.lock_unpoisoned();
            threads.retain(|t| !t.is_finished());
            threads.push(thread);
        }
        Err(_) => inner.idle_watch.store(false, SeqCst),
    }
}

impl Inner {
    /// Writes the binary form of the geometry literal `ordinal` of `dict` to `out`, for a
    /// checkpoint: its saved record, or the geometry parsed already, or parsed now; false when
    /// it is not valid. A checkpoint encodes only the geometries of the dictionary pieces it
    /// writes (#218).
    fn encode_geometry(&self, dict: &Dictionary, ordinal: u32, out: &mut Vec<u8>) -> bool {
        if let Some(r) = self
            .geometries
            .saved_record(ordinal)
            .or_else(|| dict.geometry_record(ordinal))
        {
            out.extend_from_slice(r);
            return true;
        }
        let g = match self.geometries.peek(ordinal) {
            Some(Some(g)) => g,
            Some(None) => return false,
            None => match parse_wkt_literal_uncached(&dict.term(ordinal)) {
                Some(g) => g,
                None => return false,
            },
        };
        geobin::encode(&g, out)
    }
}

/// A term of a quad being bulk-loaded: its id, or the key to add to the dictionary.
enum Slot {
    Id(u64),
    New(Kind, Box<[u8]>),
}

/// The slots of a quad's terms (subject, predicate, object, graph), looked up in `dict`.
fn slots(dict: &Dictionary, q: QuadRef<'_>, key: &mut Vec<u8>) -> [Slot; 4] {
    let mut slot = |term: TermRef<'_>| match dict::encode(term, key) {
        Encoded::Inline(id) => Slot::Id(id.0),
        Encoded::Key(kind) => match dict.get(key) {
            Some(o) => Slot::Id(TermId::new(kind, u64::from(o)).0),
            None => Slot::New(kind, key.as_slice().into()),
        },
    };
    let g = match q.graph_name {
        GraphNameRef::DefaultGraph => Slot::Id(0),
        GraphNameRef::NamedNode(n) => slot(n.into()),
        GraphNameRef::BlankNode(b) => slot(b.into()),
    };
    [
        slot(q.subject.into()),
        slot(q.predicate.into()),
        slot(q.object),
        g,
    ]
}

/// The terms of parsed quads, four per quad, with their dictionary keys in one buffer.
#[derive(Default)]
struct KeyBatch {
    bytes: Vec<u8>,
    terms: Vec<BatchTerm>,
}

enum BatchTerm {
    /// An inline id (or the default graph).
    Id(u64),
    /// A key of this kind, ending at this offset in the bytes.
    Key(Kind, u32),
}

impl KeyBatch {
    fn push(&mut self, q: QuadRef<'_>, scope: Option<&labels::Scope>, key: &mut Vec<u8>) {
        if let Some(q) = scope.and_then(|s| s.quad(q)) {
            return self.push(q.as_ref(), None, key);
        }
        let mut term = |t: TermRef<'_>| match dict::encode(t, key) {
            Encoded::Inline(id) => BatchTerm::Id(id.0),
            Encoded::Key(kind) => {
                self.bytes.extend_from_slice(key);
                BatchTerm::Key(kind, self.bytes.len() as u32)
            }
        };
        let s = term(q.subject.into());
        let p = term(q.predicate.into());
        let o = term(q.object);
        let g = match q.graph_name {
            GraphNameRef::DefaultGraph => BatchTerm::Id(0),
            GraphNameRef::NamedNode(n) => term(n.into()),
            GraphNameRef::BlankNode(b) => term(b.into()),
        };
        self.terms.extend([s, p, o, g]);
    }
}

/// Turns terms into ids, adding them to the dictionary.
struct Interner<'a> {
    store: &'a Store,
    key: Vec<u8>,
}

impl<'a> Interner<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            key: Vec::new(),
        }
    }

    fn term(&mut self, dict: &mut Dictionary, term: TermRef<'_>) -> u64 {
        match dict::encode(term, &mut self.key) {
            Encoded::Inline(id) => id.0,
            Encoded::Key(kind) => {
                let (o, _) = dict.get_or_insert(&self.key);
                TermId::new(kind, u64::from(o)).0
            }
        }
    }

    fn quad(&mut self, q: QuadRef<'_>) -> [u64; 4] {
        let mut dict = self.store.inner.dict.write_unpoisoned();
        let s = self.term(&mut dict, q.subject.into());
        let p = self.term(&mut dict, q.predicate.into());
        let o = self.term(&mut dict, q.object);
        let g = match q.graph_name {
            GraphNameRef::DefaultGraph => 0,
            GraphNameRef::NamedNode(n) => self.term(&mut dict, n.into()),
            GraphNameRef::BlankNode(b) => self.term(&mut dict, b.into()),
        };
        [s, p, o, g]
    }
}

/// What a committed transaction did: the quads it added (`true`) and removed (`false`), each
/// once and in no particular order. Writes that changed nothing (adding a quad already there,
/// or adding and then removing one) are left out. Kept as term ids and decoded on demand, so
/// that a large write does not hold its quads as text (#382).
pub struct Changes {
    quads: Vec<([u64; 4], bool)>,
    inner: Option<Arc<Inner>>,
}

impl Changes {
    /// How many quads were added or removed.
    pub fn len(&self) -> usize {
        self.quads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }

    /// The changes, decoded a few thousand at a time (the dictionary is not locked between).
    pub fn iter(&self) -> impl Iterator<Item = (bool, Quad)> + '_ {
        self.quads.chunks(4096).flat_map(move |chunk| {
            let decoded: Vec<(bool, Quad)> = match &self.inner {
                Some(inner) => {
                    let dict = inner.dict.read_unpoisoned();
                    chunk
                        .iter()
                        .map(|(q, added)| (*added, decode_quad(&dict, *q)))
                        .collect()
                }
                None => Vec::new(),
            };
            decoded
        })
    }

    /// The predicates of the changes, each once.
    pub fn predicates(&self) -> Vec<NamedNode> {
        self.predicate_terms().into_values().collect()
    }

    /// The changes that `keep` selects, decoded, in order: `keep` sees the predicate (decoded
    /// once per distinct predicate) and the object's id, so the other changes are never
    /// decoded. An update that writes a million quads but few geometries reads its
    /// geometries this way in milliseconds instead of decoding every quad (#292).
    pub fn decoded_where(
        &self,
        keep: impl Fn(&NamedNode, TermId) -> bool + Sync,
    ) -> Vec<(bool, Quad)> {
        let Some(inner) = &self.inner else {
            return Vec::new();
        };
        let predicates = self.predicate_terms();
        let selected: Vec<&([u64; 4], bool)> = self
            .quads
            .par_iter()
            .filter(|(q, _)| predicates.get(&q[1]).is_some_and(|p| keep(p, TermId(q[2]))))
            .collect();
        // A few thousand at a time: the dictionary is not locked between.
        selected
            .chunks(4096)
            .flat_map(|chunk| {
                let dict = inner.dict.read_unpoisoned();
                chunk
                    .iter()
                    .map(|(q, added)| (*added, decode_quad(&dict, *q)))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// How many quads each graph gained and lost, in graph order (the default graph first):
    /// each graph is decoded once, the quads never are (#207: the audit log).
    pub fn per_graph(&self) -> Vec<GraphChanges> {
        let mut counts: FxHashMap<u64, (u64, u64)> = FxHashMap::default();
        for (q, added) in &self.quads {
            let c = counts.entry(q[3]).or_default();
            if *added {
                c.0 += 1;
            } else {
                c.1 += 1;
            }
        }
        let Some(inner) = &self.inner else {
            return Vec::new();
        };
        let dict = inner.dict.read_unpoisoned();
        let mut graphs: Vec<GraphChanges> = counts
            .into_iter()
            .map(|(g, (added, removed))| GraphChanges {
                graph: match g {
                    0 => GraphName::DefaultGraph,
                    g => match decode_term(&dict, g) {
                        Term::NamedNode(n) => n.into(),
                        Term::BlankNode(b) => b.into(),
                        t => unreachable!("literal graph name {t}"),
                    },
                },
                added,
                removed,
            })
            .collect();
        graphs.sort_by(|a, b| match (&a.graph, &b.graph) {
            (GraphName::DefaultGraph, GraphName::DefaultGraph) => std::cmp::Ordering::Equal,
            (GraphName::DefaultGraph, _) => std::cmp::Ordering::Less,
            (_, GraphName::DefaultGraph) => std::cmp::Ordering::Greater,
            (a, b) => a.to_string().cmp(&b.to_string()),
        });
        graphs
    }

    /// The predicate of each predicate id of the changes.
    fn predicate_terms(&self) -> FxHashMap<u64, NamedNode> {
        let Some(inner) = &self.inner else {
            return FxHashMap::default();
        };
        let ids: rustc_hash::FxHashSet<u64> = self.quads.iter().map(|(q, _)| q[1]).collect();
        let dict = inner.dict.read_unpoisoned();
        ids.into_iter()
            .filter_map(|id| match decode_term(&dict, id) {
                Term::NamedNode(n) => Some((id, n)),
                _ => None,
            })
            .collect()
    }
}

/// What a write did to one graph: see [`Changes::per_graph`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphChanges {
    pub graph: GraphName,
    /// Quads added.
    pub added: u64,
    /// Quads removed.
    pub removed: u64,
}

impl std::fmt::Debug for Changes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The term of a stored id.
fn decode_term(dict: &Dictionary, id: u64) -> Term {
    let id = TermId(id);
    dict::inline_term(id).unwrap_or_else(|| dict.term(id.payload() as u32))
}

/// The quad of stored ids (graph id 0: the default graph).
fn decode_quad(dict: &Dictionary, q: [u64; 4]) -> Quad {
    let subject = match decode_term(dict, q[0]) {
        Term::NamedNode(n) => NamedOrBlankNode::from(n),
        Term::BlankNode(b) => b.into(),
        t => unreachable!("literal subject {t}"),
    };
    let Term::NamedNode(predicate) = decode_term(dict, q[1]) else {
        unreachable!("predicate is not an IRI")
    };
    let graph_name = match q[3] {
        0 => GraphName::DefaultGraph,
        g => match decode_term(dict, g) {
            Term::NamedNode(n) => n.into(),
            Term::BlankNode(b) => b.into(),
            t => unreachable!("literal graph name {t}"),
        },
    };
    Quad::new(subject, predicate, decode_term(dict, q[2]), graph_name)
}

/// Writes of one [`Store::transaction`].
pub struct Transaction<'a> {
    interner: Interner<'a>,
    /// The quads when the transaction started.
    base: QuadIndex,
    /// What this transaction changed: quads it added (`true`, not in `base`) or removed
    /// (`false`, in `base`). A quad added and then removed again (or the other way round) is
    /// not in it, so it holds each net change once (#382).
    pending: FxHashMap<[u64; 4], bool>,
    /// [`Transaction::entail_rdfs`] ran into the graph the store keeps its entailment in: it
    /// needs no maintenance.
    entailed: bool,
    /// Under row rules (#202): every quad the transaction is asked to add is judged by this
    /// filter, whether or not it is there already, and the first it hides is kept in
    /// `refused`, so adding an existing hidden quad is refused like any other.
    judge: Option<Arc<RowFilter>>,
    refused: Option<[u64; 4]>,
}

impl<'a> Transaction<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            base: store.inner.quads.read_unpoisoned().clone(),
            interner: Interner::new(store),
            pending: FxHashMap::default(),
            entailed: false,
            judge: None,
            refused: None,
        }
    }

    /// Judges every quad this transaction is asked to add from now on by `filter` (see
    /// [`Transaction::refused`]).
    pub fn judge_adds(&mut self, filter: Arc<RowFilter>) {
        self.judge = Some(filter);
    }

    /// The first quad asked to be added that the filter of [`Transaction::judge_adds`] hides.
    pub fn refused(&self) -> Option<[u64; 4]> {
        self.refused
    }

    fn judge(&mut self, q: &[u64; 4]) {
        if self.refused.is_none() && self.judge.as_ref().is_some_and(|f| f.hides(q)) {
            self.refused = Some(*q);
        }
    }

    fn store(&self) -> &'a Store {
        self.interner.store
    }

    fn finish(self) -> FxHashMap<[u64; 4], bool> {
        self.pending
    }

    fn has(&self, q: &[u64; 4]) -> bool {
        self.pending
            .get(q)
            .copied()
            .unwrap_or_else(|| self.base.contains(q))
    }

    /// Adds a quad; returns false if it was already there.
    pub fn insert<'b>(&mut self, quad: impl Into<QuadRef<'b>>) -> bool {
        let ids = self.interner.quad(quad.into());
        self.insert_ids(ids)
    }

    /// Records the quad of `ids` as added; returns false if it was already there.
    fn insert_ids(&mut self, ids: [u64; 4]) -> bool {
        self.judge(&ids);
        match self.pending.get(&ids) {
            Some(true) => false,
            // Removed by this transaction, so in `base`: back as it was.
            Some(false) => {
                self.pending.remove(&ids);
                true
            }
            None if self.base.contains(&ids) => false,
            None => {
                self.pending.insert(ids, true);
                true
            }
        }
    }

    /// Adds quads, as [`Transaction::insert`] does one by one, but with their terms looked up
    /// and their presence checked on every core; returns how many were not there yet.
    pub fn insert_all(&mut self, quads: Vec<Quad>) -> usize {
        use rayon::prelude::*;
        // A few quads one by one, here: handing them to other threads costs more than their
        // lookups, even cold ones (a single-quad `INSERT DATA` took 25% longer, #757).
        if quads.len() < SMALL_WRITE {
            return quads.iter().filter(|q| self.insert(q.as_ref())).count();
        }
        let mut ids = Vec::with_capacity(quads.len());
        self.store().intern_into(&quads, &mut ids);
        drop(quads);
        let present: Vec<bool> = self.store().on_lookup_threads(|| {
            ids.par_iter()
                .with_min_len(LOOKUP_CHUNK)
                .map(|q| self.has(q))
                .collect()
        });
        let mut added = 0;
        for (q, present) in ids.into_iter().zip(present) {
            self.judge(&q);
            // A quad given twice is in `pending` the second time.
            if !present && self.insert_ids(q) {
                added += 1;
            }
        }
        added
    }

    /// Makes graph `into` hold what the RDFS rules derive from the other graphs, as this
    /// transaction sees them ([`entail::rdfs`], #187): the triples no longer derived leave it,
    /// the new ones come in.
    pub fn entail_rdfs(&mut self, into: NamedNodeRef<'_>) -> entail::Entailed {
        self.entail(into, entail::Regime::Rdfs)
    }

    /// [`Transaction::entail_rdfs`] under a regime.
    pub fn entail(&mut self, into: NamedNodeRef<'_>, regime: entail::Regime) -> entail::Entailed {
        self.entail_until(into, regime, &|| false)
            .expect("never cancelled")
    }

    /// [`Transaction::entail`], asking `cancelled` as it goes (#647): `None`, with the
    /// transaction's writes as they were, once it says so.
    pub fn entail_until(
        &mut self,
        into: NamedNodeRef<'_>,
        regime: entail::Regime,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<entail::Entailed> {
        // Into the graph the store keeps its entailment in: no maintenance needed after.
        // Into another, what lands there is data to the kept entailment.
        let keeps = self
            .store()
            .kept_entailment()
            .is_some_and(|(g, r)| g.as_ref() == into && r == regime);
        let g = {
            let store = self.store();
            let mut dict = store.inner.dict.write_unpoisoned();
            // The RDFS terms too, even if no quad holds them yet: a term the store lacks gets
            // no id and the rules skip it, so a kept entailment that first meets rdf:type in
            // a write would miss what the earlier data implies with it.
            for t in entail::VOCABULARY {
                self.interner.term(&mut dict, t.into());
            }
            self.interner.term(&mut dict, into.into())
        };
        let view = self.view();
        let derived = entail::entail_until(&view, Some(TermId(g)), regime, cancelled)?;
        // The graph's quads come sorted by (s, p, o) like `derived`: one merge gives what
        // leaves and what comes in, which `pending` takes as is (each is known to be in the
        // graph or not).
        let mut old = view
            .quad_ids_for_pattern(None, None, None, GraphSelector::Graph(TermId(g)))
            .map(|[s, p, o, _]| [s.0, p.0, o.0])
            .peekable();
        let mut changes: Vec<([u64; 4], bool)> = Vec::new();
        for (i, t) in derived.iter().enumerate() {
            if i % 65_536 == 65_535 && cancelled() {
                return None;
            }
            let t = [t[0].0, t[1].0, t[2].0];
            while let Some(gone) = old.next_if(|o| *o < t) {
                changes.push(([gone[0], gone[1], gone[2], g], false));
            }
            if old.next_if_eq(&t).is_none() {
                changes.push(([t[0], t[1], t[2], g], true));
            }
        }
        changes.extend(old.map(|q| ([q[0], q[1], q[2], g], false)));
        drop(view);
        let added = changes.iter().filter(|c| c.1).count();
        let out = entail::Entailed {
            triples: derived.len(),
            added,
            removed: changes.len() - added,
        };
        // Tens of millions of changes take seconds to note: asked between, and on a stop the
        // transaction's writes are put back as they were (they are mostly none: what it held
        // before, kept only then).
        let before: Option<FxHashMap<[u64; 4], bool>> =
            (!self.pending.is_empty()).then(|| self.pending.clone());
        for (i, (ids, present)) in changes.into_iter().enumerate() {
            if i % 65_536 == 65_535 && cancelled() {
                self.pending = before.unwrap_or_default();
                return None;
            }
            // Back to what the transaction started with: no change.
            if self.pending.get(&ids) == Some(&!present) && self.base.contains(&ids) == present {
                self.pending.remove(&ids);
            } else {
                self.pending.insert(ids, present);
            }
        }
        // Last asked here: the commit that follows, once begun, finishes (seconds for tens of
        // millions of triples).
        if cancelled() {
            self.pending = before.unwrap_or_default();
            return None;
        }
        // Set only now: a stopped entailment leaves the kept one to maintain.
        self.entailed |= keeps;
        Some(out)
    }

    /// Brings graph `into`, which held the entailment of the store when the transaction
    /// began, up to date with this transaction's writes (see [`Store::set_rdfs_entailment`]).
    fn maintain(&mut self, into: NamedNodeRef<'_>, regime: entail::Regime) {
        if self.pending.is_empty() {
            return;
        }
        let Some(g) = term_id(&self.store().inner.dict.read_unpoisoned(), into.into()) else {
            // Never entailed: entail.
            return self.full_entail(into, regime);
        };
        let g = g.0;
        let mut added: Vec<[TermId; 3]> = Vec::new();
        let mut removed: Vec<[TermId; 3]> = Vec::new();
        for (&[s, p, o, qg], &present) in &self.pending {
            if qg == g {
                // A write into the graph itself, which is the reasoner's: entailing again
                // undoes it.
                return self.full_entail(into, regime);
            }
            let t = [TermId(s), TermId(p), TermId(o)];
            if present {
                added.push(t);
            } else {
                removed.push(t);
            }
        }
        added.sort_unstable();
        added.dedup();
        removed.sort_unstable();
        removed.dedup();
        let view = self.view();
        let change = match regime {
            entail::Regime::Rdfs => entail::rdfs_changed(&view, TermId(g), &added, &removed),
            // Additions only: OWL 2 RL has no delete and rederive here.
            entail::Regime::OwlRl if removed.is_empty() => {
                entail::owl_rl_added(&view, &added).map(|insert| entail::Change {
                    insert,
                    remove: Vec::new(),
                })
            }
            entail::Regime::OwlRl => None,
        };
        drop(view);
        let Some(change) = change else {
            return self.full_entail(into, regime);
        };
        // Asserted now: no longer the reasoner's.
        for [s, p, o] in added {
            let q = [s.0, p.0, o.0, g];
            if self.has(&q) {
                self.remove_ids(q);
            }
        }
        for [s, p, o] in change.remove {
            self.remove_ids([s.0, p.0, o.0, g]);
        }
        for [s, p, o] in change.insert {
            self.insert_ids([s.0, p.0, o.0, g]);
        }
    }

    fn full_entail(&mut self, into: NamedNodeRef<'_>, regime: entail::Regime) {
        self.entail(into, regime);
    }

    /// Removes a quad by its ids, which is in the store as the transaction sees it.
    fn remove_ids(&mut self, ids: [u64; 4]) {
        if self.pending.remove(&ids).is_none() {
            self.pending.insert(ids, false);
        }
    }

    /// Removes a quad; returns false if it was not there.
    pub fn remove<'b>(&mut self, quad: impl Into<QuadRef<'b>>) -> bool {
        let quad = quad.into();
        let Some(ids) = quad_ids(&self.store().inner.dict.read_unpoisoned(), quad) else {
            return false;
        };
        match self.pending.get(&ids) {
            Some(false) => false,
            // Added by this transaction, so not in `base`: gone again.
            Some(true) => {
                self.pending.remove(&ids);
                true
            }
            None if !self.base.contains(&ids) => false,
            None => {
                self.pending.insert(ids, false);
                true
            }
        }
    }

    /// A read-only view of the store as this transaction sees it so far.
    /// What this transaction changed so far, as ids: each quad added (`true`) or removed
    /// (`false`), once.
    pub fn pending_ids(&self) -> Vec<([u64; 4], bool)> {
        self.pending.iter().map(|(q, a)| (*q, *a)).collect()
    }

    /// Undoes this transaction's change of `q`: a removed quad stays, an added one is not.
    pub fn keep(&mut self, q: [u64; 4]) {
        self.pending.remove(&q);
    }

    /// The data as it was when the transaction started.
    pub fn base_view(&self) -> Snapshot {
        Snapshot {
            inner: Arc::clone(&self.store().inner),
            quads: self.base.clone(),
            ephemeral: Arc::default(),
            all_graphs: false,
            merged_default: MergedDefault::Plain,
            tables: table::pending(),
            rows: None,
            generation: None,
        }
    }

    pub fn view(&self) -> Snapshot {
        let mut quads = self.base.clone();
        for (&q, &present) in &self.pending {
            if present {
                quads.insert(q);
            } else {
                quads.remove(q);
            }
        }
        Snapshot {
            inner: Arc::clone(&self.store().inner),
            quads,
            ephemeral: Arc::default(),
            all_graphs: false,
            merged_default: MergedDefault::Plain,
            tables: table::pending(),
            rows: None,
            generation: None,
        }
    }
}

fn term_id(dict: &Dictionary, term: TermRef<'_>) -> Option<TermId> {
    let mut key = Vec::new();
    match dict::encode(term, &mut key) {
        Encoded::Inline(id) => Some(id),
        Encoded::Key(kind) => dict.get(&key).map(|o| TermId::new(kind, u64::from(o))),
    }
}

/// Quads a lookup task takes at a time: few, so that a commit of 10,000 quads keeps every
/// thread of [`lookup_pool`] busy.
const LOOKUP_CHUNK: usize = 64;

/// Below this many quads, [`Transaction::insert_all`] inserts them one by one on the calling
/// thread.
const SMALL_WRITE: usize = 4 * LOOKUP_CHUNK;

/// Threads for looking up the terms and quads of a write in a store on disk (#437). On a
/// store just opened, each lookup can wait on a read of the disk, ~1 ms on a network volume,
/// with the CPU idle: on as many threads as cores, a commit of 10,000 new triples into a
/// billion-triple store took a minute. More threads keep more reads waiting at once; once
/// the pages are cached, they cost little more than the cores would.
fn lookup_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("TRIPLETRAIL_LOOKUP_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| (rayon::current_num_threads() * 4).clamp(32, 128));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("tripletrail-lookup-{i}"))
            .build()
            .expect("the lookup threads start")
    })
}

fn quad_ids(dict: &Dictionary, quad: QuadRef<'_>) -> Option<[u64; 4]> {
    let g = match quad.graph_name {
        GraphNameRef::DefaultGraph => 0,
        GraphNameRef::NamedNode(n) => term_id(dict, n.into())?.0,
        GraphNameRef::BlankNode(b) => term_id(dict, b.into())?.0,
    };
    Some([
        term_id(dict, quad.subject.into())?.0,
        term_id(dict, quad.predicate.into())?.0,
        term_id(dict, quad.object)?.0,
        g,
    ])
}

/// Terms a query made up (constants not in the store, computed values), numbered per view.
struct Ephemeral {
    /// The dictionary keys of the terms, one after another.
    keys: Vec<u8>,
    /// Where each term's key ends in `keys`.
    ends: Vec<usize>,
    /// The terms' numbers, by the hash of their key. One copy of each key, and the term made
    /// from it when asked: a query that computes a new term per row (an update's `BIND(IRI(…))`)
    /// holds about its key's length per term (#382).
    ids: hashbrown::HashTable<u64>,
}

impl Ephemeral {
    fn key(&self, i: u64) -> &[u8] {
        let i = i as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.keys[start..self.ends[i]]
    }

    fn hash(key: &[u8]) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(key)
    }

    fn get(&self, key: &[u8]) -> Option<u64> {
        self.ids
            .find(Self::hash(key), |&i| self.key(i) == key)
            .copied()
    }

    /// The number of the term of `key`, given one if it has none yet.
    fn get_or_insert(&mut self, key: &[u8]) -> u64 {
        if let Some(i) = self.get(key) {
            return i;
        }
        let i = self.ends.len() as u64;
        self.keys.extend_from_slice(key);
        self.ends.push(self.keys.len());
        let (keys, ends) = (&self.keys, &self.ends);
        let key_of = |i: u64| {
            let i = i as usize;
            &keys[if i == 0 { 0 } else { ends[i - 1] }..ends[i]]
        };
        self.ids
            .insert_unique(Self::hash(key), i, |&j| Self::hash(key_of(j)));
        i
    }

    fn term(&self, i: u64) -> Option<Term> {
        ((i as usize) < self.ends.len()).then(|| dict::decode(self.key(i)))
    }
}

impl Default for Ephemeral {
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            ends: Vec::new(),
            ids: hashbrown::HashTable::new(),
        }
    }
}

thread_local! {
    /// Buffer for dictionary keys, so looking a term up doesn't allocate.
    static KEY: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A read-only, consistent view of a [`Store`]. Cheap to clone.
#[derive(Clone)]
pub struct Snapshot {
    inner: Arc<Inner>,
    quads: QuadIndex,
    ephemeral: Arc<Mutex<Ephemeral>>,
    /// Whether "any graph" scans read the default graph too, which a query whose default
    /// graph is the union of all graphs needs (see [`Snapshot::with_all_graphs`]).
    all_graphs: bool,
    /// What the default graph is: the store's own, or an RDF merge of graphs (issue #376).
    merged_default: MergedDefault,
    /// The result tables of the query this snapshot evaluates (see [`register_table`]).
    tables: table::Tables,
    /// What this view hides (#202); `None` reads every quad.
    rows: Option<Arc<RowFilter>>,
    /// The store's [`Store::generation`] when the view was taken; `None` for a transaction's
    /// view, which holds writes not committed yet.
    generation: Option<u64>,
}

/// Up to how many graphs a merged default graph is read graph by graph (see [`GraphMerge`]);
/// more are read in one scan of all graphs.
const MERGE_GRAPH_SCANS: usize = 16;

/// The order, as positions in `[s, p, o]`, in which a scan with these positions bound gives
/// its triples: that of the permutation the quad index reads (SPO, POS or OSP, the same
/// with or without a graph).
fn scan_order(s: bool, p: bool, o: bool) -> [usize; 3] {
    match (s, p, o) {
        (false, true, _) => [1, 2, 0],
        (true, false, true) | (false, false, true) => [2, 0, 1],
        _ => [0, 1, 2],
    }
}

/// The triples of several graphs' scans, each in the same order, merged in that order, each
/// once.
struct GraphMerge {
    scans: Vec<std::iter::Peekable<Box<dyn Iterator<Item = [TermId; 4]>>>>,
    order: [usize; 3],
    last: Option<[TermId; 3]>,
}

impl Iterator for GraphMerge {
    type Item = [TermId; 3];

    fn next(&mut self) -> Option<[TermId; 3]> {
        let [a, b, c] = self.order;
        loop {
            let (i, _) = self
                .scans
                .iter_mut()
                .enumerate()
                .filter_map(|(i, scan)| scan.peek().map(|q| (i, [q[a], q[b], q[c]])))
                .min_by_key(|(_, key)| *key)?;
            let q = self.scans[i].next()?;
            let triple = [q[0], q[1], q[2]];
            if self.last != Some(triple) {
                self.last = Some(triple);
                return Some(triple);
            }
        }
    }
}

/// The default graph of a query as an RDF merge of graphs (issue #376): each triple once, as
/// Fuseki's union default graph and `FROM` with several graphs give it. SPARQL evaluation
/// asked for the quads of each graph and got a triple once per graph that holds it.
#[derive(Clone, Debug, Default)]
pub enum MergedDefault {
    /// The store's default graph.
    #[default]
    Plain,
    /// The union of all graphs, the default graph included.
    All,
    /// The merge of these graphs (the default graph is id 0), in id order.
    Graphs(Arc<[TermId]>),
}

/// What [`Snapshot::dataset_counts`] returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatasetCounts {
    /// Quads in all graphs.
    pub triples: u64,
    pub distinct_subjects: u64,
    pub distinct_predicates: u64,
    pub distinct_objects: u64,
    /// Distinct IRI subjects.
    pub entities: u64,
    /// Per predicate, in id order; `None` with more than 4,096 predicates.
    pub predicates: Option<Vec<PredicateCounts>>,
    /// Per graph (the default graph is id 0), in id order; `None` with more than 4,096.
    pub graphs: Option<Vec<GraphCounts>>,
    /// Per object of `rdf:type`, in id order.
    pub classes: Vec<ClassCounts>,
}

/// The quads of one predicate, over all graphs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredicateCounts {
    pub predicate: TermId,
    pub triples: u64,
    pub distinct_subjects: u64,
    pub distinct_objects: u64,
}

/// The quads of one graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCounts {
    /// `TermId(0)` for the default graph.
    pub graph: TermId,
    pub triples: u64,
    pub distinct_subjects: u64,
    pub distinct_predicates: u64,
    pub distinct_objects: u64,
}

/// The distinct subjects typed with one class, over all graphs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassCounts {
    pub class: TermId,
    pub instances: u64,
}

/// A [`Snapshot`] with the dictionary locked for reading, for many lookups in a row without
/// taking the lock (or allocating a key buffer) for each. Writers wait while it lives: keep it
/// for a batch of lookups, not longer. Threads may share it (#891): it takes no lock of its own.
pub struct SnapshotReader<'a> {
    snapshot: &'a Snapshot,
    dict: std::sync::RwLockReadGuard<'a, Dictionary>,
}

thread_local! {
    /// The key buffer of [`SnapshotReader::term_id`], one per thread.
    static READER_KEY: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl SnapshotReader<'_> {
    /// Whether a language string of the dictionary has language tag `tag` (as `LANG` gives
    /// it, without a base direction), from its key: the term is not decoded. `None` for any
    /// other id.
    pub fn language_is(&self, id: TermId, tag: &str) -> Option<bool> {
        if id.kind() != Kind::LangString || !id.is_dictionary() {
            return None;
        }
        let o = id.payload();
        if o == 0 || o as usize > self.dict.len() {
            return None;
        }
        // The kind byte, the tag (with `--direction`), a 0, the value.
        let key = &self.dict.raw_key(o as u32)[1..];
        let zero = key.iter().position(|&b| b == 0)?;
        let language = &key[..zero];
        let language = language
            .windows(2)
            .position(|w| w == b"--")
            .map_or(language, |at| &language[..at]);
        Some(language == tag.as_bytes())
    }

    /// The value of a string or language string of the dictionary, read from its key: the
    /// term is not decoded. `None` for any other id (an inline string included).
    pub fn string_value(&self, id: TermId) -> Option<&str> {
        if !matches!(id.kind(), Kind::String | Kind::LangString) || !id.is_dictionary() {
            return None;
        }
        let o = id.payload();
        if o == 0 || o as usize > self.dict.len() {
            return None;
        }
        let key = self.dict.raw_key(o as u32);
        let (value, _) = ranks::sort_key(key).or_else(|| {
            // A directional language string: the value after the 0.
            let zero = key[1..].iter().position(|&b| b == 0)?;
            Some((&key[zero + 2..], &[][..]))
        })?;
        std::str::from_utf8(value).ok()
    }

    /// The id of a term stored in this store, if any.
    pub fn term_id(&self, term: TermRef<'_>) -> Option<TermId> {
        READER_KEY.with_borrow_mut(|key| match dict::encode(term, key) {
            Encoded::Inline(id) => Some(id),
            Encoded::Key(kind) => self.dict.get(key).map(|o| TermId::new(kind, u64::from(o))),
        })
    }

    /// The term an id stands for.
    pub fn term(&self, id: TermId) -> Result<Term, StoreError> {
        if let Some(t) = dict::inline_term(id) {
            return Ok(t);
        }
        if id.is_dictionary() {
            let o = id.payload();
            if o == 0 || o as usize > self.dict.len() {
                return Err(StoreError::Corrupt(format!("unknown term {id:?}")));
            }
            return Ok(self.dict.term(o as u32));
        }
        self.snapshot.term(id)
    }

    /// See [`Snapshot::quad_ids_for_pattern`].
    pub fn quad_ids_for_pattern(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphSelector,
    ) -> impl Iterator<Item = [TermId; 4]> + use<> {
        self.snapshot.quad_ids_for_pattern(s, p, o, graph)
    }
}

impl Snapshot {
    /// A filter hiding the quads that repeat a triple already in a graph of a lower id (the
    /// default graph first): this view with it and [`GraphSelector::All`] reads the union of
    /// all graphs as an RDF merge, each triple once, with the scans on ids (#808). `Some(None)`
    /// when no triple is in two graphs.
    ///
    /// Found by one scan of all quads, then kept up to date after each commit from the subjects
    /// it touched ([`Store::touched_between`]), so that a write costs a scan of what it changed.
    /// The scan of the whole store is only made when `scan` says it is worth it; without it, and
    /// when the commits since the last result are not known, `None`. Also `None` for a
    /// transaction's view, once `cancelled` says so, or past [`MAX_UNION_COPIES`] copies.
    pub fn union_copies(
        &self,
        scan: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Option<Arc<RowFilter>>> {
        let generation = self.generation?;
        // Held throughout: queries that arrive together wait for one scan.
        let mut cached = self.inner.union_copies.lock_unpoisoned();
        let mut filter = match &*cached {
            Some((g, found)) if *g == generation => return Some(found.clone()),
            Some((g, found)) if *g < generation => {
                match Store::touched_between_inner(&self.inner, *g, generation) {
                    Some((resources, _)) => {
                        let mut f = found.as_deref().cloned().unwrap_or_default();
                        f.forget(&resources);
                        Some((f, resources))
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let found = match filter.take() {
            // The subjects the commits touched, judged again from all their quads.
            Some((mut f, resources)) => {
                for &s in &resources {
                    if cancelled() {
                        return None;
                    }
                    let mut scan = self.quads.scan(Pattern::of_ids(
                        Some(TermId(s)),
                        None,
                        None,
                        GraphSelector::All,
                    ));
                    Self::hide_copies(&mut scan, &mut f, cancelled)?;
                }
                f
            }
            None if scan => {
                let mut f = RowFilter::new();
                let mut scan =
                    self.quads
                        .scan(Pattern::of_ids(None, None, None, GraphSelector::All));
                Self::hide_copies(&mut scan, &mut f, cancelled)?;
                f
            }
            None => return None,
        };
        if found.hidden_quads() > MAX_UNION_COPIES {
            return None;
        }
        let found = (!found.is_empty()).then(|| Arc::new(found));
        *cached = Some((generation, found.clone()));
        Some(found)
    }

    /// What reading the RDF merge of `graphs` (the default graph is id 0) as the union of
    /// all graphs hides: every quad of another graph, and the copies of a triple in those
    /// graphs after the first that holds it, as [`Snapshot::union_copies`] does for all
    /// graphs (#204). Made from the copies over all graphs: only a triple held by several
    /// graphs can have copies within some of them. `None` as for [`Snapshot::union_copies`].
    pub fn copies_within(
        &self,
        graphs: &[TermId],
        scan: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Arc<RowFilter>> {
        let all = self.union_copies(scan, cancelled)?;
        let shown: rustc_hash::FxHashSet<u64> = graphs.iter().map(|g| g.0).collect();
        let mut filter = RowFilter::new();
        filter.show_only_graphs(shown.iter().copied());
        if let Some(all) = all {
            let mut triples: Vec<[u64; 3]> = all.quad_list().map(|q| [q[0], q[1], q[2]]).collect();
            triples.sort_unstable();
            triples.dedup();
            for [s, p, o] in triples {
                if cancelled() {
                    return None;
                }
                // In SPOG order: the lowest graph first.
                let mut first = true;
                for q in self.quads.scan(Pattern::of_ids(
                    Some(TermId(s)),
                    Some(TermId(p)),
                    Some(TermId(o)),
                    GraphSelector::All,
                )) {
                    if !shown.contains(&q[3]) {
                        continue;
                    }
                    if first {
                        first = false;
                    } else {
                        filter.hide_quad(q);
                    }
                }
            }
        }
        Some(Arc::new(filter))
    }

    /// Hides in `filter` every quad of `scan` (in SPOG order) whose triple the quad before it
    /// has: the copies of a triple are next to each other, the lowest graph first.
    fn hide_copies(
        scan: &mut index::Scan,
        filter: &mut RowFilter,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<()> {
        let mut last: Option<[u64; 3]> = None;
        let mut chunk = Vec::new();
        loop {
            chunk.clear();
            if !scan.next_chunk(&mut chunk) {
                return Some(());
            }
            if cancelled() || filter.hidden_quads() > MAX_UNION_COPIES {
                return None;
            }
            for q in &chunk {
                let triple = [q[0], q[1], q[2]];
                if last == Some(triple) {
                    filter.hide_quad(*q);
                } else {
                    last = Some(triple);
                }
            }
        }
    }

    /// Whether any quad is in a named graph (one seek). Without, the union of all graphs is
    /// the default graph.
    pub fn has_named_graphs(&self) -> bool {
        self.quads.has_named_graphs()
    }

    /// Ranks of the stored IRIs, blank nodes and `xsd:string` literals in the order of their
    /// texts (issue #171): comparing ranks of one kind compares the terms. Built on first
    /// use (one sort of those terms, holding the dictionary's read lock), and built again once
    /// the dictionary has grown by more than an eighth since; terms added in between have no
    /// rank.
    pub fn string_ranks(&self) -> Arc<StringRanks> {
        self.string_ranks_within(|covered| covered / 8)
    }

    /// [`Snapshot::string_ranks`], built again once more than `slack(covered)` terms have no
    /// rank.
    fn string_ranks_within(&self, slack: impl Fn(usize) -> usize) -> Arc<StringRanks> {
        let mut cached = self.inner.ranks.lock_unpoisoned();
        let dict = self.inner.dict.read_unpoisoned();
        if let Some(r) = cached.as_ref() {
            let covered = r.covered() as usize;
            if covered + slack(covered) >= dict.len() {
                return Arc::clone(r);
            }
        }
        let r = Arc::new(StringRanks::build(&dict));
        *cached = Some(Arc::clone(&r));
        r
    }

    /// The ranks (see [`Snapshot::string_ranks`]) of the ranked terms of `kind` whose text (an
    /// IRI, blank node label or string value) starts with `prefix`, for `STRSTARTS` and
    /// `REGEX(?x, "^...")`. Empty for a kind without ranks.
    pub fn string_prefix(
        &self,
        ranks: &StringRanks,
        kind: Kind,
        prefix: &str,
    ) -> std::ops::Range<u32> {
        ranks.prefix(&self.inner.dict.read_unpoisoned(), kind, prefix)
    }

    /// The IRIs starting with `prefix` that are the subject of a quad in any graph, at most
    /// `limit`: from the string ranks, plus the IRIs added since they were built (at most
    /// 65,536 terms, else the ranks are built again), without a scan of the quads. For Linked Data, which serves `IRI#...` with `IRI`.
    pub fn subjects_with_prefix(&self, prefix: &str, limit: usize) -> Vec<NamedNode> {
        // The terms without a rank are read one by one, so keep them few: past this, the
        // ranks are built again (once, for every later request).
        const UNRANKED: usize = 65_536;
        let ranks = self.string_ranks_within(|covered| (covered / 8).min(UNRANKED));
        let mut ids = Vec::new();
        {
            let dict = self.inner.dict.read_unpoisoned();
            for r in ranks.prefix(&dict, Kind::Iri, prefix) {
                ids.extend(ranks.id(Kind::Iri, r));
            }
            for o in ranks.covered() + 1..=dict.len() as u32 {
                let key = dict.raw_key(o);
                if key.first() == Some(&(Kind::Iri as u8))
                    && key[1..].starts_with(prefix.as_bytes())
                {
                    ids.push(TermId::new(Kind::Iri, u64::from(o)));
                }
            }
        }
        ids.into_iter()
            .filter(|&id| {
                self.quad_ids_for_pattern(Some(id), None, None, GraphSelector::All)
                    .next()
                    .is_some()
            })
            .filter_map(|id| match self.term(id) {
                Ok(Term::NamedNode(n)) => Some(n),
                _ => None,
            })
            .take(limit)
            .collect()
    }

    /// Whether every object of `predicate`, in any graph, is an IRI or a blank node, as the
    /// counts kept with the quad indexes tell (#185): `Some(true)` only when that is certain.
    /// `None` when the counts are not kept per predicate.
    pub fn objects_are_resources(&self, predicate: TermId) -> Option<bool> {
        self.quads.objects_are_resources(predicate.0)
    }

    /// A count over the triple pattern `?s ?p ?o` (three distinct variables) in `graph`, from
    /// the counts kept with the quad indexes, without a scan: [`TripleCount::Rows`] is the
    /// number of solutions, [`TripleCount::Distinct`] that of distinct values of a position.
    /// With `kinds = Some((position, kinds))`, only the solutions whose term at `position` is
    /// of one of `kinds` count (`FILTER isLiteral(?o)`); a distinct count then has to be of
    /// that same position.
    ///
    /// Writes not yet compacted are counted in (issue #213). `None` when the counts don't
    /// answer it: the permutation holds more than 4,096 uncompacted writes, the graph selector
    /// is [`GraphSelector::AnyNamed`], or the combination is not kept.
    pub fn count_triples(
        &self,
        graph: GraphSelector,
        count: TripleCount,
        kinds: Option<(usize, &[Kind])>,
    ) -> Option<u64> {
        // The position the answer is read at.
        if self.rows.is_some() {
            return None;
        }
        let at = match (count, kinds) {
            (TripleCount::Distinct(d), Some((k, _))) if d != k => return None,
            (TripleCount::Distinct(d), _) => d,
            (TripleCount::Rows, Some((k, _))) => k,
            (TripleCount::Rows, None) => 0,
        };
        if at > 2 {
            return None;
        }
        let distinct = matches!(count, TripleCount::Distinct(_));
        let sum = |k: &crate::stats::KindCounts| -> u64 {
            let counts = if distinct { &k.distinct } else { &k.rows };
            kinds.map_or(0, |(_, ks)| ks.iter().map(|k| counts[*k as usize]).sum())
        };
        match graph {
            GraphSelector::Default | GraphSelector::Graph(_) => {
                let key = match graph {
                    GraphSelector::Graph(g) => g.0,
                    _ => 0,
                };
                // GSPO, GPOS, GOSP: the graph first, then the position.
                let stats = self.quads.stats(3 + at)?;
                let Some(group) = stats.group(key)? else {
                    return Some(0);
                };
                Some(match (kinds, distinct) {
                    (Some(_), _) => sum(&group.kinds1),
                    (None, true) => group.distinct1,
                    (None, false) => group.rows,
                })
            }
            // SPOG, POSG, OSPG: the position first. Rows are quads: one per graph a triple
            // is in, as the scans of the union of all graphs give them.
            GraphSelector::All => {
                let stats = self.quads.stats(at)?;
                Some(match (kinds, distinct) {
                    (Some(_), _) => sum(&stats.kinds0),
                    (None, true) => stats.distinct[0],
                    (None, false) => stats.rows,
                })
            }
            GraphSelector::AnyNamed => None,
        }
    }

    /// Per predicate of the default graph, in id order: (predicate, quads, distinct subjects,
    /// distinct objects), from the counts kept with the quad indexes, for `?s ?p ?o GROUP BY
    /// ?p` (#243). `None` when they don't answer it: a permutation has writes not yet
    /// compacted, there are more than 4,096 predicates, or a named graph has quads (the
    /// per-predicate counts are kept over all graphs only).
    pub fn predicate_counts(&self) -> Option<Vec<(TermId, u64, u64, u64)>> {
        use crate::index::{GSPO, POSG, PSOG};
        if self.rows.is_some() {
            return None;
        }
        let graphs = self.quads.stats(GSPO)?;
        if graphs.groups.as_ref()?.iter().any(|g| g.key != 0) {
            return None;
        }
        let (by_o, by_s) = (self.quads.stats(POSG)?, self.quads.stats(PSOG)?);
        let (by_o, by_s) = (by_o.groups.as_ref()?, by_s.groups.as_ref()?);
        Some(
            by_o.iter()
                .zip(by_s)
                .map(|(o, s)| (TermId(o.key), o.rows, s.distinct1, o.distinct1))
                .collect(),
        )
    }

    /// This view for a query whose default graph is the union of all graphs: SPARQL
    /// evaluation then asks for the quads of every graph, the default one included. Otherwise
    /// such a request only comes from `GRAPH ?g`, which drops default-graph quads anyway, so
    /// they are not read.
    /// Whether every graph holding a quad is among `graphs` (sorted; the default graph is id 0).
    pub(crate) fn graphs_within(&self, graphs: &[TermId]) -> bool {
        let ids: Vec<u64> = graphs.iter().map(|g| g.0).collect();
        self.quads.graphs_within(&ids)
    }

    /// This view without the quads `rows` hides (#202): every scan of it, and so every query
    /// on it, leaves them out. The counts kept with the quad indexes do not answer for such a
    /// view ([`Snapshot::count_triples`] and the like give `None`), so their callers scan.
    pub fn with_rows(mut self, rows: Option<Arc<RowFilter>>) -> Self {
        self.rows = rows.filter(|r| !r.is_empty());
        self
    }

    /// The [`Store::generation`] of the quads this view reads; `None` for a transaction's view.
    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    /// This view with the result tables registered on this thread so far (see
    /// [`register_table`]): for a snapshot taken before the rewrite that registered them.
    pub fn with_pending_tables(mut self) -> Self {
        self.tables = table::pending();
        self
    }

    /// What this view hides, if anything.
    pub fn rows(&self) -> Option<&Arc<RowFilter>> {
        self.rows.as_ref()
    }

    pub fn with_all_graphs(mut self) -> Self {
        self.all_graphs = true;
        self
    }

    /// This view with its default graph read as an RDF merge (see [`MergedDefault`]): the
    /// quads SPARQL evaluation asks of the default graph are those of the merged graphs, each
    /// triple once.
    pub fn with_merged_default(mut self, merged: MergedDefault) -> Self {
        self.merged_default = merged;
        self
    }

    /// The triples of the merged default graph matching a pattern, each once, as default-graph
    /// quads. Read from a permutation whose last column is the graph, where the copies of a
    /// triple in several graphs are adjacent.
    fn merged_quads(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        unknown: bool,
    ) -> impl Iterator<Item = Result<InternalQuad<TermId>, StoreError>> + use<> {
        let triples = if unknown {
            Either::Left(std::iter::empty())
        } else {
            match &self.merged_default {
                // A few graphs of many, read by predicate or whole: their own scans, merged. A
                // lookup by subject or object (a join's probe) is one seek in all graphs
                // instead, with the other graphs' copies filtered out (#204).
                MergedDefault::Graphs(graphs)
                    if graphs.len() <= MERGE_GRAPH_SCANS && s.is_none() && o.is_none() =>
                {
                    Either::Right(Either::Left(GraphMerge {
                        scans: graphs
                            .iter()
                            .map(|g| {
                                let scan: Box<dyn Iterator<Item = [TermId; 4]>> = Box::new(
                                    self.quad_ids_for_pattern(s, p, o, GraphSelector::Graph(*g)),
                                );
                                scan.peekable()
                            })
                            .collect(),
                        order: scan_order(s.is_some(), p.is_some(), o.is_some()),
                        last: None,
                    }))
                }
                // All graphs in one scan: the graph is the last column of the permutation read,
                // so the copies of a triple are next to each other.
                merged => {
                    let graphs = match merged {
                        MergedDefault::Graphs(g) => Some(Arc::clone(g)),
                        _ => None,
                    };
                    let mut last: Option<[TermId; 3]> = None;
                    Either::Right(Either::Right(
                        self.quad_ids_for_pattern(s, p, o, GraphSelector::All)
                            .filter(move |q| {
                                graphs
                                    .as_ref()
                                    .is_none_or(|g| g.binary_search(&q[3]).is_ok())
                            })
                            .filter_map(move |q| {
                                let triple = [q[0], q[1], q[2]];
                                (last != Some(triple)).then(|| {
                                    last = Some(triple);
                                    triple
                                })
                            }),
                    ))
                }
            }
        };
        triples.map(|[subject, predicate, object]| {
            Ok(InternalQuad {
                subject,
                predicate,
                object,
                graph_name: None,
            })
        })
    }

    /// Locks the dictionary for a batch of lookups (see [`SnapshotReader`]).
    pub fn reader(&self) -> SnapshotReader<'_> {
        SnapshotReader {
            snapshot: self,
            dict: self.inner.dict.read_unpoisoned(),
        }
    }

    /// The quads of this view: under a row filter (#202), those it shows.
    pub fn len(&self) -> usize {
        match &self.rows {
            None => self.quads.len(),
            Some(rows) => self
                .quads
                .scan(Pattern::of_ids(None, None, None, GraphSelector::All))
                .with_rows(Some(rows))
                .count(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.rows {
            None => self.quads.len() == 0,
            Some(rows) => self
                .quads
                .scan(Pattern::of_ids(None, None, None, GraphSelector::All))
                .with_rows(Some(rows))
                .next()
                .is_none(),
        }
    }

    /// The id of a term stored in this store (or a column of this query's result tables), if
    /// any.
    pub fn term_id(&self, term: TermRef<'_>) -> Option<TermId> {
        if !self.tables.is_empty()
            && let TermRef::NamedNode(n) = term
            && let Some(id) = table::column(&self.tables, n.as_str())
        {
            return Some(id);
        }
        term_id(&self.inner.dict.read_unpoisoned(), term)
    }

    /// For each language-tagged, typed or geometry literal of the dictionary among `ids`,
    /// calls `f(i, tag)` with its position and tag: the language (with `--direction`) or the
    /// datatype IRI, as bytes. One lock of the dictionary for all of them; other ids are
    /// skipped. LANG and DATATYPE of a literal read only its kind and tag.
    pub fn literal_tags(
        &self,
        ids: &[TermId],
        f: &mut dyn FnMut(usize, &[u8]),
    ) -> Result<(), StoreError> {
        let dict = self.inner.dict.read_unpoisoned();
        for (i, id) in ids.iter().enumerate() {
            if !matches!(id.kind(), Kind::LangString | Kind::Typed | Kind::Geometry)
                || !id.is_dictionary()
            {
                continue;
            }
            let o = id.payload();
            if o == 0 || o as usize > dict.len() {
                return Err(StoreError::Corrupt(format!("unknown term {id:?}")));
            }
            let key = dict.raw_key(o as u32);
            // A compact key is a WKT literal: its datatype without writing its text.
            if dict::is_compact(key) {
                f(i, tripletrail_geo::WKT_LITERAL.as_str().as_bytes());
                continue;
            }
            let (prefix, _) = dict::split_key(key);
            // The kind byte, the tag, and its zero separator.
            f(i, &prefix[1..prefix.len() - 1]);
        }
        Ok(())
    }

    /// The term an id stands for.
    pub fn term(&self, id: TermId) -> Result<Term, StoreError> {
        if let Some(t) = dict::inline_term(id) {
            return Ok(t);
        }
        match id.kind() {
            Kind::Ephemeral => self
                .ephemeral
                .lock_unpoisoned()
                .term(id.payload())
                .ok_or_else(|| StoreError::Corrupt(format!("unknown term {id:?}"))),
            _ if id.is_dictionary() => {
                let dict = self.inner.dict.read_unpoisoned();
                let o = id.payload();
                if o == 0 || o as usize > dict.len() {
                    return Err(StoreError::Corrupt(format!("unknown term {id:?}")));
                }
                Ok(dict.term(o as u32))
            }
            // Result-table rows and columns are not data; named for completeness only.
            Kind::Row => Ok(Term::BlankNode(oxigraph::model::BlankNode::new_unchecked(
                format!("mgrow{:x}", id.payload()),
            ))),
            Kind::Column => Ok(Term::NamedNode(oxigraph::model::NamedNode::new_unchecked(
                format!(
                    "urn:tripletrail:table:{}:{}",
                    id.payload() >> 16,
                    id.payload() & 0xffff
                ),
            ))),
            _ => Err(StoreError::Corrupt(format!("not a term: {id:?}"))),
        }
    }

    /// The order of two dictionary terms of the same kind: IRIs, blank nodes or simple
    /// literals (SPARQL orders each of these by its text), or language-tagged strings without
    /// a base direction (by value, then language tag). `None` for other ids.
    pub fn cmp_texts(&self, a: TermId, b: TermId) -> Option<std::cmp::Ordering> {
        if a.kind() != b.kind()
            || !matches!(
                a.kind(),
                Kind::Iri | Kind::Blank | Kind::String | Kind::LangString
            )
            || !a.is_dictionary()
            || !b.is_dictionary()
        {
            return None;
        }
        let dict = self.inner.dict.read_unpoisoned();
        let key = |id: TermId| {
            let o = id.payload();
            (o != 0 && o as usize <= dict.len())
                .then(|| ranks::sort_key(dict.raw_key(o as u32)))
                .flatten()
        };
        // Texts are in UTF-8, whose byte order is the order of their characters.
        Some(key(a)?.cmp(&key(b)?))
    }

    /// The parsed geometry of a geometry literal in the store, parsed on first use.
    pub fn geometry(&self, id: TermId) -> Option<Arc<Geom>> {
        self.inner.geometry(id)
    }

    /// The parsed geometry of a geometry literal in the store, without keeping it in the
    /// store's cache (one already there is returned).
    pub fn geometry_transient(&self, id: TermId) -> Option<Arc<Geom>> {
        self.inner.geometry_transient(id)
    }

    fn quad_ids(&self, quad: QuadRef<'_>) -> Option<[u64; 4]> {
        quad_ids(&self.inner.dict.read_unpoisoned(), quad)
    }

    pub fn contains<'a>(&self, quad: impl Into<QuadRef<'a>>) -> bool {
        self.quad_ids(quad.into()).is_some_and(|q| {
            self.quads.contains(&q) && !self.rows.as_ref().is_some_and(|r| r.hides(&q))
        })
    }

    /// Ids of the quads matching a pattern of ids, as `[s, p, o, g]` with `g == 0` for the
    /// default graph. `graph`: see [`GraphSelector`].
    pub fn quad_ids_for_pattern(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphSelector,
    ) -> impl Iterator<Item = [TermId; 4]> + use<> {
        self.quads
            .scan(Pattern::of_ids(s, p, o, graph))
            .with_rows(self.rows.as_ref())
            .map(|q| [TermId(q[0]), TermId(q[1]), TermId(q[2]), TermId(q[3])])
    }

    /// As [`Snapshot::quad_ids_for_pattern`], sorted by position `by` (0 subject, 1
    /// predicate, 2 object) when a quad index gives that order: always for a position after
    /// the bound ones in SPO, POS or OSP order, and for the subject of `?s <p> ?o` (issue
    /// #168). Otherwise in the order of [`Snapshot::quad_ids_for_pattern`].
    pub fn quad_ids_sorted(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphSelector,
        by: usize,
    ) -> impl Iterator<Item = [TermId; 4]> + use<> {
        self.quad_ids_by(s, p, o, graph, Some(by))
    }

    /// How many quads matching a pattern of ids hold each value at position `by` (0 subject,
    /// 1 predicate, 2 object), as `(value, quads)` in id order: `?f a ?c GROUP BY ?c` with
    /// `COUNT(*)`. A value's quads are read until there are [`RUN`] of them; the rest are
    /// counted from the block index (as [`Snapshot::estimate_ids`] does) and skipped by a
    /// seek, so a value with millions of quads costs a few blocks. `None` when no permutation
    /// gives the order, for [`GraphSelector::AnyNamed`] (whose estimates are not exact), or
    /// past `max` values, or once `cancelled`.
    #[allow(clippy::too_many_arguments)]
    pub fn value_counts(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphSelector,
        by: usize,
        max: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<(TermId, u64)>> {
        /// Quads of one value read before its count is taken from the block index.
        const RUN: u64 = 256;
        if by > 2
            || matches!(graph, GraphSelector::AnyNamed)
            || [s, p, o][by].is_some()
            || self.rows.is_some()
        {
            return None;
        }
        let pattern = Pattern::of_ids(s, p, o, graph);
        let mut scan = self.quads.scan_sorted(pattern, Some(by));
        if scan.sorted_by() != Some(by) {
            return None;
        }
        let mut out: Vec<(TermId, u64)> = Vec::new();
        let mut read = 0u64;
        while let Some(q) = scan.next() {
            read += 1;
            if read.is_multiple_of(65_536) && cancelled() {
                return None;
            }
            let v = q[by];
            let full = out.len() >= max;
            match out.last_mut() {
                Some((last, n)) if last.0 == v => {
                    *n += 1;
                    if *n == RUN {
                        let mut bound = [s, p, o];
                        bound[by] = Some(TermId(v));
                        let [s, p, o] = bound;
                        *n = self.quads.estimate(Pattern::of_ids(s, p, o, graph)) as u64;
                        if cancelled() {
                            return None;
                        }
                        match v.checked_add(1) {
                            Some(next) => scan.seek(next),
                            None => break,
                        }
                    }
                }
                Some((last, _)) if last.0 > v => return None,
                _ if full => return None,
                _ => out.push((TermId(v), 1)),
            }
        }
        Some(out)
    }

    /// [`Snapshot::quad_ids_sorted`] with `by: None`: any order.
    pub(crate) fn quad_ids_by(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphSelector,
        by: Option<usize>,
    ) -> impl Iterator<Item = [TermId; 4]> + use<> {
        self.quads
            .scan_sorted(Pattern::of_ids(s, p, o, graph), by)
            .with_rows(self.rows.as_ref())
            .map(|q| [TermId(q[0]), TermId(q[1]), TermId(q[2]), TermId(q[3])])
    }

    /// What a dataset description (VoID) says about the store: totals over all graphs, per
    /// graph and per predicate, from the counts kept with the quad indexes, and the instances
    /// of each class from a read of the `rdf:type` quads. Quads count once per graph they are
    /// in. While writes are not yet folded into the indexes and are under 1 % of them, the
    /// numbers leave them out; past that, the indexes are read. `None` once `cancelled`.
    pub fn dataset_counts(&self, cancelled: &dyn Fn() -> bool) -> Option<DatasetCounts> {
        use crate::index::{GOSP, GPOS, GSPO, OSPG, POSG, PSOG, SPOG};
        if self.rows.is_some() {
            return None;
        }
        let stats = |perm| self.quads.current_stats(perm, cancelled);
        let (spog, posg, ospg) = (stats(SPOG)?, stats(POSG)?, stats(OSPG)?);
        let entities = spog.kinds0.distinct[Kind::Iri as usize];
        let predicates = match (&posg.groups, &stats(PSOG)?.groups) {
            (Some(by_o), Some(by_s)) => Some(
                by_o.iter()
                    .zip(by_s)
                    .map(|(o, s)| PredicateCounts {
                        predicate: TermId(o.key),
                        triples: o.rows,
                        distinct_subjects: s.distinct1,
                        distinct_objects: o.distinct1,
                    })
                    .collect(),
            ),
            _ => None,
        };
        let (gspo, gpos, gosp) = (stats(GSPO)?, stats(GPOS)?, stats(GOSP)?);
        let graphs = match (&gspo.groups, &gpos.groups, &gosp.groups) {
            (Some(s), Some(p), Some(o)) => Some(
                s.iter()
                    .zip(p)
                    .zip(o)
                    .map(|((s, p), o)| GraphCounts {
                        graph: TermId(s.key),
                        triples: s.rows,
                        distinct_subjects: s.distinct1,
                        distinct_predicates: p.distinct1,
                        distinct_objects: o.distinct1,
                    })
                    .collect(),
            ),
            _ => None,
        };
        // Instances per class: distinct subjects per object of `rdf:type`, read in POSG order.
        let mut classes: Vec<ClassCounts> = Vec::new();
        if let Some(rdf_type) = self.term_id(oxigraph::model::vocab::rdf::TYPE.into()) {
            let mut last: Option<(TermId, TermId)> = None;
            for (i, q) in self
                .quad_ids_sorted(None, Some(rdf_type), None, GraphSelector::All, 2)
                .enumerate()
            {
                if i % 65_536 == 0 && cancelled() {
                    return None;
                }
                if last == Some((q[2], q[0])) {
                    continue;
                }
                match classes.last_mut() {
                    Some(c) if c.class == q[2] => c.instances += 1,
                    _ => classes.push(ClassCounts {
                        class: q[2],
                        instances: 1,
                    }),
                }
                last = Some((q[2], q[0]));
            }
        }
        Some(DatasetCounts {
            triples: spog.rows,
            distinct_subjects: spog.distinct[0],
            distinct_predicates: posg.distinct[0],
            distinct_objects: ospg.distinct[0],
            entities,
            predicates,
            graphs,
            classes,
        })
    }

    /// How many quads with predicate `p` one value of the subject (`subject`) or object side
    /// matches on average, weighted by how often values come up (as a join key bound from
    /// the data does), over all graphs: from the counts kept with the quad indexes (#210).
    /// `None` when they don't have it (the predicate is only in uncompacted writes, or there
    /// are more than 4,096 predicates).
    pub fn predicate_fanout(&self, p: TermId, subject: bool) -> Option<u64> {
        self.quads.fanout(p.0, subject)
    }

    /// How many quads match a pattern of ids, from the block index of the quad permutations
    /// rather than by reading the quads (two blocks are read, whatever the count). Exact, except
    /// with [`GraphSelector::AnyNamed`] and a bound term, where default-graph matches are also
    /// counted once there are more than a few hundred. For planning.
    pub fn estimate_ids(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphSelector,
    ) -> usize {
        // A term the view's rows hide everywhere matches nothing (#204): join orders start
        // there, as they would in a store without it.
        if let Some(rows) = &self.rows
            && rows.hides_all(s.map(|t| t.0), p.map(|t| t.0), o.map(|t| t.0))
        {
            return 0;
        }
        if let Some(p) = p.filter(|p| p.kind() == Kind::Column) {
            return table::estimate(&self.tables, p, s.is_some());
        }
        self.quads.estimate(Pattern::of_ids(s, p, o, graph))
    }

    /// How many quads match a pattern, without reading them (see [`Snapshot::estimate_ids`]);
    /// `graph_name: None` is every graph, the default included.
    pub fn estimate_len(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
    ) -> usize {
        let lookup = |t: Option<TermRef<'_>>| match t {
            None => Some(None),
            Some(t) => self.term_id(t).map(Some),
        };
        let graph = match graph_name {
            None => Some(GraphSelector::All),
            Some(GraphNameRef::DefaultGraph) => Some(GraphSelector::Default),
            Some(GraphNameRef::NamedNode(n)) => self.term_id(n.into()).map(GraphSelector::Graph),
            Some(GraphNameRef::BlankNode(b)) => self.term_id(b.into()).map(GraphSelector::Graph),
        };
        match (
            lookup(subject.map(Into::into)),
            lookup(predicate.map(Into::into)),
            lookup(object),
            graph,
        ) {
            (Some(s), Some(p), Some(o), Some(g)) => self.estimate_ids(s, p, o, g),
            _ => 0,
        }
    }

    /// Quads matching a pattern; `graph_name: None` is every graph, the default included.
    pub fn quads_for_pattern(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
    ) -> impl Iterator<Item = Quad> + use<> {
        let lookup = |t: Option<TermRef<'_>>| match t {
            None => Some(None),
            Some(t) => self.term_id(t).map(Some),
        };
        let graph = match graph_name {
            None => Some(GraphSelector::All),
            Some(GraphNameRef::DefaultGraph) => Some(GraphSelector::Default),
            Some(GraphNameRef::NamedNode(n)) => self.term_id(n.into()).map(GraphSelector::Graph),
            Some(GraphNameRef::BlankNode(b)) => self.term_id(b.into()).map(GraphSelector::Graph),
        };
        let ids = match (
            lookup(subject.map(Into::into)),
            lookup(predicate.map(Into::into)),
            lookup(object),
            graph,
        ) {
            (Some(s), Some(p), Some(o), Some(g)) => Some(self.quad_ids_for_pattern(s, p, o, g)),
            _ => None,
        };
        let snapshot = self.clone();
        ids.into_iter()
            .flatten()
            .filter_map(move |q| snapshot.quad(q).ok())
    }

    fn quad(&self, q: [TermId; 4]) -> Result<Quad, StoreError> {
        let subject = match self.term(q[0])? {
            Term::NamedNode(n) => NamedOrBlankNode::from(n),
            Term::BlankNode(b) => b.into(),
            t => return Err(StoreError::Corrupt(format!("literal subject {t}"))),
        };
        let Term::NamedNode(predicate) = self.term(q[1])? else {
            return Err(StoreError::Corrupt("predicate is not an IRI".into()));
        };
        let graph_name = match q[3] {
            TermId::DEFAULT_GRAPH => GraphName::DefaultGraph,
            g => match self.term(g)? {
                Term::NamedNode(n) => n.into(),
                Term::BlankNode(b) => b.into(),
                t => return Err(StoreError::Corrupt(format!("literal graph name {t}"))),
            },
        };
        Ok(Quad::new(subject, predicate, self.term(q[2])?, graph_name))
    }

    /// The id of `term` in this view: its stored id, or one made up for this view (which
    /// [`Snapshot::term`] gives back), as the evaluator's terms get.
    pub fn id_of(&self, term: Term) -> TermId {
        self.internal_id(term)
    }

    /// The id of `term`: its stored id, or one made up for this view.
    fn internal_id(&self, term: Term) -> TermId {
        if !self.tables.is_empty()
            && let Term::NamedNode(n) = &term
            && let Some(id) = table::column(&self.tables, n.as_str())
        {
            return id;
        }
        KEY.with_borrow_mut(|key| {
            let kind = match dict::encode(term.as_ref(), key) {
                Encoded::Inline(id) => return id,
                Encoded::Key(kind) => kind,
            };
            // A term made up for this view keeps its id even once the dictionary has it: an
            // update writes the quads of its first rows (#382), or another writer commits,
            // while this view is still evaluating, and one term must have one id in it.
            if let Some(i) = self.ephemeral.lock_unpoisoned().get(key) {
                return TermId::new(Kind::Ephemeral, i);
            }
            if let Some(o) = self.inner.dict.read_unpoisoned().get(key) {
                return TermId::new(kind, u64::from(o));
            }
            let i = self.ephemeral.lock_unpoisoned().get_or_insert(key);
            TermId::new(Kind::Ephemeral, i)
        })
    }
}

/// What [`Snapshot::count_triples`] counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TripleCount {
    /// The solutions of `?s ?p ?o`.
    Rows,
    /// The distinct values of a position: 0 subject, 1 predicate, 2 object.
    Distinct(usize),
}

/// Which graphs a pattern matches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphSelector {
    Default,
    Graph(TermId),
    AnyNamed,
    All,
}

impl Pattern {
    fn of_ids(
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        graph: GraphSelector,
    ) -> Self {
        Self {
            s: s.map(|t| t.0),
            p: p.map(|t| t.0),
            o: o.map(|t| t.0),
            g: match graph {
                GraphSelector::Default => Some(0),
                GraphSelector::Graph(g) => Some(g.0),
                GraphSelector::AnyNamed | GraphSelector::All => None,
            },
            all_graphs: graph == GraphSelector::All,
        }
    }

    fn named(s: Option<u64>, p: Option<u64>, o: Option<u64>, g: Option<u64>) -> Self {
        Self {
            s,
            p,
            o,
            g,
            all_graphs: false,
        }
    }
}

impl<'a> QueryableDataset<'a> for Snapshot {
    type InternalTerm = TermId;
    type Error = StoreError;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&TermId>,
        predicate: Option<&TermId>,
        object: Option<&TermId>,
        graph_name: Option<Option<&TermId>>,
    ) -> impl Iterator<Item = Result<InternalQuad<TermId>, StoreError>> + use<'a> {
        // A column of a result table: its rows, in the default graph. The rewrites put them
        // only in the query's own groups, never in `GRAPH`, so a named graph asked for is the
        // one graph of a dataset's default graph (a caller who may read one graph, #203).
        if let Some(p) = predicate.filter(|p| p.kind() == Kind::Column) {
            let rows = table::quads(&self.tables, *p, subject.copied(), object.copied());
            let p = *p;
            return Either::Left(Either::Left(rows.into_iter().map(move |(s, o)| {
                Ok(InternalQuad {
                    subject: s,
                    predicate: p,
                    object: o,
                    graph_name: None,
                })
            })));
        }
        // A term the store doesn't have matches nothing.
        let unknown = [subject, predicate, object, graph_name.flatten()]
            .into_iter()
            .flatten()
            .any(|t| t.kind() == Kind::Ephemeral);
        let graph = match graph_name {
            None if self.all_graphs => GraphSelector::All,
            None => GraphSelector::AnyNamed,
            Some(None) if !matches!(self.merged_default, MergedDefault::Plain) => {
                return Either::Left(Either::Right(self.merged_quads(
                    subject.copied(),
                    predicate.copied(),
                    object.copied(),
                    unknown,
                )));
            }
            Some(None) => GraphSelector::Default,
            Some(Some(g)) => GraphSelector::Graph(*g),
        };
        let scan = (!unknown).then(|| {
            self.quad_ids_for_pattern(subject.copied(), predicate.copied(), object.copied(), graph)
        });
        Either::Right(scan.into_iter().flatten().map(|q| {
            Ok(InternalQuad {
                subject: q[0],
                predicate: q[1],
                object: q[2],
                graph_name: (q[3] != TermId::DEFAULT_GRAPH).then_some(q[3]),
            })
        }))
    }

    fn internal_named_graphs(&self) -> impl Iterator<Item = Result<TermId, StoreError>> + use<'a> {
        let mut graphs = self.quads.graph_ids();
        // Under a row filter (#202), a graph whose every quad is hidden is not shown.
        if let Some(rows) = &self.rows {
            graphs.retain(|g| {
                self.quads
                    .scan(Pattern::named(None, None, None, Some(*g)))
                    .with_rows(Some(rows))
                    .next()
                    .is_some()
            });
        }
        graphs.into_iter().map(|g| Ok(TermId(g)))
    }

    fn contains_internal_graph_name(&self, graph_name: &TermId) -> Result<bool, StoreError> {
        if graph_name.kind() == Kind::Ephemeral {
            return Ok(false);
        }
        Ok(self
            .quads
            .scan(Pattern::named(None, None, None, Some(graph_name.0)))
            .with_rows(self.rows.as_ref())
            .next()
            .is_some())
    }

    fn internalize_term(&self, term: Term) -> Result<TermId, StoreError> {
        Ok(self.internal_id(term))
    }

    fn externalize_term(&self, term: TermId) -> Result<Term, StoreError> {
        self.term(term)
    }

    fn externalize_expression_term(&self, term: TermId) -> Result<ExpressionTerm, StoreError> {
        match term.kind() {
            Kind::Integer => Ok(ExpressionTerm::IntegerLiteral(term.as_integer().into())),
            Kind::Boolean => Ok(ExpressionTerm::BooleanLiteral((term.payload() != 0).into())),
            Kind::Double | Kind::Decimal => Ok(match term.numeric() {
                Some(Numeric::Double(d)) => ExpressionTerm::DoubleLiteral(d.into()),
                Some(Numeric::Decimal { digits, scale }) => {
                    match oxsdatatypes::Decimal::new(digits, u32::from(scale)) {
                        Ok(d) => ExpressionTerm::DecimalLiteral(d),
                        Err(_) => self.term(term)?.into(),
                    }
                }
                None => self.term(term)?.into(),
            }),
            _ => Ok(self.term(term)?.into()),
        }
    }

    fn internalize_expression_term(&self, term: ExpressionTerm) -> Result<TermId, StoreError> {
        match &term {
            ExpressionTerm::BooleanLiteral(b) => return Ok(TermId::boolean(bool::from(*b))),
            ExpressionTerm::IntegerLiteral(i) => {
                if let Some(id) = TermId::integer(i64::from(*i)) {
                    return Ok(id);
                }
            }
            _ => {}
        }
        self.internalize_term(term.into())
    }

    fn internal_term_effective_boolean_value(
        &self,
        term: TermId,
    ) -> Result<Option<bool>, StoreError> {
        match term.kind() {
            Kind::Boolean => Ok(Some(term.payload() != 0)),
            Kind::Integer => Ok(Some(term.as_integer() != 0)),
            Kind::Double | Kind::Decimal => Ok(Some(match term.numeric() {
                Some(Numeric::Double(d)) => !(d == 0.0 || d.is_nan()),
                Some(Numeric::Decimal { digits, .. }) => digits != 0,
                None => false,
            })),
            Kind::DateTime => Ok(None),
            Kind::String => Ok(Some(match self.term(term)? {
                Term::Literal(l) => !l.value().is_empty(),
                _ => true,
            })),
            Kind::Typed | Kind::Ephemeral => Ok(match self.externalize_expression_term(term)? {
                ExpressionTerm::StringLiteral(s) => Some(!s.is_empty()),
                ExpressionTerm::BooleanLiteral(b) => Some(b.into()),
                ExpressionTerm::IntegerLiteral(i) => Some(i != 0.into()),
                ExpressionTerm::DecimalLiteral(d) => Some(d != 0.into()),
                ExpressionTerm::FloatLiteral(f) => Some(!(f == 0f32.into() || f.is_nan())),
                ExpressionTerm::DoubleLiteral(d) => Some(!(d == 0f64.into() || d.is_nan())),
                _ => None,
            }),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subject, predicate or object the rows hide everywhere matches nothing in a view,
    /// and its estimates and compiled patterns say so, so join orders start there (#204).
    /// One hidden only on some subjects keeps its estimate and its other quads.
    #[test]
    fn terms_hidden_everywhere_match_nothing_and_are_estimated_so() {
        use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern, Variable};
        let store = Store::new();
        let mut nt = String::new();
        for i in 0..200 {
            nt += &format!(
                "<http://e/s{i}> <http://e/p> <http://e/o{}> .\n<http://e/s{i}> <http://e/q> \"{i}\" .\n",
                i % 3
            );
        }
        store.bulk_load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
        // Some quads in the delta too.
        store
            .insert(QuadRef::new(
                NamedNodeRef::new_unchecked("http://e/s0"),
                NamedNodeRef::new_unchecked("http://e/p"),
                NamedNodeRef::new_unchecked("http://e/o0"),
                GraphNameRef::DefaultGraph,
            ))
            .unwrap();
        store
            .insert(QuadRef::new(
                NamedNodeRef::new_unchecked("http://e/s1"),
                NamedNodeRef::new_unchecked("http://e/q"),
                NamedNodeRef::new_unchecked("http://e/o1"),
                GraphNameRef::DefaultGraph,
            ))
            .unwrap();
        let plain = store.snapshot();
        let id = |iri: &str| {
            plain
                .term_id(NamedNodeRef::new_unchecked(iri).into())
                .unwrap()
        };
        let (p, q, s5, o1) = (
            id("http://e/p"),
            id("http://e/q"),
            id("http://e/s5"),
            id("http://e/o1"),
        );
        let mut rows = RowFilter::new();
        rows.hide_predicate(p);
        rows.hide_resource(s5, false);
        rows.hide_resource(o1, true);
        rows.hide_predicate_on(id("http://e/s7"), q);
        let view = plain.clone().with_rows(Some(Arc::new(rows)));
        let pattern = |s: Option<&str>, p: Option<&str>, o: Option<&str>| {
            let term = |t: Option<&str>, v: &str| match t {
                Some(iri) => TermPattern::NamedNode(NamedNode::new_unchecked(iri)),
                None => TermPattern::Variable(Variable::new_unchecked(v)),
            };
            TriplePattern {
                subject: term(s, "s"),
                predicate: match p {
                    Some(iri) => NamedNodePattern::NamedNode(NamedNode::new_unchecked(iri)),
                    None => NamedNodePattern::Variable(Variable::new_unchecked("p")),
                },
                object: term(o, "o"),
            }
        };
        let all = GraphSelector::All;
        for (s, p_, o, ids) in [
            (None, Some("http://e/p"), None, (None, Some(p), None)),
            (Some("http://e/s5"), None, None, (Some(s5), None, None)),
            (None, None, Some("http://e/o1"), (None, None, Some(o1))),
        ] {
            assert!(plain.estimate_ids(ids.0, ids.1, ids.2, all) > 0);
            assert_eq!(view.estimate_ids(ids.0, ids.1, ids.2, all), 0, "{ids:?}");
            assert_eq!(
                view.quad_ids_for_pattern(ids.0, ids.1, ids.2, all).count(),
                0
            );
            let one = [pattern(s, p_, o)];
            assert!(!plain.compile_bgp(&one, &[]).unwrap().empty);
            assert!(view.compile_bgp(&one, &[]).unwrap().empty, "{ids:?}");
        }
        // Hidden on one subject only: estimated and read as before, less that subject.
        let q_all = plain.estimate_ids(None, Some(q), None, all);
        assert_eq!(view.estimate_ids(None, Some(q), None, all), q_all);
        assert!(
            !view
                .compile_bgp(&[pattern(None, Some("http://e/q"), None)], &[])
                .unwrap()
                .empty
        );
        // Less s7's own, s5's (hidden as a resource) and the one with object o1.
        assert_eq!(
            view.quad_ids_for_pattern(None, Some(q), None, all).count(),
            plain.quad_ids_for_pattern(None, Some(q), None, all).count() - 3
        );
    }

    #[test]
    fn subjects_with_prefix_finds_ranked_and_new_iris() {
        let store = Store::new();
        let nt = "<http://e/v> <http://e/p> \"1\" .\n<http://e/v#a> <http://e/p> \"2\" .\n\
                  <http://e/x> <http://e/p> <http://e/v#object-only> .\n";
        store.bulk_load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
        let names = |store: &Store| -> Vec<String> {
            let mut v: Vec<String> = store
                .snapshot()
                .subjects_with_prefix("http://e/v#", 10)
                .into_iter()
                .map(|n| n.into_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(names(&store), ["http://e/v#a"]);
        // Added after the ranks were built: found among the unranked terms.
        store
            .bulk_load(
                RdfFormat::NTriples,
                "<http://e/v#b> <http://e/p> \"3\" .\n".as_bytes(),
            )
            .unwrap();
        assert_eq!(names(&store), ["http://e/v#a", "http://e/v#b"]);
    }

    /// The distinct blank nodes of the store.
    fn blank_nodes(store: &Store) -> std::collections::BTreeSet<String> {
        store
            .snapshot()
            .quads_for_pattern(None, None, None, None)
            .flat_map(|q| [Term::from(q.subject), q.object])
            .filter_map(|t| match t {
                Term::BlankNode(b) => Some(b.into_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn each_load_has_its_own_blank_nodes() {
        // #840: N-Triples long enough to be read in many blocks (100 bytes in tests), each
        // parsed in parts, with `_:shared` in the first line and the last: one node per load.
        let mut nt = String::from("_:shared <http://e/p> \"first\" .\n");
        for i in 0..300 {
            nt += &format!("<http://e/s{i}> <http://e/p> _:n{} .\n", i % 7);
        }
        nt += "<http://e/last> <http://e/p> _:shared .\n";
        for format in [RdfFormat::NTriples, RdfFormat::Turtle] {
            let store = Store::new();
            let load = |blank_nodes| {
                store
                    .bulk_load_with_blank_nodes(
                        RdfParser::from_format(format),
                        nt.as_bytes(),
                        blank_nodes,
                    )
                    .unwrap()
            };
            assert_eq!(load(BlankNodes::PerLoad), 302);
            assert_eq!(blank_nodes(&store).len(), 8, "{format}");
            // `_:shared` is one node: the literal's subject is the last line's object.
            let snapshot = store.snapshot();
            let first: Vec<Quad> = snapshot
                .quads_for_pattern(
                    None,
                    None,
                    Some(
                        Term::from(oxigraph::model::Literal::new_simple_literal("first")).as_ref(),
                    ),
                    None,
                )
                .collect();
            let last = NamedNode::new_unchecked("http://e/last");
            let object = snapshot
                .quads_for_pattern(Some(last.as_ref().into()), None, None, None)
                .next()
                .unwrap()
                .object;
            assert_eq!(Term::from(first[0].subject.clone()), object);
            assert_ne!(
                object,
                Term::from(oxigraph::model::BlankNode::new_unchecked("shared"))
            );
            drop(snapshot);
            // A second load of the same text: its own 8 nodes, and its quads are new.
            assert_eq!(load(BlankNodes::PerLoad), 302);
            assert_eq!(blank_nodes(&store).len(), 16, "{format}");
            // Kept labels: `_:shared` as written, and again as written on a second load.
            let store = Store::new();
            for _ in 0..2 {
                store
                    .bulk_load_with_blank_nodes(
                        RdfParser::from_format(format),
                        nt.as_bytes(),
                        BlankNodes::Keep,
                    )
                    .unwrap();
            }
            assert_eq!(blank_nodes(&store).len(), 8, "{format}");
            assert!(blank_nodes(&store).contains("shared"));
        }
    }

    #[test]
    fn bulk_load_reads_in_blocks() {
        let mut nt = String::new();
        for i in 0..500 {
            nt += &format!(
                "<http://e/s{i}> <http://e/p> \"{}\" .\n",
                "x".repeat(i % 300)
            );
        }
        nt += "<http://e/last> <http://e/p> _:b1 ."; // no final line end
        let store = Store::new();
        assert_eq!(
            store.bulk_load(RdfFormat::NTriples, nt.as_bytes()).unwrap(),
            501
        );
        let expected: Vec<Quad> = RdfParser::from_format(RdfFormat::NTriples)
            .for_slice(nt.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
        for q in &expected {
            assert!(q.object.is_blank_node() || store.contains(q), "{q}");
        }
        assert_eq!(store.len(), 501);
        assert!(
            store
                .bulk_load(
                    RdfFormat::NTriples,
                    "<http://e/a> <http://e/p> .\n".as_bytes()
                )
                .is_err()
        );
        assert_eq!(store.len(), 501);
    }

    #[test]
    fn turtle_loads_hand_batches_to_the_interner() {
        let mut ttl = String::from("@prefix e: <http://e/> .\n");
        for i in 0..50 {
            ttl += &format!(
                "e:s{i} e:p \"v{}\", {i}, \"{i}.5\"^^<http://www.w3.org/2001/XMLSchema#decimal> ; e:q e:s{} .\n",
                i % 7,
                i / 2
            );
        }
        let store = Store::new();
        assert_eq!(
            store.bulk_load(RdfFormat::Turtle, ttl.as_bytes()).unwrap(),
            200
        );
        let expected: Vec<Quad> = RdfParser::from_format(RdfFormat::Turtle)
            .for_slice(ttl.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
        for q in &expected {
            assert!(store.contains(q), "{q}");
        }
        assert_eq!(store.len(), 200);
        // A syntax error after a few batches: nothing is added.
        let bad = format!("{ttl}e:x e:p .\n");
        assert!(store.bulk_load(RdfFormat::Turtle, bad.as_bytes()).is_err());
        assert_eq!(store.len(), 200);
    }
}
