//! The eight quad permutations, each an immutable [`Segment`] plus a small sorted delta of
//! added and removed quads that compaction folds in.
//!
//! Compaction writes the delta as a [`Run`] next to the segment rather than rewriting the
//! segment (#218): reads merge the segment with its few runs, runs merge with each other as
//! they grow (size-tiered), and only once they hold an eighth of the segment's quads is the
//! whole permutation rewritten.
//!
//! Quads are `[s, p, o, g]` term ids with `g == 0` for the default graph. With the graph
//! bound (default graph included) a pattern is a range scan of GSPO, GPOS or GOSP; with "any
//! named graph" it is a scan of SPOG, POSG or OSPG that skips the default graph.
//!
//! PSOG and GPSO hold the same quads as POSG and GPOS with subject before object, so a scan
//! of `?s <p> ?o` can come out sorted by subject: [`QuadIndex::scan_sorted`] reads them when
//! the caller wants that order (a merge join on the subject, issue #168). Plain scans don't.

use crate::segment::{Key, Range, Segment};
use rayon::prelude::*;
/// A delta: a persistent ordered set, so a snapshot keeps its version for the cost of a
/// reference count and a write next to a live snapshot copies only the path it changes
/// (`O(log n)`), not the whole set.
type Set = imbl::OrdSet<Key>;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// Deltas of up to this many quads per permutation get their counts adjusted for them (a few
/// block reads per quad, once per version); larger ones leave count queries to scans.
const STATS_DELTA_MAX: usize = 4096;
/// A bulk load adds its quads as a run, rather than rebuilding the index, when the index
/// holds at least this many quads per new quad.
const PARTIAL_RATIO: usize = 16;
/// The runs of a permutation are folded into its segment once they hold this many quads per
/// segment quad, or more.
const RUNS_RATIO: usize = 8;
/// A merge in the background folds a permutation's runs into its segment once they hold one
/// quad per this many segment quads, or once there are more than [`MERGE_RUNS`] of them.
/// Each merge rewrites the whole segment: merging every new run made 60 commits of 10,000
/// quads into a store of a billion rewrite it again and again, and slowed those commits from
/// seconds to minutes while it ran (#437). Below this, a read merges the few runs in, which
/// their filters keep cheap.
const MERGE_RATIO: usize = 64;
/// See [`MERGE_RATIO`].
const MERGE_RUNS: usize = 8;
/// A store idle for a while merges its remaining runs into segments of up to this many
/// quads (#442): rewriting a larger one takes minutes of I/O that the next writes would wait
/// on (a billion quads: about 20 minutes per permutation set), so its runs wait for
/// [`MERGE_RATIO`].
pub(crate) const IDLE_MERGE_MAX: usize = 256 << 20;
/// A store idle for a while merges runs that hold at least one quad per this many segment
/// quads: a rewrite of the whole segment for a handful of quads, after every few writes,
/// would cost more than reading them in.
const IDLE_MERGE_RATIO: usize = 1024;
/// A new run merges with the run before it while that one holds at most this many times its
/// quads (size-tiered: a few runs of growing sizes).
const RUN_TIER: usize = 4;
/// Permutations of fewer quads than this are rewritten at each compaction, as before runs:
/// that takes a fraction of a second, and reads of a segment without runs are the fastest
/// (on the rail KG, 12.5M quads, merging runs in the background cost more in reads than it
/// saved in writes). Larger ones, whose rewrite grows with them, get runs.
static RUNS_FROM: AtomicUsize = AtomicUsize::new(if cfg!(test) { 0 } else { 1 << 24 });

/// Sets the size from which permutations get runs ([`RUNS_FROM`]), for the whole process:
/// for tests of runs on small stores.
pub(crate) fn set_runs_from(quads: usize) {
    RUNS_FROM.store(quads, Ordering::Relaxed);
}
/// A delta of up to this many quads per permutation is copied into sorted arrays on its first
/// read. A larger one is copied only once this version of it has been read
/// [`FLAT_AFTER_READS`] times: before that, a read takes the part of the persistent sets in
/// its range. Otherwise a store that alternates single writes and reads (rdflib's
/// `graph.remove` over oxrdflib does a lookup then a remove per triple) copies the whole delta,
/// up to `COMPACT_AT` quads, for every read: quadratic in the number of writes.
const FLAT_MIN: usize = 1024;
const FLAT_AFTER_READS: usize = 64;

/// Column order of each permutation, as positions in `[s, p, o, g]`.
/// The first six are those of stores saved before PSOG and GPSO existed; those two are built
/// from SPOG when such a store opens.
pub(crate) const ORDERS: [[usize; 4]; PERMS] = [
    [0, 1, 2, 3], // SPOG
    [1, 2, 0, 3], // POSG
    [2, 0, 1, 3], // OSPG
    [3, 0, 1, 2], // GSPO
    [3, 1, 2, 0], // GPOS
    [3, 2, 0, 1], // GOSP
    [1, 0, 2, 3], // PSOG
    [3, 1, 0, 2], // GPSO
];
pub(crate) const PERMS: usize = 8;
/// `(permutation, source)`: the permutation is the source with the permutation's first
/// column moved to the front, in an order that builds each source first (see
/// [`QuadIndex::from_quads`]).
const DERIVED: [(usize, usize); PERMS - 1] = [
    (OSPG, SPOG),
    (POSG, OSPG),
    (GSPO, SPOG),
    (GPOS, POSG),
    (GOSP, OSPG),
    (PSOG, SPOG),
    (GPSO, PSOG),
];
pub(crate) const PERM_NAMES: [&str; PERMS] = [
    "spog", "posg", "ospg", "gspo", "gpos", "gosp", "psog", "gpso",
];
pub(crate) const SPOG: usize = 0;
pub(crate) const POSG: usize = 1;
pub(crate) const OSPG: usize = 2;
pub(crate) const GSPO: usize = 3;
pub(crate) const GPOS: usize = 4;
pub(crate) const GOSP: usize = 5;
pub(crate) const PSOG: usize = 6;
const GPSO: usize = 7;

#[inline]
fn permute(q: &Key, order: &[usize; 4]) -> Key {
    [q[order[0]], q[order[1]], q[order[2]], q[order[3]]]
}

/// The key position of each column, for a permutation's column order.
fn inverse(order: &[usize; 4]) -> [usize; 4] {
    let mut from = [0; 4];
    for (i, &c) in order.iter().enumerate() {
        from[c] = i;
    }
    from
}

#[derive(Clone, Default)]
pub(crate) struct Perm {
    pub(crate) segment: Arc<Segment>,
    /// Changes to `segment` from earlier compactions, oldest first ([`Run`]).
    pub(crate) runs: Arc<[Run]>,
    /// The counts of `segment` with the runs applied, once known (with runs only).
    pub(crate) runs_stats: Arc<OnceLock<Arc<crate::stats::SegStats>>>,
    /// Which segment this is ([`new_id`]; 0 for one built whole, which has no runs), so a
    /// merge in the background knows whether its result still applies.
    base_id: u64,
    /// How many of the first runs a merge in the background is folding into the segment:
    /// compactions leave them as they are.
    frozen: usize,
    added: Set,
    removed: Set,
    /// What is worked out from this version of the delta when first asked for. Shared with
    /// the snapshots of this version, so one computes it for all; a write starts a new one.
    cache: Arc<DeltaCache>,
}

/// Quads added to a permutation's segment and quads removed from it, each a segment of its
/// own. The runs of a permutation never overlap: an added quad is in neither the segment nor
/// another run, a removed quad is in the segment and in no other run.
#[derive(Clone)]
pub(crate) struct Run {
    pub(crate) added: Arc<Segment>,
    pub(crate) removed: Arc<Segment>,
    /// Which run this is ([`new_id`]).
    id: u64,
    /// The key prefixes of the run, built when first needed (#326).
    filter: Arc<OnceLock<PrefixFilter>>,
}

/// A number no segment or run had before.
fn new_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Run {
    fn new(added: Arc<Segment>, removed: Arc<Segment>, id: u64) -> Self {
        Self {
            added,
            removed,
            id,
            filter: Arc::default(),
        }
    }

    fn len(&self) -> usize {
        self.added.len + self.removed.len
    }

    /// The run's filter; `segment` is the one the run applies to.
    fn filter(&self, segment: &Segment) -> &PrefixFilter {
        self.filter.get_or_init(|| {
            PrefixFilter::build(
                &self.added,
                &self.removed,
                segment.stats.get().map(|s| s.distinct),
            )
        })
    }

    /// Whether the run may hold keys in `low..=high`: `false` only if it holds none.
    #[inline]
    fn may_hold(&self, segment: &Segment, low: &Key, high: &Key) -> bool {
        let fixed = (0..4).take_while(|&i| low[i] == high[i]).count();
        fixed == 0 || self.filter(segment).may_hold(low, fixed)
    }
}

/// A Bloom filter over the key prefixes of a run (lengths 1 to 4), added and removed keys
/// alike, so lookups, counts and scans with a fixed prefix skip the runs that cannot hold it
/// (#326). One filter per prefix length, each in split-block layout: a prefix sets one bit in
/// each of the eight words of one 256-bit block, sized at 10 bits per key of the run, so
/// about 1 % false positives or fewer.
///
/// A prefix length is left out when the run holds more than a quarter of the segment's
/// distinct prefixes of that length: most reads would find it anyway, and the probe would
/// cost more than it saves.
#[derive(Default)]
pub(crate) struct PrefixFilter {
    /// By prefix length less one; `None` for a length left out.
    lens: [Option<Box<[Block]>>; 4],
}

type Block = [u32; 8];

const SALT: [u32; 8] = [
    0x47b6137b, 0x44974d91, 0x8824ad5b, 0xa2b7289d, 0x705495c7, 0x2df1424b, 0x9efc4947, 0x5c6bfb31,
];

/// The hash of `key[..len]`.
#[inline]
fn prefix_hash(key: &Key, len: usize) -> u64 {
    let mut h = len as u64;
    for &x in &key[..len] {
        h = (h ^ x).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^= h >> 29;
    }
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^ (h >> 32)
}

/// The block of hash `h` among `blocks`, and the bit it sets in each word.
#[inline]
fn block_bits(blocks: usize, h: u64) -> (usize, Block) {
    let b = ((u128::from(h >> 32) * blocks as u128) >> 32) as usize;
    let low = h as u32;
    (b, SALT.map(|s| 1 << (low.wrapping_mul(s) >> 27)))
}

/// Builds a [`PrefixFilter`] from a run's keys as they are written, in one pass.
pub(crate) struct FilterBuilder {
    last: Option<Key>,
    counts: [u64; 4],
    lens: [Box<[Block]>; 4],
}

impl FilterBuilder {
    /// For a run of at most `keys` keys.
    fn new(keys: usize) -> Self {
        let blocks = (keys * 10).div_ceil(256).max(1);
        Self {
            last: None,
            counts: [0; 4],
            lens: std::array::from_fn(|_| vec![[0; 8]; blocks].into()),
        }
    }

    /// Adds a key; keys come sorted, up to [`FilterBuilder::restart`].
    #[inline]
    fn push(&mut self, k: &Key) {
        // Only the prefixes past the first column where it differs from the key before.
        let from = self
            .last
            .map_or(0, |l| (0..4).take_while(|&i| l[i] == k[i]).count());
        for len in from + 1..=4 {
            self.counts[len - 1] += 1;
            let blocks = &mut self.lens[len - 1];
            let (b, bits) = block_bits(blocks.len(), prefix_hash(k, len));
            for (w, bit) in blocks[b].iter_mut().zip(bits) {
                *w |= bit;
            }
        }
        self.last = Some(*k);
    }

    /// Starts another sorted sequence of keys.
    fn restart(&mut self) {
        self.last = None;
    }

    /// The filter; `distinct` are the segment's distinct prefixes by length, when known
    /// (without them, every length is kept).
    fn finish(self, distinct: Option<[u64; 4]>) -> PrefixFilter {
        let counts = self.counts;
        let mut i = 0;
        PrefixFilter {
            lens: self.lens.map(|blocks| {
                let kept = distinct.is_none_or(|d| counts[i] * 4 <= d[i]);
                i += 1;
                kept.then_some(blocks)
            }),
        }
    }
}

impl PrefixFilter {
    /// The filter of a run read back from a checkpoint.
    fn build(added: &Segment, removed: &Segment, distinct: Option<[u64; 4]>) -> Self {
        let mut builder = FilterBuilder::new(added.len + removed.len);
        for segment in [added, removed] {
            builder.restart();
            let mut range = segment.range_of([0; 4], [u64::MAX; 4]);
            while let Some(k) = range.next(segment) {
                builder.push(&k);
            }
        }
        builder.finish(distinct)
    }

