//! An immutable, sorted, compressed run of quads in one permutation order.
//!
//! Quads are cut into blocks of at most [`BLOCK`] quads. The first quad of each block is kept
//! uncompressed in `firsts` (the block index, searched by binary search). A block stores each
//! of the four columns on its own (#169), bit-packed with a frame of reference: the column's
//! smallest value in the block, then each value minus it in as many bits as the largest
//! needs. Sorted quads share long prefixes, so the leading columns of a block take a few bits
//! or none, and any quad of a block can be read without decoding the ones before it.
//!
//! Block layout, at `offsets[b]` in `bytes`: four bytes with each column's bit width (0 to
//! 64), four varints with `firsts[b][c] - min` per column, then the columns one after the
//! other, `n * bits[c]` bits each, least significant bit first. [`PAD`] zero bytes follow
//! the last block, so a value is read with one unaligned load.

use crate::buf::Buf;
use crate::persist::{RunFile, RunWriter, Spill};
use crate::stats::{SegStats, StatsBuilder};
use rayon::prelude::*;
use std::sync::{Arc, OnceLock};

pub(crate) type Key = [u64; 4];

/// The most quads a block holds.
pub(crate) const BLOCK: usize = 64;

/// Zero bytes after the last block.
pub(crate) const PAD: usize = 16;

#[derive(Default)]
pub(crate) struct Segment {
    pub(crate) len: usize,
    pub(crate) firsts: Buf<Key>,
    /// `offsets[b]..offsets[b + 1]` is block `b` in `bytes`.
    pub(crate) offsets: Buf<u64>,
    /// `starts[b]..starts[b + 1]`: the quads of block `b`.
    pub(crate) starts: Buf<u64>,
    /// The blocks, then [`PAD`] zero bytes.
    pub(crate) bytes: Buf<u8>,
    /// Counts of the quads (see [`crate::stats`]): computed while building, read from the
    /// checkpoint, or computed on first use.
    pub(crate) stats: OnceLock<Arc<SegStats>>,
    /// Whether this segment is a graph-first one read from the blocks of another (#152): of a
    /// permutation whose last column is the graph, its quads all in the default graph
    /// (graph 0), as the permutation with the graph moved to the front (SPOG as GSPO). The
    /// blocks are the other segment's, shared, with their columns in its order; `firsts`
    /// is in this segment's order. See [`Segment::graph_first`].
    pub(crate) graph_first: bool,
    /// The file of a fold this segment was read from, if any: deleted when the last segment
    /// read from it goes. Last, so the maps above are dropped before it.
    pub(crate) file: Option<Arc<RunFile>>,
}

/// A key of a permutation with the graph last, with the graph moved to the front.
#[inline]
pub(crate) fn graph_to_front(k: &Key) -> Key {
    [k[3], k[0], k[1], k[2]]
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A varint at `*pos`, or `None` past the end of `bytes` or after ten bytes.
fn get_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7f).checked_shl(shift).unwrap_or(0);
        if b < 0x80 {
            return Some(v);
        }
    }
    None
}

fn bits_for(v: u64) -> u32 {
    u64::BITS - v.leading_zeros()
}

fn mask(bits: u32) -> u64 {
    if bits == 64 {
        u64::MAX
    } else {
        (1 << bits) - 1
    }
}

/// The `bits`-bit value at bit `bit` of `bytes` (with [`PAD`] bytes after it).
#[inline(always)]
fn read_bits(bytes: &[u8], bit: usize, _bits: u32, mask: u64) -> u64 {
    let at = bit / 8;
    let shift = bit % 8;
    // Nine bytes hold any value of up to 64 bits at any shift; one bounds check.
    let Some(w) = bytes.get(at..at + 9) else {
        return 0;
    };
    let lo = u64::from_le_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]);
    // The ninth byte's bits above the low word (none at shift 0).
    let hi = (u64::from(w[8]) << (63 - shift)) << 1;
    ((lo >> shift) | hi) & mask
}

/// Appends `quads` (sorted, 1 to [`u32::MAX`] of them) to `out` as one block.
fn encode_block(out: &mut Vec<u8>, quads: &[Key]) {
    let mut min = quads[0];
    let mut max = quads[0];
    for q in quads {
        for c in 0..4 {
            min[c] = min[c].min(q[c]);
            max[c] = max[c].max(q[c]);
        }
    }
    let bits: [u32; 4] = std::array::from_fn(|c| bits_for(max[c] - min[c]));
    out.extend(bits.map(|b| b as u8));
    for c in 0..4 {
        put_varint(out, quads[0][c] - min[c]);
    }
    let (mut acc, mut held) = (0u128, 0u32);
    for c in 0..4 {
        for q in quads {
            acc |= u128::from(q[c] - min[c]) << held;
            held += bits[c];
            while held >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                held -= 8;
            }
        }
    }
    if held > 0 {
        out.push(acc as u8);
    }
}

/// How to read one column of a block.
#[derive(Clone, Copy, Default)]
struct Col {
    base: u64,
    /// The bit position of the column's first value in `bytes`.
    bit: usize,
    bits: u32,
    mask: u64,
}

/// The columns of block `b` of `s`, and the length of its data in bytes (header included),
/// or `None` if the header is malformed.
fn columns(s: &Segment, b: usize, n: usize) -> Option<([Col; 4], usize)> {
    let bytes: &[u8] = &s.bytes;
    let start = *s.offsets.get(b)? as usize;
    let header = bytes.get(start..start + 4)?;
    let mut pos = start + 4;
    let mut cols = [Col::default(); 4];
    let first = s.firsts.get(b)?;
    // A graph-first segment's blocks hold the columns in the order of the one it shares them
    // with, graph last.
    let first = if s.graph_first {
        &[first[1], first[2], first[3], first[0]]
    } else {
        first
    };
    for c in 0..4 {
        let bits = u32::from(header[c]);
        if bits > 64 {
            return None;
        }
        let back = get_varint(bytes, &mut pos)?;
        cols[c] = Col {
            base: first[c].wrapping_sub(back),
            bit: 0,
            bits,
            mask: mask(bits),
        };
    }
    let mut bit = pos * 8;
    for col in &mut cols {
        col.bit = bit;
        bit += n * col.bits as usize;
    }
    if s.graph_first {
        cols = [cols[3], cols[0], cols[1], cols[2]];
    }
    Some((cols, bit.div_ceil(8) - start))
}

impl Segment {
    /// Builds a segment from quads that are sorted and free of duplicates.
    pub(crate) fn from_sorted(quads: impl IntoIterator<Item = Key>) -> Self {
        Self::with_block(quads, BLOCK, None)
    }

