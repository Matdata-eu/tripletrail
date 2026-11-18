//! A hierarchical navigable small world graph (HNSW; Malkov and Yashunin, 2016) over unit
//! `f32` vectors, for approximate nearest-neighbour search by inner product (#236).
//!
//! The graph is built by several threads at once, each node behind its own lock, and then
//! frozen into flat arrays that searches read without locks. A frozen graph can be extended
//! with more nodes, which unfreezes a copy of it.

use std::cell::RefCell;
use std::cmp::Ordering as Cmp;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The links a node keeps on the layers above the bottom one.
pub(crate) const M: usize = 32;
/// The links a node keeps on the bottom layer.
const M0: usize = 2 * M;
/// How many candidates an insertion considers.
const EF_CONSTRUCTION: usize = 200;
/// The highest layer.
const MAX_LEVEL: u8 = 15;
const NONE: u32 = u32::MAX;

/// The inner product of two vectors of the same length. On x86-64 processors with AVX2 and
/// FMA it uses them (about twice as fast as the SSE2 every x86-64 has).
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static AVX2: OnceLock<bool> = OnceLock::new();
        if *AVX2.get_or_init(|| {
            std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("fma")
        }) {
            // SAFETY: the processor has the features, as checked above.
            return unsafe { dot_avx2(a, b) };
        }
    }
    dot_lanes(a, b, |x, y, acc| x * y + acc)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn dot_avx2(a: &[f32], b: &[f32]) -> f32 {
    // Fused: one instruction with FMA, but a slow library call without it.
    dot_lanes(a, b, f32::mul_add)
}

/// The inner product in sixteen lanes, so the compiler can use vector instructions;
/// `madd(x, y, acc)` is `x * y + acc`.
#[inline(always)]
fn dot_lanes(a: &[f32], b: &[f32], madd: impl Fn(f32, f32, f32) -> f32) -> f32 {
    const L: usize = 16;
    let (ca, ra) = a.as_chunks::<L>();
    let (cb, rb) = b.as_chunks::<L>();
    let mut acc = [0f32; L];
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..L {
            acc[i] = madd(x[i], y[i], acc[i]);
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

/// Puts `v` in 8 bits: the codes, and the scale that turns them back into numbers.
pub(crate) fn quantise(v: &[f32], codes: &mut Vec<i8>) -> f32 {
    let largest = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    let scale = if largest > 0.0 { largest / 127.0 } else { 1.0 };
    codes.extend(
        v.iter()
            .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8),
    );
    scale
}

/// A vector to compare with a graph's nodes: its numbers, and in 8 bits.
pub(crate) struct Query {
    /// The graphs of the index compare codes only; the tests' compare numbers.
    #[cfg_attr(not(test), allow(dead_code))]
    pub numbers: Vec<f32>,
    pub codes: Vec<i8>,
    pub scale: f32,
}

impl Query {
    pub(crate) fn new(numbers: Vec<f32>) -> Self {
        let mut codes = Vec::with_capacity(numbers.len());
        let scale = quantise(&numbers, &mut codes);
        Self {
            numbers,
            codes,
            scale,
        }
    }
}

/// The inner product of two vectors of 8-bit codes, in whole numbers.
pub(crate) fn dot_codes(a: &[i8], b: &[i8]) -> i32 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static AVX2: OnceLock<bool> = OnceLock::new();
        if *AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2")) {
            // SAFETY: the processor has the feature, as checked above.
            return unsafe { dot_codes_avx2(a, b) };
        }
    }
    dot_codes_lanes(a, b)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn dot_codes_avx2(a: &[i8], b: &[i8]) -> i32 {
    dot_codes_lanes(a, b)
}

#[inline(always)]
fn dot_codes_lanes(a: &[i8], b: &[i8]) -> i32 {
    const L: usize = 32;
    let (ca, ra) = a.as_chunks::<L>();
    let (cb, rb) = b.as_chunks::<L>();
    let mut acc = [0i32; L];
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..L {
            acc[i] += i32::from(i16::from(x[i]) * i16::from(y[i]));
        }
    }
    let mut s: i32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += i32::from(*x) * i32::from(*y);
    }
    s
}