    /// Whether some key of the run may start with `key[..len]`.
    #[inline]
    fn may_hold(&self, key: &Key, len: usize) -> bool {
        let Some(blocks) = &self.lens[len - 1] else {
            return true;
        };
        let (b, bits) = block_bits(blocks.len(), prefix_hash(key, len));
        blocks[b].iter().zip(bits).all(|(w, bit)| w & bit != 0)
    }

    fn heap_size(&self) -> usize {
        self.lens.iter().flatten().map(|b| b.len() * 32).sum()
    }
}

#[derive(Default)]
struct DeltaCache {
    /// The delta as two sorted arrays, for scans and range counts: a binary search and a
    /// slice walk, where the persistent sets need an allocated iterator per seek.
    flat: OnceLock<Flat>,
    /// The segment's counts with the delta applied; `None` when the delta is too large.
    stats: OnceLock<Option<Arc<crate::stats::SegStats>>>,
    /// Reads of this version that took their range from the persistent sets.
    reads: AtomicUsize,
}

#[derive(Clone, Default)]
struct Flat {
    added: Arc<[Key]>,
    removed: Arc<[Key]>,
}

impl Flat {
    /// The keys of `keys` in `low..=high`, as index bounds.
    #[inline]
    fn bounds(keys: &[Key], low: Key, high: Key) -> (usize, usize) {
        let start = keys.partition_point(|k| *k < low);
        let end = start + keys[start..].partition_point(|k| *k <= high);
        (start, end)
    }

    /// The delta's keys in order: `1` added, `-1` removed.
    fn merged(&self) -> Vec<(Key, i64)> {
        let mut out = Vec::with_capacity(self.added.len() + self.removed.len());
        let (mut a, mut r) = (0, 0);
        while a < self.added.len() || r < self.removed.len() {
            if r == self.removed.len() || (a < self.added.len() && self.added[a] < self.removed[r])
            {
                out.push((self.added[a], 1));
                a += 1;
            } else {
                out.push((self.removed[r], -1));
                r += 1;
            }
        }
        out
    }
}

impl Perm {
    /// Whether the segment with its runs holds `q`.
    fn stored_contains(&self, q: &Key) -> bool {
        for run in self.runs.iter() {
            if !run.filter(&self.segment).may_hold(q, 4) {
                continue;
            }
            if run.added.contains(q) {
                return true;
            }
            if run.removed.contains(q) {
                return false;
            }
        }
        self.segment.contains(q)
    }

    /// Quads of the segment with its runs in `low..=high`.
    fn stored_count(&self, low: Key, high: Key) -> usize {
        let mut n = self.segment.count_range(low, high);
        for run in self
            .runs
            .iter()
            .filter(|r| r.may_hold(&self.segment, &low, &high))
        {
            n += run.added.count_range(low, high);
            n -= run.removed.count_range(low, high);
        }
        n
    }

    /// The counts of the segment with its runs.
    fn stored_stats(&self) -> Arc<crate::stats::SegStats> {
        if self.runs.is_empty() {
            return self.segment.stats();
        }
        Arc::clone(self.runs_stats.get_or_init(|| {
            let mut builder = crate::stats::StatsBuilder::default();
            for k in self.stored_range([0; 4], [u64::MAX; 4]) {
                builder.push(&k);
            }
            Arc::new(builder.finish())
        }))
    }

    /// The keys of the segment with its runs in `low..=high`, in order.
    fn stored_range(&self, low: Key, high: Key) -> impl Iterator<Item = Key> {
        let mut range = self.segment.range_of(low, high);
        let mut runs = RunMerge::new(&self.runs, &self.segment, low, high);
        let segment = Arc::clone(&self.segment);
        std::iter::from_fn(move || match &mut runs {
            None => range.next(&segment),
            Some(r) => r.next(&mut range, &segment),
        })
    }

    fn stored_heap_size(&self) -> usize {
        self.segment.heap_size()
            + self
                .runs
                .iter()
                .map(|r| {
                    r.added.heap_size()
                        + r.removed.heap_size()
                        + r.filter.get().map_or(0, PrefixFilter::heap_size)
                })
                .sum::<usize>()
    }

    /// Whether a merge when the store is idle folds the runs into the segment (#442).
    fn idle_merges(&self) -> bool {
        let in_runs = self.runs.iter().map(Run::len).sum::<usize>();
        !self.runs.is_empty()
            && self.segment.len <= IDLE_MERGE_MAX
            && in_runs * IDLE_MERGE_RATIO >= self.segment.len
    }

    /// Whether a merge in the background should fold the runs into the segment
    /// ([`MERGE_RATIO`]).
    fn wants_merge(&self) -> bool {
        let in_runs = self.runs.iter().map(Run::len).sum::<usize>();
        !self.runs.is_empty()
            && (self.runs.len() > MERGE_RUNS || in_runs * MERGE_RATIO >= self.segment.len)
    }

    fn stored_mapped_size(&self) -> usize {
        self.segment.mapped_size()
            + self
                .runs
                .iter()
                .map(|r| r.added.mapped_size() + r.removed.mapped_size())
                .sum::<usize>()
    }

    /// This permutation with `delta` (sorted changes to the segment with its runs, `1` added,
    /// `-1` removed) folded in, and no delta: as a new run, merged with the runs before it of
    /// up to [`RUN_TIER`] times its size, or the whole permutation rewritten once its runs
    /// would hold an eighth of its segment.
    ///
    /// With `pinned`, the segment is never rewritten: the changes go to runs, which take
    /// memory in proportion to them.
    fn folded(
        &self,
        delta: &[(Key, i64)],
        spill: Option<&crate::persist::Spill>,
        pinned: bool,
    ) -> Perm {
        let rewrite = !pinned;
        let keep = |runs: Arc<[Run]>, runs_stats| Perm {
            segment: Arc::clone(&self.segment),
            runs,
            runs_stats,
            base_id: self.base_id,
            frozen: self.frozen,
            ..Perm::default()
        };
        if delta.is_empty() {
            return keep(Arc::clone(&self.runs), Arc::clone(&self.runs_stats));
        }
        if rewrite && self.runs.is_empty() && self.segment.len < RUNS_FROM.load(Ordering::Relaxed) {
            return Perm {
                segment: fold(&self.segment, delta, spill),
                base_id: new_id(),
                ..Perm::default()
            };
        }
        // A change that undoes one of a run (a removed quad added back, an added one
        // removed) takes that run out: merging all of them is simplest, and rare.
        let undoes = !self.runs.is_empty()
            && delta.par_iter().any(|(k, sign)| {
                self.runs.iter().any(|r| {
                    r.filter(&self.segment).may_hold(k, 4)
                        && if *sign > 0 {
                            r.removed.contains(k)
                        } else {
                            r.added.contains(k)
                        }
                })
            });
        let in_runs = self.runs.iter().map(Run::len).sum::<usize>() + delta.len();
        // While a merge in the background folds the first runs in, the runs may grow to a
        // quarter of the segment before the permutation is rewritten here.
        let ratio = if self.frozen > 0 {
            RUNS_RATIO / 2
        } else {
            RUNS_RATIO
        };
        if rewrite && (in_runs * ratio >= self.segment.len || (undoes && self.frozen > 0)) {
            // Everything into a new segment, counted as it is written.
            let mut counts = crate::stats::StatsBuilder::default();
            let segment = Segment::from_sorted_in(
                FoldIn {
                    seg: self.segment.iter().peekable(),
                    changes: signed_merge(&self.runs, delta).peekable(),
                }
                .inspect(|k| counts.push(k)),
                spill,
            );
            let _ = segment.stats.set(Arc::new(counts.finish()));
            return Perm {
                segment: Arc::new(segment),
                base_id: new_id(),
                ..Perm::default()
            };
        }
        let stats = Arc::new(
            self.stored_stats()
                .with_delta(&|low, high| self.stored_count(low, high), delta),
        );
        let mut from = self.runs.len();
        if undoes {
            from = 0;
        } else {
            let mut size = delta.len();
            while from > self.frozen && self.runs[from - 1].len() <= RUN_TIER * size {
                from -= 1;
                size += self.runs[from].len();
            }
        }
        let mut removed = Vec::new();
        let mut filter =
            FilterBuilder::new(delta.len() + self.runs[from..].iter().map(Run::len).sum::<usize>());
        let added = Segment::from_sorted_in(
            signed_merge(&self.runs[from..], delta)
                .inspect(|(k, _)| filter.push(k))
                .filter_map(|(k, sign)| {
                    if sign < 0 {
                        removed.push(k);
                        None
                    } else {
                        Some(k)
                    }
                }),
            spill,
        );
        let removed = Segment::from_sorted_in(removed, spill);
        let mut runs: Vec<Run> = self.runs[..from].to_vec();
        if added.len + removed.len > 0 {
            let run = Run::new(Arc::new(added), Arc::new(removed), new_id());
            // Built as the run was written, rather than by the first read.
            let _ = run
                .filter
                .set(filter.finish(self.segment.stats.get().map(|s| s.distinct)));
            runs.push(run);
        }
        let runs_stats: Arc<OnceLock<_>> = Arc::default();
        if !runs.is_empty() {
            let _ = runs_stats.set(stats);
        }
        keep(runs.into(), runs_stats)
    }

    /// The segment with the runs folded in, and its counts, for a merge in the background:
    /// on the thread that calls it, so it takes one core.
    /// `None` when `stop` was set meanwhile (checked every few thousand quads).
    fn merged_runs(
        &self,
        spill: Option<&crate::persist::Spill>,
        stop: &AtomicBool,
    ) -> Option<MergedPerm> {
        let mut counts = crate::stats::StatsBuilder::default();
        let mut n = 0u32;
        let mut stopped = false;
        let segment = Segment::from_sorted_in(
            FoldIn {
                seg: self.segment.iter().peekable(),
                changes: signed_merge(&self.runs, &[]).peekable(),
            }
            .take_while(|_| {
                n = n.wrapping_add(1);
                if n.is_multiple_of(4096) && stop.load(Ordering::Relaxed) {
                    stopped = true;
                }
                !stopped
            })
            .inspect(|k| counts.push(k)),
            spill,
        );
        if stopped {
            return None;
        }
        let _ = segment.stats.set(Arc::new(counts.finish()));
        Some(MergedPerm {
            base_id: self.base_id,
            runs: self.runs.iter().map(|r| r.id).collect(),
            segment: Arc::new(segment),
        })
    }

    /// Starts a new version of the delta cache before a write.
    fn written(&mut self) {
        match Arc::get_mut(&mut self.cache) {
            Some(cache) => {
                cache.flat.take();
                cache.stats.take();
                *cache.reads.get_mut() = 0;
            }
            None => self.cache = Arc::default(),
        }
    }

    fn has_delta(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }

    fn flat(&self) -> &Flat {
        self.cache.flat.get_or_init(|| Flat {
            added: self.added.iter().copied().collect(),
            removed: self.removed.iter().copied().collect(),
        })
    }

    /// Net quads of the delta in `low..=high`.
    fn delta_count(&self, low: Key, high: Key) -> i64 {
        if !self.has_delta() {
            return 0;
        }
        if let Some(flat) = self.flat_for_read() {
            let (a0, a1) = Flat::bounds(&flat.added, low, high);
            let (r0, r1) = Flat::bounds(&flat.removed, low, high);
            return (a1 - a0) as i64 - (r1 - r0) as i64;
        }
        self.added.range(low..=high).count() as i64 - self.removed.range(low..=high).count() as i64
    }

    /// The flat copy of the delta, made now if the delta is small or has been read often
    /// enough; `None`: read the persistent sets (see [`FLAT_MIN`]).
    fn flat_for_read(&self) -> Option<&Flat> {
        if let Some(flat) = self.cache.flat.get() {
            return Some(flat);
        }
        let small = self.added.len() + self.removed.len() <= FLAT_MIN;
        (small || self.cache.reads.fetch_add(1, Ordering::Relaxed) >= FLAT_AFTER_READS)
            .then(|| self.flat())
    }

    /// Cursors over the added and the removed keys in `low..=high`.
    fn delta_cursors(&self, low: Key, high: Key) -> (Option<SetCursor>, Option<SetCursor>) {
        match self.flat_for_read() {
            Some(flat) => (
                SetCursor::new(&flat.added, low, high),
                SetCursor::new(&flat.removed, low, high),
            ),
            None => {
                let part = |set: &Set| {
                    let keys: Arc<[Key]> = set.range(low..=high).copied().collect();
                    SetCursor::new(&keys, low, high)
                };
                (part(&self.added), part(&self.removed))
            }
        }
    }
}

/// A version of the quad set. Cloning is cheap (reference counts only).
#[derive(Clone, Default)]
pub(crate) struct QuadIndex {
    pub(crate) perms: [Perm; PERMS],
    len: usize,
    /// Where folds write the segments they build, for a store on disk (#218).
    pub(crate) spill: Option<Arc<crate::persist::Spill>>,
    /// The segments are shared with another store, this one's base (an overlay, #406):
    /// changes stay in runs and the large segments are never rewritten, so the index takes
    /// memory in proportion to its changes, not to its base.
    pub(crate) pinned: bool,
}