    /// As [`Segment::from_sorted`], written to a file of `spill` as it is built if there is
    /// one.
    pub(crate) fn from_sorted_in(
        quads: impl IntoIterator<Item = Key>,
        spill: Option<&Spill>,
    ) -> Self {
        Self::with_block(quads, BLOCK, spill)
    }

    /// Every quad, in order, decoded on every core into `out` (which is cleared first).
    pub(crate) fn decode_all(&self, out: &mut Vec<Key>) {
        /// Blocks decoded by one task.
        const PART: usize = 1024;
        out.clear();
        out.resize(self.len, [0; 4]);
        let mut parts = Vec::with_capacity(self.blocks().div_ceil(PART));
        let mut rest = &mut out[..];
        for b0 in (0..self.blocks()).step_by(PART) {
            let b1 = (b0 + PART).min(self.blocks());
            let n = self.block_start(b1) - self.block_start(b0);
            let (part, r) = std::mem::take(&mut rest).split_at_mut(n);
            parts.push((b0..b1, part));
            rest = r;
        }
        let bytes: &[u8] = &self.bytes;
        parts.into_par_iter().for_each(|(blocks, out)| {
            let mut at = 0;
            for b in blocks {
                let n = self.block_len(b);
                // Checked when the segment was built or read.
                let cols = columns(self, b, n).map_or([Col::default(); 4], |(c, _)| c);
                for (c, col) in cols.iter().enumerate() {
                    let mut bit = col.bit;
                    for k in &mut out[at..at + n] {
                        k[c] = col
                            .base
                            .wrapping_add(read_bits(bytes, bit, col.bits, col.mask));
                        bit += col.bits as usize;
                    }
                }
                at += n;
            }
        });
    }

    /// As [`Segment::from_sorted`], from a slice: the blocks are encoded on every core, and
    /// the counts kept with the segment are taken meanwhile.
    pub(crate) fn from_sorted_slice(quads: &[Key]) -> Self {
        /// Quads per part encoded by one task.
        const PART: usize = 1024 * BLOCK;
        if quads.len() <= PART {
            return Self::from_sorted(quads.iter().copied());
        }
        let (parts, stats) = rayon::join(
            || {
                quads
                    .par_chunks(PART)
                    .map(|part| {
                        let mut out = Builder::default();
                        for block in part.chunks(BLOCK) {
                            out.block(block);
                        }
                        out
                    })
                    .collect::<Vec<_>>()
            },
            || {
                let mut stats = StatsBuilder::default();
                for q in quads {
                    stats.push(q);
                }
                stats.finish()
            },
        );
        let mut out = Builder {
            len: 0,
            firsts: Vec::with_capacity(quads.len().div_ceil(BLOCK)),
            offsets: Vec::with_capacity(quads.len().div_ceil(BLOCK) + 1),
            starts: Vec::with_capacity(quads.len().div_ceil(BLOCK) + 1),
            bytes: Vec::with_capacity(parts.iter().map(|p| p.bytes.len()).sum::<usize>() + PAD),
            ..Builder::default()
        };
        for part in parts {
            let (len, bytes) = (out.len as u64, out.bytes.len() as u64);
            out.firsts.extend_from_slice(&part.firsts);
            out.offsets.extend(part.offsets.iter().map(|o| o + bytes));
            out.starts.extend(part.starts.iter().map(|s| s + len));
            out.bytes.extend_from_slice(&part.bytes);
            out.len += part.len;
        }
        let segment = out.finish();
        let _ = segment.stats.set(Arc::new(stats));
        segment
    }

    /// As [`Segment::from_sorted_slice`], written to a file of `spill` when given: the
    /// blocks are encoded on every core, a few parts at a time, and appended to the file.
    pub(crate) fn from_sorted_slice_in(quads: &[Key], spill: Option<&Spill>) -> Self {
        /// Quads per part encoded by one task.
        const PART: usize = 1024 * BLOCK;
        if spill.is_none() {
            return Self::from_sorted_slice(quads);
        }
        let (segment, stats) = rayon::join(
            || {
                let mut out = Builder::new(spill);
                let group = PART * rayon::current_num_threads().max(1);
                for quads in quads.chunks(group) {
                    let parts: Vec<Builder> = quads
                        .par_chunks(PART)
                        .map(|part| {
                            let mut b = Builder::default();
                            for block in part.chunks(BLOCK) {
                                b.block(block);
                            }
                            b
                        })
                        .collect();
                    for part in parts {
                        out.append(part);
                    }
                }
                out.finish()
            },
            || {
                let mut stats = StatsBuilder::default();
                for q in quads {
                    stats.push(q);
                }
                stats.finish()
            },
        );
        let _ = segment.stats.set(Arc::new(stats));
        segment
    }

