//! Embedding vectors (#236): literals of the datatype `tt:vector`
//! (`http://w3id.org/tripletrail/fn#vector`), written as a JSON array of numbers
//! (`"[0.12, -0.5, 3e-2]"^^tt:vector`), and the similarity functions `tt:cosine`, `tt:dot` and
//! `tt:l2` over them. A plain string in the same form is a vector too, so that a query
//! vector can be passed as a parameter.

use crate::Error;
use crate::hnsw::{self, Frozen};
use crate::text::Source;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, Term};
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tripletrail_store::{GraphSelector, Snapshot, TermId};

/// The datatype of vector literals.
pub const VECTOR: &str = "http://w3id.org/tripletrail/fn#vector";

/// The most dimensions a vector may have.
pub const MAX_DIMENSIONS: usize = 65_536;

/// The numbers of a vector literal (or of a string in the same form); `None` when the term is
/// not one: not a JSON array of finite numbers, empty, or longer than [`MAX_DIMENSIONS`].
pub fn parse(t: &Term) -> Option<Vec<f64>> {
    let Term::Literal(l) = t else { return None };
    if l.datatype().as_str() != VECTOR && l.datatype() != xsd::STRING {
        return None;
    }
    parse_text(l.value())
}

/// The numbers of a vector's lexical form.
pub fn parse_text(text: &str) -> Option<Vec<f64>> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        // JSON numbers only: no `inf`, `NaN`, `+1` or `.5`.
        let digits = part.strip_prefix('-').unwrap_or(part);
        if !digits.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let x: f64 = part.parse().ok()?;
        if !x.is_finite() || out.len() == MAX_DIMENSIONS {
            return None;
        }
        out.push(x);
    }
    Some(out)
}

/// A vector literal of `values`.
pub fn literal(values: &[f64]) -> Literal {
    let text = values
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Literal::new_typed_literal(
        format!("[{text}]"),
        oxigraph::model::NamedNode::new_unchecked(VECTOR),
    )
}

/// The dot product; `None` when the dimensions differ.
pub fn dot(a: &[f64], b: &[f64]) -> Option<f64> {
    (a.len() == b.len()).then(|| a.iter().zip(b).map(|(x, y)| x * y).sum())
}

/// The cosine similarity, from -1 to 1; `None` when the dimensions differ or a vector is
/// all zeros. Computed on the unit vectors, so very large and very small numbers neither
/// overflow nor vanish.
pub fn cosine(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.len() != b.len() {
        return None;
    }
    let (a, b) = (scaled(a)?, scaled(b)?);
    let norms = dot(&a, &a)? * dot(&b, &b)?;
    Some((dot(&a, &b)? / norms.sqrt()).clamp(-1.0, 1.0))
}

/// `v` divided by its largest number (in absolute value), so that its squares neither overflow
/// nor underflow; `None` for a vector of only zeros.
fn scaled(v: &[f64]) -> Option<Vec<f64>> {
    let largest = v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    if largest == 0.0 || !largest.is_finite() {
        return None;
    }
    Some(v.iter().map(|x| x / largest).collect())
}

/// `v` divided by its Euclidean norm; `None` for a vector of only zeros. The numbers are
/// first divided by the largest of them, so the squares neither overflow nor underflow.
pub fn unit(v: &[f64]) -> Option<Vec<f64>> {
    let scaled = scaled(v)?;
    let norm = scaled.iter().map(|x| x * x).sum::<f64>().sqrt();
    Some(scaled.into_iter().map(|x| x / norm).collect())
}

/// The Euclidean distance; `None` when the dimensions differ.
pub fn l2(a: &[f64], b: &[f64]) -> Option<f64> {
    (a.len() == b.len()).then(|| {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f64>()
            .sqrt()
    })
}

/// One indexed (subject, vector literal) pair.
#[derive(Clone, Copy, Debug)]
struct Row {
    subject: TermId,
    literal: TermId,
    /// Where its numbers start in [`VectorIndex::blocks`], and how many there are.
    start: u32,
    len: u32,
    /// Not removed by an update since.
    live: bool,
}

/// How many numbers a block of [`Blocks`] holds at most.
const BLOCK: usize = 1 << 20;

/// Numbers in blocks that copies of an index share: a copy changes only its last block, and
/// a graph being built reads the blocks without copying them.
#[derive(Clone, Debug, Default)]
struct Blocks(Vec<Arc<Vec<f32>>>);

