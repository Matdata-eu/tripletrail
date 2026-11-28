use geo::BoundingRect;
use oxigraph::model::{Literal, NamedNodeRef, Term};
use rstar::{AABB, RTree, RTreeObject};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tripletrail_geo::{Crs, Geom, parse_wkt_literal};
use tripletrail_store::{Dataset, DatasetError, TermId};

const AS_WKT: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT");

/// An R-tree entry: a bounding box and the slot of its literal (see [`SpatialIndex::slots`]).
///
/// The box is in `f32`, rounded outwards, so it holds the geometry's box: candidates are a
/// superset of the exact ones, and the exact tests are on the geometry. An entry is 20 bytes,
/// and a tree node (a vector and a box) 40: the index takes about 50 bytes a geometry.
///
/// Kept without a niche on purpose: rstar's bulk load collects each leaf in place when a tree
/// node is no larger than an entry, and the leaf then keeps the whole capacity of the slab it
/// was cut from (over 900 MB for 700k entries of 128 bytes, and twice the time).
#[derive(Clone, Copy)]
pub(crate) struct Entry {
    envelope: AABB<[f32; 2]>,
    slot: u32,
}

/// `envelope` in `f32`, rounded outwards so that it holds `envelope`.
fn widen(envelope: &AABB<[f64; 2]>) -> AABB<[f32; 2]> {
    let down = |v: f64| {
        let f = v as f32;
        if f64::from(f) > v { f.next_down() } else { f }
    };
    let up = |v: f64| {
        let f = v as f32;
        if f64::from(f) < v { f.next_up() } else { f }
    };
    let (lo, hi) = (envelope.lower(), envelope.upper());
    AABB::from_corners([down(lo[0]), down(lo[1])], [up(hi[0]), up(hi[1])])
}

const _: () = assert!(size_of::<rstar::RTreeNode<Entry>>() > size_of::<Entry>());

impl Entry {
    /// The bounding box, as lower and upper corners (holding the geometry's, see [`Entry`]).
    pub(crate) fn bbox(&self) -> ([f64; 2], [f64; 2]) {
        let (lo, hi) = (self.envelope.lower(), self.envelope.upper());
        (
            [f64::from(lo[0]), f64::from(lo[1])],
            [f64::from(hi[0]), f64::from(hi[1])],
        )
    }
}

/// Entries are the same when they are the same literal.
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot
    }
}

/// What a slot stands for, besides its term id: its literal and geometry once read. On the
/// tripletrail store only the literal's id is known when the index is built (from bounding boxes
/// alone); the literal and its geometry are read from the store when a query first asks for
/// them, and kept in chunks of [`CACHE_CHUNK`] slots made on first use, so a slot no query
/// asked for costs only its id.
#[derive(Clone, Default)]
struct Cached {
    literal: OnceLock<Box<Literal>>,
    geom: OnceLock<Arc<Geom>>,
}

const CACHE_CHUNK: usize = 1024;

/// The slots of the entries: term ids (`TermId::DEFAULT_GRAPH` when not known, which no
/// geometry has) and what has been read for them.
#[derive(Clone, Default)]
struct Slots {
    ids: Vec<TermId>,
    cache: Vec<OnceLock<Box<[Cached]>>>,
    /// Roughly the heap bytes of the literals read into `cache` (#225).
    literal_bytes: Bytes,
}

/// A byte count that a clone copies.
#[derive(Default)]
struct Bytes(AtomicUsize);

impl Clone for Bytes {
    fn clone(&self) -> Self {
        Self(AtomicUsize::new(self.0.load(Ordering::Relaxed)))
    }
}

/// Roughly the heap bytes of a literal: its struct, lexical form and datatype or language.
fn literal_size(l: &Literal) -> usize {
    size_of::<Literal>()
        + l.value().len()
        + l.language().map_or(l.datatype().as_str().len(), str::len)
}

impl Slots {
    /// A new slot; `known`: its literal and geometry, when read already.
    fn push(&mut self, id: Option<TermId>, known: Option<(&Literal, Arc<Geom>)>) -> u32 {
        let slot = u32::try_from(self.ids.len()).expect("fewer than 2^32 geometries");
        self.ids.push(id.unwrap_or(TermId::DEFAULT_GRAPH));
        let chunks = self.ids.len().div_ceil(CACHE_CHUNK);
        self.cache.resize_with(chunks, OnceLock::new);
        if let Some((literal, geom)) = known {
            let c = self.cached(slot);
            if c.literal.set(Box::new(literal.clone())).is_ok() {
                self.literal_bytes
                    .0
                    .fetch_add(literal_size(literal), Ordering::Relaxed);
            }
            let _ = c.geom.set(geom);
        }
        slot
    }