    /// As [`Segment::from_sorted`], with blocks of `block` quads (at most [`BLOCK`]).
    fn with_block(
        quads: impl IntoIterator<Item = Key>,
        block: usize,
        spill: Option<&Spill>,
    ) -> Self {
        let mut out = Builder::new(spill);
        let mut stats = StatsBuilder::default();
        let mut chunk: Vec<Key> = Vec::with_capacity(block);
        for q in quads {
            debug_assert!(
                chunk.last().is_none_or(|p| *p < q),
                "quads are sorted and unique"
            );
            stats.push(&q);
            chunk.push(q);
            if chunk.len() == block {
                out.block(&chunk);
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            out.block(&chunk);
        }
        let segment = out.finish();
        let _ = segment.stats.set(Arc::new(stats.finish()));
        segment
    }

    /// A segment saved by an earlier version (varint-delta blocks of `block` quads, or of
    /// `starts[b + 1] - starts[b]`), converted to this format; `None` if it is not
    /// consistent.
    pub(crate) fn from_legacy(
        len: usize,
        firsts: Buf<Key>,
        offsets: Buf<u64>,
        starts: Option<Buf<u64>>,
        bytes: Buf<u8>,
    ) -> Option<Self> {
        let old = legacy::Segment::new(len, firsts, offsets, starts, bytes);
        if !old.is_consistent() {
            return None;
        }
        Some(Self::from_sorted(old.iter()))
    }

    /// This segment with a delta applied (`delta` in key order: `1` a quad added, not in the
    /// segment, `-1` one removed, in it), without re-encoding what the delta doesn't touch:
    /// blocks without a delta key are copied byte for byte, the others are decoded, merged
    /// with their delta keys and written again (#213). Blocks may then hold from half of
    /// [`BLOCK`] to [`BLOCK`] quads (the last one fewer): a block left with fewer is merged
    /// with the next one, one with more is split. The counts are those of the segment
    /// adjusted for the delta ([`SegStats::with_delta`]).
    pub(crate) fn merged(self: &Arc<Self>, delta: &[(Key, i64)], spill: Option<&Spill>) -> Segment {
        let mut out = Builder::new(spill);
        let blocks = self.blocks();
        let mut d = 0;
        // Merged quads of touched blocks not written yet: fewer than half a block.
        let mut pending: Vec<Key> = Vec::new();
        let mut decoded: Vec<Key> = Vec::with_capacity(BLOCK);
        for b in 0..blocks {
            // The delta keys of block `b`: those before the next block's first (and, for the
            // first block, those before its own first too).
            let end = if b + 1 < blocks {
                d + delta[d..].partition_point(|(k, _)| *k < self.firsts[b + 1])
            } else {
                delta.len()
            };
            if end == d && pending.is_empty() {
                out.copy_block(self, b);
                continue;
            }
            decoded.clear();
            let cursor = Cursor::at_block(self, b);
            decoded.extend((0..cursor.n).map(|i| cursor.key(self, i)));
            let mut keys = delta[d..end].iter().peekable();
            for &q in &decoded {
                while let Some((k, sign)) = keys.next_if(|(k, _)| *k < q) {
                    if *sign > 0 {
                        pending.push(*k);
                    }
                }
                // A removed quad is in the segment: skip it.
                if keys.next_if(|(k, sign)| *k == q && *sign < 0).is_none() {
                    pending.push(q);
                }
            }
            pending.extend(keys.filter(|(_, s)| *s > 0).map(|(k, _)| *k));
            d = end;
            if pending.len() >= BLOCK / 2 {
                out.blocks_of(&pending);
                pending.clear();
            }
        }
        // Added quads past the last block, or a segment that had none.
        pending.extend(delta[d..].iter().filter(|(_, s)| *s > 0).map(|(k, _)| *k));
        out.blocks_of(&pending);
        let segment = out.finish();
        let _ = segment.stats.set(Arc::new(
            self.stats()
                .with_delta(&|low, high| self.count_range(low, high), delta),
        ));
        debug_assert!(segment.is_consistent());
        segment
    }

    /// This segment, of a permutation with the graph last (SPOG, POSG, OSPG, PSOG) whose
    /// quads are all in the default graph, as the permutation with the graph moved to the
    /// front (GSPO, GPOS, GOSP, GPSO): the same quads in the same order, read from the same
    /// blocks (shared from now on, not copied), so it takes only its `firsts` (#152). `None`
    /// when a quad is in a named graph, or the segment is itself graph-first.
    pub(crate) fn graph_first(&mut self) -> Option<Segment> {
        let firsts: Vec<Key> = self.firsts.iter().map(graph_to_front).collect();
        self.graph_first_with(firsts.into(), true)
    }

    /// [`Segment::graph_first`] with its `firsts` given (as saved). With `check`, `None` also
    /// when they are not this segment's with the graph moved to the front; without, only
    /// their number is checked, so that a mapped segment is not read (a checkpoint's files
    /// are checked against their sums).
    pub(crate) fn graph_first_with(&mut self, firsts: Buf<Key>, check: bool) -> Option<Segment> {
        if self.graph_first
            || firsts.len() != self.firsts.len()
            || check
                && (!self.only_default_graph()
                    || firsts
                        .iter()
                        .zip(self.firsts.iter())
                        .any(|(f, s)| *f != graph_to_front(s)))
        {
            return None;
        }
        let segment = Segment {
            len: self.len,
            firsts,
            offsets: self.offsets.share(),
            starts: self.starts.share(),
            bytes: self.bytes.share(),
            stats: OnceLock::new(),
            graph_first: true,
            file: None,
        };
        // Its counts are another permutation's: computed on first use, or read from
        // `stats.bin`.
        Some(segment)
    }

    /// Whether every quad has graph 0 (the last column): the last column's bit width is 0
    /// and its base 0 in every block.
    pub(crate) fn only_default_graph(&self) -> bool {
        (0..self.blocks()).all(|b| {
            self.firsts[b][3] == 0
                && columns(self, b, self.block_len(b)).is_some_and(|(c, _)| c[3].bits == 0)
        })
    }

    /// Whether the blocks hold on average less than three quarters of [`BLOCK`] (after
    /// partial compactions): scans then read more block headers than they need to.
    pub(crate) fn is_fragmented(&self) -> bool {
        self.blocks() * BLOCK * 3 > self.len * 4 + 3 * BLOCK
    }

    /// The segment's counts, computed by a scan the first time if the segment was read from
    /// a checkpoint without them.
    pub(crate) fn stats(&self) -> Arc<SegStats> {
        Arc::clone(
            self.stats
                .get_or_init(|| Arc::new(SegStats::of(self.quads()))),
        )
    }

    /// Every quad, in order.
    pub(crate) fn quads(&self) -> impl Iterator<Item = Key> + '_ {
        let mut range = self.range_of([0; 4], [u64::MAX; 4]);
        std::iter::from_fn(move || range.next(self))
    }

    /// Whether a segment read from disk is consistent enough to be read without going out of
    /// bounds: as many offsets and starts as blocks plus one, blocks of 1 to [`BLOCK`] quads
    /// that add up to its length, first quads in order, and block headers whose columns fill
    /// exactly the block's bytes and give back its first quad.
    pub(crate) fn is_consistent(&self) -> bool {
        let blocks = self.firsts.len();
        if self.len == 0 {
            return blocks == 0 && self.offsets.len() <= 1 && self.starts.len() <= 1;
        }
        let (offsets, starts): (&[u64], &[u64]) = (&self.offsets, &self.starts);
        if starts.len() != blocks + 1
            || starts[0] != 0
            || starts[blocks] != self.len as u64
            || starts
                .windows(2)
                .any(|w| w[0] >= w[1] || w[1] - w[0] > BLOCK as u64)
            || offsets.len() != blocks + 1
            || offsets[0] != 0
            || offsets.windows(2).any(|w| w[0] > w[1])
            || self.bytes.len() as u64 != offsets[blocks] + PAD as u64
            || self.firsts.windows(2).any(|w| w[0] >= w[1])
        {
            return false;
        }
        (0..blocks).all(|b| {
            let n = self.block_len(b);
            // The columns fill the block's bytes, so every read stays inside `bytes`.
            let Some((cols, size)) = columns(self, b, n) else {
                return false;
            };
            if size as u64 != offsets[b + 1] - offsets[b] {
                return false;
            }
            let mut cursor = Cursor::done();
            cursor.n = n;
            cursor.cols = cols;
            cursor.key(self, 0) == self.firsts[b]
        })
    }