impl Blocks {
    /// Appends `v` (in one block): where it starts.
    fn push(&mut self, v: &[f32]) -> Result<u32, Error> {
        if self.0.last().is_none_or(|b| b.len() + v.len() > BLOCK) {
            if self.0.len() >= (1usize << 32) / BLOCK {
                return Err(Error::Request(
                    "too many vector numbers for one index".into(),
                ));
            }
            self.0.push(Arc::default());
        }
        let i = self.0.len() - 1;
        let block = Arc::make_mut(&mut self.0[i]);
        let at = block.len();
        if block.capacity() < at + v.len() {
            let want = (block.capacity() * 2).max(at + v.len()).min(BLOCK);
            block.reserve_exact(want - at);
        }
        block.extend_from_slice(v);
        Ok((i * BLOCK + at) as u32)
    }

    fn get(&self, start: u32, len: u32) -> &[f32] {
        let (b, at) = (start as usize / BLOCK, start as usize % BLOCK);
        &self.0[b][at..at + len as usize]
    }

    fn heap_size(&self) -> usize {
        self.0.iter().map(|b| b.capacity() * 4).sum()
    }
}

/// A graph of the vectors of one length: node `i` is the row `rows[i]` of the index. The
/// graph is walked with the vectors in 8 bits (a quarter of the memory to read), and what it
/// finds is ranked again with the full vectors.
#[derive(Debug)]
struct Graph {
    /// Each node's numbers in 8 bits, `dim` after `dim`; times its scale, they are the vector.
    codes: Vec<i8>,
    scales: Vec<f32>,
    rows: Vec<u32>,
    dim: u32,
    links: Frozen,
    /// How many rows the index had when the graph was made: later rows are not in it.
    covered: usize,
    /// Rows before `covered` that the graph does not hold either: those written since a
    /// graph read from disk was saved.
    extra: Vec<u32>,
}

impl hnsw::Vectors for Graph {
    fn dot(&self, q: &hnsw::Query, node: u32) -> f32 {
        let at = node as usize * self.dim as usize;
        let d = hnsw::dot_codes(&q.codes, &self.codes[at..at + self.dim as usize]);
        d as f32 * q.scale * self.scales[node as usize]
    }
    fn decode(&self, node: u32, out: &mut Vec<f32>) {
        let at = node as usize * self.dim as usize;
        let scale = self.scales[node as usize];
        out.clear();
        out.extend(
            self.codes[at..at + self.dim as usize]
                .iter()
                .map(|&c| f32::from(c) * scale),
        );
    }
    fn dot_nodes(&self, a: u32, b: u32) -> f32 {
        let d = self.dim as usize;
        let (a, b) = (a as usize, b as usize);
        let codes = hnsw::dot_codes(
            &self.codes[a * d..(a + 1) * d],
            &self.codes[b * d..(b + 1) * d],
        );
        codes as f32 * self.scales[a] * self.scales[b]
    }
    fn len(&self) -> usize {
        self.rows.len()
    }
}

impl Graph {
    /// Appends `v` in 8 bits.
    fn push(&mut self, v: &[f32]) {
        let scale = hnsw::quantise(v, &mut self.codes);
        self.scales.push(scale);
    }

    fn heap_size(&self) -> usize {
        self.codes.capacity()
            + (self.scales.capacity() + self.rows.capacity() + self.extra.capacity()) * 4
            + self.links.heap_size()
    }

    fn empty(dim: u32, covered: usize) -> Self {
        Self {
            codes: Vec::new(),
            scales: Vec::new(),
            rows: Vec::new(),
            dim,
            links: Frozen::default(),
            covered,
            extra: Vec::new(),
        }
    }

