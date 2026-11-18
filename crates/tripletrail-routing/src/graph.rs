use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};

/// FxHash (rustc's hasher): much faster than SipHash for interning terms, and DoS resistance
/// does not matter for edge sets built from the store's own data.
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0xf135_7aea_2e62_a9c5;
        let (chunks, remainder) = bytes.as_chunks::<8>();
        for c in chunks {
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(*c)).wrapping_mul(K);
        }
        for &b in remainder {
            self.0 = (self.0.rotate_left(5) ^ u64::from(b)).wrapping_mul(K);
        }
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0xf135_7aea_2e62_a9c5);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub(crate) type FxSet<K> = std::collections::HashSet<K, BuildHasherDefault<FxHasher>>;

/// One row of a pgRouting `edges_sql` result, with ids already made dense.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeRow {
    /// Dense edge id (index into the caller's edge-term table).
    pub id: u32,
    /// Dense source vertex id.
    pub source: u32,
    /// Dense target vertex id.
    pub target: u32,
    /// Weight `source -> target`. Negative means the arc does not exist (pgRouting semantics).
    pub cost: f64,
    /// Weight `target -> source`. Negative or `None` means the arc does not exist.
    pub reverse_cost: Option<f64>,
}

/// Compressed sparse row adjacency.
#[derive(Debug, Default, Clone)]
pub(crate) struct Csr {
    pub(crate) offsets: Vec<u32>,
    pub(crate) head: Vec<u32>,
    pub(crate) weight: Vec<f64>,
    pub(crate) edge: Vec<u32>,
}

impl Csr {
    fn build(num_vertices: usize, arcs: &[(u32, u32, f64, u32)]) -> Self {
        let mut offsets = vec![0u32; num_vertices + 1];
        for &(tail, _, _, _) in arcs {
            offsets[tail as usize + 1] += 1;
        }
        for i in 0..num_vertices {
            offsets[i + 1] += offsets[i];
        }
        let mut cursor = offsets.clone();
        let mut head = vec![0u32; arcs.len()];
        let mut weight = vec![0f64; arcs.len()];
        let mut edge = vec![0u32; arcs.len()];
        for &(tail, h, w, e) in arcs {
            let slot = cursor[tail as usize] as usize;
            cursor[tail as usize] += 1;
            head[slot] = h;
            weight[slot] = w;
            edge[slot] = e;
        }
        Self {
            offsets,
            head,
            weight,
            edge,
        }
    }

    #[inline]
    pub(crate) fn arcs(&self, v: u32) -> impl Iterator<Item = (u32, f64, u32)> + '_ {
        let start = self.offsets[v as usize] as usize;
        let end = self.offsets[v as usize + 1] as usize;
        (start..end).map(move |i| (self.head[i], self.weight[i], self.edge[i]))
    }
}

/// An immutable routing graph: forward and backward CSR plus optional vertex coordinates.
#[derive(Debug, Clone)]
pub struct Graph {
    num_vertices: usize,
    pub(crate) fwd: Csr,
    pub(crate) bwd: Csr,
    pub(crate) coords: Option<Vec<(f64, f64)>>,
}

impl Graph {
    /// Roughly how many bytes the graph takes in memory.
    pub fn approx_bytes(&self) -> usize {
        let csr = |c: &Csr| c.offsets.len() * 4 + c.head.len() * 16;
        csr(&self.fwd) + csr(&self.bwd) + self.coords.as_ref().map_or(0, |c| c.len() * 16)
    }

    /// Builds the graph from an edge set, following pgRouting's `directed` semantics:
    /// with `directed = false` every existing `cost`/`reverse_cost` arc is usable both ways.
    pub fn build(num_vertices: usize, edges: &[EdgeRow], directed: bool) -> Self {
        Self::build_with(num_vertices, edges, directed, false)
    }

    /// Like [`Graph::build`], but negative costs make arcs too (pgr_bellmanFord and
    /// pgr_edwardMoore): only an unbound `reverse_cost` means no reverse arc.
    pub fn build_keeping_negative(num_vertices: usize, edges: &[EdgeRow], directed: bool) -> Self {
        Self::build_with(num_vertices, edges, directed, true)
    }