    pub(crate) fn heap_size(&self) -> usize {
        self.firsts.heap_size()
            + self.offsets.heap_size()
            + self.bytes.heap_size()
            + self.starts.heap_size()
    }

    pub(crate) fn mapped_size(&self) -> usize {
        self.firsts.mapped_size()
            + self.offsets.mapped_size()
            + self.bytes.mapped_size()
            + self.starts.mapped_size()
    }

    fn blocks(&self) -> usize {
        self.firsts.len()
    }

    /// The position of block `b`'s first quad (`b` up to the number of blocks).
    #[inline]
    fn block_start(&self, b: usize) -> usize {
        self.starts[b] as usize
    }

    #[inline]
    fn block_len(&self, b: usize) -> usize {
        self.block_start(b + 1) - self.block_start(b)
    }

    /// The block that would hold `key`: the last block whose first quad is `<= key`.
    fn block_for(&self, key: &Key) -> usize {
        self.firsts.partition_point(|f| f <= key).saturating_sub(1)
    }

    pub(crate) fn contains(&self, key: &Key) -> bool {
        if self.len == 0 {
            return false;
        }
        let mut cursor = Cursor::at_block(self, self.block_for(key));
        cursor.skip_below(self, key);
        cursor.i < cursor.n && cursor.key(self, cursor.i) == *key
    }

    /// The number of quads `q` with `low <= q <= high`, without decoding the blocks in between:
    /// the block sizes are known, so only the two blocks at the ends of the range are read.
    pub(crate) fn count_range(self: &Arc<Self>, low: Key, high: Key) -> usize {
        if self.len == 0 || low > high {
            return 0;
        }
        // The position of the first quad `>= key`.
        let rank = |key: &Key| -> usize {
            let b = self.firsts.partition_point(|f| f < key);
            if b == 0 {
                return 0;
            }
            // The quad is in block `b - 1` (whose first quad is below `key`) or starts `b`.
            let mut cursor = Cursor::at_block(self, b - 1);
            cursor.skip_below(self, key);
            self.block_start(b - 1) + cursor.i
        };
        let end = match high.iter().rposition(|&c| c != u64::MAX) {
            // The first key above `high`.
            Some(i) => {
                let mut next = high;
                next[i] += 1;
                next[i + 1..].fill(0);
                rank(&next)
            }
            None => self.len,
        };
        end.saturating_sub(rank(&low))
    }

    /// All quads, in order.
    pub(crate) fn iter(self: &Arc<Self>) -> SegmentIter {
        self.range([0; 4], [u64::MAX; 4])
    }

    /// Quads `q` with `low <= q <= high`, in order.
    pub(crate) fn range(self: &Arc<Self>, low: Key, high: Key) -> SegmentIter {
        SegmentIter {
            segment: Arc::clone(self),
            range: self.range_of(low, high),
        }
    }

    /// Like [`Segment::range`], for a caller that keeps the segment itself.
    pub(crate) fn range_of(&self, low: Key, high: Key) -> Range {
        let cursor = if self.len == 0 {
            Cursor::done()
        } else {
            let mut cursor = Cursor::at_block(self, self.block_for(&low));
            cursor.skip_below(self, &low);
            cursor
        };
        Range { cursor, low, high }
    }
}

/// Writes a segment block by block, on the heap or, given a [`Spill`], to a file a few
/// megabytes at a time.
#[derive(Default)]
struct Builder {
    len: usize,
    firsts: Vec<Key>,
    offsets: Vec<u64>,
    starts: Vec<u64>,
    /// The block bytes not written to `run` yet.
    bytes: Vec<u8>,
    /// Block bytes written to `run` (they come before `bytes`).
    written: u64,
    run: Option<RunWriter>,
}

/// Block bytes a [`Builder`] collects before writing them to its file.
const SPILL_AT: usize = if cfg!(test) { 4096 } else { 4 << 20 };

impl Builder {
    fn new(spill: Option<&Spill>) -> Self {
        let run = spill.and_then(|s| match s.writer() {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("tripletrail-store: building a segment on the heap: {e}");
                None
            }
        });
        Self {
            run,
            ..Self::default()
        }
    }

    /// Appends the blocks of `part`, built on the heap, after these.
    fn append(&mut self, part: Builder) {
        let (len, at) = (self.len as u64, self.at());
        self.firsts.extend_from_slice(&part.firsts);
        self.offsets.extend(part.offsets.iter().map(|o| o + at));
        self.starts.extend(part.starts.iter().map(|s| s + len));
        self.bytes.extend_from_slice(&part.bytes);
        self.len += part.len;
        if self.bytes.len() >= SPILL_AT {
            self.spill();
        }
    }

    /// Where the next block starts.
    fn at(&self) -> u64 {
        self.written + self.bytes.len() as u64
    }

    /// Starts a block with first quad `first`.
    fn start(&mut self, first: Key) {
        if self.bytes.len() >= SPILL_AT {
            self.spill();
        }
        self.offsets.push(self.at());
        self.starts.push(self.len as u64);
        self.firsts.push(first);
    }

    /// Writes the bytes collected to the file, if there is one.
    fn spill(&mut self) {
        let Some(run) = &mut self.run else { return };
        match run.write(&self.bytes) {
            Ok(()) => {
                self.written += self.bytes.len() as u64;
                self.bytes.clear();
            }
            Err(e) => {
                eprintln!("tripletrail-store: building a segment on the heap: {e}");
                // What the file holds comes back to the heap, and the rest stays there.
                let run = self.run.take().expect("checked above");
                let mut bytes = run
                    .read_back()
                    .expect("a segment file just written can be read back");
                bytes.append(&mut self.bytes);
                self.bytes = bytes;
                self.written = 0;
            }
        }
    }

    /// `quads` (sorted, 1 to [`BLOCK`]) as one block.
    fn block(&mut self, quads: &[Key]) {
        self.start(quads[0]);
        encode_block(&mut self.bytes, quads);
        self.len += quads.len();
    }

    /// Block `b` of `s` as it is (decoded and written again from a graph-first segment, whose
    /// blocks have their columns in another order).
    fn copy_block(&mut self, s: &Segment, b: usize) {
        if s.graph_first {
            let cursor = Cursor::at_block(s, b);
            let quads: Vec<Key> = (0..cursor.n).map(|i| cursor.key(s, i)).collect();
            self.block(&quads);
            return;
        }
        self.start(s.firsts[b]);
        let (from, to) = (s.offsets[b] as usize, s.offsets[b + 1] as usize);
        self.bytes.extend_from_slice(&s.bytes[from..to]);
        self.len += s.block_len(b);
    }

    /// `quads` (sorted) as blocks of equal size, as few as hold at most [`BLOCK`] each.
    fn blocks_of(&mut self, quads: &[Key]) {
        let n = quads.len();
        if n == 0 {
            return;
        }
        let k = n.div_ceil(BLOCK);
        let mut at = 0;
        for i in 0..k {
            let size = n / k + usize::from(i < n % k);
            self.block(&quads[at..at + size]);
            at += size;
        }
    }

    fn finish(mut self) -> Segment {
        self.offsets.push(self.at());
        self.starts.push(self.len as u64);
        self.bytes.extend_from_slice(&[0; PAD]);
        self.spill();
        if let Some(run) = self.run.take() {
            match run.finish(self.len, &self.firsts, &self.offsets, &self.starts) {
                Ok(segment) => return segment,
                Err((e, bytes)) => {
                    eprintln!("tripletrail-store: building a segment on the heap: {e}");
                    self.bytes = bytes;
                }
            }
        }
        // Built by doubling: give back what the doubling left over.
        self.bytes.shrink_to_fit();
        self.firsts.shrink_to_fit();
        self.offsets.shrink_to_fit();
        self.starts.shrink_to_fit();
        Segment {
            len: self.len,
            firsts: self.firsts.into(),
            offsets: self.offsets.into(),
            starts: self.starts.into(),
            bytes: self.bytes.into(),
            stats: OnceLock::new(),
            graph_first: false,
            file: None,
        }
    }
}