/// A quad pattern over term ids. `graph`: `Some(0)` is the default graph, `Some(g)` a named
/// graph, `None` any named graph.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pattern {
    pub(crate) s: Option<u64>,
    pub(crate) p: Option<u64>,
    pub(crate) o: Option<u64>,
    pub(crate) g: Option<u64>,
    /// With `g == None`: every graph, the default one included, instead of named graphs only.
    pub(crate) all_graphs: bool,
}

/// The memory a bulk load may use for the quads it sorts, in bytes, for the whole process
/// (`usize::MAX`: no limit, every quad sorted in memory). Set from `--memory-limit` (#218).
static MEMORY_LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// [`MEMORY_LIMIT`], when one is set.
pub(crate) fn memory_limit() -> Option<usize> {
    Some(MEMORY_LIMIT.load(Ordering::Relaxed)).filter(|&m| m != usize::MAX)
}

/// Sets [`MEMORY_LIMIT`].
pub(crate) fn set_memory_limit(bytes: usize) {
    MEMORY_LIMIT.store(bytes, Ordering::Relaxed);
}

/// How many quads a sorter keeps in memory before it writes them out as a sorted segment:
/// half the memory limit, at 32 bytes a quad (sorting in place needs no more).
fn sort_chunk() -> usize {
    (MEMORY_LIMIT.load(Ordering::Relaxed) / 2 / std::mem::size_of::<Key>()).max(4096)
}

/// Sorts quads that may not fit in memory (an external sort, #218): they are collected in
/// chunks of [`sort_chunk`] quads, and each full chunk is sorted and written as a segment
/// file. Without a file to write to (an in-memory store) or a memory limit, every quad stays
/// in memory.
pub(crate) struct Sorter {
    chunk: Vec<Key>,
    cap: usize,
    runs: Vec<Arc<Segment>>,
    spill: Option<Arc<crate::persist::Spill>>,
    /// Quads pushed, duplicates included.
    pushed: usize,
}

impl Sorter {
    pub(crate) fn new(spill: Option<Arc<crate::persist::Spill>>) -> Self {
        let cap = if spill.is_some() {
            sort_chunk()
        } else {
            usize::MAX
        };
        Self {
            chunk: Vec::new(),
            cap,
            runs: Vec::new(),
            spill,
            pushed: 0,
        }
    }

    /// A sorter that keeps `1/parts` of the quads [`Sorter::new`] would in memory, for
    /// `parts` sorters at work at once.
    pub(crate) fn with_share(spill: Option<Arc<crate::persist::Spill>>, parts: usize) -> Self {
        let mut sorter = Self::new(spill);
        if sorter.cap != usize::MAX {
            sorter.cap = (sorter.cap / parts.max(1)).max(4096);
        }
        sorter
    }

    #[inline]
    pub(crate) fn push(&mut self, k: Key) {
        self.chunk.push(k);
        self.pushed += 1;
        if self.chunk.len() >= self.cap {
            self.write_chunk();
        }
    }

    /// Where the sorter writes its chunks.
    pub(crate) fn spill(&self) -> Option<&Arc<crate::persist::Spill>> {
        self.spill.as_ref()
    }

    /// Quads pushed so far, duplicates included.
    pub(crate) fn len(&self) -> usize {
        self.pushed
    }

    /// Whether some quads were written out: then [`Sorter::into_vec`] cannot be used.
    pub(crate) fn spilled(&self) -> bool {
        !self.runs.is_empty()
    }

    fn write_chunk(&mut self) {
        let mut chunk = std::mem::take(&mut self.chunk);
        chunk.par_sort_unstable();
        chunk.dedup();
        let segment = Segment::from_sorted_slice_in(&chunk, self.spill.as_deref());
        self.runs.push(Arc::new(segment));
    }

    /// The quads, unsorted, when none were written out.
    pub(crate) fn into_vec(self) -> Vec<Key> {
        assert!(!self.spilled(), "the quads are in memory");
        self.chunk
    }

    /// The quads in order, without duplicates, merged with `extra` (in order too).
    fn sorted(mut self, extra: Option<Arc<Segment>>) -> impl Iterator<Item = Key> {
        if !self.chunk.is_empty() {
            self.write_chunk();
        }
        let mut runs = self.runs;
        runs.extend(extra);
        MergeSorted::new(runs.iter().map(Segment::iter).collect())
    }
}

/// How many index orders a bulk load larger than the memory limit sorts at once (#218).
/// Sorting them side by side keeps more cores busy, but the sorted runs of each order wait on
/// disk until its merge: four at once was as fast as all seven on 4 cores (20M triples) and
/// bounds the extra temporary disk to three more orders' runs than sorting one at a time.
const SORTED_AT_ONCE: usize = 4;

/// The keys of sorted segments, merged, without duplicates.
struct MergeSorted {
    heads: std::collections::BinaryHeap<std::cmp::Reverse<(Key, usize)>>,
    iters: Vec<crate::segment::SegmentIter>,
    last: Option<Key>,
}

impl MergeSorted {
    fn new(mut iters: Vec<crate::segment::SegmentIter>) -> Self {
        let heads = iters
            .iter_mut()
            .enumerate()
            .filter_map(|(i, it)| Some(std::cmp::Reverse((it.next()?, i))))
            .collect();
        Self {
            heads,
            iters,
            last: None,
        }
    }
}

impl Iterator for MergeSorted {
    type Item = Key;

    fn next(&mut self) -> Option<Key> {
        loop {
            let std::cmp::Reverse((k, i)) = self.heads.pop()?;
            if let Some(next) = self.iters[i].next() {
                self.heads.push(std::cmp::Reverse((next, i)));
            }
            if self.last != Some(k) {
                self.last = Some(k);
                return Some(k);
            }
        }
    }
}

impl QuadIndex {
    /// Builds every permutation from quads in any order (duplicates allowed).
    ///
    /// The permutations are built one after another in the buffer `quads` came in, each read
    /// back from the first segment, so the build needs one key array beyond the segments
    /// rather than one per permutation.
    pub(crate) fn from_quads(mut quads: Vec<Key>) -> Self {
        quads.par_sort_unstable();
        quads.dedup();
        let len = quads.len();
        let mut segments: [Option<Arc<Segment>>; PERMS] = Default::default();
        segments[SPOG] = Some(Arc::new(Segment::from_sorted_slice(&quads)));
        // Each other permutation is another one with its first column moved to the front: the
        // other one's order, sorted by that column alone (stably), is this one's order.
        for (perm, source) in DERIVED {
            let from = inverse(&ORDERS[source]);
            let order = &ORDERS[perm];
            segments[source]
                .as_ref()
                .expect("built before")
                .decode_all(&mut quads);
            quads.par_iter_mut().for_each(|k| {
                let q = [k[from[0]], k[from[1]], k[from[2]], k[from[3]]];
                *k = permute(&q, order);
            });
            quads.par_sort_by_key(|k| k[0]);
            segments[perm] = Some(Arc::new(Segment::from_sorted_slice(&quads)));
        }
        let perms: Vec<Perm> = segments
            .into_iter()
            .map(|s| Perm {
                segment: s.expect("all built"),
                ..Perm::default()
            })
            .collect();
        Self {
            perms: perms.try_into().unwrap_or_else(|_| unreachable!()),
            len,
            spill: None,
            pinned: false,
        }
    }