    /// Writes the graph to `path` (through a file beside it, renamed): each node's (subject,
    /// literal) pair, codes and links. `rows` are the index's rows.
    fn save(&self, path: &std::path::Path, property: &str, rows: &[Row]) -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = path.with_extension("part");
        let mut w = std::io::BufWriter::new(std::fs::File::create(&part)?);
        w.write_all(GRAPH_MAGIC)?;
        w.write_all(&(property.len() as u32).to_le_bytes())?;
        w.write_all(property.as_bytes())?;
        w.write_all(&self.dim.to_le_bytes())?;
        w.write_all(&(self.rows.len() as u64).to_le_bytes())?;
        for &r in &self.rows {
            let (s, l) = rows
                .get(r as usize)
                .map_or((u64::MAX, u64::MAX), |row| (row.subject.0, row.literal.0));
            w.write_all(&s.to_le_bytes())?;
            w.write_all(&l.to_le_bytes())?;
        }
        for x in &self.scales {
            w.write_all(&x.to_le_bytes())?;
        }
        w.write_all(&self.codes.iter().map(|&c| c as u8).collect::<Vec<u8>>())?;
        self.links.write(&mut w)?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&part, path)
    }

    /// The graph saved at `path`, its nodes matched to the index's `rows` by their (subject,
    /// literal) pairs; `None` when there is none, it is of another property or length, or
    /// more than a quarter of its nodes are no longer in the index.
    fn load(path: &std::path::Path, property: &str, dim: u32, rows: &[Row]) -> Option<Self> {
        use std::io::Read as _;
        let mut r = std::io::BufReader::new(std::fs::File::open(path).ok()?);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic).ok()?;
        let len = hnsw::read_u32(&mut r).ok()? as usize;
        if magic != *GRAPH_MAGIC || len != property.len() {
            return None;
        }
        let mut name = vec![0u8; len];
        r.read_exact(&mut name).ok()?;
        if name != property.as_bytes() || hnsw::read_u32(&mut r).ok()? != dim {
            return None;
        }
        let n = usize::try_from(hnsw::read_u64(&mut r).ok()?).ok()?;
        if n > rows.len().saturating_mul(2) + 1024 {
            return None;
        }
        let at: HashMap<(TermId, TermId), u32> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.live && row.len == dim)
            .map(|(i, row)| ((row.subject, row.literal), i as u32))
            .collect();
        let mut graph = Self::empty(dim, rows.len());
        let mut lost = 0;
        for _ in 0..n {
            let s = hnsw::read_u64(&mut r).ok()?;
            let l = hnsw::read_u64(&mut r).ok()?;
            match at.get(&(TermId(s), TermId(l))) {
                Some(&row) => graph.rows.push(row),
                None => {
                    lost += 1;
                    graph.rows.push(u32::MAX);
                }
            }
        }
        if lost * 4 > n {
            return None;
        }
        let scales = hnsw::read_u32s(&mut r, n).ok()?;
        graph.scales = scales.into_iter().map(f32::from_bits).collect();
        let mut codes = vec![0u8; n.checked_mul(dim as usize)?];
        r.read_exact(&mut codes).ok()?;
        graph.codes = codes.into_iter().map(|c| c as i8).collect();
        graph.links = Frozen::read(&mut r).ok()?;
        if graph.links.len() != n {
            return None;
        }
        let mut held = vec![false; rows.len()];
        for &row in &graph.rows {
            if let Some(h) = held.get_mut(row as usize) {
                *h = true;
            }
        }
        graph.extra = at
            .values()
            .copied()
            .filter(|&r| !held[r as usize])
            .collect();
        graph.extra.sort_unstable();
        Some(graph)
    }
}

const GRAPH_MAGIC: &[u8; 8] = b"TTVECG01";

/// The graph of the vectors of one length, and whether one is being made.
#[derive(Debug, Default)]
struct GraphState {
    graph: Option<Arc<Graph>>,
    building: bool,
}

/// What the copies of an index share: their graphs, by vector length.
#[derive(Debug)]
struct Shared {
    graphs: Mutex<HashMap<u32, GraphState>>,
    graph_min: AtomicUsize,
    /// How many candidates a graph search keeps at least (0: the default).
    width: AtomicUsize,
    /// Where the graphs are kept on disk (with the vector length added), if they are.
    file: Mutex<Option<std::path::PathBuf>>,
}

/// One graph is made at a time, on all the threads there are.
static BUILDS: Mutex<()> = Mutex::new(());

/// How many vectors of one length an index needs before it makes a graph of them, unless
/// [`VectorIndex::set_graph_min`] says otherwise.
pub const GRAPH_MIN: usize = 20_000;

/// How many candidates a graph search keeps at least (twice `k` when that is more).
const SEARCH_WIDTH: usize = 512;