/// Quads decoded at a time by [`Cursor::next`]: few enough that a lookup reading a handful
/// of quads decodes little more, many enough that each column is read in a tight loop.
const CHUNK: usize = 16;

/// A position inside a segment: quad `i` of the `n` of block `block`.
struct Cursor {
    block: usize,
    i: usize,
    n: usize,
    cols: [Col; 4],
    /// Quads `at..at + buf.len()` of the block, decoded.
    buf: Vec<Key>,
    at: usize,
    /// Quads the next [`Cursor::fill`] decodes. Right after a seek, which often reads only a
    /// few, quads are read one by one first: `burst` counts up to [`CHUNK`] through them.
    burst: usize,
}

impl Cursor {
    fn done() -> Self {
        Self {
            block: usize::MAX,
            i: 0,
            n: 0,
            cols: [Col::default(); 4],
            buf: Vec::new(),
            at: 0,
            burst: CHUNK,
        }
    }

    fn at_block(s: &Segment, block: usize) -> Self {
        let mut cursor = Self::done();
        cursor.enter(s, block);
        cursor
    }

    /// Moves to the start of block `block` (done past the last), keeping the buffer.
    fn enter(&mut self, s: &Segment, block: usize) {
        self.buf.clear();
        self.at = 0;
        self.i = 0;
        self.burst = CHUNK;
        if block >= s.blocks() {
            self.block = usize::MAX;
            self.n = 0;
            return;
        }
        self.block = block;
        self.n = s.block_len(block);
        // Checked when the segment was built or read.
        self.cols = columns(s, block, self.n).map_or([Col::default(); 4], |(c, _)| c);
    }

    /// Quad `i` of the block.
    #[inline(always)]
    fn key(&self, s: &Segment, i: usize) -> Key {
        let bytes: &[u8] = &s.bytes;
        std::array::from_fn(|c| {
            let col = &self.cols[c];
            col.base.wrapping_add(read_bits(
                bytes,
                col.bit + i * col.bits as usize,
                col.bits,
                col.mask,
            ))
        })
    }

    /// Whether quad `i` of the block is below `key`, reading its columns only as far as
    /// they decide it.
    #[inline]
    fn below(&self, s: &Segment, i: usize, key: &Key) -> bool {
        let bytes: &[u8] = &s.bytes;
        for (c, col) in self.cols.iter().enumerate() {
            let v = col.base.wrapping_add(read_bits(
                bytes,
                col.bit + i * col.bits as usize,
                col.bits,
                col.mask,
            ));
            if v != key[c] {
                return v < key[c];
            }
        }
        false
    }

    /// Decodes quads `i..` (up to [`CHUNK`]) into the buffer, a column at a time.
    #[inline(never)]
    fn fill(&mut self, s: &Segment, i: usize) {
        let bytes: &[u8] = &s.bytes;
        let m = CHUNK.min(self.n - i);
        self.buf.clear();
        self.buf.resize(m, [0; 4]);
        for c in 0..4 {
            let col = self.cols[c];
            if col.bits == 0 {
                for k in &mut self.buf {
                    k[c] = col.base;
                }
                continue;
            }
            let mut bit = col.bit + i * col.bits as usize;
            for k in &mut self.buf {
                k[c] = col
                    .base
                    .wrapping_add(read_bits(bytes, bit, col.bits, col.mask));
                bit += col.bits as usize;
            }
        }
        self.at = i;
    }

    /// Moves on past the quads of the current block below `key` (a binary search: any quad
    /// of a block can be read on its own).
    fn skip_below(&mut self, s: &Segment, key: &Key) {
        let (mut lo, mut hi) = (self.i, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.below(s, mid, key) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.i = lo;
        self.burst = 0;
    }

    #[inline(always)]
    fn next(&mut self, s: &Segment) -> Option<Key> {
        if self.i == self.n {
            if self.block == usize::MAX || self.block + 1 >= s.blocks() {
                return None;
            }
            self.enter(s, self.block + 1);
        }
        let i = self.i;
        let mut j = i.wrapping_sub(self.at);
        if j >= self.buf.len() {
            if self.burst < CHUNK {
                self.burst += 1;
                self.i += 1;
                return Some(self.key(s, i));
            }
            self.fill(s, i);
            j = 0;
        }
        self.i += 1;
        Some(self.buf[j])
    }
}

/// `a < b`, unrolled (the slice comparison behind `<` on arrays is not).
#[inline]
pub(crate) fn less(a: &Key, b: &Key) -> bool {
    (a[0], a[1], a[2], a[3]) < (b[0], b[1], b[2], b[3])
}

/// A key range of a segment, read with the segment passed to each call.
pub(crate) struct Range {
    cursor: Cursor,
    low: Key,
    high: Key,
}

impl Range {
    /// Ends the range: it yields nothing more.
    pub(crate) fn clear(&mut self) {
        self.cursor = Cursor::done();
    }