    /// Builds every permutation from the quads of `sorter` (in any order, duplicates
    /// allowed) and the quads of `current`, if any. When the sorter wrote its quads out,
    /// each permutation is built by an external sort, so no more than [`sort_chunk`] quads
    /// are in memory at a time and the segments are written straight to files (#218).
    pub(crate) fn from_sorter(sorter: Sorter, current: Option<&QuadIndex>) -> Self {
        let spill = sorter.spill.clone();
        if !sorter.spilled() {
            let mut quads = sorter.into_vec();
            if let Some(current) = current {
                quads.extend(current.iter());
            }
            return Self::from_quads(quads);
        }
        // The current quads, in order, as one more sorted input.
        let extra = current
            .filter(|c| c.len() > 0)
            .map(|c| Arc::new(Segment::from_sorted_in(c.iter(), spill.as_deref())));
        let spog = Arc::new(Segment::from_sorted_in(
            sorter.sorted(extra),
            spill.as_deref(),
        ));
        let len = spog.len;
        // Every other permutation is sorted from SPOG, [`SORTED_AT_ONCE`] at a time: each has
        // its share of the memory for its chunks, so the merges and the writing of the
        // segments, which run on one thread each, run side by side.
        let derived: Vec<(usize, Arc<Segment>)> = DERIVED
            .chunks(SORTED_AT_ONCE)
            .flat_map(|orders| {
                orders
                    .par_iter()
                    .map(|&(perm, _)| {
                        let order = &ORDERS[perm];
                        let mut sorter = Sorter::with_share(spill.clone(), orders.len());
                        for k in spog.iter() {
                            sorter.push(permute(&k, order));
                        }
                        let segment =
                            Segment::from_sorted_in(sorter.sorted(None), spill.as_deref());
                        (perm, Arc::new(segment))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut segments: [Option<Arc<Segment>>; PERMS] = Default::default();
        segments[SPOG] = Some(spog);
        for (perm, segment) in derived {
            segments[perm] = Some(segment);
        }
        let perms: Vec<Perm> = segments
            .into_iter()
            .map(|s| Perm {
                segment: s.expect("all built"),
                ..Perm::default()
            })
            .collect();
        Self {
            perms: perms.try_into().unwrap_or_else(|_| unreachable!()),
            len,
            spill,
            pinned: false,
        }
    }

    /// From saved segments in [`ORDERS`] order. Stores saved with only the first six get the
    /// others built from SPOG here.
    /// Takes `segments`, the same quads as this index's segments as read back from a
    /// checkpoint (mapped), in their place, keeping their counts: the copies on the heap are
    /// let go. Nothing changes if the index has a delta or the segments differ in number or
    /// length.
    pub(crate) fn use_saved(&mut self, segments: Vec<(Segment, Vec<(Segment, Segment)>)>) {
        let same = |a: &Segment, b: &Segment| a.len == b.len;
        if self.delta_len() > 0
            || segments.len() != self.perms.len()
            || segments.iter().zip(&self.perms).any(|((s, runs), p)| {
                !same(s, &p.segment)
                    || runs.len() != p.runs.len()
                    || runs
                        .iter()
                        .zip(p.runs.iter())
                        .any(|((a, r), run)| !same(a, &run.added) || !same(r, &run.removed))
            })
        {
            return;
        }
        let keep = |saved: Segment, old: &Segment| {
            if let Some(stats) = old.stats.get() {
                let _ = saved.stats.set(Arc::clone(stats));
            }
            Arc::new(saved)
        };
        for ((saved, runs), perm) in segments.into_iter().zip(&mut self.perms) {
            perm.segment = keep(saved, &perm.segment);
            perm.runs = runs
                .into_iter()
                .zip(perm.runs.iter())
                .map(|((a, r), run)| Run {
                    added: keep(a, &run.added),
                    removed: keep(r, &run.removed),
                    id: run.id,
                    filter: Arc::clone(&run.filter),
                })
                .collect();
        }
    }

    /// Sets the runs of each permutation, as read back from a checkpoint.
    pub(crate) fn set_runs(&mut self, runs: Vec<Vec<(Segment, Segment)>>) {
        for (perm, runs) in self.perms.iter_mut().zip(runs) {
            perm.runs = runs
                .into_iter()
                .map(|(a, r)| Run::new(Arc::new(a), Arc::new(r), new_id()))
                .collect();
            perm.base_id = new_id();
        }
        self.len = self.stored_len(0);
    }

    /// Whether some permutation has runs that no merge in the background is folding in, and
    /// enough of them to be worth rewriting its segment for ([`MERGE_RATIO`]).
    pub(crate) fn has_unmerged_runs(&self) -> bool {
        self.perms
            .iter()
            .any(|p| p.runs.len() > p.frozen && p.wants_merge())
    }

    /// The quads in the runs and the delta of SPOG, added and removed: an upper bound of the
    /// changes since the segment was written.
    pub(crate) fn unmerged_len(&self) -> usize {
        self.perms[SPOG].runs.iter().map(Run::len).sum::<usize>() + self.delta_len()
    }

    /// Runs of every permutation.
    pub(crate) fn run_count(&self) -> usize {
        self.perms.iter().map(|p| p.runs.len()).sum()
    }

    /// Whether some permutation has runs that a merge when the store is idle folds in: those
    /// of segments up to [`IDLE_MERGE_MAX`] quads, once they hold one quad per
    /// [`IDLE_MERGE_RATIO`].
    pub(crate) fn has_idle_runs(&self) -> bool {
        self.perms.iter().any(Perm::idle_merges)
    }

    /// [`QuadIndex::start_merge`] for every permutation with the runs
    /// [`QuadIndex::has_idle_runs`] looks for: for a merge when the store is idle (#442).
    pub(crate) fn start_merge_idle(&mut self) -> QuadIndex {
        for perm in &mut self.perms {
            perm.frozen = if perm.idle_merges() {
                perm.runs.len()
            } else {
                0
            };
        }
        let mut index = self.clone();
        for perm in &mut index.perms {
            if perm.frozen == 0 {
                perm.runs = Arc::default();
            }
        }
        index
    }

    /// Starts a merge in the background of the runs of every permutation that has enough of
    /// them into its segment: marks them so compactions leave them be, and returns the index
    /// to merge, in which the other permutations have no runs.
    pub(crate) fn start_merge(&mut self) -> QuadIndex {
        for perm in &mut self.perms {
            perm.frozen = if perm.wants_merge() {
                perm.runs.len()
            } else {
                0
            };
        }
        let mut index = self.clone();
        for perm in &mut index.perms {
            if perm.frozen == 0 {
                perm.runs = Arc::default();
            }
        }
        index
    }

    /// The segments of `self` (from [`QuadIndex::start_merge`]) with their runs folded in,
    /// on two threads of their own at the lowest priority, so that they take only the CPU
    /// time queries leave.
    /// Merges the runs of every permutation into its segment; `None` when `stop` was set
    /// meanwhile.
    pub(crate) fn merge_runs(&self, stop: &AtomicBool) -> Option<Vec<MergedPerm>> {
        let spill = self.spill.as_deref();
        let next = AtomicUsize::new(0);
        // Two at most: the merge also takes memory bandwidth and page cache from queries.
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(2));
        let mut merged: Vec<(usize, MergedPerm)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        lower_priority();
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(p) = self.perms.get(i) else { break };
                            done.push((
                                i,
                                if p.runs.is_empty() {
                                    MergedPerm::default()
                                } else {
                                    match p.merged_runs(spill, stop) {
                                        Some(m) => m,
                                        None => break,
                                    }
                                },
                            ));
                        }
                        done
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap_or_default())
                .collect()
        });
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        merged.sort_by_key(|(i, _)| *i);
        // A permutation a worker failed on is left out: its runs stay.
        let mut out: Vec<MergedPerm> = (0..PERMS).map(|_| MergedPerm::default()).collect();
        for (i, m) in merged {
            out[i] = m;
        }
        Some(out)
    }

    /// Puts the segments of a finished merge in place of the segments and runs they were
    /// built from, in each permutation whose segment and first runs are still those (the
    /// runs that came after stay); the others are left as they are. The quads don't change.
    pub(crate) fn finish_merge(&mut self, merged: Vec<MergedPerm>) {
        for perm in &mut self.perms {
            perm.frozen = 0;
        }
        for (perm, merged) in self.perms.iter_mut().zip(merged) {
            let same = !merged.runs.is_empty()
                && perm.base_id == merged.base_id
                && perm.runs.len() >= merged.runs.len()
                && perm
                    .runs
                    .iter()
                    .zip(&merged.runs)
                    .all(|(r, id)| r.id == *id);
            if !same {
                continue;
            }
            let rest: Arc<[Run]> = perm.runs[merged.runs.len()..].into();
            perm.segment = merged.segment;
            perm.base_id = new_id();
            perm.runs = rest;
            // With runs left, their counts stay those of the same quads; without, the new
            // segment has its own.
            if perm.runs.is_empty() {
                perm.runs_stats = Arc::default();
            }
        }
    }

    /// Every quad, `[s, p, o, g]`, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Key> {
        self.perms[0].stored_range([0; 4], [u64::MAX; 4])
    }

    /// The counts of permutation `perm` with its runs but not its delta, as saved.
    pub(crate) fn stored_stats(&self, perm: usize) -> Arc<crate::stats::SegStats> {
        self.perms[perm].stored_stats()
    }

    /// Quads of permutation `perm` without the delta, to check saved counts against.
    pub(crate) fn stored_len(&self, perm: usize) -> usize {
        let p = &self.perms[perm];
        p.segment.len + p.runs.iter().map(|r| r.added.len).sum::<usize>()
            - p.runs.iter().map(|r| r.removed.len).sum::<usize>()
    }

    /// Sets the counts of permutation `perm` with its runs, read from a checkpoint.
    pub(crate) fn set_stored_stats(&self, perm: usize, stats: crate::stats::SegStats) {
        let p = &self.perms[perm];
        let stats = Arc::new(stats);
        if p.runs.is_empty() {
            let _ = p.segment.stats.set(stats);
        } else {
            let _ = p.runs_stats.set(stats);
        }
    }

    pub(crate) fn from_segments(segments: Vec<Segment>) -> Self {
        let len = segments.first().map_or(0, |s| s.len);
        let mut segments: Vec<Arc<Segment>> = segments.into_iter().map(Arc::new).collect();
        if segments.len() < PERMS
            && let Some(first) = segments.first()
        {
            let built: Vec<Arc<Segment>> = ORDERS[segments.len()..]
                .par_iter()
                .map(|order| {
                    let mut quads: Vec<Key> = first.iter().map(|q| permute(&q, order)).collect();
                    quads.par_sort_unstable();
                    Arc::new(Segment::from_sorted(quads))
                })
                .collect();
            segments.extend(built);
        }
        let perms: Vec<Perm> = segments
            .into_iter()
            .map(|segment| Perm {
                segment,
                ..Perm::default()
            })
            .collect();
        Self {
            perms: perms.try_into().unwrap_or_else(|_| unreachable!()),
            len,
            spill: None,
            pinned: false,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Whether every object of predicate `p`, in any graph, is an IRI or a blank node: from
    /// the kinds counted per predicate in POSG (which still count removed quads, so they only
    /// err towards `false`) and the quads added since. `None` when the counts are not kept
    /// per predicate (more than [`crate::stats::MAX_GROUPS`] predicates).
    pub(crate) fn objects_are_resources(&self, p: u64) -> Option<bool> {
        let perm = &self.perms[POSG];
        let resource = |kind: usize| kind == 1 || kind == 2;
        let stats = perm.stored_stats();
        if let Some(group) = stats.group(p)? {
            let counts = &group.kinds1.rows;
            if (0..counts.len()).any(|k| counts[k] > 0 && !resource(k)) {
                return Some(false);
            }
        }
        if !perm.added.is_empty() {
            let added = &perm.flat().added;
            let (a, b) = Flat::bounds(added, [p, 0, 0, 0], [p, u64::MAX, u64::MAX, u64::MAX]);
            if added[a..b].iter().any(|k| !resource((k[1] & 0xf) as usize)) {
                return Some(false);
            }
        }
        Some(true)
    }

    /// The counts of permutation `perm` (an index into [`ORDERS`]), with its delta applied.
    /// `None` when the delta is too large to adjust them for (more than [`STATS_DELTA_MAX`]
    /// quads): count queries then scan until compaction folds it in.
    pub(crate) fn stats(&self, perm: usize) -> Option<Arc<crate::stats::SegStats>> {
        let p = &self.perms[perm];
        if !p.has_delta() {
            return Some(p.stored_stats());
        }
        p.cache
            .stats
            .get_or_init(|| {
                (p.added.len() + p.removed.len() <= STATS_DELTA_MAX).then(|| {
                    Arc::new(
                        p.stored_stats()
                            .with_delta(&|low, high| p.stored_count(low, high), &p.flat().merged()),
                    )
                })
            })
            .clone()
    }

    /// The counts of permutation `perm`'s segment with its runs, whether or not it has a
    /// delta.
    pub(crate) fn segment_stats(&self, perm: usize) -> Arc<crate::stats::SegStats> {
        self.perms[perm].stored_stats()
    }

    /// The counts of permutation `perm` with its delta: the segment's own while the delta is
    /// under 1 % of the segment (then they are approximate), else counted by reading the
    /// permutation in order. For descriptions such as VoID, not for answering queries.
    pub(crate) fn current_stats(
        &self,
        perm: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Arc<crate::stats::SegStats>> {
        let p = &self.perms[perm];
        let delta = p.added.len() + p.removed.len();
        let stats = p.stored_stats();
        if delta == 0 || (delta as u64).saturating_mul(100) < stats.rows {
            return Some(stats);
        }
        let order = ORDERS[perm];
        let mut builder = crate::stats::StatsBuilder::default();
        for (i, q) in self
            .range_scan(perm, [0; 4], [u64::MAX; 4], false)
            .enumerate()
        {
            if i % 65_536 == 0 && cancelled() {
                return None;
            }
            builder.push(&permute(&q, &order));
        }
        Some(Arc::new(builder.finish()))
    }

    /// How many quads one value of the subject (`subject`) or object side of predicate `p`
    /// has, weighted by how often values come up: from the counts of PSOG / POSG, for all
    /// graphs. The segment's numbers also stand while a delta exists (a fanout changes
    /// slowly). `None` when the segment doesn't hold `p` or keeps no per-predicate counts.
    pub(crate) fn fanout(&self, p: u64, subject: bool) -> Option<u64> {
        let stats = self.segment_stats(if subject { PSOG } else { 1 });
        Some(stats.group(p)??.fanout1())
    }

    pub(crate) fn delta_len(&self) -> usize {
        self.perms[0].added.len() + self.perms[0].removed.len()
    }

    pub(crate) fn heap_size(&self) -> usize {
        self.perms.iter().map(Perm::stored_heap_size).sum::<usize>() + self.delta_len() * PERMS * 48
    }

    /// Bytes of checkpoint files the segments are mapped from.
    pub(crate) fn mapped_size(&self) -> usize {
        self.perms.iter().map(Perm::stored_mapped_size).sum()
    }

    pub(crate) fn contains(&self, q: &Key) -> bool {
        let p = &self.perms[0];
        if p.added.contains(q) {
            return true;
        }
        !p.removed.contains(q) && p.stored_contains(q)
    }

    /// Adds a quad (`[s, p, o, g]`); returns false if it was already there.
    pub(crate) fn insert(&mut self, q: Key) -> bool {
        let base = &self.perms[0];
        let in_segment = if base.removed.contains(&q) {
            true
        } else if base.added.contains(&q) || base.stored_contains(&q) {
            return false;
        } else {
            false
        };
        for (perm, order) in self.perms.iter_mut().zip(&ORDERS) {
            let k = permute(&q, order);
            perm.written();
            if in_segment {
                perm.removed.remove(&k);
            } else {
                perm.added.insert(k);
            }
        }
        self.len += 1;
        true
    }

    /// Removes a quad; returns false if it was not there.
    pub(crate) fn remove(&mut self, q: Key) -> bool {
        let base = &self.perms[0];
        let in_added = base.added.contains(&q);
        if !in_added && (base.removed.contains(&q) || !base.stored_contains(&q)) {
            return false;
        }
        for (perm, order) in self.perms.iter_mut().zip(&ORDERS) {
            let k = permute(&q, order);
            perm.written();
            if in_added {
                perm.added.remove(&k);
            } else {
                perm.removed.insert(k);
            }
        }
        self.len -= 1;
        true
    }

    /// Folds the deltas into new segments.
    pub(crate) fn compact(&mut self) {
        if self.delta_len() == 0 {
            return;
        }
        let spill = self.spill.clone();
        let pinned = self.pinned;
        self.perms.par_iter_mut().for_each(|perm| {
            *perm = perm.folded(&perm.flat().merged(), spill.as_deref(), pinned);
        });
    }

    /// Whether adding `n` quads to a compacted index is cheaper by merging them into the
    /// touched blocks ([`Self::merge_in`]) than by rebuilding every permutation.
    /// A pinned index always merges them in, so that its base is not copied.
    pub(crate) fn merges_in(&self, n: usize) -> bool {
        self.len > 0 && (self.pinned || n * PARTIAL_RATIO <= self.len)
    }

    /// Adds `quads` (sorted, distinct and not in the index) to a compacted index by
    /// rewriting only the blocks they touch in each permutation (#213).
    pub(crate) fn merge_in(&mut self, quads: &[Key]) {
        debug_assert_eq!(self.delta_len(), 0);
        self.len += quads.len();
        self.fold_spog(quads.iter().map(|q| (*q, 1)).collect());
    }

    /// Applies `changes` (`true`: insert, `false`: remove; each quad at most once) and
    /// folds them with the deltas into new segments, in one pass over each permutation:
    /// for a write that would fill the delta anyway, cheaper than inserting it into the
    /// persistent sets and compacting after. Returns the time the lookups of the changes in
    /// the stored quads took, for the slow-commit log (#437).
    pub(crate) fn fold_in(&mut self, changes: &[(Key, bool)]) -> std::time::Duration {
        let start = std::time::Instant::now();
        let current = self.perms[0].flat().merged();
        // Which changes change anything, looked up in parallel: a large write is mostly
        // misses in the segment, each a seek (#292).
        let changes: Vec<(Key, i64)> = changes
            .par_iter()
            .filter(|(q, present)| *present != self.contains(q))
            .map(|&(q, present)| (q, if present { 1 } else { -1 }))
            .collect();
        let looked_up = start.elapsed();
        self.len = (self.len as i64 + changes.iter().map(|c| c.1).sum::<i64>()) as usize;
        // Without a delta (the usual case: a large write after one that compacted), the
        // changes are the delta as they are.
        let mut delta: Vec<(Key, i64)> = if current.is_empty() {
            changes
        } else {
            let mut delta: rustc_hash::FxHashMap<Key, i64> = current.into_iter().collect();
            for (q, sign) in changes {
                // Undoing a change of the delta leaves the segment's state.
                match delta.entry(q) {
                    std::collections::hash_map::Entry::Occupied(e) => {
                        debug_assert_eq!(*e.get(), -sign);
                        e.remove();
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(sign);
                    }
                }
            }
            delta.into_iter().collect()
        };
        delta.par_sort_unstable();
        self.fold_spog(delta);
        looked_up
    }

    /// Folds `delta`, the sorted SPOG changes relative to the segments, into every
    /// permutation, leaving no delta. `self.len` must already count it.
    fn fold_spog(&mut self, delta: Vec<(Key, i64)>) {
        let spill = self.spill.clone();
        let pinned = self.pinned;
        self.perms.par_iter_mut().enumerate().for_each(|(i, perm)| {
            *perm = if i == 0 {
                perm.folded(&delta, spill.as_deref(), pinned)
            } else {
                let mut keys: Vec<(Key, i64)> = delta
                    .iter()
                    .map(|(q, d)| (permute(q, &ORDERS[i]), *d))
                    .collect();
                keys.par_sort_unstable();
                perm.folded(&keys, spill.as_deref(), pinned)
            };
        });
    }

    /// Whether a quad is in a named graph: one seek.
    pub(crate) fn has_named_graphs(&self) -> bool {
        self.range_scan(3, [1, 0, 0, 0], [u64::MAX; 4], false)
            .next()
            .is_some()
    }

    /// The ids of the named graphs, in order: one seek per graph, not a scan of its quads.
    pub(crate) fn graph_ids(&self) -> Vec<u64> {
        let mut graphs = Vec::new();
        let mut from = 1;
        while let Some(q) = self
            .range_scan(3, [from, 0, 0, 0], [u64::MAX; 4], false)
            .next()
        {
            graphs.push(q[3]);
            match q[3].checked_add(1) {
                Some(next) => from = next,
                None => break,
            }
        }
        graphs
    }

    /// Whether every graph holding a quad is in `ids` (sorted; the default graph is id 0):
    /// one seek per graph of `ids` at most, stopping at the first graph outside them.
    pub(crate) fn graphs_within(&self, ids: &[u64]) -> bool {
        if ids.binary_search(&0).is_err()
            && self
                .range_scan(3, [0; 4], [0, u64::MAX, u64::MAX, u64::MAX], false)
                .next()
                .is_some()
        {
            return false;
        }
        let mut from = 1;
        while let Some(q) = self
            .range_scan(3, [from, 0, 0, 0], [u64::MAX; 4], false)
            .next()
        {
            if ids.binary_search(&q[3]).is_err() {
                return false;
            }
            match q[3].checked_add(1) {
                Some(next) => from = next,
                None => break,
            }
        }
        true
    }

    /// The quads of `self` that `before` lacks, and those of `before` that `self` lacks, as
    /// `[s, p, o, g]`: one merge of their SPOG scans. Their ids must mean the same terms (one
    /// store forked from the other, see [`crate::Store::fork`]).
    pub(crate) fn difference(&self, before: &QuadIndex) -> (Vec<Key>, Vec<Key>) {
        let mut a = self
            .range_scan(SPOG, [0; 4], [u64::MAX; 4], false)
            .peekable();
        let mut b = before
            .range_scan(SPOG, [0; 4], [u64::MAX; 4], false)
            .peekable();
        let (mut added, mut removed) = (Vec::new(), Vec::new());
        loop {
            match (a.peek(), b.peek()) {
                (None, None) => break,
                (Some(x), None) => {
                    added.push(*x);
                    a.next();
                }
                (None, Some(y)) => {
                    removed.push(*y);
                    b.next();
                }
                (Some(x), Some(y)) => match x.cmp(y) {
                    std::cmp::Ordering::Less => {
                        added.push(*x);
                        a.next();
                    }
                    std::cmp::Ordering::Greater => {
                        removed.push(*y);
                        b.next();
                    }
                    std::cmp::Ordering::Equal => {
                        a.next();
                        b.next();
                    }
                },
            }
        }
        (added, removed)
    }

    /// Quads matching `pattern`, as `[s, p, o, g]`.
    pub(crate) fn scan(&self, pattern: Pattern) -> Scan {
        let (perm, low, high, named_only) = Self::bounds(pattern);
        self.range_scan(perm, low, high, named_only)
    }

    /// Quads matching `pattern`, sorted by column `by` (`Some`: 0 subject, 1 predicate, 2 object) where
    /// one permutation gives that order with the pattern's bound terms first. Otherwise in the
    /// order of [`QuadIndex::scan`].
    pub(crate) fn scan_sorted(&self, pattern: Pattern, by: Option<usize>) -> Scan {
        let (perm, low, high, named_only) = Self::bounds(pattern);
        // `?s <p> ?o` reads POSG / GPOS, sorted by object: PSOG / GPSO give it by subject.
        let perm =
            match perm {
                1 | 4 if by == Some(0) && pattern.s.is_none() && pattern.o.is_none() => {
                    if perm == 1 { PSOG } else { GPSO }
                }
                // Nothing bound but the graph: the order of the position asked for (#243).
                0 | 3
                    if pattern.s.is_none()
                        && pattern.p.is_none()
                        && pattern.o.is_none()
                        && matches!(by, Some(1 | 2))
                        && (perm == 3) == pattern.g.is_some() =>
                {
                    match (perm, by) {
                        (0, Some(1)) => POSG,
                        (0, _) => OSPG,
                        (_, Some(1)) => GPOS,
                        _ => GOSP,
                    }
                }
                _ => return self.range_scan(perm, low, high, named_only),
            };
        // The same bound prefix (graph, predicate), in the new order.
        let (low, high) = Self::prefix_range(perm, pattern);
        self.range_scan(perm, low, high, named_only)
    }

    /// The key range of permutation `perm` holding the quads with `pattern`'s bound terms, as
    /// far as they form a prefix of its order.
    fn prefix_range(perm: usize, pattern: Pattern) -> (Key, Key) {
        let bound = [pattern.s, pattern.p, pattern.o, pattern.g];
        let mut low = [0; 4];
        let mut high = [u64::MAX; 4];
        for (i, &col) in ORDERS[perm].iter().enumerate() {
            match bound[col] {
                Some(v) => {
                    low[i] = v;
                    high[i] = v;
                }
                None => break,
            }
        }
        (low, high)
    }

    /// How many quads match `pattern`, without reading them: the block index of the
    /// permutation [`QuadIndex::scan`] would read, plus the delta. Exact, except that a pattern
    /// over named graphs only with a bound subject, predicate or object also counts matches in
    /// the default graph once there are more than a few hundred.
    pub(crate) fn estimate(&self, pattern: Pattern) -> usize {
        let (perm, low, high, named_only) = Self::bounds(pattern);
        let data = &self.perms[perm];
        let segment = data.stored_count(low, high) as i64;
        let total = (segment + data.delta_count(low, high)).max(0) as usize;
        if named_only && total <= 256 {
            return self.scan(pattern).count();
        }
        total
    }

    /// The permutation a pattern is read from, the key range, and whether default-graph quads
    /// in that range are to be skipped.
    fn bounds(pattern: Pattern) -> (usize, Key, Key, bool) {
        let Pattern {
            s,
            p,
            o,
            g,
            all_graphs,
        } = pattern;
        let spo = [s, p, o];
        // (permutation, columns of the permutation that are bound, in order)
        let (perm, named_only) = match g {
            Some(_) => (
                match (s.is_some(), p.is_some(), o.is_some()) {
                    (_, true, true) if s.is_none() => 4,
                    (false, true, false) => 4,
                    (true, false, true) | (false, false, true) => 5,
                    _ => 3,
                },
                false,
            ),
            None => {
                if spo.iter().all(Option::is_none) {
                    if all_graphs { (0, false) } else { (3, true) }
                } else {
                    (
                        match (s.is_some(), p.is_some(), o.is_some()) {
                            (false, true, _) => 1,
                            (true, false, true) | (false, false, true) => 2,
                            _ => 0,
                        },
                        !all_graphs,
                    )
                }
            }
        };
        let (mut low, high) = Self::prefix_range(perm, pattern);
        if perm == 3 && g.is_none() && !all_graphs {
            // Every named graph: graph ids above the default graph's 0.
            low[0] = 1;
        }
        (perm, low, high, named_only)
    }

    fn range_scan(&self, perm: usize, low: Key, high: Key, skip_default: bool) -> Scan {
        let p = &self.perms[perm];
        // Only the part of the delta inside the range counts: a scan that the delta doesn't
        // touch reads the segment alone.
        let delta = p
            .has_delta()
            .then(|| {
                let (added, removed) = p.delta_cursors(low, high);
                (added.is_some() || removed.is_some()).then(|| Delta::new(added, removed))
            })
            .flatten();
        let fixed = (0..4).take_while(|&i| low[i] == high[i]).count();
        Scan {
            range: p.segment.range_of(low, high),
            segment: Arc::clone(&p.segment),
            runs: RunMerge::new(&p.runs, &p.segment, low, high).map(Box::new),
            delta,
            from: inverse(&ORDERS[perm]),
            skip_default,
            rows: None,
            filtered: skip_default,
            low,
            fixed,
            sorted_by: ORDERS[perm].get(fixed).copied(),
            chunk: Vec::new(),
        }
    }
}

/// `segment` with `delta` (its sorted changes: `1` added, `-1` removed) folded in. A delta
/// small next to the segment touches few blocks, so only those are rewritten (#213); a
/// segment left with many part-full blocks, or a large delta, is rebuilt in full.
fn fold(
    segment: &Arc<Segment>,
    delta: &[(Key, i64)],
    spill: Option<&crate::persist::Spill>,
) -> Arc<Segment> {
    if delta.len() * PARTIAL_RATIO <= segment.len {
        let merged = Arc::new(segment.merged(delta, spill));
        return if merged.is_fragmented() {
            Arc::new(Segment::from_sorted_in(merged.iter(), spill))
        } else {
            merged
        };
    }
    let mut counts = crate::stats::StatsBuilder::default();
    let rebuilt = Segment::from_sorted_in(
        FoldIn {
            seg: segment.iter().peekable(),
            changes: delta.iter().copied().peekable(),
        }
        .inspect(|k| counts.push(k)),
        spill,
    );
    let _ = rebuilt.stats.set(Arc::new(counts.finish()));
    Arc::new(rebuilt)
}

/// The changes of `runs` and `delta` together, in order, as one sorted list of changes to
/// the segment: `1` added, `-1` removed. A change of `delta` that undoes one of a run leaves
/// neither.
fn signed_merge<'a>(
    runs: &[Run],
    delta: &'a [(Key, i64)],
) -> impl Iterator<Item = (Key, i64)> + 'a {
    let mut parts: Vec<Box<dyn Iterator<Item = (Key, i64)> + 'a>> = Vec::new();
    for run in runs {
        parts.push(Box::new(run.added.iter().map(|k| (k, 1))));
        parts.push(Box::new(run.removed.iter().map(|k| (k, -1))));
    }
    parts.push(Box::new(delta.iter().copied()));
    let mut heads: Vec<Option<(Key, i64)>> = parts.iter_mut().map(Iterator::next).collect();
    std::iter::from_fn(move || {
        loop {
            let key = heads.iter().flatten().map(|h| h.0).min()?;
            let mut sum = 0;
            for (head, part) in heads.iter_mut().zip(&mut parts) {
                if let Some((k, sign)) = *head
                    && k == key
                {
                    sum += sign;
                    *head = part.next();
                }
            }
            if sum != 0 {
                return Some((key, sum.signum()));
            }
        }
    })
}