/// Where a graph's nodes find their numbers.
pub(crate) trait Vectors: Sync {
    /// The inner product of `q` with node `node`'s vector.
    fn dot(&self, q: &Query, node: u32) -> f32;
    /// Node `node`'s vector, into `out`.
    fn decode(&self, node: u32, out: &mut Vec<f32>);
    /// The inner product of two nodes' vectors.
    fn dot_nodes(&self, a: u32, b: u32) -> f32;
    fn len(&self) -> usize;
}

/// A (similarity, node) pair; greater is more similar, ties to the lower node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Scored(pub f32, pub u32);

impl Eq for Scored {}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Cmp> {
        Some(self.cmp(other))
    }
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Cmp {
        self.0
            .total_cmp(&other.0)
            .then_with(|| other.1.cmp(&self.1))
    }
}

/// The level of a node: random, but the same for the same seed.
fn level(seed: u64) -> u8 {
    // splitmix64
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    let u = ((z >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
    let ml = 1.0 / (M as f64).ln();
    ((-u.ln() * ml).floor() as u64).min(u64::from(MAX_LEVEL)) as u8
}

thread_local! {
    /// Marks for the nodes a search has seen: a node is seen when its mark is the epoch.
    static VISITED: RefCell<(Vec<u32>, u32)> = const { RefCell::new((Vec::new(), 0)) };
}

/// Runs `f` with a fresh set of seen nodes for a graph of `n` nodes.
fn with_visited<R>(n: usize, f: impl FnOnce(&mut dyn FnMut(u32) -> bool) -> R) -> R {
    VISITED.with(|cell| {
        let mut guard = cell.borrow_mut();
        let (marks, epoch) = &mut *guard;
        if marks.len() < n {
            marks.resize(n, 0);
        }
        *epoch = epoch.wrapping_add(1);
        if *epoch == 0 {
            marks.fill(0);
            *epoch = 1;
        }
        let e = *epoch;
        let mut first_visit = |node: u32| {
            let m = &mut marks[node as usize];
            if *m == e {
                false
            } else {
                *m = e;
                true
            }
        };
        f(&mut first_visit)
    })
}

/// The best-first search of one layer: the `ef` nodes most similar to `q` found from
/// `entry`, best first. `links(node)` gives a node's links on the layer.
fn search_layer(
    vectors: &dyn Vectors,
    q: &Query,
    entry: &[Scored],
    ef: usize,
    mut links: impl FnMut(u32, &mut Vec<u32>),
) -> Vec<Scored> {
    with_visited(vectors.len(), |first_visit| {
        // Candidates to expand, most similar first; and the best found, least similar on top.
        let mut candidates: BinaryHeap<Scored> = BinaryHeap::new();
        let mut found: BinaryHeap<std::cmp::Reverse<Scored>> = BinaryHeap::new();
        for e in entry {
            if first_visit(e.1) {
                candidates.push(*e);
                found.push(std::cmp::Reverse(*e));
            }
        }
        while found.len() > ef {
            found.pop();
        }
        let mut next = Vec::with_capacity(M0);
        while let Some(c) = candidates.pop() {
            if found.len() >= ef && found.peek().is_some_and(|w| c < w.0) {
                break;
            }
            next.clear();
            links(c.1, &mut next);
            for &n in &next {
                if !first_visit(n) {
                    continue;
                }
                let s = Scored(vectors.dot(q, n), n);
                if found.len() < ef || found.peek().is_some_and(|w| s > w.0) {
                    candidates.push(s);
                    found.push(std::cmp::Reverse(s));
                    if found.len() > ef {
                        found.pop();
                    }
                }
            }
        }
        let mut out: Vec<Scored> = found.into_iter().map(|r| r.0).collect();
        out.sort_unstable_by(|a, b| b.cmp(a));
        out
    })
}

/// Picks at most `m` links from `candidates` (best first): a candidate is kept only if it is
/// more similar to the node than to every link kept before it, so the links point in
/// different directions (the paper's heuristic).
fn select(vectors: &dyn Vectors, candidates: &[Scored], m: usize) -> Vec<u32> {
    let mut kept: Vec<Scored> = Vec::with_capacity(m);
    for c in candidates {
        if kept.len() == m {
            break;
        }
        if kept.iter().all(|k| vectors.dot_nodes(c.1, k.1) < c.0) {
            kept.push(*c);
        }
    }
    kept.into_iter().map(|s| s.1).collect()
}

/// A graph being built: each node's links per layer, behind a lock per node.
struct Builder<'a> {
    vectors: &'a dyn Vectors,
    levels: Vec<u8>,
    links: Vec<Mutex<Vec<Vec<u32>>>>,
    /// The entry node and its level; `NONE` before the first node.
    entry: Mutex<(u32, u8)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Builder<'_> {
    fn links_of(&self, node: u32, layer: u8, out: &mut Vec<u32>) {
        let l = lock(&self.links[node as usize]);
        if let Some(list) = l.get(layer as usize) {
            out.extend_from_slice(list);
        }
    }

    fn insert(&self, node: u32) {
        let level = self.levels[node as usize];
        let (entry, top) = {
            let mut e = lock(&self.entry);
            if e.0 == NONE {
                *e = (node, level);
                return;
            }
            *e
        };
        let mut q = Vec::new();
        self.vectors.decode(node, &mut q);
        let q = &Query::new(q);
        let mut cur = Scored(self.vectors.dot(q, entry), entry);
        let mut next = Vec::new();
        for layer in (level.saturating_add(1)..=top).rev() {
            loop {
                let mut moved = false;
                next.clear();
                self.links_of(cur.1, layer, &mut next);
                for &n in &next {
                    let s = Scored(self.vectors.dot(q, n), n);
                    if s > cur {
                        cur = s;
                        moved = true;
                    }
                }
                if !moved {
                    break;
                }
            }
        }
        let mut entries = vec![cur];
        for layer in (0..=level.min(top)).rev() {
            let found = search_layer(self.vectors, q, &entries, EF_CONSTRUCTION, |n, out| {
                self.links_of(n, layer, out)
            });
            let chosen = select(self.vectors, &found, M);
            lock(&self.links[node as usize])[layer as usize] = chosen.clone();
            let most = if layer == 0 { M0 } else { M };
            for &n in &chosen {
                let mut l = lock(&self.links[n as usize]);
                let list = &mut l[layer as usize];
                if list.contains(&node) {
                    continue;
                }
                if list.len() < most {
                    list.push(node);
                    continue;
                }
                let mut candidates: Vec<Scored> = list
                    .iter()
                    .chain(std::iter::once(&node))
                    .map(|&o| Scored(self.vectors.dot_nodes(n, o), o))
                    .collect();
                candidates.sort_unstable_by(|a, b| b.cmp(a));
                *list = select(self.vectors, &candidates, most);
            }
            entries = found;
        }
        if level > top {
            let mut e = lock(&self.entry);
            if level > e.1 {
                *e = (node, level);
            }
        }
    }

    /// Inserts nodes `from..` on half the cores.
    fn insert_all(&self, from: usize, stop: &(dyn Fn() -> bool + Sync)) {
        let n = self.levels.len();
        let mut start = from;
        // The first node is the entry: insert it, and a few more, alone.
        while start < n && start < from + 1 {
            self.insert(start as u32);
            start += 1;
        }
        let next = AtomicUsize::new(start);
        // Half the cores, so that the queries the server answers meanwhile keep the rest.
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().div_ceil(2))
            .unwrap_or(1)
            .min(n.saturating_sub(start).div_ceil(1024).max(1));
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n || (i.is_multiple_of(1024) && stop()) {
                            break;
                        }
                        self.insert(i as u32);
                    }
                });
            }
        });
    }

    fn freeze(self) -> Frozen {
        let n = self.levels.len();
        let mut layer0 = vec![NONE; n * M0];
        let mut upper_start = vec![NONE; n];
        let mut upper = Vec::new();
        for (i, links) in self.links.into_iter().enumerate() {
            let links = links.into_inner().unwrap_or_else(|e| e.into_inner());
            if let Some(l0) = links.first() {
                layer0[i * M0..i * M0 + l0.len()].copy_from_slice(l0);
            }
            if links.len() > 1 {
                upper_start[i] = upper.len() as u32;
                for l in &links[1..] {
                    let at = upper.len();
                    upper.resize(at + M, NONE);
                    upper[at..at + l.len()].copy_from_slice(l);
                }
            }
        }
        let entry = self.entry.into_inner().unwrap_or_else(|e| e.into_inner());
        Frozen {
            levels: self.levels,
            layer0,
            upper_start,
            upper,
            entry: entry.0,
            top: entry.1,
        }
    }
}