    fn id(&self, slot: u32) -> Option<TermId> {
        Some(self.ids[slot as usize]).filter(|&id| id != TermId::DEFAULT_GRAPH)
    }

    fn cached(&self, slot: u32) -> &Cached {
        let slot = slot as usize;
        let chunk = self.cache[slot / CACHE_CHUNK].get_or_init(|| {
            std::iter::repeat_with(Cached::default)
                .take(CACHE_CHUNK)
                .collect()
        });
        &chunk[slot % CACHE_CHUNK]
    }

    fn literal(&self, slot: u32, store: &Dataset) -> &Literal {
        self.cached(slot).literal.get_or_init(|| {
            let literal = match (store, self.id(slot)) {
                (Dataset::TripleTrail(s), Some(id)) => match s.dictionary_term(id) {
                    Some(Term::Literal(l)) => Some(l),
                    _ => None,
                },
                _ => None,
            };
            // Not reached: ids come from the store, which never drops a term.
            let literal = literal.unwrap_or_else(|| Literal::new_simple_literal(""));
            self.literal_bytes
                .0
                .fetch_add(literal_size(&literal), Ordering::Relaxed);
            Box::new(literal)
        })
    }

    fn geom(&self, slot: u32, store: &Dataset) -> &Arc<Geom> {
        self.cached(slot).geom.get_or_init(|| {
            match (store, self.id(slot)) {
                (Dataset::TripleTrail(s), Some(id)) => s.geometry_of(id),
                _ => None,
            }
            // Not reached: the box was read from this geometry.
            .unwrap_or_else(|| {
                Arc::new(Geom::new(
                    geo::Geometry::GeometryCollection(geo::GeometryCollection(Vec::new())),
                    Crs::Crs84,
                ))
            })
        })
    }

    fn is(&self, slot: u32, id: Option<TermId>, literal: &Literal) -> bool {
        match (self.id(slot), id) {
            (Some(a), Some(b)) => a == b,
            _ => self
                .cached(slot)
                .literal
                .get()
                .is_some_and(|l| **l == *literal),
        }
    }
}

impl RTreeObject for Entry {
    type Envelope = AABB<[f32; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

/// Distance to the bounding box, for nearest-entry searches.
impl rstar::PointDistance for Entry {
    fn distance_2(&self, point: &[f32; 2]) -> f32 {
        self.envelope.distance_2(point)
    }
}

/// Which predicates' geometry literals the index holds: all of them by default. A store with
/// large geometries no query tests spatially (schematic drawings, say) can leave them out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexedPredicates {
    /// Only these predicates, if set.
    pub only: Option<HashSet<String>>,
    /// Never these predicates.
    pub except: HashSet<String>,
}

impl IndexedPredicates {
    /// Whether the geometry literals of `predicate` are indexed.
    pub fn covers(&self, predicate: &str) -> bool {
        self.only.as_ref().is_none_or(|o| o.contains(predicate)) && !self.except.contains(predicate)
    }

