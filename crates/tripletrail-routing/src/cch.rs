//! Customizable contraction hierarchies (Dibbelt, Strasser and Wagner): the preprocessing is
//! split into a metric-independent part, which orders the vertices and adds the shortcuts
//! any metric could need, and a customization that computes the weights of all arcs for one
//! metric. When only costs change (a closed track, a speed restriction, another cost column),
//! a new customization takes a fraction of the time a contraction hierarchy needs to be built
//! again, and the queries stay as fast.
//!
//! The order comes from minimum-degree elimination, which suits sparse, tree-like networks
//! such as railways. Queries walk the elimination tree upwards from both ends, without a
//! priority queue. Results are the same as Dijkstra's (paths of equal cost may differ).

use crate::graph::Graph;
use crate::search::{NONE, Path, PathStep};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// The metric-independent part plus one customization.
#[derive(Debug, Clone)]
pub struct Cch {
    /// Vertex id -> rank (position in the elimination order).
    rank: Vec<u32>,
    /// Rank -> vertex id.
    order: Vec<u32>,
    /// Arcs of rank `r` are `first[r]..first[r + 1]`, each to a higher rank (`head`), sorted.
    first: Vec<u32>,
    head: Vec<u32>,
    /// Lower end of each arc.
    tail: Vec<u32>,
    /// Elimination tree: the lowest-ranked upper neighbour, or `NONE`.
    parent: Vec<u32>,
    metric: Metric,
}

/// Weights of every arc in both directions, with what each stands for.
#[derive(Debug, Clone, Default)]
struct Metric {
    /// `tail -> head`.
    up: Vec<f64>,
    /// `head -> tail`.
    down: Vec<f64>,
    /// Middle rank of a shortcut, or `NONE` for an original arc.
    up_via: Vec<u32>,
    down_via: Vec<u32>,
    /// Edge id of an original arc.
    up_edge: Vec<u32>,
    down_edge: Vec<u32>,
}

/// The upper neighbours of each vertex after minimum-degree elimination, and the order.
fn eliminate(graph: &Graph) -> (Vec<u32>, Vec<Vec<u32>>) {
    let n = graph.num_vertices();
    let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
    for v in 0..n as u32 {
        for (w, _, _) in graph.fwd.arcs(v) {
            if v != w {
                adj[v as usize].push(w);
                adj[w as usize].push(v);
            }
        }
    }
    for list in &mut adj {
        list.sort_unstable();
        list.dedup();
    }
    let mut queue: BinaryHeap<Reverse<(usize, u32)>> = (0..n as u32)
        .map(|v| Reverse((adj[v as usize].len(), v)))
        .collect();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut upper: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut merged = Vec::new();
    while let Some(Reverse((deg, v))) = queue.pop() {
        let vi = v as usize;
        if done[vi] || deg != adj[vi].len() {
            continue;
        }
        done[vi] = true;
        order.push(v);
        let nbrs = std::mem::take(&mut adj[vi]);
        // Make the neighbours a clique (the fill-in), without v.
        for &x in &nbrs {
            let xi = x as usize;
            merged.clear();
            let (a, b) = (&adj[xi], &nbrs);
            let (mut i, mut j) = (0, 0);
            while i < a.len() || j < b.len() {
                let next = match (a.get(i), b.get(j)) {
                    (Some(&p), Some(&q)) if p == q => {
                        i += 1;
                        j += 1;
                        p
                    }
                    (Some(&p), Some(&q)) if p < q => {
                        i += 1;
                        p
                    }
                    (Some(_), Some(&q)) | (None, Some(&q)) => {
                        j += 1;
                        q
                    }
                    (Some(&p), None) => {
                        i += 1;
                        p
                    }
                    (None, None) => unreachable!(),
                };
                if next != v && next != x {
                    merged.push(next);
                }
            }
            std::mem::swap(&mut adj[xi], &mut merged);
            queue.push(Reverse((adj[xi].len(), x)));
        }
        upper[vi] = nbrs;
    }
    (order, upper)
}