/// A built graph's links, in flat arrays.
#[derive(Clone, Debug, Default)]
pub(crate) struct Frozen {
    levels: Vec<u8>,
    /// `M0` slots per node, `NONE` past its links.
    layer0: Vec<u32>,
    /// Where a node's links above the bottom layer start in `upper` (`M` slots per layer),
    /// `NONE` for a node only on the bottom layer.
    upper_start: Vec<u32>,
    upper: Vec<u32>,
    entry: u32,
    top: u8,
}

impl Frozen {
    /// Builds the graph of `vectors`' nodes, all of them. `seed(node)` picks a node's
    /// level. `stop` is asked now and then; `None` when it said to stop.
    pub(crate) fn build(
        vectors: &dyn Vectors,
        seed: impl Fn(u32) -> u64,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Option<Self> {
        Self::default().extend(vectors, seed, stop)
    }

    /// This graph with `vectors`' nodes past its own added (the first nodes being its own);
    /// `None` when `stop` said to stop.
    pub(crate) fn extend(
        &self,
        vectors: &dyn Vectors,
        seed: impl Fn(u32) -> u64,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Option<Self> {
        let have = self.len();
        let n = vectors.len();
        let mut levels = self.levels.clone();
        levels.extend((have..n).map(|i| level(seed(i as u32))));
        let mut links = Vec::with_capacity(n);
        for (i, &level) in levels.iter().enumerate() {
            let mut per_layer = vec![Vec::new(); level as usize + 1];
            if i < have {
                for (l, list) in per_layer.iter_mut().enumerate() {
                    *list = self.links(i as u32, l as u8).to_vec();
                }
            }
            links.push(Mutex::new(per_layer));
        }
        let builder = Builder {
            vectors,
            levels,
            links,
            entry: Mutex::new(if have == 0 {
                (NONE, 0)
            } else {
                (self.entry, self.top)
            }),
        };
        builder.insert_all(have, stop);
        (!stop()).then(|| builder.freeze())
    }

    /// How many nodes.
    pub(crate) fn len(&self) -> usize {
        self.levels.len()
    }

    fn links(&self, node: u32, layer: u8) -> &[u32] {
        let list = if layer == 0 {
            &self.layer0[node as usize * M0..(node as usize + 1) * M0]
        } else {
            let start = self.upper_start[node as usize];
            if start == NONE || layer > self.levels[node as usize] {
                return &[];
            }
            let at = start as usize + (layer as usize - 1) * M;
            &self.upper[at..at + M]
        };
        let end = list.iter().position(|&x| x == NONE).unwrap_or(list.len());
        &list[..end]
    }

    /// The `ef` nodes most similar to `q` that the search finds, best first.
    pub(crate) fn search(&self, vectors: &dyn Vectors, q: &Query, ef: usize) -> Vec<Scored> {
        if self.len() == 0 {
            return Vec::new();
        }
        let mut cur = Scored(vectors.dot(q, self.entry), self.entry);
        for layer in (1..=self.top).rev() {
            loop {
                let mut moved = false;
                for &n in self.links(cur.1, layer) {
                    let s = Scored(vectors.dot(q, n), n);
                    if s > cur {
                        cur = s;
                        moved = true;
                    }
                }
                if !moved {
                    break;
                }
            }
        }
        search_layer(vectors, q, &[cur], ef.max(1), |n, out| {
            out.extend_from_slice(self.links(n, 0))
        })
    }

    /// Writes the links to `w` (read back by [`Frozen::read`]).
    pub(crate) fn write(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        w.write_all(&(self.len() as u64).to_le_bytes())?;
        w.write_all(&self.entry.to_le_bytes())?;
        w.write_all(&[self.top])?;
        w.write_all(&self.levels)?;
        write_u32s(w, &self.layer0)?;
        write_u32s(w, &self.upper_start)?;
        w.write_all(&(self.upper.len() as u64).to_le_bytes())?;
        write_u32s(w, &self.upper)
    }

    /// Links written by [`Frozen::write`]; checks that every link is to a node.
    pub(crate) fn read(r: &mut impl std::io::Read) -> std::io::Result<Self> {
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a vector graph");
        let n = usize::try_from(read_u64(r)?).map_err(|_| bad())?;
        if n >= NONE as usize {
            return Err(bad());
        }
        let entry = read_u32(r)?;
        let mut top = [0u8];
        r.read_exact(&mut top)?;
        let mut levels = vec![0u8; n];
        r.read_exact(&mut levels)?;
        let layer0 = read_u32s(r, n.checked_mul(M0).ok_or_else(bad)?)?;
        let upper_start = read_u32s(r, n)?;
        let upper_len = usize::try_from(read_u64(r)?).map_err(|_| bad())?;
        let upper = read_u32s(r, upper_len)?;
        let frozen = Self {
            levels,
            layer0,
            upper_start,
            upper,
            entry,
            top: top[0],
        };
        let node = |x: u32| x == NONE || (x as usize) < n;
        let fits = (0..n).all(|i| {
            let start = frozen.upper_start[i];
            let level = frozen.levels[i] as usize;
            level <= MAX_LEVEL as usize
                && level <= frozen.top as usize
                && if level == 0 {
                    start == NONE
                } else {
                    start != NONE
                        && (start as usize)
                            .checked_add(level * M)
                            .is_some_and(|end| end <= frozen.upper.len())
                }
        });
        if !fits
            || (n > 0 && (entry as usize >= n || frozen.levels[entry as usize] != frozen.top))
            || !frozen.layer0.iter().chain(&frozen.upper).all(|&x| node(x))
        {
            return Err(bad());
        }
        Ok(frozen)
    }

    /// The memory the graph takes, in bytes.
    pub(crate) fn heap_size(&self) -> usize {
        self.levels.capacity()
            + (self.layer0.capacity() + self.upper_start.capacity() + self.upper.capacity()) * 4
    }
}

fn write_u32s(w: &mut impl std::io::Write, xs: &[u32]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(xs.len().min(1 << 16) * 4);
    for chunk in xs.chunks(1 << 16) {
        buf.clear();
        for x in chunk {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        w.write_all(&buf)?;
    }
    Ok(())
}

pub(crate) fn read_u32(r: &mut impl std::io::Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub(crate) fn read_u64(r: &mut impl std::io::Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Reads `n` numbers; fails without allocating them all when the input is shorter.
pub(crate) fn read_u32s(r: &mut impl std::io::Read, n: usize) -> std::io::Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 18];
    let mut left = n;
    while left > 0 {
        let take = left.min(buf.len() / 4);
        r.read_exact(&mut buf[..take * 4])?;
        out.extend(
            buf[..take * 4]
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );
        left -= take;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flat {
        dim: usize,
        data: Vec<f32>,
    }

    impl Flat {
        fn vector(&self, node: u32) -> &[f32] {
            &self.data[node as usize * self.dim..(node as usize + 1) * self.dim]
        }
    }

    impl Vectors for Flat {
        fn dot(&self, q: &Query, node: u32) -> f32 {
            dot(&q.numbers, self.vector(node))
        }
        fn decode(&self, node: u32, out: &mut Vec<f32>) {
            out.clear();
            out.extend_from_slice(self.vector(node));
        }
        fn dot_nodes(&self, a: u32, b: u32) -> f32 {
            dot(self.vector(a), self.vector(b))
        }
        fn len(&self) -> usize {
            self.data.len() / self.dim
        }
    }

    /// `n` unit vectors of `dim` numbers around 50 centres.
    fn clustered(n: usize, dim: usize, seed: u64) -> Flat {
        let mut state = seed;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
        };
        let centres: Vec<Vec<f32>> = (0..50)
            .map(|_| (0..dim).map(|_| next()).collect())
            .collect();
        let mut data = Vec::with_capacity(n * dim);
        for i in 0..n {
            let c = &centres[i % 50];
            let v: Vec<f32> = c.iter().map(|x| x + 0.6 * next()).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            data.extend(v.iter().map(|x| x / norm));
        }
        Flat { dim, data }
    }

    fn exact(f: &Flat, q: &[f32], k: usize) -> Vec<u32> {
        let mut all: Vec<Scored> = (0..f.len() as u32)
            .map(|i| Scored(dot(q, f.vector(i)), i))
            .collect();
        all.sort_unstable_by(|a, b| b.cmp(a));
        all.into_iter().take(k).map(|s| s.1).collect()
    }

    fn recall(graph: &Frozen, f: &Flat, queries: &Flat, k: usize, ef: usize) -> f64 {
        let mut hit = 0;
        for i in 0..queries.len() as u32 {
            let q = queries.vector(i);
            let truth = exact(f, q, k);
            let got: Vec<u32> = graph
                .search(f, &Query::new(q.to_vec()), ef)
                .into_iter()
                .take(k)
                .map(|s| s.1)
                .collect();
            hit += truth.iter().filter(|t| got.contains(t)).count();
        }
        hit as f64 / (k * queries.len()) as f64
    }

    #[test]
    fn dot_matches_the_plain_sum() {
        let a: Vec<f32> = (0..37).map(|i| i as f32 * 0.1).collect();
        let b: Vec<f32> = (0..37).map(|i| 1.0 - i as f32 * 0.01).collect();
        let plain: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - plain).abs() < 1e-3);
    }

    #[test]
    fn finds_the_nearest_with_high_recall() {
        let f = clustered(5000, 32, 1);
        let queries = clustered(100, 32, 2);
        let graph = Frozen::build(&f, u64::from, &|| false).unwrap();
        assert_eq!(graph.len(), 5000);
        let r = recall(&graph, &f, &queries, 10, 64);
        assert!(r >= 0.95, "recall {r}");
    }

    #[test]
    fn an_extended_graph_finds_the_new_nodes() {
        let all = clustered(4000, 16, 3);
        let first = Flat {
            dim: 16,
            data: all.data[..2000 * 16].to_vec(),
        };
        let graph = Frozen::build(&first, u64::from, &|| false).unwrap();
        let graph = graph.extend(&all, u64::from, &|| false).unwrap();
        assert_eq!(graph.len(), 4000);
        // Each new node finds itself.
        for i in (2000..4000).step_by(97) {
            let top = graph.search(&all, &Query::new(all.vector(i).to_vec()), 32);
            assert_eq!(top[0].1, i, "node {i}");
        }
        let queries = clustered(50, 16, 4);
        assert!(recall(&graph, &all, &queries, 10, 64) >= 0.95);
    }

    #[test]
    fn a_written_graph_reads_back_the_same() {
        let f = clustered(2000, 8, 7);
        let graph = Frozen::build(&f, u64::from, &|| false).unwrap();
        let mut bytes = Vec::new();
        graph.write(&mut bytes).unwrap();
        let back = Frozen::read(&mut &bytes[..]).unwrap();
        let q = Query::new(f.vector(5).to_vec());
        assert_eq!(graph.search(&f, &q, 16), back.search(&f, &q, 16));
        // Cut short, or with a link past the nodes: refused.
        assert!(Frozen::read(&mut &bytes[..bytes.len() - 1]).is_err());
        let mut bad = bytes.clone();
        let at = 8 + 4 + 1 + 2000;
        bad[at..at + 4].copy_from_slice(&5000u32.to_le_bytes());
        assert!(Frozen::read(&mut &bad[..]).is_err());
    }

    #[test]
    fn a_stopped_build_gives_nothing() {
        let f = clustered(3000, 8, 5);
        assert!(Frozen::build(&f, u64::from, &|| true).is_none());
    }
}