    /// Whether every predicate is covered, so a pattern with a variable predicate is too.
    pub fn covers_all(&self) -> bool {
        self.only.is_none() && self.except.is_empty()
    }
}

/// Whether `term` is a literal of a geometry datatype (`geo:wktLiteral`, GeoJSON, GML, KML,
/// H3), the literals the index holds.
pub(crate) fn is_geometry_literal(term: &Term) -> bool {
    matches!(term, Term::Literal(l) if tripletrail_geo::is_geometry_datatype(l.datatype().as_str()))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IndexStats {
    pub geometries: usize,
    /// Geometry literals that are invalid or empty, and `geo:asWKT` objects that are not
    /// geometry literals: they are not in the R-trees.
    pub skipped: usize,
    pub build: Duration,
}

/// R-trees over the distinct geometry literals of the store, whatever predicate carries them
/// (see [`IndexedPredicates`]): one for geographic coordinates
/// (CRS84/EPSG:4326, as longitude/latitude) and one per other CRS.
#[derive(Clone)]
pub struct SpatialIndex {
    geographic: RTree<Entry>,
    projected: Vec<(String, RTree<Entry>)>,
    /// Literals that are not in a tree: invalid, or empty geometries.
    unindexed: Vec<Literal>,
    /// `geo:asWKT` objects that are not geometry literals.
    non_literals: usize,
    /// The predicates whose geometry literals are indexed.
    predicates: IndexedPredicates,
    /// What each entry stands for, by [`Entry::slot`]; a removed entry leaves its slot.
    slots: Slots,
    /// The store the index was built on, which slots read their literal and geometry from.
    store: Dataset,
    stats: IndexStats,
    /// Histograms of the trees (geographic first, then `projected` in order), built on the
    /// first estimate and dropped by [`SpatialIndex::apply`].
    histograms: OnceLock<Vec<Histogram>>,
}

/// Cells per side of a [`Histogram`].
const GRID: usize = 64;
/// Estimates below this many candidates are replaced by an exact count of the R-tree entries
/// whose bounding box meets the envelope (no geometry test), which is cheap at that size.
const EXACT_BELOW: f64 = 4096.0;

/// A coarse grid over a tree's extent counting the entries whose bounding-box centre falls in
/// each cell, with the mean bounding-box size: enough to estimate how many entries meet an
/// envelope in time independent of the answer.
#[derive(Clone, Debug)]
struct Histogram {
    min: [f64; 2],
    cell: [f64; 2],
    counts: Vec<u32>,
    /// Mean half width and half height of the entries' bounding boxes.
    half: [f64; 2],
}

impl Histogram {
    fn build(tree: &RTree<Entry>) -> Self {
        let n = tree.size();
        let extent = tree.root().envelope();
        let (lo, hi) = (extent.lower().map(f64::from), extent.upper().map(f64::from));
        let cell = [
            ((hi[0] - lo[0]) / GRID as f64).max(f64::MIN_POSITIVE),
            ((hi[1] - lo[1]) / GRID as f64).max(f64::MIN_POSITIVE),
        ];
        let mut h = Self {
            min: lo,
            cell,
            counts: vec![0; GRID * GRID],
            half: [0.0; 2],
        };
        let mut half = [0.0; 2];
        for e in tree.iter() {
            let (l, u) = e.bbox();
            half[0] += (u[0] - l[0]) / 2.0;
            half[1] += (u[1] - l[1]) / 2.0;
            let c = [(l[0] + u[0]) / 2.0, (l[1] + u[1]) / 2.0];
            let (i, j) = (h.column(c[0], 0), h.column(c[1], 1));
            h.counts[j * GRID + i] += 1;
        }
        if n > 0 {
            h.half = [half[0] / n as f64, half[1] / n as f64];
        }
        h
    }

    fn column(&self, v: f64, axis: usize) -> usize {
        (((v - self.min[axis]) / self.cell[axis]).floor().max(0.0) as usize).min(GRID - 1)
    }

    /// About how many entries have a bounding box meeting `envelope`: those whose centre lies
    /// within `envelope` grown by the mean half size, counting cells by the share they overlap.
    fn estimate(&self, envelope: &AABB<[f64; 2]>) -> f64 {
        let lo = [
            envelope.lower()[0] - self.half[0],
            envelope.lower()[1] - self.half[1],
        ];
        let hi = [
            envelope.upper()[0] + self.half[0],
            envelope.upper()[1] + self.half[1],
        ];
        let share = |axis: usize, k: usize| {
            let start = self.min[axis] + k as f64 * self.cell[axis];
            let end = start + self.cell[axis];
            // The last cell also holds the entries on the extent's upper edge.
            let end = if k == GRID - 1 { f64::INFINITY } else { end };
            let start = if k == 0 { f64::NEG_INFINITY } else { start };
            let overlap = hi[axis].min(end) - lo[axis].max(start);
            if overlap < 0.0 {
                0.0
            } else if start.is_infinite() || end.is_infinite() {
                // Open-ended edge cells: counted whole once the envelope reaches them.
                1.0
            } else {
                (overlap / self.cell[axis]).min(1.0)
            }
        };
        let columns: Vec<(usize, f64)> = (0..GRID)
            .map(|i| (i, share(0, i)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        let mut total = 0.0;
        for j in 0..GRID {
            let sy = share(1, j);
            if sy == 0.0 {
                continue;
            }
            for &(i, sx) in &columns {
                total += f64::from(self.counts[j * GRID + i]) * sx * sy;
            }
        }
        total
    }
}

impl SpatialIndex {
    /// Indexes every geometry literal in the store, whatever predicate carries it.
    pub fn build(store: &Dataset) -> Result<Self, DatasetError> {
        Self::build_with(store, IndexedPredicates::default())
    }

    /// Indexes the geometry literals that are the object of a predicate `predicates` covers.
    pub fn build_with(
        store: &Dataset,
        predicates: IndexedPredicates,
    ) -> Result<Self, DatasetError> {
        let start = Instant::now();
        // `geo:asWKT` objects that are not geometry literals: the nearest-neighbour rewrite
        // needs every `geo:asWKT` value in the index (see `outside`).
        let non_literals = if predicates.covers(AS_WKT.as_str()) {
            store.non_geometry_objects(AS_WKT)?
        } else {
            0
        };
        let mut skipped = non_literals;
        let mut geographic = Vec::new();
        let mut projected: Vec<(String, Vec<Entry>)> = Vec::new();
        let mut unindexed = Vec::new();
        let mut slots = Slots::default();
        let mut place = |crs: &Crs,
                         envelope: AABB<[f64; 2]>,
                         id: Option<TermId>,
                         known: Option<(&Literal, Arc<Geom>)>| {
            let entry = Entry {
                envelope: widen(&envelope),
                slot: slots.push(id, known),
            };
            match crs {
                Crs::Other(iri) => match projected.iter_mut().find(|(i, _)| i == iri) {
                    Some((_, v)) => v.push(entry),
                    None => projected.push((iri.clone(), vec![entry])),
                },
                _ => geographic.push(entry),
            }
        };
        // Reading boxes (the tripletrail store) or parsing (Oxigraph's) is spread over the cores.
        let threads = tripletrail_store::par::cores();
        if let (Dataset::TripleTrail(s), Some(ids)) =
            (store, store.geometry_ids(&|p| predicates.covers(p))?)
        {
            // The tripletrail store: bounding boxes alone, read from the saved records (or the
            // geometries parsed already); no geometry is decoded.
            let chunk = ids.len().div_ceil(threads).max(4096);
            let boxes: Vec<Vec<_>> = std::thread::scope(|scope| {
                let workers: Vec<_> = ids
                    .chunks(chunk)
                    .map(|part| {
                        scope.spawn(move || part.iter().map(|&id| s.geometry_bbox(id)).collect())
                    })
                    .collect();
                workers.into_iter().map(|w| w.join().unwrap()).collect()
            });
            for (bbox, &id) in boxes.into_iter().flatten().zip(&ids) {
                match bbox {
                    Some((crs, Some(b))) => place(
                        &crs,
                        AABB::from_corners([b[0], b[1]], [b[2], b[3]]),
                        Some(id),
                        None,
                    ),
                    // Not valid, or empty (intersects nothing).
                    _ => {
                        skipped += 1;
                        if let Some(Term::Literal(l)) = s.dictionary_term(id) {
                            unindexed.push(l);
                        }
                    }
                }
            }
        } else {
            let literals = store.geometry_literals(&|p| predicates.covers(p))?;
            let chunk = literals.len().div_ceil(threads).max(1024);
            let parsed: Vec<Vec<Option<Parsed>>> = std::thread::scope(|scope| {
                let workers: Vec<_> = literals
                    .chunks(chunk)
                    .map(|part| {
                        scope.spawn(move || part.iter().map(|(l, g)| parse(store, l, g)).collect())
                    })
                    .collect();
                workers.into_iter().map(|w| w.join().unwrap()).collect()
            });
            for (parsed, (literal, _)) in parsed.into_iter().flatten().zip(&literals) {
                let Some((envelope, geom)) = parsed else {
                    skipped += 1; // not WKT, or empty geometry (intersects nothing)
                    unindexed.push(literal.clone());
                    continue;
                };
                place(&geom.crs.clone(), envelope, None, Some((literal, geom)));
            }
        }
        let geographic = RTree::bulk_load(geographic);
        let geometries = geographic.size() + projected.iter().map(|(_, v)| v.len()).sum::<usize>();
        Ok(Self {
            geographic,
            projected: projected
                .into_iter()
                .map(|(iri, v)| (iri, RTree::bulk_load(v)))
                .collect(),
            unindexed,
            non_literals,
            predicates,
            slots,
            store: store.clone(),
            stats: IndexStats {
                geometries,
                skipped,
                build: start.elapsed(),
            },
            histograms: OnceLock::new(),
        })
    }

    /// The index of the entries whose literal `keep` accepts (its term id on the tripletrail
    /// store, and the literal): what a caller who may read some graphs only sees (#203). The
    /// slots are shared, so no geometry is read again; the trees are built anew.
    pub fn retain(&self, keep: &(dyn Fn(Option<TermId>, &Literal) -> bool + Sync)) -> Self {
        let start = Instant::now();
        let visible = |e: &&Entry| keep(self.slots.id(e.slot), self.literal(e));
        let geographic =
            RTree::bulk_load(self.geographic.iter().filter(visible).cloned().collect());
        let projected: Vec<(String, RTree<Entry>)> = self
            .projected
            .iter()
            .map(|(iri, tree)| {
                let entries = tree.iter().filter(visible).cloned().collect();
                (iri.clone(), RTree::bulk_load(entries))
            })
            .collect();
        let unindexed: Vec<Literal> = self
            .unindexed
            .iter()
            .filter(|l| keep(None, l))
            .cloned()
            .collect();
        let geometries = geographic.size() + projected.iter().map(|(_, t)| t.size()).sum::<usize>();
        Self {
            geographic,
            projected,
            stats: IndexStats {
                geometries,
                skipped: unindexed.len(),
                build: start.elapsed(),
            },
            unindexed,
            non_literals: self.non_literals,
            predicates: self.predicates.clone(),
            slots: self.slots.clone(),
            store: self.store.clone(),
            histograms: OnceLock::new(),
        }
    }

    /// Brings the index up to date after writes to the store. `changes` are the quads that
    /// were added (`true`) or removed (`false`), in order, as predicate and object; `store` is
    /// the store after the writes. Only the objects named in `changes` are looked at.
    pub fn apply<'a>(
        &mut self,
        store: &Dataset,
        changes: impl IntoIterator<Item = (bool, NamedNodeRef<'a>, &'a Term)>,
    ) -> Result<(), DatasetError> {
        let start = Instant::now();
        let mut touched = Vec::new();
        // Per literal: whether a quad with it was removed (else it was only added, so a quad
        // with it is still in the store and it needs no lookup).
        let mut seen: HashMap<&Literal, bool> = HashMap::new();
        for (added, predicate, object) in changes {
            if !self.predicates.covers(predicate.as_str()) {
                continue;
            }
            match object {
                Term::Literal(l) if is_geometry_literal(object) => match seen.entry(l) {
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(!added);
                        touched.push(l);
                    }
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        *e.get_mut() |= !added;
                    }
                },
                // Each change is one quad, so this keeps the count of other objects.
                _ if predicate != AS_WKT => {}
                _ if added => self.non_literals += 1,
                _ => self.non_literals = self.non_literals.saturating_sub(1),
            }
        }
        // Whether each literal is wanted, its geometry and whether it is in the index already:
        // read-only, so spread over the cores (a big update touches many literals, and each
        // search of the tree for a point visits every long line whose box holds it).
        let threads = tripletrail_store::par::cores();
        let chunk = touched.len().div_ceil(threads).max(4096);
        let this = &*self;
        let seen = &seen;
        let looked_up: Vec<Vec<Result<Looked<'_>, DatasetError>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = touched
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .map(|&literal| this.look_up(store, literal, !seen[literal]))
                            .collect()
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        // New entries, put in their trees at the end: in bulk when they are many.
        let mut new: Vec<(Crs, Entry)> = Vec::new();
        for looked in looked_up.into_iter().flatten() {
            let (literal, wanted, parsed) = looked?;
            match parsed {
                Some((envelope, geom, id, present)) => {
                    let crs = geom.crs.clone();
                    if wanted && present.is_none() {
                        let slot = self.slots.push(id, Some((literal, geom)));
                        new.push((
                            crs,
                            Entry {
                                envelope: widen(&envelope),
                                slot,
                            },
                        ));
                        self.stats.geometries += 1;
                    } else if let Some(e) = present.filter(|_| !wanted) {
                        self.tree_mut(&crs).remove(&e);
                        self.stats.geometries -= 1;
                    }
                }
                None => {
                    let position = self.unindexed.iter().position(|l| l == literal);
                    match (wanted, position) {
                        (true, None) => self.unindexed.push(literal.clone()),
                        (false, Some(p)) => {
                            self.unindexed.swap_remove(p);
                        }
                        _ => {}
                    }
                }
            }
        }
        self.insert_all(new);
        self.projected.retain(|(_, t)| t.size() > 0);
        self.histograms = OnceLock::new();
        self.stats.skipped = self.unindexed.len() + self.non_literals;
        self.stats.build += start.elapsed();
        Ok(())
    }

    /// The predicates whose geometry literals are indexed.
    pub fn predicates(&self) -> &IndexedPredicates {
        &self.predicates
    }

    /// For [`SpatialIndex::apply`]: whether a literal is wanted in the index (`known`: a quad
    /// with it was added and none removed, so it is; else the store is asked), and if it is a
    /// geometry, its box, geometry, term id and entry in the index, if it has one.
    fn look_up<'l>(
        &self,
        store: &Dataset,
        literal: &'l Literal,
        known: bool,
    ) -> Result<Looked<'l>, DatasetError> {
        let term = Term::Literal(literal.clone());
        let mut wanted = known;
        if !wanted {
            for quad in store.quads_for_pattern(None, None, Some(term.as_ref()), None) {
                if self.predicates.covers(quad?.predicate.as_str()) {
                    wanted = true;
                    break;
                }
            }
        }
        let parsed = parse(store, literal, &None).map(|(envelope, geom)| {
            let id = match store {
                Dataset::TripleTrail(s) => s.term_id(term.as_ref()),
                Dataset::Oxigraph(_) => None,
            };
            let present = self
                .candidates(&geom.crs, envelope)
                .find(|e| e.envelope == widen(&envelope) && self.slots.is(e.slot, id, literal))
                .copied();
            (envelope, geom, id, present)
        });
        Ok((literal, wanted, parsed))
    }

    /// Puts new entries in their trees: one by one when they are few next to the tree, else
    /// by loading the tree again in bulk, which is quicker than as many inserts.
    fn insert_all(&mut self, mut new: Vec<(Crs, Entry)>) {
        while let Some((crs, _)) = new.first() {
            let crs = crs.clone();
            let (these, rest): (Vec<_>, Vec<_>) = new.into_iter().partition(|(c, _)| *c == crs);
            new = rest;
            let tree = self.tree_mut(&crs);
            if these.len() < 1024 || these.len() < tree.size() / 8 {
                for (_, e) in these {
                    tree.insert(e);
                }
            } else {
                let all = std::mem::take(tree)
                    .into_iter()
                    .chain(these.into_iter().map(|(_, e)| e))
                    .collect();
                *tree = RTree::bulk_load(all);
            }
        }
    }

    fn tree_mut(&mut self, crs: &Crs) -> &mut RTree<Entry> {
        let Crs::Other(iri) = crs else {
            return &mut self.geographic;
        };
        let i = match self.projected.iter().position(|(i, _)| i == iri) {
            Some(i) => i,
            None => {
                self.projected.push((iri.clone(), RTree::new()));
                self.projected.len() - 1
            }
        };
        &mut self.projected[i].1
    }

    pub fn stats(&self) -> IndexStats {
        self.stats
    }

    /// Roughly the heap bytes of the index: its trees, slots, the literals read into them,
    /// and its histograms. The parsed geometries are the store's, and not counted here
    /// (#225).
    pub fn heap_size(&self) -> usize {
        let entries =
            self.geographic.size() + self.projected.iter().map(|(_, t)| t.size()).sum::<usize>();
        // Leaves hold the entries; the inner nodes add about one in six.
        let trees = entries * size_of::<rstar::RTreeNode<Entry>>() * 7 / 6;
        let slots = &self.slots;
        let chunks = slots.cache.iter().filter(|c| c.get().is_some()).count();
        let slots = slots.ids.capacity() * size_of::<TermId>()
            + slots.cache.capacity() * size_of::<OnceLock<Box<[Cached]>>>()
            + chunks * CACHE_CHUNK * size_of::<Cached>()
            + slots.literal_bytes.0.load(Ordering::Relaxed);
        let unindexed: usize = self.unindexed.iter().map(literal_size).sum();
        let histograms = self.histograms.get().map_or(0, |h| {
            h.iter()
                .map(|h| size_of::<Histogram>() + h.counts.capacity() * size_of::<u32>())
                .sum()
        });
        trees + slots + unindexed + histograms
    }

    /// The geometry literal of an entry.
    pub(crate) fn literal(&self, e: &Entry) -> &Literal {
        self.slots.literal(e.slot, &self.store)
    }

    /// The parsed geometry of an entry.
    pub(crate) fn geom(&self, e: &Entry) -> &Arc<Geom> {
        self.slots.geom(e.slot, &self.store)
    }

    /// Every indexed entry in the coordinate space of `crs`.
    pub(crate) fn entries_in<'a>(&'a self, crs: &Crs) -> Box<dyn Iterator<Item = &'a Entry> + 'a> {
        match crs {
            Crs::Other(iri) => match self.projected.iter().find(|(i, _)| i == iri) {
                Some((_, t)) => Box::new(t.iter()),
                None => Box::new(std::iter::empty()),
            },
            _ => Box::new(self.geographic.iter()),
        }
    }

    /// Every indexed entry.
    pub(crate) fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.geographic
            .iter()
            .chain(self.projected.iter().flat_map(|(_, t)| t.iter()))
    }

    /// Whether every `geo:asWKT` value is in the index: the predicate is indexed and all its
    /// objects are geometry literals.
    pub(crate) fn has_every_wkt_value(&self) -> bool {
        self.non_literals == 0 && self.predicates.covers(AS_WKT.as_str())
    }

    /// The distance, in the coordinates of `crs`, from `point` to the bounding box of the
    /// `n`-th nearest entry in that coordinate space (of the farthest if there are fewer).
    pub(crate) fn nth_nearest_distance(&self, crs: &Crs, point: [f64; 2], n: usize) -> Option<f64> {
        let tree = match crs {
            Crs::Other(iri) => &self.projected.iter().find(|(i, _)| i == iri)?.1,
            _ => &self.geographic,
        };
        tree.nearest_neighbor_iter_with_distance_2([point[0] as f32, point[1] as f32])
            .take(n.max(1))
            .last()
            .map(|(_, d2)| f64::from(d2).sqrt())
    }

    /// Every indexed literal that is not in the coordinate space of `crs`: indexed in another
    /// CRS, invalid, or empty.
    pub(crate) fn outside(&self, crs: &Crs) -> Vec<Literal> {
        let geographic = !matches!(crs, Crs::Other(_));
        let mut out: Vec<Literal> = self.unindexed.clone();
        if !geographic {
            out.extend(self.geographic.iter().map(|e| self.literal(e).clone()));
        }
        for (iri, tree) in &self.projected {
            if geographic || !matches!(crs, Crs::Other(i) if i == iri) {
                out.extend(tree.iter().map(|e| self.literal(e).clone()));
            }
        }
        out
    }

    /// The number of indexed geometries in the coordinate space of `crs`.
    pub(crate) fn len_in(&self, crs: &Crs) -> usize {
        match crs {
            Crs::Other(iri) => self
                .projected
                .iter()
                .find(|(i, _)| i == iri)
                .map_or(0, |(_, t)| t.size()),
            _ => self.geographic.size(),
        }
    }

    /// About how many entries in the coordinate space of `crs` have a bounding box meeting
    /// `envelope` (the candidates [`SpatialIndex::candidates`] would return, before any exact
    /// test). Read from a coarse grid histogram of the tree, so it costs the same however many
    /// entries match; small answers are counted exactly in the R-tree instead.
    pub fn estimate_candidates(&self, crs: &Crs, envelope: AABB<[f64; 2]>) -> usize {
        let position = match crs {
            Crs::Other(iri) => match self.projected.iter().position(|(i, _)| i == iri) {
                Some(p) => p + 1,
                None => return 0,
            },
            _ => 0,
        };
        let tree = if position == 0 {
            &self.geographic
        } else {
            &self.projected[position - 1].1
        };
        if tree.size() == 0 {
            return 0;
        }
        let histograms = self.histograms.get_or_init(|| {
            std::iter::once(&self.geographic)
                .chain(self.projected.iter().map(|(_, t)| t))
                .map(Histogram::build)
                .collect()
        });
        let estimate = histograms[position].estimate(&envelope);
        if estimate < EXACT_BELOW {
            return tree
                .locate_in_envelope_intersecting(widen(&envelope))
                .take(4 * EXACT_BELOW as usize)
                .count();
        }
        (estimate.round() as usize).min(tree.size())
    }

    /// Entries in the same coordinate space as `crs` whose bounding box intersects `envelope`.
    pub(crate) fn candidates<'a>(
        &'a self,
        crs: &Crs,
        envelope: AABB<[f64; 2]>,
    ) -> Box<dyn Iterator<Item = &'a Entry> + 'a> {
        let tree = match crs {
            Crs::Other(iri) => match self.projected.iter().find(|(i, _)| i == iri) {
                Some((_, t)) => t,
                None => return Box::new(std::iter::empty()),
            },
            _ => &self.geographic,
        };
        Box::new(tree.locate_in_envelope_intersecting(widen(&envelope)))
    }
}