    fn build_with(num_vertices: usize, edges: &[EdgeRow], directed: bool, negative: bool) -> Self {
        let mut arcs = Vec::with_capacity(edges.len() * 2);
        let exists = |c: f64| c >= 0.0 || (negative && !c.is_nan());
        for e in edges {
            let rc = e.reverse_cost.unwrap_or(f64::NAN);
            if exists(e.cost) {
                arcs.push((e.source, e.target, e.cost, e.id));
                if !directed {
                    arcs.push((e.target, e.source, e.cost, e.id));
                }
            }
            if exists(rc) {
                arcs.push((e.target, e.source, rc, e.id));
                if !directed {
                    arcs.push((e.source, e.target, rc, e.id));
                }
            }
        }
        let fwd = Csr::build(num_vertices, &arcs);
        for a in &mut arcs {
            std::mem::swap(&mut a.0, &mut a.1);
        }
        let bwd = Csr::build(num_vertices, &arcs);
        Self {
            num_vertices,
            fwd,
            bwd,
            coords: None,
        }
    }

    /// The same graph without the arcs of the edges `disabled` marks (indexed by edge id; ids
    /// past its end stay). The vertices and their coordinates do not change, so a contraction
    /// hierarchy of this graph can be customized for the result.
    pub fn without_edges(&self, disabled: &[bool]) -> Self {
        let keep = |e: u32| !disabled.get(e as usize).copied().unwrap_or(false);
        let filter = |csr: &Csr| {
            let mut out = Csr {
                offsets: Vec::with_capacity(csr.offsets.len()),
                head: Vec::with_capacity(csr.head.len()),
                weight: Vec::with_capacity(csr.weight.len()),
                edge: Vec::with_capacity(csr.edge.len()),
            };
            out.offsets.push(0);
            for v in 0..self.num_vertices {
                let (a, b) = (csr.offsets[v] as usize, csr.offsets[v + 1] as usize);
                for i in a..b {
                    if keep(csr.edge[i]) {
                        out.head.push(csr.head[i]);
                        out.weight.push(csr.weight[i]);
                        out.edge.push(csr.edge[i]);
                    }
                }
                out.offsets.push(out.head.len() as u32);
            }
            out
        };
        Self {
            num_vertices: self.num_vertices,
            fwd: filter(&self.fwd),
            bwd: filter(&self.bwd),
            coords: self.coords.clone(),
        }
    }

    /// Attaches vertex coordinates (used by A*). `coords.len()` must equal the vertex count.
    pub fn with_coords(mut self, coords: Vec<(f64, f64)>) -> Self {
        assert_eq!(coords.len(), self.num_vertices);
        self.coords = Some(coords);
        self
    }

    pub fn num_vertices(&self) -> usize {
        self.num_vertices
    }

    pub fn num_arcs(&self) -> usize {
        self.fwd.head.len()
    }

    pub fn has_coords(&self) -> bool {
        self.coords.is_some()
    }

    /// The vertex coordinates, when every vertex has some.
    pub fn coords(&self) -> Option<&[(f64, f64)]> {
        self.coords.as_deref()
    }

    /// The arcs leaving `v`: `(head, cost, edge)`.
    pub fn arcs(&self, v: u32) -> impl Iterator<Item = (u32, f64, u32)> + '_ {
        self.fwd.arcs(v)
    }

    /// The arcs entering `v`: `(tail, cost, edge)`.
    pub fn arcs_in(&self, v: u32) -> impl Iterator<Item = (u32, f64, u32)> + '_ {
        self.bwd.arcs(v)
    }

    #[inline]
    pub(crate) fn out_degree(&self, v: u32) -> usize {
        (self.fwd.offsets[v as usize + 1] - self.fwd.offsets[v as usize]) as usize
    }

    #[inline]
    pub(crate) fn in_degree(&self, v: u32) -> usize {
        (self.bwd.offsets[v as usize + 1] - self.bwd.offsets[v as usize]) as usize
    }
}

/// The undirected simple view of a graph: every edge id once between its two ends (at its
/// cheapest arc), loops left out. Neighbours are listed by edge id.
#[derive(Debug, Clone)]
pub(crate) struct Undirected {
    offsets: Vec<u32>,
    /// `(neighbour, edge, cost)`.
    adj: Vec<(u32, u32, f64)>,
    /// `(a, b, edge, cost)` with `a < b`, by edge id.
    pub(crate) edges: Vec<(u32, u32, u32, f64)>,
}