    /// Moves on to the keys at or above `key`. Blocks wholly before it are not read: the
    /// block that holds `key` is found by a galloping search from the current one, so short
    /// skips cost a few comparisons and long ones a binary search; inside the block, a binary
    /// search.
    pub(crate) fn seek(&mut self, s: &Segment, key: Key) {
        if !less(&self.low, &key) {
            return;
        }
        self.low = key;
        let b = self.cursor.block;
        if b == usize::MAX {
            return;
        }
        if less(&self.high, &key) {
            self.cursor = Cursor::done();
            return;
        }
        let firsts: &[Key] = &s.firsts;
        if b + 1 < firsts.len() && !less(&key, &firsts[b + 1]) {
            // The last block whose first key is `<= key`, after `b`.
            let mut step = 1;
            while b + step < firsts.len() && !less(&key, &firsts[b + step]) {
                step *= 2;
            }
            let lo = b + step / 2;
            let hi = (b + step).min(firsts.len());
            let block = lo + firsts[lo..hi].partition_point(|f| !less(&key, f)) - 1;
            self.cursor.enter(s, block);
        }
        self.cursor.skip_below(s, &key);
    }

    #[inline(always)]
    pub(crate) fn next(&mut self, segment: &Segment) -> Option<Key> {
        loop {
            let q = self.cursor.next(segment)?;
            if less(&self.high, &q) {
                self.cursor = Cursor::done();
                return None;
            }
            if !less(&q, &self.low) {
                return Some(q);
            }
        }
    }

    /// Appends the next keys of the range to `out`, up to [`CHUNK`] at a time, decoded a
    /// column at a time; `false` once the range is done (nothing appended).
    pub(crate) fn next_chunk(&mut self, s: &Segment, out: &mut Vec<Key>) -> bool {
        let c = &mut self.cursor;
        if c.i == c.n {
            if c.block == usize::MAX || c.block + 1 >= s.blocks() {
                return false;
            }
            c.enter(s, c.block + 1);
        }
        if c.i.wrapping_sub(c.at) >= c.buf.len() {
            c.fill(s, c.i);
        }
        let keys = &c.buf[c.i - c.at..];
        // The cursor never stands below `low`; only the end of the range is checked.
        let take = if keys.last().is_some_and(|k| less(&self.high, k)) {
            keys.partition_point(|k| !less(&self.high, k))
        } else {
            keys.len()
        };
        out.extend_from_slice(&keys[..take]);
        if take < keys.len() {
            self.cursor = Cursor::done();
            return take > 0;
        }
        c.i += take;
        true
    }
}

/// Iterates a key range of a segment it shares ownership of.
pub(crate) struct SegmentIter {
    segment: Arc<Segment>,
    range: Range,
}

impl Iterator for SegmentIter {
    type Item = Key;

    #[inline]
    fn next(&mut self) -> Option<Key> {
        self.range.next(&self.segment)
    }
}

/// The block format of checkpoints written before #169 step 3, read only to convert them:
/// quads as differences from the quad before (one byte saying which column first differs,
/// the gap in that column, then the remaining columns as they are, all as varints).
mod legacy {
    use super::{Key, get_varint};
    use crate::buf::Buf;

    pub(super) struct Segment {
        len: usize,
        block: usize,
        firsts: Buf<Key>,
        offsets: Buf<u64>,
        starts: Option<Buf<u64>>,
        bytes: Buf<u8>,
    }

    impl Segment {
        pub(super) fn new(
            len: usize,
            firsts: Buf<Key>,
            offsets: Buf<u64>,
            starts: Option<Buf<u64>>,
            bytes: Buf<u8>,
        ) -> Self {
            // The files don't record the block size, but every block size was a power of two
            // and only the last block was short.
            let block = if firsts.len() <= 1 {
                len.max(1)
            } else {
                len.div_ceil(firsts.len()).next_power_of_two()
            };
            Self {
                len,
                block,
                firsts,
                offsets,
                starts,
                bytes,
            }
        }

        fn block_len(&self, b: usize) -> usize {
            match &self.starts {
                Some(starts) => (starts[b + 1] - starts[b]) as usize,
                None => ((b + 1) * self.block).min(self.len) - b * self.block,
            }
        }

        /// As the old `is_consistent`, but every block is checked to decode to exactly its
        /// bytes, since the conversion reads them all.
        pub(super) fn is_consistent(&self) -> bool {
            let blocks = self.firsts.len();
            if self.len == 0 {
                return blocks == 0 && self.bytes.is_empty() && self.offsets.len() <= 1;
            }
            let sizes_ok = match &self.starts {
                Some(starts) => {
                    starts.len() == blocks + 1
                        && starts[0] == 0
                        && starts[blocks] == self.len as u64
                        && starts.windows(2).all(|w| w[0] < w[1])
                }
                None => self.block != 0 && blocks == self.len.div_ceil(self.block),
            };
            if !sizes_ok
                || self.offsets.len() != blocks + 1
                || self.offsets[0] != 0
                || self.offsets.last() != Some(&(self.bytes.len() as u64))
                || self.offsets.windows(2).any(|w| w[0] > w[1])
                || self.firsts.windows(2).any(|w| w[0] >= w[1])
            {
                return false;
            }
            (0..blocks).all(|b| {
                let mut pos = self.offsets[b] as usize;
                let end = self.offsets[b + 1] as usize;
                let bytes = &self.bytes[..end];
                for _ in 1..self.block_len(b) {
                    let Some(&i) = bytes.get(pos) else {
                        return false;
                    };
                    pos += 1;
                    for _ in usize::from(i.min(3))..4 {
                        if get_varint(bytes, &mut pos).is_none() {
                            return false;
                        }
                    }
                }
                pos == end
            })
        }

        /// Every quad, in order (on a consistent segment).
        pub(super) fn iter(&self) -> impl Iterator<Item = Key> + '_ {
            (0..self.firsts.len()).flat_map(move |b| {
                let mut pos = self.offsets[b] as usize;
                let mut q = self.firsts[b];
                (0..self.block_len(b)).map(move |k| {
                    if k > 0 {
                        let bytes: &[u8] = &self.bytes;
                        let i = usize::from(bytes[pos].min(3));
                        pos += 1;
                        let mut v = || get_varint(bytes, &mut pos).unwrap_or(0);
                        q[i] = q[i].wrapping_add(v());
                        for c in &mut q[i + 1..] {
                            *c = v();
                        }
                    }
                    q
                })
            })
        }
    }