impl Cch {
    /// Orders the vertices, adds the shortcuts and customizes for the graph's weights.
    pub fn build(graph: &Graph) -> Self {
        let n = graph.num_vertices();
        let (order, upper) = eliminate(graph);
        let mut rank = vec![0u32; n];
        for (r, &v) in order.iter().enumerate() {
            rank[v as usize] = r as u32;
        }
        let mut first = Vec::with_capacity(n + 1);
        let mut head = Vec::new();
        let mut tail = Vec::new();
        let mut parent = vec![NONE; n];
        first.push(0);
        for (r, &v) in order.iter().enumerate() {
            let mut ups: Vec<u32> = upper[v as usize]
                .iter()
                .map(|&x| rank[x as usize])
                .collect();
            ups.sort_unstable();
            parent[r] = ups.first().copied().unwrap_or(NONE);
            tail.extend(std::iter::repeat_n(r as u32, ups.len()));
            head.extend(ups);
            first.push(head.len() as u32);
        }
        let mut cch = Self {
            rank,
            order,
            first,
            head,
            tail,
            parent,
            metric: Metric::default(),
        };
        let ok = cch.customize(graph);
        debug_assert!(ok, "the graph's own arcs are in its topology");
        cch
    }

    pub fn num_vertices(&self) -> usize {
        self.order.len()
    }

    /// Arcs of the hierarchy, shortcuts included.
    pub fn num_arcs(&self) -> usize {
        self.head.len()
    }

    /// The arc between ranks `lo < hi`.
    fn arc(&self, lo: u32, hi: u32) -> Option<usize> {
        let (a, b) = (
            self.first[lo as usize] as usize,
            self.first[lo as usize + 1] as usize,
        );
        self.head[a..b].binary_search(&hi).ok().map(|i| a + i)
    }

    /// Computes the weights for `graph`, which must have the vertices this hierarchy was built
    /// for. Returns `false`, leaving the hierarchy unusable, if the graph has an arc between
    /// vertices the topology does not connect; build a new hierarchy then.
    pub fn customize(&mut self, graph: &Graph) -> bool {
        let m = self.head.len();
        let mut metric = Metric {
            up: vec![f64::INFINITY; m],
            down: vec![f64::INFINITY; m],
            up_via: vec![NONE; m],
            down_via: vec![NONE; m],
            up_edge: vec![NONE; m],
            down_edge: vec![NONE; m],
        };
        if graph.num_vertices() != self.order.len() {
            self.metric = metric;
            return false;
        }
        for v in 0..graph.num_vertices() as u32 {
            for (w, weight, edge) in graph.fwd.arcs(v) {
                if v == w {
                    continue;
                }
                let (rv, rw) = (self.rank[v as usize], self.rank[w as usize]);
                let Some(i) = self.arc(rv.min(rw), rv.max(rw)) else {
                    self.metric = metric;
                    return false;
                };
                let (w_slot, e_slot) = if rv < rw {
                    (&mut metric.up[i], &mut metric.up_edge[i])
                } else {
                    (&mut metric.down[i], &mut metric.down_edge[i])
                };
                if weight < *w_slot {
                    *w_slot = weight;
                    *e_slot = edge;
                }
            }
        }
        // Lower triangles, lowest rank first: x -> r -> y improves x -> y.
        for r in 0..self.order.len() {
            let (a, b) = (self.first[r] as usize, self.first[r + 1] as usize);
            for i in a..b {
                let x = self.head[i];
                for j in i + 1..b {
                    let y = self.head[j];
                    let k = self.arc(x, y).expect("upper neighbours form a clique");
                    let via = metric.down[i] + metric.up[j];
                    if via < metric.up[k] {
                        metric.up[k] = via;
                        metric.up_via[k] = r as u32;
                    }
                    let via = metric.down[j] + metric.up[i];
                    if via < metric.down[k] {
                        metric.down[k] = via;
                        metric.down_via[k] = r as u32;
                    }
                }
            }
        }
        self.metric = metric;
        true
    }