/// A segment's quads with sorted changes to it folded in: added quads are not in it, removed
/// ones are.
struct FoldIn<S: Iterator<Item = Key>, C: Iterator<Item = (Key, i64)>> {
    seg: std::iter::Peekable<S>,
    changes: std::iter::Peekable<C>,
}

impl<S: Iterator<Item = Key>, C: Iterator<Item = (Key, i64)>> Iterator for FoldIn<S, C> {
    type Item = Key;

    fn next(&mut self) -> Option<Key> {
        loop {
            match (self.seg.peek(), self.changes.peek()) {
                (None, None) => return None,
                (Some(s), Some((c, sign))) if c <= s => {
                    let (c, sign) = (*c, *sign);
                    self.changes.next();
                    if sign > 0 {
                        return Some(c);
                    }
                    // Removed: it is the segment's next quad.
                    self.seg.next();
                }
                (Some(_), _) => return self.seg.next(),
                (None, Some(_)) => {
                    let (c, sign) = self.changes.next()?;
                    if sign > 0 {
                        return Some(c);
                    }
                }
            }
        }
    }
}

/// The keys of one segment of a run inside a scan's range, read ahead by one.
struct RunCursor {
    segment: Arc<Segment>,
    range: Range,
    head: Option<Key>,
}

impl RunCursor {
    /// `None` if `segment` has no keys in `low..=high`.
    fn new(segment: &Arc<Segment>, low: Key, high: Key) -> Option<Self> {
        let mut range = segment.range_of(low, high);
        let head = Some(range.next(segment)?);
        Some(Self {
            segment: Arc::clone(segment),
            range,
            head,
        })
    }