    #[cfg(test)]
    pub(super) fn encode(quads: &[Key], block: usize) -> (Vec<Key>, Vec<u64>, Vec<u8>) {
        let (mut firsts, mut offsets, mut bytes) = (Vec::new(), vec![0], Vec::new());
        for (n, q) in quads.iter().enumerate() {
            if n % block == 0 {
                if n > 0 {
                    offsets.push(bytes.len() as u64);
                }
                firsts.push(*q);
            } else {
                let prev = quads[n - 1];
                let i = (0..4).find(|&i| q[i] != prev[i]).unwrap_or(3);
                bytes.push(i as u8);
                super::put_varint(&mut bytes, q[i] - prev[i]);
                for &c in &q[i + 1..] {
                    super::put_varint(&mut bytes, c);
                }
            }
        }
        if !quads.is_empty() {
            offsets.push(bytes.len() as u64);
        }
        (firsts, offsets, bytes)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn segments_written_to_files_match_those_on_the_heap() {
        let dir = tripletrail_testdir::tempdir("spill");
        let spill = Spill::new(&dir).unwrap();
        let quads: Vec<Key> = (0..50_000u64).map(|i| [i / 7, i % 7, i * 3, 0]).collect();
        let heap = Arc::new(Segment::from_sorted(quads.iter().copied()));
        let file = Arc::new(Segment::from_sorted_in(quads.iter().copied(), Some(&spill)));
        assert!(file.file.is_some() && file.heap_size() == 0);
        assert!(file.iter().eq(heap.iter()));
        // A partial merge, block by block, to a file.
        let delta: Vec<(Key, i64)> =
            vec![([3, 1, 1, 0], 1), (quads[700], -1), ([9000, 0, 0, 0], 1)];
        let merged = Arc::new(file.merged(&delta, Some(&spill)));
        let expected = Arc::new(heap.merged(&delta, None));
        assert!(merged.file.is_some());
        assert!(merged.iter().eq(expected.iter()));
        let runs = || std::fs::read_dir(dir.join("runs")).unwrap().count();
        assert_eq!(runs(), 2);
        // A file goes with the last segment read from it.
        drop(file);
        assert_eq!(runs(), 1);
    }

    use super::*;

    fn quads(n: u64) -> Vec<Key> {
        let mut v: Vec<Key> = (0..n)
            .map(|i| [i / 7, (i * 31) % 5, i * 1_000_003 % 977, i % 3])
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// A segment of the default graph read graph first (#152) holds the quads of the
    /// permutation with the graph first, built on its own: the same reads, ranges, counts and
    /// merges, from the same blocks.
    #[test]
    fn graph_first_segments_read_the_blocks_of_another() {
        // Graph last, all in the default graph.
        let spog: Vec<Key> = quads(5_000)
            .into_iter()
            .map(|q| [q[0], q[1], q[2], 0])
            .collect();
        let mut spog_seg = Segment::from_sorted_slice(&spog);
        let gspo: Vec<Key> = spog.iter().map(graph_to_front).collect();
        let built = Arc::new(Segment::from_sorted_slice(&gspo));
        let view = Arc::new(spog_seg.graph_first().unwrap());
        assert!(view.graph_first && view.is_consistent());
        assert!(
            std::ptr::eq(view.bytes.as_ptr(), spog_seg.bytes.as_ptr()),
            "shared"
        );
        assert_eq!(view.iter().collect::<Vec<_>>(), gspo);
        let mut all = Vec::new();
        view.decode_all(&mut all);
        assert_eq!(all, gspo);
        assert_eq!(*view.stats(), *built.stats());
        for (i, k) in gspo.iter().enumerate().step_by(37) {
            assert!(view.contains(k));
            let high = gspo[(i + 300).min(gspo.len() - 1)];
            assert_eq!(view.count_range(*k, high), built.count_range(*k, high));
            assert_eq!(
                view.range(*k, high).collect::<Vec<_>>(),
                built.range(*k, high).collect::<Vec<_>>()
            );
        }
        assert!(!view.contains(&[1, 0, 0, 0]));
        // A delta, here with a quad in a named graph: blocks read graph first are written
        // again, not copied, and the result is a segment of its own.
        let delta: Vec<(Key, i64)> = vec![(gspo[10], -1), ([0, 3, 9, 9_999], 1), ([7, 1, 2, 3], 1)];
        let merged = view.merged(&delta, None);
        assert!(!merged.graph_first && merged.is_consistent());
        let mut want: Vec<Key> = gspo.clone();
        want.remove(10);
        want.push([0, 3, 9, 9_999]);
        want.push([7, 1, 2, 3]);
        want.sort_unstable();
        assert_eq!(Arc::new(merged).iter().collect::<Vec<_>>(), want);
        // A segment with a quad in a named graph has none; nor has a graph-first one.
        let mut named = Segment::from_sorted_slice(&quads(100));
        assert!(named.graph_first().is_none());
        let mut again = Segment {
            graph_first: true,
            ..Segment::from_sorted_slice(&spog)
        };
        assert!(again.graph_first().is_none());
        // Saved firsts that are not the segment's are refused when checked.
        let mut wrong: Vec<Key> = spog_seg.firsts.iter().map(graph_to_front).collect();
        wrong[1][1] += 1;
        assert!(
            spog_seg
                .graph_first_with(wrong.clone().into(), true)
                .is_none()
        );
        assert!(spog_seg.graph_first_with(wrong.into(), false).is_some());
    }

    #[test]
    fn merging_a_delta_rewrites_only_touched_blocks_and_keeps_every_read() {
        let mut x: u64 = 3;
        let mut next = |m: u64| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) % m
        };
        for (n, block) in [(0, 64), (1, 64), (5, 64), (40, 64), (3000, 64), (3000, 16)] {
            let mut model: std::collections::BTreeSet<Key> = quads(n).into_iter().collect();
            let mut s = Arc::new(Segment::with_block(model.iter().copied(), block, None));
            for round in 0..12 {
                let mut delta = std::collections::BTreeMap::new();
                for _ in 0..(1 + round * 7) {
                    let q = [next(500), next(5), next(977), next(3)];
                    if model.remove(&q) {
                        if delta.remove(&q).is_none() {
                            delta.insert(q, -1);
                        }
                    } else {
                        model.insert(q);
                        if delta.remove(&q).is_none() {
                            delta.insert(q, 1);
                        }
                    }
                }
                // Remove a whole run too, so blocks empty out.
                if round == 5 {
                    let run: Vec<Key> = model.iter().skip(100).take(200).copied().collect();
                    for q in run {
                        model.remove(&q);
                        if delta.remove(&q).is_none() {
                            delta.insert(q, -1);
                        }
                    }
                }
                let delta: Vec<(Key, i64)> = delta.into_iter().collect();
                let merged = Arc::new(s.merged(&delta, None));
                assert!(merged.is_consistent(), "n = {n} round {round}");
                let all: Vec<Key> = model.iter().copied().collect();
                assert_eq!(
                    merged.iter().collect::<Vec<_>>(),
                    all,
                    "n = {n} round {round}"
                );
                assert_eq!(*merged.stats(), SegStats::of(all.iter().copied()));
                // Untouched blocks keep their bytes: same first quad, same length.
                if round == 0 && n > 1000 {
                    let same = (0..s.blocks())
                        .filter(|&b| {
                            let f = s.firsts[b];
                            let c = merged.firsts.partition_point(|x| *x < f);
                            c < merged.blocks()
                                && merged.firsts[c] == f
                                && merged.block_len(c) == s.block_len(b)
                        })
                        .count();
                    assert!(same + 2 >= s.blocks(), "n = {n}");
                }
                // Blocks hold from half a block to a block, except the last.
                let starts: &[u64] = &merged.starts;
                for w in starts.windows(2).take(merged.blocks().saturating_sub(1)) {
                    assert!((block.min(BLOCK / 2)..=BLOCK).contains(&((w[1] - w[0]) as usize)));
                }
                for q in all.iter().step_by(11) {
                    assert!(merged.contains(q));
                }
                for (i, low) in all.iter().enumerate().step_by(37) {
                    for (j, high) in all.iter().enumerate().skip(i).step_by(53) {
                        assert_eq!(merged.count_range(*low, *high), j - i + 1);
                    }
                }
                s = merged;
            }
        }
    }