/// The vectors that the objects of one property hold, in what a [`Source`] shows, for
/// nearest-neighbour search (#236). Made on first use and brought up to date after writes, as
/// the full-text index is.
///
/// Up to [`GRAPH_MIN`] vectors of a length, every one is compared: exact. Past it, a graph
/// (HNSW) of them is made in the background, and searches then follow it: approximate, but
/// it compares only a few thousand. Vectors written after the graph was made are compared
/// one by one until there are enough to extend it.
#[derive(Clone, Debug)]
pub struct VectorIndex {
    property: NamedNode,
    rows: Vec<Row>,
    /// The numbers of every row as a unit vector.
    blocks: Blocks,
    /// The (subject, object) pairs of the property, sorted, with their row numbers
    /// ([`NOT_VECTOR`] for an object that is not a vector, [`STRING`] for a plain string).
    pairs: Vec<((TermId, TermId), u32)>,
    live: usize,
    /// How many pairs have a plain string for object, which `tt:cosine` reads too.
    strings: usize,
    /// How many live rows there are of each length.
    lengths: HashMap<u32, usize>,
    shared: Arc<Shared>,
    /// The store generation the index is of.
    pub generation: u64,
}

const NOT_VECTOR: u32 = u32::MAX;
/// The row number of a pair whose object is a plain string: not indexed, but `tt:cosine`
/// reads a string in the form of a vector.
const STRING: u32 = u32::MAX - 1;

/// A nearest neighbour: the subject, the vector literal, and its cosine similarity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Neighbour {
    pub subject: TermId,
    pub literal: TermId,
    pub score: f64,
}