    fn advance(&mut self) {
        self.head = self.range.next(&self.segment);
    }

    fn seek(&mut self, key: Key) {
        if self.head.is_some_and(|h| h < key) {
            self.range.seek(&self.segment, key);
            self.head = self.range.next(&self.segment);
        }
    }
}

/// The state of merging the runs of a permutation into a scan of its segment.
struct RunMerge {
    added: Vec<RunCursor>,
    removed: Vec<RunCursor>,
    /// A segment key read but not returned yet, because an added key came first.
    pending: Option<Key>,
    /// Buffers for [`RunMerge::next_chunk`].
    chunk: Vec<Key>,
    added_keys: Vec<Key>,
    removed_keys: Vec<Key>,
}

impl RunMerge {
    /// `None` when no run has keys in `low..=high`.
    fn new(runs: &[Run], segment: &Segment, low: Key, high: Key) -> Option<Self> {
        let mut added = Vec::new();
        let mut removed = Vec::new();
        for r in runs.iter().filter(|r| r.may_hold(segment, &low, &high)) {
            added.extend(RunCursor::new(&r.added, low, high));
            removed.extend(RunCursor::new(&r.removed, low, high));
        }
        (!added.is_empty() || !removed.is_empty()).then_some(Self {
            added,
            removed,
            pending: None,
            chunk: Vec::new(),
            added_keys: Vec::new(),
            removed_keys: Vec::new(),
        })
    }

    /// The next key of the segment's `range` with the runs merged in.
    #[inline]
    fn next(&mut self, range: &mut Range, segment: &Segment) -> Option<Key> {
        loop {
            let k = match self.pending.take() {
                Some(k) => Some(k),
                None => range.next(segment),
            };
            // The smallest added key, if it comes first: added keys are in no segment.
            let first = self
                .added
                .iter_mut()
                .filter(|c| c.head.is_some())
                .min_by_key(|c| c.head);
            if let Some(c) = first
                && let Some(a) = c.head
                && k.is_none_or(|k| a < k)
            {
                c.advance();
                self.pending = k;
                return Some(a);
            }
            let k = k?;
            // Removed keys are in the segment, so none is passed over.
            let mut gone = false;
            for c in &mut self.removed {
                while c.head.is_some_and(|r| r < k) {
                    c.advance();
                }
                if c.head == Some(k) {
                    c.advance();
                    gone = true;
                }
            }
            if !gone {
                return Some(k);
            }
        }
    }

    fn seek(&mut self, key: Key) {
        if self.pending.is_some_and(|k| k < key) {
            self.pending = None;
        }
        for c in self.added.iter_mut().chain(&mut self.removed) {
            c.seek(key);
        }
    }

    /// Appends the next keys of the segment's `range` with the runs merged in to `keys` (a
    /// chunk of the segment at a time); `false` once both are done. Only without a pending
    /// key.
    fn next_chunk(&mut self, range: &mut Range, segment: &Segment, keys: &mut Vec<Key>) -> bool {
        debug_assert!(self.pending.is_none());
        let mut chunk = std::mem::take(&mut self.chunk);
        chunk.clear();
        let more = range.next_chunk(segment, &mut chunk);
        // The run keys up to the chunk's last key, or all that are left after the segment's.
        let limit = if more {
            chunk[chunk.len() - 1]
        } else {
            [u64::MAX; 4]
        };
        let take = |cursors: &mut Vec<RunCursor>, into: &mut Vec<Key>| {
            for c in cursors.iter_mut() {
                while let Some(k) = c.head
                    && k <= limit
                {
                    into.push(k);
                    c.advance();
                }
            }
        };
        let (mut added, mut removed) = (
            std::mem::take(&mut self.added_keys),
            std::mem::take(&mut self.removed_keys),
        );
        added.clear();
        removed.clear();
        take(&mut self.added, &mut added);
        take(&mut self.removed, &mut removed);
        if self.added.len() > 1 {
            added.sort_unstable();
        }
        if self.removed.len() > 1 {
            removed.sort_unstable();
        }
        let start = keys.len();
        if added.is_empty() && removed.is_empty() {
            keys.extend_from_slice(&chunk);
        } else {
            let (mut a, mut r) = (0, 0);
            for &k in &chunk {
                while a < added.len() && added[a] < k {
                    keys.push(added[a]);
                    a += 1;
                }
                // Removed keys are in the segment, in the same order.
                if r < removed.len() && removed[r] == k {
                    r += 1;
                } else {
                    keys.push(k);
                }
            }
            keys.extend_from_slice(&added[a..]);
        }
        let appended = keys.len() > start;
        self.chunk = chunk;
        self.added_keys = added;
        self.removed_keys = removed;
        // A chunk whose keys were all removed: there may be more.
        more || appended
    }
}

/// Gives the calling thread the lowest scheduling priority (Linux: nice 19; Windows: idle),
/// where the system offers one per thread.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn setpriority(which: i32, who: u32, prio: i32) -> i32;
        }
        // PRIO_PROCESS with `who` 0 is the calling thread on Linux.
        // SAFETY: a plain system call on the calling thread.
        unsafe {
            setpriority(0, 0, 19);
        }
    }
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        // THREAD_PRIORITY_IDLE.
        // SAFETY: the pseudo handle of the calling thread.
        unsafe {
            SetThreadPriority(GetCurrentThread(), -15);
        }
    }
}

/// A permutation's segment with its runs folded in, by a merge in the background, and what
/// it was built from.
#[derive(Default)]
pub(crate) struct MergedPerm {
    base_id: u64,
    runs: Vec<u64>,
    segment: Arc<Segment>,
}

impl MergedPerm {
    /// Flushes the segment's file, if it has one (see [`crate::persist::RunFile::sync`]); a
    /// failure leaves it to the checkpoint.
    pub(crate) fn sync(&self) {
        if let Some(file) = &self.segment.file {
            let _ = file.sync();
        }
    }
}

/// A pattern scan over one permutation. Owns what it reads, so it outlives the index.
pub(crate) struct Scan {
    segment: Arc<Segment>,
    range: Range,
    /// The permutation's runs with keys in the range, when it has any.
    runs: Option<Box<RunMerge>>,
    /// The permutation's delta, when it has one.
    delta: Option<Delta>,
    /// `from[c]`: the key position of column `c` of `[s, p, o, g]`.
    from: [usize; 4],
    skip_default: bool,
    /// The quads a row filter hides (#202).
    rows: Option<Arc<crate::RowFilter>>,
    /// `skip_default || rows.is_some()`: the one test an unfiltered scan makes.
    filtered: bool,
    /// The range's low end, and how many of its leading key positions are fixed.
    low: Key,
    fixed: usize,
    /// The column of `[s, p, o, g]` the quads come sorted by after the fixed ones.
    sorted_by: Option<usize>,
    /// A buffer for [`Scan::next_chunk`].
    chunk: Vec<Key>,
}

/// The state of merging a delta into a scan: the added and removed keys inside the scan's
/// range.
struct Delta {
    added: Option<SetCursor>,
    removed: Option<SetCursor>,
    /// The smaller of the next added and the next removed key (`[u64::MAX; 4]` when neither
    /// is left): segment keys before it are returned as they are.
    limit: Key,
    /// A segment key read but not returned yet, because an added key came first.
    pending: Option<Key>,
}

impl Delta {
    fn new(added: Option<SetCursor>, removed: Option<SetCursor>) -> Self {
        let mut d = Self {
            added,
            removed,
            limit: [u64::MAX; 4],
            pending: None,
        };
        d.set_limit();
        d
    }

    fn set_limit(&mut self) {
        let a = self.added.as_ref().and_then(SetCursor::peek);
        let r = self.removed.as_ref().and_then(SetCursor::peek);
        self.limit = match (a, r) {
            (Some(a), Some(r)) => a.min(r),
            (Some(k), None) | (None, Some(k)) => k,
            (None, None) => [u64::MAX; 4],
        };
    }
}

/// Reads the keys of one side of a delta inside a range, in order.
struct SetCursor {
    keys: Arc<[Key]>,
    pos: usize,
    end: usize,
}

impl SetCursor {
    /// `None` if `keys` has none in `low..=high`.
    fn new(keys: &Arc<[Key]>, low: Key, high: Key) -> Option<Self> {
        let (pos, end) = Flat::bounds(keys, low, high);
        (pos < end).then(|| Self {
            keys: Arc::clone(keys),
            pos,
            end,
        })
    }

    #[inline]
    fn peek(&self) -> Option<Key> {
        (self.pos < self.end).then(|| self.keys[self.pos])
    }

    #[inline]
    fn advance(&mut self) {
        self.pos += 1;
    }

    /// Moves on to the keys at or above `key`.
    fn seek(&mut self, key: Key) {
        self.pos += self.keys[self.pos..self.end].partition_point(|k| *k < key);
    }
}

impl Scan {
    /// The column of `[s, p, o, g]` (0 to 3) the quads come sorted by, if one does: the first
    /// one the pattern leaves open in the permutation read.
    pub(crate) fn sorted_by(&self) -> Option<usize> {
        self.sorted_by
    }

    /// This scan without the quads `rows` hides (#202).
    pub(crate) fn with_rows(mut self, rows: Option<&Arc<crate::RowFilter>>) -> Self {
        if let Some(rows) = rows {
            // A subject, predicate or object the rows hide everywhere: nothing to read (#204).
            let fixed = |c: usize| (self.from[c] < self.fixed).then(|| self.low[self.from[c]]);
            if rows.hides_all(fixed(0), fixed(1), fixed(2)) {
                self.range.clear();
                self.runs = None;
                self.delta = None;
            }
            self.rows = Some(Arc::clone(rows));
            self.filtered = true;
        }
        self
    }

    /// Whether the quad is left out: a default-graph quad of a named-graphs scan, or one a
    /// row filter hides.
    #[inline]
    fn drops(&self, q: &Key) -> bool {
        (self.skip_default && q[3] == 0) || self.rows.as_ref().is_some_and(|r| r.hides(q))
    }

    /// Skips to the first quad whose [`Scan::sorted_by`] column is at least `value`, without
    /// reading the blocks in between (#169). Does nothing for a scan not sorted by a column,
    /// or for a value not past the current position.
    pub(crate) fn seek(&mut self, value: u64) {
        if self.fixed >= 4 {
            return;
        }
        let mut key = self.low;
        key[self.fixed] = value;
        key[self.fixed + 1..].fill(0);
        if key <= self.low {
            return;
        }
        self.range.seek(&self.segment, key);
        if let Some(r) = &mut self.runs {
            r.seek(key);
        }
        if let Some(d) = &mut self.delta {
            if d.pending.is_some_and(|k| k < key) {
                d.pending = None;
            }
            for c in [&mut d.added, &mut d.removed].into_iter().flatten() {
                c.seek(key);
            }
            d.set_limit();
        }
    }

    /// Appends the next quads (as `[s, p, o, g]`) to `out`, a block chunk at a time when the
    /// scan has no delta; `false` once the scan is done.
    pub(crate) fn next_chunk(&mut self, out: &mut Vec<Key>) -> bool {
        if self.delta.is_some() || self.runs.as_ref().is_some_and(|r| r.pending.is_some()) {
            let start = out.len();
            out.extend(self.by_ref().take(16));
            return out.len() > start;
        }
        let mut keys = std::mem::take(&mut self.chunk);
        let more = loop {
            keys.clear();
            let more = match &mut self.runs {
                None => self.range.next_chunk(&self.segment, &mut keys),
                Some(r) => r.next_chunk(&mut self.range, &self.segment, &mut keys),
            };
            if !more {
                break false;
            }
            let f = self.from;
            let start = out.len();
            out.extend(
                keys.iter()
                    .map(|k| [k[f[0] & 3], k[f[1] & 3], k[f[2] & 3], k[f[3] & 3]])
                    .filter(|q| !self.filtered || !self.drops(q)),
            );
            if out.len() > start {
                break true;
            }
        };
        self.chunk = keys;
        more
    }