    #[test]
    fn varints_of_every_length_round_trip() {
        let mut values: Vec<u64> = (0..64).flat_map(|b| [1u64 << b, (1u64 << b) - 1]).collect();
        values.extend([u64::MAX, 0x7f, 0x80, 0x3fff, 0x4000, 12_345_678_901]);
        let mut bytes = Vec::new();
        for &v in &values {
            put_varint(&mut bytes, v);
        }
        let mut pos = 0;
        for &v in &values {
            assert_eq!(get_varint(&bytes, &mut pos), Some(v));
        }
        assert_eq!(pos, bytes.len());
        assert_eq!(get_varint(&[0x80], &mut 0), None);
    }

    #[test]
    fn parallel_builds_and_decodes_match_the_sequential_ones() {
        let mut quads: Vec<Key> = (0..300_000u64)
            .map(|i| {
                [
                    i / 7,
                    (i * 31) % 11,
                    i.wrapping_mul(2_654_435_761) % 100_000,
                    i % 3,
                ]
            })
            .collect();
        quads.sort_unstable();
        quads.dedup();
        let a = Segment::from_sorted(quads.iter().copied());
        let b = Segment::from_sorted_slice(&quads);
        assert_eq!(a.len, b.len);
        assert_eq!(&a.firsts[..], &b.firsts[..]);
        assert_eq!(&a.offsets[..], &b.offsets[..]);
        assert_eq!(&a.starts[..], &b.starts[..]);
        assert_eq!(&a.bytes[..], &b.bytes[..]);
        assert_eq!(a.stats.get().unwrap().rows, b.stats.get().unwrap().rows);
        let mut out = vec![[9; 4]; 3];
        b.decode_all(&mut out);
        assert_eq!(out, quads);
    }

    #[test]
    fn columns_of_every_width_round_trip() {
        // Values spread over 0 to 64 bits in each column, sorted.
        for spread in [0u32, 1, 7, 8, 9, 31, 55, 56, 57, 63, 64] {
            let top = mask(spread);
            let mut all: Vec<Key> = (0..300u64)
                .map(|i| {
                    let v = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) & top;
                    [i / 50, v, top - v, (i * 7) & top]
                })
                .collect();
            all.sort_unstable();
            all.dedup();
            let s = Arc::new(Segment::from_sorted(all.iter().copied()));
            assert!(s.is_consistent(), "spread {spread}");
            assert_eq!(s.iter().collect::<Vec<_>>(), all, "spread {spread}");
            for q in &all {
                assert!(s.contains(q), "spread {spread}");
            }
        }
    }

    #[test]
    fn round_trip_and_ranges() {
        for (n, block) in [0, 1, 15, 16, 17, 63, 64, 65, 127, 128, 129, 1000]
            .into_iter()
            .flat_map(|n| [(n, 64), (n, 16)])
        {
            let all = quads(n);
            let s = Arc::new(Segment::with_block(all.iter().copied(), block, None));
            assert!(s.is_consistent(), "n = {n}");
            assert_eq!(s.iter().collect::<Vec<_>>(), all, "n = {n}");
            for q in &all {
                assert!(s.contains(q));
            }
            assert_eq!(s.count_range([0; 4], [u64::MAX; 4]), all.len());
            for (i, low) in all.iter().enumerate().step_by(7) {
                for (j, high) in all.iter().enumerate().skip(i).step_by(13) {
                    assert_eq!(s.count_range(*low, *high), j - i + 1, "n = {n}");
                }
            }
            assert!(!s.contains(&[u64::MAX; 4]));
            for (low, high) in [
                ([3, 0, 0, 0], [3, u64::MAX, u64::MAX, u64::MAX]),
                ([10, 2, 0, 0], [40, 0, 0, 0]),
                ([10, 2, 5, 1], [10, 2, 5, 1]),
            ] {
                let expected: Vec<_> = all
                    .iter()
                    .copied()
                    .filter(|q| *q >= low && *q <= high)
                    .collect();
                assert_eq!(s.range(low, high).collect::<Vec<_>>(), expected, "n = {n}");
                assert_eq!(s.count_range(low, high), expected.len(), "n = {n}");
            }
            // Seeks forward, inside a block and across blocks.
            let mut range = s.range_of([0; 4], [u64::MAX; 4]);
            for target in all.iter().step_by(9) {
                range.seek(&s, *target);
                assert_eq!(range.next(&s), Some(*target), "n = {n}");
            }
        }
    }

    #[test]
    fn legacy_segments_convert() {
        for (n, block) in [(0, 16), (1, 16), (17, 16), (1000, 16), (1000, 32)] {
            let all = quads(n);
            let (firsts, offsets, bytes) = legacy::encode(&all, block);
            let s = Segment::from_legacy(
                n as usize,
                firsts.into(),
                offsets.into(),
                None,
                bytes.into(),
            )
            .unwrap_or_else(|| panic!("n = {n}"));
            let s = Arc::new(s);
            assert!(s.is_consistent());
            assert_eq!(s.iter().collect::<Vec<_>>(), all, "n = {n}");
        }
        // A damaged one is refused.
        let all = quads(100);
        let (firsts, offsets, mut bytes) = legacy::encode(&all, 16);
        bytes.pop();
        assert!(
            Segment::from_legacy(all.len(), firsts.into(), offsets.into(), None, bytes.into())
                .is_none()
        );
    }
}