    /// Expands an arc into original `(tail vertex, edge, cost)` steps.
    fn unpack(&self, arc: usize, up: bool, steps: &mut Vec<(u32, u32, f64)>) {
        let mut stack = vec![(arc, up)];
        while let Some((i, up)) = stack.pop() {
            let (lo, hi) = (self.tail[i], self.head[i]);
            let m = &self.metric;
            let via = if up { m.up_via[i] } else { m.down_via[i] };
            if via == NONE {
                let (from, edge, w) = if up {
                    (lo, m.up_edge[i], m.up[i])
                } else {
                    (hi, m.down_edge[i], m.down[i])
                };
                steps.push((self.order[from as usize], edge, w));
                continue;
            }
            let to_lo = self.arc(via, lo).expect("shortcut arc");
            let to_hi = self.arc(via, hi).expect("shortcut arc");
            if up {
                // lo -> via (down on (via, lo)), then via -> hi (up on (via, hi)).
                stack.push((to_hi, true));
                stack.push((to_lo, false));
            } else {
                // hi -> via (down on (via, hi)), then via -> lo (up on (via, lo)).
                stack.push((to_lo, true));
                stack.push((to_hi, false));
            }
        }
    }
}

/// Reusable state for [`cch_dijkstra`].
#[derive(Debug, Default)]
pub struct CchSpace {
    fwd: Chain,
    bwd: Chain,
}

impl CchSpace {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Default)]
struct Chain {
    dist: Vec<f64>,
    /// Arc by which a rank was reached.
    pred: Vec<u32>,
    /// Ranks on the elimination-tree path, from the start upwards.
    path: Vec<u32>,
}

impl Chain {
    fn walk(&mut self, cch: &Cch, start: u32, up: bool) {
        let n = cch.order.len();
        if self.dist.len() != n {
            self.dist = vec![f64::INFINITY; n];
            self.pred = vec![NONE; n];
            self.path.clear();
        }
        for &r in &self.path {
            self.dist[r as usize] = f64::INFINITY;
            self.pred[r as usize] = NONE;
        }
        self.path.clear();
        self.dist[start as usize] = 0.0;
        let mut r = start;
        let weights = if up { &cch.metric.up } else { &cch.metric.down };
        while r != NONE {
            self.path.push(r);
            let d = self.dist[r as usize];
            if d.is_finite() {
                let arcs = cch.first[r as usize] as usize..cch.first[r as usize + 1] as usize;
                for (i, (&x, &w)) in arcs
                    .clone()
                    .zip(cch.head[arcs.clone()].iter().zip(&weights[arcs]))
                {
                    let nd = d + w;
                    let x = x as usize;
                    if nd < self.dist[x] {
                        self.dist[x] = nd;
                        self.pred[x] = i as u32;
                    }
                }
            }
            r = cch.parent[r as usize];
        }
    }
}

/// One-to-many shortest paths on a customized hierarchy (same costs as [`crate::dijkstra`]).
pub fn cch_dijkstra(
    cch: &Cch,
    space: &mut CchSpace,
    source: u32,
    targets: &[u32],
) -> Vec<Option<Path>> {
    let CchSpace { fwd, bwd } = space;
    let s = cch.rank[source as usize];
    fwd.walk(cch, s, true);
    targets
        .iter()
        .map(|&target| {
            bwd.walk(cch, cch.rank[target as usize], false);
            let (meet, cost) = bwd
                .path
                .iter()
                .map(|&r| (r, fwd.dist[r as usize] + bwd.dist[r as usize]))
                .min_by(|a, b| a.1.total_cmp(&b.1))?;
            if cost.is_infinite() {
                return None;
            }
            let mut arcs = Vec::new();
            let mut r = meet;
            while fwd.pred[r as usize] != NONE {
                let i = fwd.pred[r as usize] as usize;
                arcs.push((i, true));
                r = cch.tail[i];
            }
            arcs.reverse();
            let mut r = meet;
            while bwd.pred[r as usize] != NONE {
                let i = bwd.pred[r as usize] as usize;
                arcs.push((i, false));
                r = cch.tail[i];
            }
            let mut raw = Vec::new();
            for (i, up) in arcs {
                cch.unpack(i, up, &mut raw);
            }
            let mut steps = Vec::with_capacity(raw.len() + 1);
            let mut agg = 0.0;
            for (node, edge, c) in raw {
                steps.push(PathStep {
                    node,
                    edge: Some(edge),
                    cost: c,
                    agg_cost: agg,
                });
                agg += c;
            }
            steps.push(PathStep {
                node: target,
                edge: None,
                cost: 0.0,
                agg_cost: agg,
            });
            Some(Path {
                start_vid: source,
                end_vid: target,
                steps,
            })
        })
        .collect()
}