    /// The next key in permutation order, delta merged in.
    #[inline]
    fn next_key(&mut self) -> Option<Key> {
        let Some(d) = &mut self.delta else {
            return stored_next(&mut self.runs, &mut self.range, &self.segment);
        };
        loop {
            let k = match d.pending.take() {
                Some(k) => k,
                None => match stored_next(&mut self.runs, &mut self.range, &self.segment) {
                    Some(k) => k,
                    None => {
                        // The segment's range is done: the added keys left.
                        let a = d.added.as_mut()?;
                        let k = a.peek()?;
                        a.advance();
                        return Some(k);
                    }
                },
            };
            if crate::segment::less(&k, &d.limit) {
                return Some(k);
            }
            if let Some(a) = d.added.as_mut()
                && let Some(next) = a.peek()
                && next < k
            {
                // Added keys are not in the segment.
                a.advance();
                d.pending = Some(k);
                d.set_limit();
                return Some(next);
            }
            if let Some(r) = d.removed.as_mut() {
                // Removed keys are in the segment, so none is passed over.
                while r.peek().is_some_and(|r| r < k) {
                    r.advance();
                }
                if r.peek() == Some(k) {
                    r.advance();
                    d.set_limit();
                    continue;
                }
            }
            d.set_limit();
            return Some(k);
        }
    }
}

/// The next key of a scan's segment range, its runs merged in.
#[inline(always)]
fn stored_next(
    runs: &mut Option<Box<RunMerge>>,
    range: &mut Range,
    segment: &Segment,
) -> Option<Key> {
    match runs {
        None => range.next(segment),
        Some(r) => r.next(range, segment),
    }
}

impl Iterator for Scan {
    type Item = Key;

    #[inline]
    fn next(&mut self) -> Option<Key> {
        loop {
            let k = self.next_key()?;
            let f = &self.from;
            // `& 3` lets the compiler drop the bounds checks.
            let q = [k[f[0] & 3], k[f[1] & 3], k[f[2] & 3], k[f[3] & 3]];
            if self.filtered && self.drops(&q) {
                continue;
            }
            return Some(q);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(p: &Perm) -> impl Iterator<Item = Key> {
        p.stored_range([0; 4], [u64::MAX; 4])
    }

    fn consistent(p: &Perm) -> bool {
        p.segment.is_consistent()
            && p.runs
                .iter()
                .all(|r| r.added.is_consistent() && r.removed.is_consistent())
    }

    #[test]
    fn run_filters_hold_every_prefix_and_few_others() {
        let mut added: Vec<Key> = (0..20_000u64).map(|i| [i % 97, i % 13, i, i % 5]).collect();
        added.sort_unstable();
        let removed: Vec<Key> = (0..5_000u64).map(|i| [1_000 + i, 2, 3, i]).collect();
        let filter = PrefixFilter::build(
            &Segment::from_sorted(added.clone()),
            &Segment::from_sorted(removed.clone()),
            None,
        );
        for k in added.iter().chain(&removed) {
            for len in 1..=4 {
                assert!(filter.may_hold(k, len), "{k:?} {len}");
            }
        }
        let others = (0..100_000u64)
            .filter(|&i| {
                filter.may_hold(&[i, 500_000 + i, i, 1], 4)
                    || filter.may_hold(&[50_000 + i, 0, 0, 0], 1)
            })
            .count();
        assert!(others < 4_000, "{others} false positives in 200000");
        let empty = PrefixFilter::build(&Segment::default(), &Segment::default(), Some([0; 4]));
        assert!(!empty.may_hold(&[1, 2, 3, 4], 2));
        // A run that touches most of the segment's subjects leaves them out of its filter.
        let dense = PrefixFilter::build(
            &Segment::from_sorted(added.clone()),
            &Segment::default(),
            Some([100, u64::MAX, u64::MAX, u64::MAX]),
        );
        assert!(dense.may_hold(&[123_456, 0, 0, 0], 1));
    }

    #[test]
    fn small_runs_wait_for_more_before_a_merge_rewrites_the_segment() {
        let key = |i: u64| [i / 8, i % 8, i * 7 % 1001, 0];
        let mut index = QuadIndex::from_quads((0..64_000u64).map(key).collect());
        let mut next = 64_000u64;
        let mut add = |index: &mut QuadIndex, n: u64| {
            for i in next..next + n {
                index.insert(key(i));
            }
            next += n;
            index.compact();
        };
        // 500 new quads per 64,000: under one in 64, no merge.
        add(&mut index, 500);
        assert_eq!(index.perms[0].runs.len(), 1);
        assert!(!index.has_unmerged_runs());
        let merge = index.start_merge();
        assert!(merge.perms.iter().all(|p| p.runs.is_empty()));
        assert!(index.perms.iter().all(|p| p.frozen == 0));
        index.finish_merge(merge.merge_runs(&AtomicBool::new(false)).unwrap());
        assert_eq!(index.perms[0].runs.len(), 1, "the run stays");
        // From one in 64, the runs merge into the segment.
        add(&mut index, 600);
        assert!(index.has_unmerged_runs());
        let merge = index.start_merge();
        let merged = merge.merge_runs(&AtomicBool::new(false)).unwrap();
        index.finish_merge(merged);
        assert!(index.perms.iter().all(|p| p.runs.is_empty()));
        assert_eq!(index.len(), 65_100);
    }

    #[test]
    fn compactions_into_runs_keep_every_read() {
        let mut x: u64 = 7;
        let mut next = |m: u64| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) % m
        };
        let key = |i: u64| [i * 7 % 101, i % 9, i * 13 % 211, i % 4];
        let base: Vec<Key> = (0..20_000u64).map(key).collect();
        let mut index = QuadIndex::from_quads(base.clone());
        let mut model: std::collections::BTreeSet<Key> = base.iter().copied().collect();
        let mut most_runs = 0;
        let mut folded_into_segment = false;
        for round in 0..40 {
            // Mostly new quads, some removals of the segment's, and in some rounds quads
            // added back or removed again (changes that undo a run's).
            for _ in 0..(50 + next(400)) {
                let q = [next(150), next(9), next(400), next(4)];
                if round % 5 == 4 && next(2) == 0 {
                    assert_eq!(index.remove(q), model.remove(&q));
                } else {
                    assert_eq!(index.insert(q), model.insert(q));
                }
            }
            for _ in 0..next(30) {
                let q = base[next(base.len() as u64) as usize];
                if round % 3 == 2 {
                    assert_eq!(index.insert(q), model.insert(q));
                } else {
                    assert_eq!(index.remove(q), model.remove(&q));
                }
            }
            let segment = Arc::clone(&index.perms[0].segment);
            index.compact();
            most_runs = most_runs.max(index.perms[0].runs.len());
            folded_into_segment |= !Arc::ptr_eq(&segment, &index.perms[0].segment);
            assert_eq!(index.len(), model.len(), "round {round}");
            let quads: Vec<Key> = model.iter().copied().collect();
            let rebuilt = QuadIndex::from_quads(quads.clone());
            for (perm, (a, b)) in index.perms.iter().zip(&rebuilt.perms).enumerate() {
                assert!(consistent(a), "perm {perm} round {round}");
                assert!(stored(a).eq(stored(b)), "perm {perm} round {round}");
                assert_eq!(
                    index.stats(perm),
                    rebuilt.stats(perm),
                    "perm {perm} round {round}"
                );
            }
            if round % 4 == 0 {
                for p in patterns() {
                    let mut got: Vec<Key> = index.scan(p).collect();
                    got.sort_unstable();
                    let expected = naive(&quads, p);
                    assert_eq!(got, expected, "{p:?} round {round}");
                    // A chunk at a time too, with seeks in between.
                    let mut scan = index.scan(p);
                    let mut chunks = Vec::new();
                    while scan.next_chunk(&mut chunks) {}
                    chunks.sort_unstable();
                    assert_eq!(chunks, expected, "{p:?} round {round}");
                    let estimate = index.estimate(p);
                    if p.g.is_some() || p.all_graphs {
                        assert_eq!(estimate, expected.len(), "{p:?} round {round}");
                    }
                    for (by, v) in (0..3).flat_map(|by| [3, 41, 120].map(|v| (by, v))) {
                        let mut a = index.scan_sorted(p, Some(by));
                        let mut b = rebuilt.scan_sorted(p, Some(by));
                        // A chunk first, so that the seek starts inside the scan.
                        let mut first = Vec::new();
                        a.next_chunk(&mut first);
                        let skipped: Vec<Key> = b.by_ref().take(first.len()).collect();
                        assert_eq!(first, skipped, "{p:?} by {by} round {round}");
                        a.seek(v);
                        b.seek(v);
                        let mut got = Vec::new();
                        while a.next_chunk(&mut got) {}
                        let want: Vec<Key> = b.collect();
                        assert_eq!(got, want, "{p:?} by {by} seek {v} round {round}");
                    }
                    // Seeks skip to the same quads as the rebuilt index's.
                    for by in 0..3 {
                        let mut a = index.scan_sorted(p, Some(by));
                        let mut b = rebuilt.scan_sorted(p, Some(by));
                        if let Some(col) = a.sorted_by() {
                            a.seek(37);
                            b.seek(37);
                            assert!(a.eq(b), "{p:?} by {col} round {round}");
                        }
                    }
                }
                for q in quads.iter().step_by(97) {
                    assert!(index.contains(q));
                }
            }
            // A delta on top of the runs reads the same too.
            if round % 7 == 3 {
                let q = [999, 1, 2, 3];
                index.insert(q);
                model.insert(q);
                let quads: Vec<Key> = model.iter().copied().collect();
                for p in patterns() {
                    let mut got: Vec<Key> = index.scan(p).collect();
                    got.sort_unstable();
                    assert_eq!(got, naive(&quads, p), "{p:?} round {round}");
                }
            }
        }
        assert!(most_runs >= 2, "{most_runs}");
        assert!(folded_into_segment);
    }