impl Undirected {
    pub(crate) fn new(graph: &Graph) -> Self {
        let n = graph.num_vertices();
        let mut edges: Vec<(u32, u32, u32, f64)> = Vec::with_capacity(graph.num_arcs());
        for v in 0..n as u32 {
            for (w, c, e) in graph.fwd.arcs(v) {
                if v != w {
                    edges.push((v.min(w), v.max(w), e, c));
                }
            }
        }
        edges.sort_by(|x, y| {
            (x.2, x.0, x.1)
                .cmp(&(y.2, y.0, y.1))
                .then(x.3.total_cmp(&y.3))
        });
        edges.dedup_by(|later, first| (later.0, later.1, later.2) == (first.0, first.1, first.2));
        let mut offsets = vec![0u32; n + 1];
        for &(a, b, _, _) in &edges {
            offsets[a as usize + 1] += 1;
            offsets[b as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let mut cursor = offsets.clone();
        let mut adj = vec![(0, 0, 0.0); edges.len() * 2];
        for &(a, b, e, c) in &edges {
            adj[cursor[a as usize] as usize] = (b, e, c);
            cursor[a as usize] += 1;
            adj[cursor[b as usize] as usize] = (a, e, c);
            cursor[b as usize] += 1;
        }
        Self {
            offsets,
            adj,
            edges,
        }
    }

    pub(crate) fn num_vertices(&self) -> usize {
        self.offsets.len() - 1
    }

    #[inline]
    pub(crate) fn neighbours(&self, v: u32) -> &[(u32, u32, f64)] {
        &self.adj[self.offsets[v as usize] as usize..self.offsets[v as usize + 1] as usize]
    }

    pub(crate) fn degree(&self, v: u32) -> usize {
        (self.offsets[v as usize + 1] - self.offsets[v as usize]) as usize
    }
}

/// Interns arbitrary vertex and edge keys (e.g. RDF terms) into dense ids while collecting rows.
#[derive(Debug)]
pub struct GraphBuilder<K> {
    vertex_ids: FxMap<K, u32>,
    vertices: Vec<K>,
    edge_ids: FxMap<K, u32>,
    edges: Vec<K>,
    rows: Vec<EdgeRow>,
    coords: Vec<Option<(f64, f64)>>,
}

impl<K: Hash + Eq + Clone> Default for GraphBuilder<K> {
    fn default() -> Self {
        Self {
            vertex_ids: FxMap::default(),
            vertices: Vec::new(),
            edge_ids: FxMap::default(),
            edges: Vec::new(),
            rows: Vec::new(),
            coords: Vec::new(),
        }
    }
}

impl<K: Hash + Eq + Clone> GraphBuilder<K> {
    pub fn with_capacity(edges: usize) -> Self {
        Self {
            vertex_ids: FxMap::with_capacity_and_hasher(edges, Default::default()),
            vertices: Vec::with_capacity(edges),
            edge_ids: FxMap::with_capacity_and_hasher(edges, Default::default()),
            edges: Vec::with_capacity(edges),
            rows: Vec::with_capacity(edges),
            coords: Vec::with_capacity(edges),
        }
    }

    pub fn vertex(&mut self, key: &K) -> u32 {
        if let Some(id) = self.vertex_ids.get(key) {
            return *id;
        }
        let id = u32::try_from(self.vertices.len()).expect("more than u32::MAX vertices");
        self.vertex_ids.insert(key.clone(), id);
        self.vertices.push(key.clone());
        self.coords.push(None);
        id
    }

    fn edge(&mut self, key: K) -> u32 {
        if let Some(id) = self.edge_ids.get(&key) {
            return *id;
        }
        let id = u32::try_from(self.edges.len()).expect("more than u32::MAX edges");
        self.edge_ids.insert(key.clone(), id);
        self.edges.push(key);
        id
    }

    #[allow(clippy::too_many_arguments)] // mirrors the pgRouting edge columns
    /// Adds one edge-set row. Optional `(x1, y1)`/`(x2, y2)` are the source/target coordinates.
    pub fn add(
        &mut self,
        id: K,
        source: &K,
        target: &K,
        cost: f64,
        reverse_cost: Option<f64>,
        source_xy: Option<(f64, f64)>,
        target_xy: Option<(f64, f64)>,
    ) {
        let id = self.edge(id);
        let source = self.vertex(source);
        let target = self.vertex(target);
        if let Some(xy) = source_xy {
            self.coords[source as usize] = Some(xy);
        }
        if let Some(xy) = target_xy {
            self.coords[target as usize] = Some(xy);
        }
        self.rows.push(EdgeRow {
            id,
            source,
            target,
            cost,
            reverse_cost,
        });
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Finishes the graph. Coordinates are attached only if every vertex has some.
    pub fn build(self, directed: bool) -> InternedGraph<K> {
        let mut graph = Graph::build(self.vertices.len(), &self.rows, directed);
        if !self.coords.is_empty() && self.coords.iter().all(Option::is_some) {
            graph = graph.with_coords(self.coords.into_iter().map(Option::unwrap).collect());
        }
        InternedGraph {
            graph,
            vertex_ids: self.vertex_ids,
            vertices: self.vertices,
            edges: self.edges,
            rows: self.rows,
        }
    }
}

/// A graph together with the mapping between dense ids and the caller's keys.
#[derive(Debug)]
pub struct InternedGraph<K> {
    pub graph: Graph,
    pub vertex_ids: FxMap<K, u32>,
    pub vertices: Vec<K>,
    pub edges: Vec<K>,
    /// The edge-set rows the graph was built from.
    pub rows: Vec<EdgeRow>,
}

impl<K: Hash + Eq> InternedGraph<K> {
    pub fn vertex_id(&self, key: &K) -> Option<u32> {
        self.vertex_ids.get(key).copied()
    }
}

impl<K: Hash + Eq + Clone> InternedGraph<K> {
    /// A copy of the graph with extra vertices placed along edges (pgRouting's `points_sql`):
    /// `(key, edge, fraction)` puts a vertex at `fraction` of the way from the edge's source
    /// to its target. The edge is split there; each part keeps the edge's id and a share of
    /// its costs in proportion to its length fraction. Points on unknown edges, with a
    /// fraction outside 0..=1, or whose key is already a vertex are ignored.
    pub fn with_points(&self, points: &[(K, u32, f64)], directed: bool) -> Self {
        let mut vertex_ids = self.vertex_ids.clone();
        let mut vertices = self.vertices.clone();
        let mut coords = self.graph.coords.clone();
        // Points per edge id, by fraction.
        let mut on_edge: FxMap<u32, Vec<(f64, u32)>> = FxMap::default();
        let edge_rows: FxMap<u32, EdgeRow> = self.rows.iter().map(|r| (r.id, *r)).collect();
        for (key, edge, fraction) in points {
            let Some(row) = edge_rows.get(edge) else {
                continue;
            };
            if !(0.0..=1.0).contains(fraction) || vertex_ids.contains_key(key) {
                continue;
            }
            let id = u32::try_from(vertices.len()).expect("more than u32::MAX vertices");
            vertex_ids.insert(key.clone(), id);
            vertices.push(key.clone());
            if let Some(c) = &mut coords {
                let (a, b) = (c[row.source as usize], c[row.target as usize]);
                c.push((a.0 + (b.0 - a.0) * fraction, a.1 + (b.1 - a.1) * fraction));
            }
            on_edge.entry(*edge).or_default().push((*fraction, id));
        }
        let mut rows = Vec::with_capacity(self.rows.len() + points.len());
        for r in &self.rows {
            let Some(stops) = on_edge.get_mut(&r.id) else {
                rows.push(*r);
                continue;
            };
            stops.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut from = (0.0, r.source);
            for &(f, v) in stops.iter().chain(std::iter::once(&(1.0, r.target))) {
                let share = f - from.0;
                rows.push(EdgeRow {
                    id: r.id,
                    source: from.1,
                    target: v,
                    // A negative cost marks a missing direction and stays negative.
                    cost: if r.cost < 0.0 { r.cost } else { r.cost * share },
                    reverse_cost: r.reverse_cost.map(|c| if c < 0.0 { c } else { c * share }),
                });
                from = (f, v);
            }
        }
        let mut graph = Graph::build(vertices.len(), &rows, directed);
        if let Some(c) = coords {
            graph = graph.with_coords(c);
        }
        Self {
            graph,
            vertex_ids,
            vertices,
            edges: self.edges.clone(),
            rows,
        }
    }
}