impl Cch {
    /// Writes the metric-independent part; the weights are customized again on reading.
    pub(crate) fn write(&self, w: &mut crate::persist::Writer) {
        w.u64(self.order.len() as u64);
        for &v in &self.order {
            w.u32(v);
        }
        w.u64(self.head.len() as u64);
        for &f in &self.first[1..] {
            w.u32(f);
        }
        for &h in &self.head {
            w.u32(h);
        }
    }

    /// Reads what [`Cch::write`] wrote and customizes it for `graph`.
    pub(crate) fn read(r: &mut crate::persist::Reader, graph: &Graph) -> Option<Self> {
        let n = r.len(4)?;
        (n == graph.num_vertices()).then_some(())?;
        let order = (0..n).map(|_| r.u32()).collect::<Option<Vec<_>>>()?;
        let mut rank = vec![NONE; n];
        for (i, &v) in order.iter().enumerate() {
            let slot = rank.get_mut(v as usize)?;
            (*slot == NONE).then_some(())?;
            *slot = i as u32;
        }
        let m = r.len(4)?;
        let mut first = Vec::with_capacity(n + 1);
        first.push(0u32);
        for _ in 0..n {
            let f = r.u32()?;
            (f >= *first.last()? && f as usize <= m).then_some(())?;
            first.push(f);
        }
        (*first.last()? as usize == m).then_some(())?;
        let head = (0..m).map(|_| r.u32()).collect::<Option<Vec<_>>>()?;
        let mut tail = Vec::with_capacity(m);
        let mut parent = vec![NONE; n];
        for lo in 0..n {
            let ups = &head[first[lo] as usize..first[lo + 1] as usize];
            // Strictly increasing and above `lo`.
            let ok = ups.iter().all(|&h| (h as usize) > lo && (h as usize) < n)
                && ups.windows(2).all(|w| w[0] < w[1]);
            ok.then_some(())?;
            parent[lo] = ups.first().copied().unwrap_or(NONE);
            tail.extend(std::iter::repeat_n(lo as u32, ups.len()));
        }
        let mut cch = Self {
            rank,
            order,
            first,
            head,
            tail,
            parent,
            metric: Metric::default(),
        };
        // A topology that is not chordal would make customization panic; check the cliques.
        for lo in 0..n {
            let ups = &cch.head[cch.first[lo] as usize..cch.first[lo + 1] as usize];
            for (i, &x) in ups.iter().enumerate() {
                for &y in &ups[i + 1..] {
                    cch.arc(x, y)?;
                }
            }
        }
        cch.customize(graph).then_some(cch)
    }
}

impl Cch {
    /// The same hierarchy for a graph whose vertex `v` is this one's `old_id[v]` (the same
    /// vertices numbered differently), to be customized for that graph. `None` if `old_id` is
    /// not a renumbering of this hierarchy's vertices.
    pub fn relabel(&self, old_id: &[u32]) -> Option<Self> {
        let n = self.order.len();
        (old_id.len() == n).then_some(())?;
        let mut rank = vec![NONE; n];
        let mut order = vec![NONE; n];
        for (new, &old) in old_id.iter().enumerate() {
            let r = *self.rank.get(old as usize)?;
            (order[r as usize] == NONE).then_some(())?;
            order[r as usize] = new as u32;
            rank[new] = r;
        }
        Some(Self {
            rank,
            order,
            ..self.clone()
        })
    }
}