/// The (subject, object) pairs of `property` in the graphs `source` reads, sorted, each once.
fn property_pairs(source: &Source, property: &NamedNode) -> Vec<(TermId, TermId)> {
    let snapshot = &source.snapshot;
    let Some(pid) = snapshot.term_id(property.as_ref().into()) else {
        return Vec::new();
    };
    let mut pairs: Vec<(TermId, TermId)> = snapshot
        .quad_ids_for_pattern(None, Some(pid), None, GraphSelector::All)
        .filter(|q| source.reads(q[3]))
        .map(|[s, _, o, _]| (s, o))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// The vector literal `id` as a unit vector of `f32`, normalised in `f64`; `None` if it is not
/// a vector, or only zeros.
fn numbers(snapshot: &Snapshot, id: TermId) -> Result<Vec<f32>, u32> {
    let term = snapshot.term(id).map_err(|_| NOT_VECTOR)?;
    let Term::Literal(l) = &term else {
        return Err(NOT_VECTOR);
    };
    // Only literals of the datatype: a plain string is a query, not data.
    if l.datatype() == xsd::STRING {
        return Err(STRING);
    }
    if l.datatype().as_str() != VECTOR {
        return Err(NOT_VECTOR);
    }
    let v = parse(&term).and_then(|v| unit(&v)).ok_or(NOT_VECTOR)?;
    Ok(to_f32(&v))
}

fn to_f32(v: &[f64]) -> Vec<f32> {
    v.iter().map(|&x| x as f32).collect()
}

impl VectorIndex {
    fn empty(property: &NamedNode, generation: u64) -> Self {
        Self {
            property: property.clone(),
            rows: Vec::new(),
            blocks: Blocks::default(),
            pairs: Vec::new(),
            live: 0,
            strings: 0,
            lengths: HashMap::new(),
            shared: Arc::new(Shared {
                graphs: Mutex::new(HashMap::new()),
                graph_min: AtomicUsize::new(GRAPH_MIN),
                width: AtomicUsize::new(0),
                file: Mutex::new(None),
            }),
            generation,
        }
    }

    /// Indexes the vectors of `property` that `source` shows. `stop` is asked now and then,
    /// and its error ends the build (a deadline).
    pub fn build_from(
        source: &Source,
        property: &NamedNode,
        stop: &dyn Fn() -> Result<(), Error>,
    ) -> Result<Self, Error> {
        let mut index = Self::empty(property, source.snapshot.generation().unwrap_or_default());
        let pairs = property_pairs(source, property);
        index.pairs.reserve_exact(pairs.len());
        // A literal shared by several subjects is read once.
        let mut read: HashMap<TermId, Result<(u32, u32), u32>> = HashMap::new();
        for (k, (subject, literal)) in pairs.into_iter().enumerate() {
            if k % 4096 == 0 {
                stop()?;
            }
            let at = match read.get(&literal) {
                Some(at) => *at,
                None => {
                    let at = match numbers(&source.snapshot, literal) {
                        Ok(v) => Ok(index.store(&v)?),
                        Err(n) => Err(n),
                    };
                    read.insert(literal, at);
                    at
                }
            };
            let n = match at {
                Ok((start, len)) => index.push(subject, literal, start, len)?,
                Err(n) => n,
            };
            index.strings += usize::from(n == STRING);
            index.pairs.push(((subject, literal), n));
        }
        Ok(index)
    }

    /// An index of the given (subject, literal, numbers), for tests and measurements: the
    /// numbers need not be in a store.
    #[doc(hidden)]
    pub fn from_vectors(
        property: &NamedNode,
        vectors: impl IntoIterator<Item = (TermId, TermId, Vec<f64>)>,
    ) -> Result<Self, Error> {
        let mut index = Self::empty(property, 0);
        for (subject, literal, v) in vectors {
            if let Some(u) = unit(&v) {
                let (start, len) = index.store(&to_f32(&u))?;
                index.push(subject, literal, start, len)?;
            }
        }
        Ok(index)
    }

    /// Brings an index made by [`VectorIndex::build_from`] to what `source` shows now: the
    /// pairs added and removed are found by comparing the property's pairs with the index's,
    /// and only the added literals are read. Past a quarter changed, it is built again.
    pub fn update_from(
        &mut self,
        source: &Source,
        stop: &dyn Fn() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let generation = source.snapshot.generation().unwrap_or_default();
        if generation == self.generation {
            return Ok(());
        }
        let now = property_pairs(source, &self.property);
        let mut kept = Vec::with_capacity(now.len());
        let (mut added, mut removed) = (Vec::new(), Vec::new());
        let mut old = std::mem::take(&mut self.pairs).into_iter().peekable();
        for pair in now {
            while let Some(o) = old.next_if(|o| o.0 < pair) {
                removed.push(o);
            }
            match old.next_if(|o| o.0 == pair) {
                Some(o) => kept.push(o),
                None => added.push(pair),
            }
        }
        removed.extend(old);
        let dead = self.rows.len() - self.live + removed.len();
        if (added.len() + removed.len()) * 4 > self.live || dead > self.live.max(64) {
            let graph_min = self.graph_min();
            let file = lock(&self.shared.file).clone();
            *self = Self::build_from(source, &self.property.clone(), stop)?;
            self.set_graph_min(graph_min);
            self.set_graph_file(file);
            return Ok(());
        }
        for (_, n) in removed {
            if n == STRING {
                self.strings -= 1;
            } else if n != NOT_VECTOR {
                let row = &mut self.rows[n as usize];
                row.live = false;
                self.live -= 1;
                if let Some(c) = self.lengths.get_mut(&row.len) {
                    *c -= 1;
                }
            }
        }
        for (subject, literal) in added {
            let n = match numbers(&source.snapshot, literal) {
                Ok(v) => {
                    let (start, len) = self.store(&v)?;
                    self.push(subject, literal, start, len)?
                }
                Err(n) => n,
            };
            self.strings += usize::from(n == STRING);
            kept.push(((subject, literal), n));
        }
        kept.sort_unstable();
        self.pairs = kept;
        self.generation = generation;
        Ok(())
    }

    /// Appends `v`'s numbers: where they start, and how many.
    fn store(&mut self, v: &[f32]) -> Result<(u32, u32), Error> {
        Ok((self.blocks.push(v)?, v.len() as u32))
    }

    fn push(
        &mut self,
        subject: TermId,
        literal: TermId,
        start: u32,
        len: u32,
    ) -> Result<u32, Error> {
        let n = u32::try_from(self.rows.len())
            .ok()
            .filter(|n| *n < STRING)
            .ok_or_else(|| Error::Request("too many vectors for one index".into()))?;
        self.rows.push(Row {
            subject,
            literal,
            start,
            len,
            live: true,
        });
        self.live += 1;
        *self.lengths.entry(len).or_default() += 1;
        Ok(n)
    }

    /// The property indexed.
    pub fn property(&self) -> &NamedNode {
        &self.property
    }

    /// How many vectors are indexed.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// How many vectors of `len` numbers are indexed.
    pub fn len_of(&self, len: usize) -> usize {
        u32::try_from(len)
            .ok()
            .and_then(|l| self.lengths.get(&l))
            .copied()
            .unwrap_or(0)
    }

    /// How many objects of the property are plain strings: not indexed, though `tt:cosine`
    /// reads one in the form of a vector.
    pub fn strings(&self) -> usize {
        self.strings
    }

    /// The `k` vectors most similar to `query`, every one compared: exact whatever the
    /// number of vectors, unlike [`VectorIndex::nearest`]. The scores are computed in single
    /// precision: within [`VectorIndex::exact_error`] of `tt:cosine`.
    pub fn exact(&self, query: &[f64], k: usize) -> Vec<Neighbour> {
        let Some(q) = unit(query) else {
            return Vec::new();
        };
        if k == 0 {
            return Vec::new();
        }
        let mut best: BinaryHeap<Candidate> = BinaryHeap::new();
        self.scan(0, &to_f32(&q), Some(k), &mut best);
        finish(best)
    }

    /// How far a score of [`VectorIndex::exact`] may be from the cosine in double precision,
    /// for vectors of `len` numbers: each number rounded to single precision, and the sum
    /// rounded at each of its `len` steps.
    pub fn exact_error(len: usize) -> f64 {
        1e-6 + len as f64 * 1.2e-7
    }

    /// How many vectors of one length it takes to make a graph of them ([`GRAPH_MIN`]
    /// unless set); shared by this index's copies.
    pub fn graph_min(&self) -> usize {
        self.shared.graph_min.load(Ordering::Relaxed)
    }

    pub fn set_graph_min(&self, n: usize) {
        self.shared.graph_min.store(n.max(1), Ordering::Relaxed);
    }

    /// Keeps the graphs in files `{base}-{length}.hnsw`, so that a restart reads them instead of
    /// building them again; `None` keeps them in memory only. Shared by this index's copies.
    pub fn set_graph_file(&self, base: Option<std::path::PathBuf>) {
        *lock(&self.shared.file) = base;
    }

    fn graph_path(&self, dim: u32) -> Option<std::path::PathBuf> {
        lock(&self.shared.file).as_ref().map(|base| {
            let mut name = base.file_name().unwrap_or_default().to_owned();
            name.push(format!("-{dim}.hnsw"));
            base.with_file_name(name)
        })
    }

    /// Sets how many candidates a graph search keeps at least, for measurements.
    #[doc(hidden)]
    pub fn set_search_width(&self, n: usize) {
        self.shared.width.store(n, Ordering::Relaxed);
    }

    /// How many vectors of `len` numbers the graph of that length holds (0 for none yet),
    /// and whether one is being made.
    pub fn graph_status(&self, len: usize) -> (usize, bool) {
        let graphs = lock(&self.shared.graphs);
        graphs.get(&(len as u32)).map_or((0, false), |s| {
            (s.graph.as_ref().map_or(0, |g| g.rows.len()), s.building)
        })
    }

    /// The memory the index takes, in bytes, roughly.
    pub fn heap_size(&self) -> usize {
        let graphs: usize = lock(&self.shared.graphs)
            .values()
            .filter_map(|s| s.graph.as_ref())
            .map(|g| g.heap_size())
            .sum();
        self.rows.capacity() * std::mem::size_of::<Row>()
            + self.blocks.heap_size()
            + self.pairs.capacity() * std::mem::size_of::<((TermId, TermId), u32)>()
            + graphs
    }

    fn vector(&self, row: &Row) -> &[f32] {
        self.blocks.get(row.start, row.len)
    }

    /// The `k` vectors most similar to `query` by cosine (all of them for `None`), best first,
    /// among those of its length; ties by subject and literal, so
    /// the answer does not depend on the order of the writes. Past [`GRAPH_MIN`] vectors of
    /// the length, a graph of them is followed once it is made: approximate.
    pub fn nearest(&self, query: &[f64], k: Option<usize>) -> Vec<Neighbour> {
        let Some(q) = unit(query) else {
            return Vec::new();
        };
        let q = to_f32(&q);
        if k == Some(0) {
            return Vec::new();
        }
        let dim = q.len() as u32;
        let mut best: BinaryHeap<Candidate> = BinaryHeap::new();
        let Some(k) = k else {
            self.scan(0, &q, None, &mut best);
            return finish(best);
        };
        let count = self.lengths.get(&dim).copied().unwrap_or(0);
        if count < self.graph_min() {
            self.scan(0, &q, Some(k), &mut best);
            return finish(best);
        }
        let (graph, building) = {
            let graphs = lock(&self.shared.graphs);
            graphs
                .get(&dim)
                .map_or((None, false), |s| (s.graph.clone(), s.building))
        };
        let covered = graph.as_ref().map_or(0, |g| g.covered.min(self.rows.len()));
        // The rows the graph does not hold, one by one.
        let mut tail = self.scan(covered, &q, Some(k), &mut best);
        if let Some(g) = &graph {
            for &r in &g.extra {
                if let Some(row) = self.rows.get(r as usize)
                    && row.live
                {
                    tail += 1;
                    offer(&mut best, Some(k), hnsw::dot(self.vector(row), &q), row);
                }
            }
        }
        if !building
            && graph
                .as_ref()
                .is_none_or(|g| tail > (g.rows.len() / 8).max(1024))
        {
            self.make_graph(dim, graph.clone());
        }
        if let Some(graph) = graph {
            self.follow(&graph, &q, k, &mut best);
        }
        finish(best)
    }

    /// Compares `q` with the live rows of its length from `from` on, keeping the best `k` in
    /// `best`: how many rows of the length there were.
    fn scan(
        &self,
        from: usize,
        q: &[f32],
        k: Option<usize>,
        best: &mut BinaryHeap<Candidate>,
    ) -> usize {
        let mut seen = 0;
        for row in &self.rows[from.min(self.rows.len())..] {
            if !row.live || row.len as usize != q.len() {
                continue;
            }
            seen += 1;
            offer(best, k, hnsw::dot(self.vector(row), q), row);
        }
        seen
    }

    /// Searches `graph` for the best `k` live rows, widening the search while too few are
    /// live (removed rows stay in the graph); past a quarter of the graph, compares them all.
    /// What the graph finds is compared again with the full vectors.
    fn follow(&self, graph: &Graph, q: &[f32], k: usize, best: &mut BinaryHeap<Candidate>) {
        let width = match self.shared.width.load(Ordering::Relaxed) {
            0 => SEARCH_WIDTH,
            w => w,
        };
        let mut ef = (2 * k).max(width);
        let query = hnsw::Query::new(q.to_vec());
        loop {
            if ef > graph.rows.len() / 4 {
                for &r in &graph.rows {
                    if let Some(row) = self.rows.get(r as usize)
                        && row.live
                    {
                        offer(best, Some(k), hnsw::dot(self.vector(row), q), row);
                    }
                }
                return;
            }
            let found = graph.links.search(graph, &query, ef);
            // A copy of the index older than the graph does not have its last rows.
            let live: Vec<&Row> = found
                .iter()
                .filter_map(|s| self.rows.get(graph.rows[s.1 as usize] as usize))
                .filter(|row| row.live)
                .collect();
            if live.len() >= k || found.len() < ef {
                for row in live {
                    offer(best, Some(k), hnsw::dot(self.vector(row), q), row);
                }
                return;
            }
            ef *= 4;
        }
    }

    /// Starts making (or extending) the graph of the vectors of `dim` numbers, on a thread of
    /// its own; searches go on without it until it is ready.
    fn make_graph(&self, dim: u32, old: Option<Arc<Graph>>) {
        {
            let mut graphs = lock(&self.shared.graphs);
            let state = graphs.entry(dim).or_default();
            if state.building {
                return;
            }
            state.building = true;
        }
        let covered = self.rows.len();
        let from = old.as_ref().map_or(0, |g| g.covered.min(covered));
        // The rows to add: those the old graph left out, and the later ones. The thread puts
        // their numbers in 8 bits, from the blocks (shared, not copied).
        let mut added: Vec<(u32, u32)> = Vec::new();
        let mut take = |n: usize, row: &Row| {
            if row.live && row.len == dim {
                added.push((n as u32, row.start));
            }
        };
        if let Some(g) = &old {
            for &r in &g.extra {
                if let Some(row) = self.rows.get(r as usize) {
                    take(r as usize, row);
                }
            }
        }
        for (n, row) in self.rows.iter().enumerate().skip(from) {
            take(n, row);
        }
        let blocks = self.blocks.clone();
        let file = self.graph_path(dim);
        // What the thread needs of the rows: to match a saved graph's nodes, and to save.
        let rows = file.as_ref().map(|_| self.rows.clone()).unwrap_or_default();
        let property = self.property.as_str().to_owned();
        let shared = Arc::downgrade(&self.shared);
        let done = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("vector-graph".into())
            .spawn(move || {
                let _turn = lock(&BUILDS);
                let stop = || shared.strong_count() == 0;
                if old.is_none()
                    && let Some(path) = &file
                    && !stop()
                    && let Some(graph) = Graph::load(path, &property, dim, &rows)
                {
                    if let Some(shared) = shared.upgrade() {
                        let mut graphs = lock(&shared.graphs);
                        let state = graphs.entry(dim).or_default();
                        state.building = false;
                        state.graph = Some(Arc::new(graph));
                    }
                    return;
                }
                let mut graph = match &old {
                    Some(g) => Graph {
                        codes: g.codes.clone(),
                        scales: g.scales.clone(),
                        rows: g.rows.clone(),
                        ..Graph::empty(dim, covered)
                    },
                    None => Graph::empty(dim, covered),
                };
                graph.codes.reserve_exact(added.len() * dim as usize);
                for &(row, start) in &added {
                    graph.push(blocks.get(start, dim));
                    graph.rows.push(row);
                }
                drop(blocks);
                let seed = |node: u32| u64::from(graph.rows[node as usize]);
                let links = if stop() {
                    None
                } else {
                    match &old {
                        Some(g) => g.links.extend(&graph, seed, &stop),
                        None => Frozen::build(&graph, seed, &stop),
                    }
                };
                let Some(links) = links else {
                    if let Some(shared) = shared.upgrade() {
                        lock(&shared.graphs).entry(dim).or_default().building = false;
                    }
                    return;
                };
                graph.links = links;
                let graph = Arc::new(graph);
                if let Some(shared) = shared.upgrade() {
                    let mut graphs = lock(&shared.graphs);
                    let state = graphs.entry(dim).or_default();
                    state.building = false;
                    state.graph = Some(Arc::clone(&graph));
                }
                // Saved after it is in use; still under the turn, so saves do not overlap.
                if let Some(path) = &file
                    && let Err(e) = graph.save(path, &property, &rows)
                {
                    eprintln!("saving the vector graph {}: {e}", path.display());
                }
            });
        if spawned.is_err()
            && let Some(shared) = done.upgrade()
        {
            lock(&shared.graphs).entry(dim).or_default().building = false;
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps `row` (of similarity `score`) in `best` if it is among the best `k`.
fn offer(best: &mut BinaryHeap<Candidate>, k: Option<usize>, score: f32, row: &Row) {
    // Both are unit vectors: the dot product is the cosine.
    let score = f64::from(score);
    if score.is_nan() {
        return;
    }
    let c = Candidate {
        score: score.clamp(-1.0, 1.0),
        subject: row.subject,
        literal: row.literal,
    };
    if let Some(k) = k
        && best.len() == k
    {
        if best.peek().is_some_and(|worst| c < *worst) {
            best.pop();
            best.push(c);
        }
        return;
    }
    best.push(c);
}

/// The candidates best first.
fn finish(best: BinaryHeap<Candidate>) -> Vec<Neighbour> {
    let mut out: Vec<Candidate> = best.into_vec();
    out.sort();
    out.into_iter()
        .map(|c| Neighbour {
            subject: c.subject,
            literal: c.literal,
            score: c.score,
        })
        .collect()
}

/// A neighbour as the heap orders it: better is less (a higher score, then the lower ids).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Candidate {
    score: f64,
    subject: TermId,
    literal: TermId,
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then(self.subject.cmp(&other.subject))
            .then(self.literal.cmp(&other.literal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_arrays_of_numbers() {
        assert_eq!(parse_text("[1, -2.5, 3e-1]"), Some(vec![1.0, -2.5, 0.3]));
        assert_eq!(parse_text(" [0] "), Some(vec![0.0]));
        for bad in [
            "[]", "1, 2", "[1,]", "[NaN]", "[inf]", "[.5]", "[+1]", "[1e400]", "[a]",
        ] {
            assert_eq!(parse_text(bad), None, "{bad}");
        }
        let v = Term::from(literal(&[0.5, 1.0]));
        assert_eq!(parse(&v), Some(vec![0.5, 1.0]));
        assert_eq!(parse(&Term::from(Literal::from(1))), None);
    }

    #[test]
    fn measures_vectors() {
        assert_eq!(dot(&[1.0, 2.0], &[3.0, 4.0]), Some(11.0));
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 2.0]), Some(0.0));
        assert_eq!(cosine(&[1.0, 1.0], &[2.0, 2.0]), Some(1.0));
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), None);
        assert!((cosine(&[1e300, 1e300], &[1.0, 0.0]).unwrap() - 0.5f64.sqrt()).abs() < 1e-12);
        assert_eq!(cosine(&[1e-320, 0.0], &[1.0, 0.0]), Some(1.0));
        assert_eq!(l2(&[0.0, 0.0], &[3.0, 4.0]), Some(5.0));
        assert_eq!(dot(&[1.0], &[1.0, 2.0]), None);
    }
}