    fn naive(quads: &[Key], p: Pattern) -> Vec<Key> {
        let mut v: Vec<Key> = quads
            .iter()
            .copied()
            .filter(|q| {
                p.s.is_none_or(|s| q[0] == s)
                    && p.p.is_none_or(|x| q[1] == x)
                    && p.o.is_none_or(|o| q[2] == o)
                    && match p.g {
                        Some(g) => q[3] == g,
                        None => p.all_graphs || q[3] != 0,
                    }
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn patterns() -> Vec<Pattern> {
        let mut out = Vec::new();
        for s in [None, Some(2)] {
            for p in [None, Some(1)] {
                for o in [None, Some(3)] {
                    for g in [None, Some(0), Some(2)] {
                        for all_graphs in [false, true] {
                            out.push(Pattern {
                                s,
                                p,
                                o,
                                g,
                                all_graphs: all_graphs && g.is_none(),
                            });
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn seeks_skip_to_the_sorted_value_with_and_without_delta() {
        let all: Vec<Key> = (0..5000u64)
            .map(|i| [i % 211, i % 3, (i * 7919) % 401, i % 2])
            .collect();
        let (base, extra) = all.split_at(4000);
        let mut index = QuadIndex::from_quads(base.to_vec());
        for compacted in [false, true] {
            if !compacted {
                for q in extra {
                    index.insert(*q);
                }
                for q in base.iter().step_by(5) {
                    index.remove(*q);
                }
            } else {
                index.compact();
            }
            for p in patterns() {
                for by in [None, Some(0), Some(2)] {
                    let full: Vec<Key> = index.scan_sorted(p, by).collect();
                    let Some(col) = index.scan_sorted(p, by).sorted_by() else {
                        continue;
                    };
                    // Seek to a few values in order, reading two quads after each.
                    let mut scan = index.scan_sorted(p, by);
                    let mut at = 0;
                    for v in [0, 3, 50, 51, 52, 200, 399, 1000] {
                        scan.seek(v);
                        // The first not yet read with a value at least `v`.
                        while at < full.len() && full[at][col] < v {
                            at += 1;
                        }
                        for _ in 0..2 {
                            assert_eq!(scan.next(), full.get(at).copied(), "{p:?} {by:?} {v}");
                            at += 1;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn missing_permutations_are_built_from_spog() {
        let all: Vec<Key> = (0..300u64).map(|i| [i % 5, i % 3, i % 7, i % 4]).collect();
        let full = QuadIndex::from_quads(all.clone());
        let old: Vec<Segment> = full.perms[..6]
            .iter()
            .map(|p| Segment::from_sorted(p.segment.iter()))
            .collect();
        let opened = QuadIndex::from_segments(old);
        for (a, b) in full.perms.iter().zip(&opened.perms) {
            assert!(a.segment.iter().eq(b.segment.iter()));
        }
    }

    #[test]
    fn scans_match_a_filter_with_and_without_delta() {
        let all: Vec<Key> = (0..600u64).map(|i| [i % 5, i % 3, i % 7, i % 4]).collect();
        let (base, extra) = all.split_at(400);
        let mut index = QuadIndex::from_quads(base.to_vec());
        let mut model: std::collections::BTreeSet<Key> = base.iter().copied().collect();
        for q in extra {
            assert_eq!(index.insert(*q), model.insert(*q));
        }
        for q in base.iter().step_by(3) {
            assert_eq!(index.remove(*q), model.remove(q));
        }
        let quads: Vec<Key> = model.iter().copied().collect();
        assert_eq!(index.len(), quads.len());
        // The counts of every permutation, with the delta applied.
        for (perm, order) in ORDERS.iter().enumerate() {
            let mut keys: Vec<Key> = quads.iter().map(|q| permute(q, order)).collect();
            keys.sort_unstable();
            let want = crate::stats::SegStats::of(keys);
            assert_eq!(index.stats(perm).as_deref(), Some(&want), "perm {perm}");
        }
        for compacted in [false, true] {
            if compacted {
                index.compact();
                assert_eq!(index.delta_len(), 0);
            }
            for p in patterns() {
                let mut got: Vec<Key> = index.scan(p).collect();
                got.sort_unstable();
                let expected = naive(&quads, p);
                assert_eq!(got, expected, "{p:?} compacted={compacted}");
                // The same quads in the same order, a chunk at a time.
                let mut chunked = Vec::new();
                let mut scan = index.scan(p);
                while scan.next_chunk(&mut chunked) {}
                assert_eq!(
                    chunked,
                    index.scan(p).collect::<Vec<_>>(),
                    "{p:?} compacted={compacted}"
                );
                for by in 0..3 {
                    let sorted: Vec<Key> = index.scan_sorted(p, Some(by)).collect();
                    let mut set = sorted.clone();
                    set.sort_unstable();
                    assert_eq!(set, expected, "{p:?} by {by} compacted={compacted}");
                    // `?s <p> ?o` comes out by subject.
                    if by == 0 && p.p.is_some() && p.s.is_none() && p.o.is_none() {
                        assert!(
                            sorted.is_sorted_by_key(|q| q[0]),
                            "{p:?} compacted={compacted}"
                        );
                    }
                }
                assert_eq!(
                    index.estimate(p),
                    expected.len(),
                    "{p:?} compacted={compacted}"
                );
            }
        }
    }

    #[test]
    fn scans_read_a_large_delta_from_the_sets() {
        let key = |i: u64| [i * 7 % 101, i % 9, i * 13 % 211, i % 4];
        let all: Vec<Key> = (0..6000u64).map(key).collect();
        let (base, extra) = all.split_at(4000);
        let mut index = QuadIndex::from_quads(base.to_vec());
        let mut model: std::collections::BTreeSet<Key> = base.iter().copied().collect();
        for q in extra {
            assert_eq!(index.insert(*q), model.insert(*q));
        }
        for q in base.iter().step_by(2) {
            assert_eq!(index.remove(*q), model.remove(q));
        }
        assert!(index.delta_len() > FLAT_MIN);
        let quads: Vec<Key> = model.iter().copied().collect();
        // Twice: the first reads take their range from the persistent sets, and once a
        // permutation has been read often enough, the later ones read its flat copy.
        for round in 0..2 {
            for p in patterns() {
                let mut got: Vec<Key> = index.scan(p).collect();
                got.sort_unstable();
                let expected = naive(&quads, p);
                assert_eq!(got, expected, "{p:?} round {round}");
                for by in 0..3 {
                    let mut sorted: Vec<Key> = index.scan_sorted(p, Some(by)).collect();
                    sorted.sort_unstable();
                    assert_eq!(sorted, expected, "{p:?} by {by} round {round}");
                }
                // Exact, but over named graphs only it may count default-graph quads too.
                let estimate = index.estimate(p);
                if p.g.is_some() || p.all_graphs {
                    assert_eq!(estimate, expected.len(), "{p:?} round {round}");
                } else {
                    assert!(estimate >= expected.len(), "{p:?} round {round}");
                }
            }
            if round == 0 {
                assert!(index.perms.iter().any(|p| p.cache.flat.get().is_none()));
            }
        }
        assert!(index.perms.iter().any(|p| p.cache.flat.get().is_some()));
    }

    #[test]
    fn merging_quads_in_matches_a_rebuild() {
        let key = |i: u64| [i * 7 % 101, i % 9, i * 13 % 211, i % 4];
        let base: Vec<Key> = (0..5000u64).map(key).collect();
        let mut index = QuadIndex::from_quads(base.clone());
        let mut all: std::collections::BTreeSet<Key> = base.iter().copied().collect();
        for round in 0..3u64 {
            let mut extra: Vec<Key> = (0..250u64)
                .map(|i| key(5000 + round * 977 + i * 31))
                .map(|q| [q[0], q[1], q[2] + 1000 * (round + 1), q[3]])
                .collect();
            extra.sort_unstable();
            extra.dedup();
            extra.retain(|q| !index.contains(q));
            assert!(index.merges_in(extra.len()));
            index.merge_in(&extra);
            all.extend(&extra);
            let quads: Vec<Key> = all.iter().copied().collect();
            let rebuilt = QuadIndex::from_quads(quads.clone());
            assert_eq!(index.len(), rebuilt.len());
            for (perm, (a, b)) in index.perms.iter().zip(&rebuilt.perms).enumerate() {
                assert!(consistent(a), "perm {perm}");
                assert!(stored(a).eq(stored(b)), "perm {perm}");
                assert_eq!(index.stats(perm), rebuilt.stats(perm), "perm {perm}");
            }
            for p in patterns() {
                let mut got: Vec<Key> = index.scan(p).collect();
                got.sort_unstable();
                assert_eq!(got, naive(&quads, p), "{p:?} round {round}");
            }
        }
    }

    #[test]
    fn a_pinned_index_keeps_its_changes_in_runs_beside_the_shared_segments() {
        let key = |i: u64| [i * 7 % 101, i % 9, i * 13 % 211, i % 4];
        let base: Vec<Key> = (0..5000u64).map(key).collect();
        let shared = QuadIndex::from_quads(base.clone());
        let mut index = shared.clone();
        index.pinned = true;
        let every = Pattern {
            s: None,
            p: None,
            o: None,
            g: None,
            all_graphs: true,
        };
        let mut all: std::collections::BTreeSet<Key> = shared.scan(every).collect();
        for round in 0..6u64 {
            // Inserts, then removes of base quads and of earlier inserts, folded and compacted
            // in turn, and a large one that would rewrite an unpinned index.
            let extra: Vec<Key> = (0..300u64)
                .map(|i| [1000 + round * 300 + i, i % 5, i, i % 3])
                .collect();
            if round % 2 == 0 {
                for q in &extra {
                    index.insert(*q);
                }
                for q in all.iter().step_by(37).take(40).copied().collect::<Vec<_>>() {
                    index.remove(q);
                    all.remove(&q);
                }
                index.compact();
            } else {
                let mut changes: Vec<(Key, bool)> = extra.iter().map(|q| (*q, true)).collect();
                let gone: Vec<Key> = all.iter().step_by(11).take(500).copied().collect();
                changes.extend(gone.iter().map(|q| (*q, false)));
                for q in &gone {
                    all.remove(q);
                }
                index.fold_in(&changes);
            }
            all.extend(&extra);
            assert!(index.merges_in(10_000));
            for (perm, (a, b)) in index.perms.iter().zip(&shared.perms).enumerate() {
                assert!(
                    Arc::ptr_eq(&a.segment, &b.segment),
                    "perm {perm} round {round}"
                );
            }
            let quads: Vec<Key> = all.iter().copied().collect();
            assert_eq!(index.len(), quads.len());
            for p in patterns() {
                let mut got: Vec<Key> = index.scan(p).collect();
                got.sort_unstable();
                assert_eq!(got, naive(&quads, p), "{p:?} round {round}");
            }
        }
        // The base is as it was.
        let mut got: Vec<Key> = shared.scan(every).collect();
        got.sort_unstable();
        let mut expected = base;
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(got, expected);
        assert!(index.unmerged_len() > 0);
    }

    #[test]
    fn folding_changes_in_matches_inserts_and_removes() {
        let key = |i: u64| [i * 7 % 101, i % 9, i * 13 % 211, i % 4];
        let all: Vec<Key> = (0..6000u64).map(key).collect();
        // Small next to the segments (blocks merged), then large (rebuilt).
        for (base_len, extra, every, changes) in [(5000, 20, 101, 100), (300, 100, 11, 2000)] {
            let mut index = QuadIndex::from_quads(all[..base_len].to_vec());
            let mut model = QuadIndex::from_quads(all[..base_len].to_vec());
            // A delta to fold with: inserts and removes, some of them undone below.
            for q in &all[base_len..base_len + extra] {
                index.insert(*q);
                model.insert(*q);
            }
            for q in all[..base_len].iter().step_by(every) {
                index.remove(*q);
                model.remove(*q);
            }
            let mut batch = std::collections::HashMap::new();
            for (n, q) in all
                .iter()
                .enumerate()
                .skip(base_len / 2)
                .step_by(3)
                .take(changes)
            {
                batch.insert(*q, n % 2 == 0);
            }
            for (q, present) in &batch {
                if *present {
                    model.insert(*q);
                } else {
                    model.remove(*q);
                }
            }
            index.fold_in(&batch.into_iter().collect::<Vec<_>>());
            assert_eq!(index.delta_len(), 0);
            assert_eq!(index.len(), model.len());
            model.compact();
            for (perm, (a, b)) in index.perms.iter().zip(&model.perms).enumerate() {
                assert!(consistent(a), "perm {perm}");
                assert!(stored(a).eq(stored(b)), "perm {perm}");
                assert_eq!(index.stats(perm), model.stats(perm), "perm {perm}");
            }
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release -p tripletrail-store --lib runs_read_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn runs_read_cost() {
        let key = |i: u64| [i / 8, i % 8, i.wrapping_mul(2654435761) % 1_000_000, 0];
        let n = 4_000_000u64;
        let mut index = QuadIndex::from_quads((0..n).map(key).collect());
        // Compactions of 65k random new quads, like small commits, until a few runs.
        let mut i = n;
        while index.perms[0].runs.len() < 2
            || index.perms[0].runs.iter().map(Run::len).sum::<usize>() < 300_000
        {
            for _ in 0..65_536 {
                let r = i.wrapping_mul(0x9E3779B97F4A7C15) % (n * 2);
                index.insert([r / 8, r % 8 + 8 * (i % 2), i % 1_000_000, 0]);
                i += 1;
            }
            index.compact();
        }
        let sizes: Vec<usize> = index.perms[0].runs.iter().map(Run::len).collect();
        let rebuilt = QuadIndex::from_quads(index.iter().collect());
        let time = |name: &str, f: &dyn Fn(&QuadIndex) -> usize| {
            for (label, idx) in [("runs", &index), ("one segment", &rebuilt)] {
                let t = std::time::Instant::now();
                let mut r = 0;
                for _ in 0..3 {
                    r = f(idx);
                }
                println!(
                    "{name:<28} {label:<12} {:>8.1} ms ({r})",
                    t.elapsed().as_secs_f64() * 1000.0 / 3.0
                );
            }
        };
        println!("runs {sizes:?}");
        time("full scan, chunks", &|idx| {
            let mut s = idx.scan(Pattern {
                s: None,
                p: None,
                o: None,
                g: None,
                all_graphs: true,
            });
            let mut out = Vec::new();
            let mut n = 0;
            while s.next_chunk(&mut out) {
                n += out.len();
                out.clear();
            }
            n
        });
        time("100k point lookups", &|idx| {
            (0..100_000u64)
                .filter(|i| idx.contains(&key(i * 37)))
                .count()
        });
        time("100k seeks in ?s p1 ?o", &|idx| {
            let mut s = idx.scan_sorted(
                Pattern {
                    s: None,
                    p: Some(1),
                    o: None,
                    g: None,
                    all_graphs: true,
                },
                Some(0),
            );
            let mut n = 0;
            for v in (0..500_000u64).step_by(5) {
                s.seek(v);
                n += usize::from(s.next().is_some());
            }
            n
        });
        time("10k ranges s ?p ?o", &|idx| {
            (0..10_000u64)
                .map(|v| {
                    idx.scan(Pattern {
                        s: Some(v * 41),
                        p: None,
                        o: None,
                        g: None,
                        all_graphs: true,
                    })
                    .count()
                })
                .sum()
        });
    }
}