type Parsed = (AABB<[f64; 2]>, Arc<Geom>);

/// A literal an update touched, whether the index wants it, and if it is a geometry: its box,
/// geometry, term id and present entry.
type Looked<'l> = (
    &'l Literal,
    bool,
    Option<(AABB<[f64; 2]>, Arc<Geom>, Option<TermId>, Option<Entry>)>,
);

/// The bounding box and parsed geometry of a literal (`stored`: parsed by the store already);
/// `None` if it is not a valid geometry, or empty.
fn parse(store: &Dataset, literal: &Literal, stored: &Option<Arc<Geom>>) -> Option<Parsed> {
    let term = Term::Literal(literal.clone());
    let geom = match stored {
        Some(g) => Arc::clone(g),
        None => store
            .geometry(term.as_ref())
            .or_else(|| parse_wkt_literal(&term))?,
    };
    let rect = geom.geometry.bounding_rect()?;
    let envelope = AABB::from_corners([rect.min().x, rect.min().y], [rect.max().x, rect.max().y]);
    Some((envelope, geom))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{GraphName, NamedNode, Quad};

    fn wkt_literal(wkt: String) -> Literal {
        Literal::new_typed_literal(
            wkt,
            NamedNode::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
        )
    }

    fn geometry(i: usize, wkt: String) -> Quad {
        Quad::new(
            NamedNode::new_unchecked(format!("http://example.com/g{i}")),
            AS_WKT,
            wkt_literal(wkt),
            GraphName::DefaultGraph,
        )
    }

    fn exact(index: &SpatialIndex, envelope: AABB<[f64; 2]>) -> usize {
        index.candidates(&Crs::Crs84, envelope).count()
    }

    fn close(estimate: usize, exact: usize) -> bool {
        (estimate as f64 - exact as f64).abs() <= 0.15 * exact as f64
    }

    #[test]
    fn candidate_estimates_follow_the_data() {
        // 200 x 200 points, 100 x 100 more in the south-west quarter, and short lines.
        let mut wkts: Vec<String> = (0..40_000)
            .map(|i| {
                let (x, y) = ((i % 200) as f64 * 0.05, (i / 200) as f64 * 0.05);
                format!("POINT({x} {y})")
            })
            .collect();
        wkts.extend((0..10_000).map(|i| {
            let (x, y) = (
                0.01 + (i % 100) as f64 * 0.04,
                0.01 + (i / 100) as f64 * 0.04,
            );
            format!("POINT({x} {y})")
        }));
        wkts.extend((0..5_000).map(|i| {
            let (x, y) = ((i % 100) as f64 * 0.1, (i / 100) as f64 * 0.2);
            format!("LINESTRING({x} {y}, {} {})", x + 0.03, y + 0.04)
        }));
        let store: Dataset = tripletrail_store::Store::new().into();
        store
            .load_quads(wkts.into_iter().enumerate().map(|(i, w)| geometry(i, w)))
            .unwrap();
        let mut index = SpatialIndex::build(&store).unwrap();
        for (lo, hi) in [
            ([-1.0, -1.0], [20.0, 20.0]),
            ([0.0, 0.0], [4.0, 4.0]),
            ([2.0, 3.0], [9.0, 6.5]),
            ([5.0, 5.0], [9.95, 9.95]),
        ] {
            let envelope = AABB::from_corners(lo, hi);
            let estimate = index.estimate_candidates(&Crs::Crs84, envelope);
            let exact = exact(&index, envelope);
            assert!(
                exact > EXACT_BELOW as usize && close(estimate, exact),
                "{lo:?} {hi:?}: estimate {estimate}, exact {exact}"
            );
        }
        // Small answers are exact.
        let small = AABB::from_corners([1.0, 1.0], [1.5, 1.2]);
        assert_eq!(
            index.estimate_candidates(&Crs::Crs84, small),
            exact(&index, small)
        );
        let away = AABB::from_corners([50.0, 50.0], [60.0, 60.0]);
        assert_eq!(index.estimate_candidates(&Crs::Crs84, away), 0);
        // Another CRS has its own tree.
        let other = Crs::Other("http://www.opengis.net/def/crs/EPSG/0/25833".into());
        assert_eq!(index.estimate_candidates(&other, small), 0);

        // 5000 points in one corner: the histogram is rebuilt after the write.
        let corner = AABB::from_corners([8.9, 8.9], [9.2, 9.2]);
        let before = index.estimate_candidates(&Crs::Crs84, corner);
        let added: Vec<Quad> = (0..5_000)
            .map(|i| {
                let (x, y) = (9.0 + (i % 50) as f64 * 0.001, 9.0 + (i / 50) as f64 * 0.001);
                geometry(100_000 + i, format!("POINT({x} {y})"))
            })
            .collect();
        for q in &added {
            store.insert(q).unwrap();
        }
        index
            .apply(
                &store,
                added
                    .iter()
                    .map(|q| (true, q.predicate.as_ref(), &q.object)),
            )
            .unwrap();
        let after = index.estimate_candidates(&Crs::Crs84, corner);
        let exact = exact(&index, corner);
        assert!(after > before + 4_000, "{before} -> {after}");
        assert!(close(after, exact), "{after} vs {exact}");
    }
}
