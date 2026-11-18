//! Parsed geometry literals by dictionary ordinal, read without locks.
//!
//! Ordinals are dense and never reused, so the table is an array indexed by ordinal rather
//! than a hash map: two-level, so it grows by chunks without moving what is already there
//! and readers never wait for a writer. A lookup is two indexed loads.

use crate::geobin::Saved;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tripletrail_geo::Geom;

const CHUNK_BITS: u32 = 14;
const CHUNK: usize = 1 << CHUNK_BITS;
/// Chunks for every `u32` ordinal: 2^18 slots of 24 bytes, 6 MB per store, allocated lazily
/// on first use.
const CHUNKS: usize = 1 << (32 - CHUNK_BITS);

type Slot = OnceLock<Option<Arc<Geom>>>;
type Chunk = OnceLock<Box<[Slot]>>;

#[derive(Default)]
pub(crate) struct Geometries {
    chunks: OnceLock<Box<[Chunk]>>,
    /// The geometries saved with the checkpoint the store was opened from.
    saved: Option<Saved>,
    /// Roughly the heap bytes of the parsed geometries and of the table's chunks (#225).
    bytes: AtomicUsize,
    /// The geometries parsed successfully so far.
    parsed: AtomicUsize,
}

impl Geometries {
    pub(crate) fn with_saved(saved: Option<Saved>) -> Self {
        Self {
            saved,
            ..Self::default()
        }
    }

    /// The saved geometry of `ordinal`, decoded (no parsing).
    pub(crate) fn saved(&self, ordinal: u32) -> Option<Geom> {
        self.saved.as_ref()?.get(ordinal)
    }

    /// The saved record of `ordinal`, as it is.
    pub(crate) fn saved_record(&self, ordinal: u32) -> Option<&[u8]> {
        self.saved.as_ref()?.record(ordinal)
    }

    /// The saved box of `ordinal` (CRS and corners), if the checkpoint saved one.
    pub(crate) fn saved_bbox(
        &self,
        ordinal: u32,
    ) -> Option<(tripletrail_geo::Crs, Option<[f64; 4]>)> {
        self.saved.as_ref()?.bbox(ordinal)
    }

    /// The geometry of `ordinal` if it was asked for already (`Some(None)`: not valid).
    pub(crate) fn peek(&self, ordinal: u32) -> Option<Option<Arc<Geom>>> {
        let chunks = self.chunks.get()?;
        let chunk = chunks[(ordinal >> CHUNK_BITS) as usize].get()?;
        chunk[ordinal as usize & (CHUNK - 1)].get().cloned()
    }

    fn slot(&self, ordinal: u32) -> &Slot {
        let chunks = self.chunks.get_or_init(|| {
            self.bytes
                .fetch_add(CHUNKS * size_of::<Chunk>(), Ordering::Relaxed);
            std::iter::repeat_with(OnceLock::new).take(CHUNKS).collect()
        });
        let chunk = chunks[(ordinal >> CHUNK_BITS) as usize].get_or_init(|| {
            self.bytes
                .fetch_add(CHUNK * size_of::<Slot>(), Ordering::Relaxed);
            std::iter::repeat_with(OnceLock::new).take(CHUNK).collect()
        });
        &chunk[ordinal as usize & (CHUNK - 1)]
    }

    /// The geometry of `ordinal`, parsed with `parse` the first time it is asked for (two
    /// threads asking at once may both parse it; one result is kept). `parse` gives `None`
    /// for an ordinal that has no term yet: nothing is kept then.
    pub(crate) fn get_or_parse(
        &self,
        ordinal: u32,
        parse: impl FnOnce() -> Option<Option<Arc<Geom>>>,
    ) -> Option<Arc<Geom>> {
        let slot = self.slot(ordinal);
        if let Some(g) = slot.get() {
            return g.clone();
        }
        let parsed = parse()?;
        slot.get_or_init(|| {
            if let Some(g) = &parsed {
                self.parsed.fetch_add(1, Ordering::Relaxed);
                self.bytes
                    .fetch_add(size_of::<Geom>() + g.heap_size(), Ordering::Relaxed);
            }
            parsed
        })
        .clone()
    }

    /// Roughly the heap bytes of the geometries parsed so far, and of their table.
    pub(crate) fn heap_size(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// How many geometries were parsed successfully so far.
    pub(crate) fn parsed(&self) -> usize {
        self.parsed.load(Ordering::Relaxed)
    }
}
