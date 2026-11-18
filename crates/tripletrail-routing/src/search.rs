use crate::graph::{Csr, Graph};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

pub(crate) const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Item {
    pub(crate) key: f64,
    pub(crate) node: u32,
}

impl Eq for Item {}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap on key.
        other
            .key
            .total_cmp(&self.key)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Reusable per-thread search state; only touched entries are reset between searches.
#[derive(Debug, Default)]
pub struct SearchSpace {
    pub(crate) dist: Vec<f64>,
    pred_node: Vec<u32>,
    pred_edge: Vec<u32>,
    pred_cost: Vec<f64>,
    pub(crate) settled: Vec<bool>,
    touched: Vec<u32>,
    pub(crate) heap: BinaryHeap<Item>,
}

impl std::fmt::Debug for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {})", self.node, self.key)
    }
}

impl SearchSpace {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self, n: usize) {
        if self.dist.len() != n {
            self.dist = vec![f64::INFINITY; n];
            self.pred_node = vec![NONE; n];
            self.pred_edge = vec![NONE; n];
            self.pred_cost = vec![0.0; n];
            self.settled = vec![false; n];
            self.touched.clear();
        } else {
            for &v in &self.touched {
                let v = v as usize;
                self.dist[v] = f64::INFINITY;
                self.pred_node[v] = NONE;
                self.pred_edge[v] = NONE;
                self.settled[v] = false;
            }
            self.touched.clear();
        }
        self.heap.clear();
    }

    #[inline]
    pub(crate) fn relax(&mut self, v: u32, d: f64, key: f64, from: u32, edge: u32, w: f64) -> bool {
        let vi = v as usize;
        if d < self.dist[vi] {
            if self.dist[vi].is_infinite() && self.pred_node[vi] == NONE {
                self.touched.push(v);
            }
            self.dist[vi] = d;
            self.pred_node[vi] = from;
            self.pred_edge[vi] = edge;
            self.pred_cost[vi] = w;
            self.heap.push(Item { key, node: v });
            true
        } else {
            false
        }
    }

    pub(crate) fn start(&mut self, s: u32, key: f64) {
        let si = s as usize;
        if self.dist[si].is_infinite() {
            self.touched.push(s);
        }
        self.dist[si] = 0.0;
        self.heap.push(Item { key, node: s });
    }

    /// Path to `t`, a reached vertex, from the root it was reached from (for searches started
    /// from several vertices).
    pub(crate) fn path_from_root(&self, t: u32) -> Path {
        let mut root = t;
        while self.pred_node[root as usize] != NONE {
            root = self.pred_node[root as usize];
        }
        self.path_to(root, t).expect("t was reached")
    }

    /// Path from the search root to `t`, following predecessors (forward search).
    pub(crate) fn path_to(&self, start: u32, t: u32) -> Option<Path> {
        if self.dist[t as usize].is_infinite() {
            return None;
        }
        let mut rev = Vec::new();
        let mut v = t;
        while v != start {
            let p = self.pred_node[v as usize];
            rev.push((p, self.pred_edge[v as usize], self.pred_cost[v as usize]));
            v = p;
        }
        rev.reverse();
        let mut steps = Vec::with_capacity(rev.len() + 1);
        let mut agg = 0.0;
        for (node, edge, cost) in rev {
            steps.push(PathStep {
                node,
                edge: Some(edge),
                cost,
                agg_cost: agg,
            });
            agg += cost;
        }
        steps.push(PathStep {
            node: t,
            edge: None,
            cost: 0.0,
            agg_cost: agg,
        });
        Some(Path {
            start_vid: start,
            end_vid: t,
            steps,
        })
    }
}

/// One row of a pgRouting path result (`node, edge, cost, agg_cost`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathStep {
    pub node: u32,
    /// Edge taken from `node`; `None` on the last row (pgRouting's `-1`).
    pub edge: Option<u32>,
    pub cost: f64,
    pub agg_cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub start_vid: u32,
    pub end_vid: u32,
    pub steps: Vec<PathStep>,
}

impl Path {
    pub fn total_cost(&self) -> f64 {
        self.steps.last().map_or(0.0, |s| s.agg_cost)
    }
}

/// One-to-many Dijkstra (pgr_dijkstra). Stops once every target is settled.
/// Returns one entry per target, `None` when unreachable.
pub fn dijkstra(
    graph: &Graph,
    space: &mut SearchSpace,
    source: u32,
    targets: &[u32],
) -> Vec<Option<Path>> {
    space.reset(graph.num_vertices());
    space.start(source, 0.0);
    let mut remaining = targets.len();
    let mut is_target = vec![
        false;
        if targets.len() > 1 {
            graph.num_vertices()
        } else {
            0
        }
    ];
    for &t in targets {
        if targets.len() > 1 {
            is_target[t as usize] = true;
        }
    }
    while let Some(Item { key: d, node: u }) = space.heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        if space.settled[u as usize] || d > space.dist[u as usize] {
            continue;
        }
        space.settled[u as usize] = true;
        let hit = if targets.len() == 1 {
            u == targets[0]
        } else {
            !targets.is_empty() && is_target[u as usize]
        };
        if hit {
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
        for (v, w, e) in graph.fwd.arcs(u) {
            let nd = d + w;
            space.relax(v, nd, nd, u, e, w);
        }
    }
    targets.iter().map(|&t| space.path_to(source, t)).collect()
}

/// The cheapest path from any of `sources` to any of `targets` using only arcs whose edge
/// `allow` accepts (a corridor). `allow` is called once per arc scanned, so it should be a
/// lookup, such as a bitset.
pub fn dijkstra_between(
    graph: &Graph,
    space: &mut SearchSpace,
    sources: &[u32],
    targets: &[u32],
    allow: impl Fn(u32) -> bool,
) -> Option<Path> {
    space.reset(graph.num_vertices());
    for &s in sources {
        if space.dist[s as usize] != 0.0 {
            space.start(s, 0.0);
        }
    }
    let mut found = NONE;
    while let Some(Item { key: d, node: u }) = space.heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        if space.settled[u as usize] || d > space.dist[u as usize] {
            continue;
        }
        space.settled[u as usize] = true;
        if targets.contains(&u) {
            found = u;
            break;
        }
        for (v, w, e) in graph.fwd.arcs(u) {
            if allow(e) {
                let nd = d + w;
                space.relax(v, nd, nd, u, e, w);
            }
        }
    }
    (found != NONE).then(|| space.path_from_root(found))
}

/// One-to-one bidirectional Dijkstra (pgr_bdDijkstra).
pub fn bd_dijkstra(
    graph: &Graph,
    fwd: &mut SearchSpace,
    bwd: &mut SearchSpace,
    source: u32,
    target: u32,
) -> Option<Path> {
    let n = graph.num_vertices();
    fwd.reset(n);
    bwd.reset(n);
    fwd.start(source, 0.0);
    bwd.start(target, 0.0);
    let mut best = if source == target { 0.0 } else { f64::INFINITY };
    let mut meet = if source == target { source } else { NONE };

    fn step(
        csr: &Csr,
        me: &mut SearchSpace,
        other: &SearchSpace,
        best: &mut f64,
        meet: &mut u32,
    ) -> bool {
        while let Some(Item { key: d, node: u }) = me.heap.pop() {
            if crate::cancel::stopped() {
                break;
            }
            if me.settled[u as usize] || d > me.dist[u as usize] {
                continue;
            }
            me.settled[u as usize] = true;
            for (v, w, e) in csr.arcs(u) {
                let nd = d + w;
                me.relax(v, nd, nd, u, e, w);
                let total = me.dist[v as usize] + other.dist[v as usize];
                if total < *best {
                    *best = total;
                    *meet = v;
                }
            }
            return true;
        }
        false
    }

    loop {
        let top_f = fwd.heap.peek().map_or(f64::INFINITY, |i| i.key);
        let top_b = bwd.heap.peek().map_or(f64::INFINITY, |i| i.key);
        if top_f + top_b >= best || (top_f.is_infinite() && top_b.is_infinite()) {
            break;
        }
        if top_f <= top_b {
            if !step(&graph.fwd, fwd, bwd, &mut best, &mut meet) {
                break;
            }
        } else if !step(&graph.bwd, bwd, fwd, &mut best, &mut meet) {
            break;
        }
    }
    if meet == NONE {
        return None;
    }
    // Forward half from the forward tree, then walk the backward tree to the target.
    let mut path = fwd.path_to(source, meet)?;
    path.steps.pop();
    let mut agg = path.steps.last().map_or(0.0, |s| s.agg_cost + s.cost);
    let mut v = meet;
    while v != target {
        let next = bwd.pred_node[v as usize];
        let edge = bwd.pred_edge[v as usize];
        let cost = bwd.pred_cost[v as usize];
        path.steps.push(PathStep {
            node: v,
            edge: Some(edge),
            cost,
            agg_cost: agg,
        });
        agg += cost;
        v = next;
    }
    path.steps.push(PathStep {
        node: target,
        edge: None,
        cost: 0.0,
        agg_cost: agg,
    });
    path.end_vid = target;
    Some(path)
}

/// A* heuristics. Codes 0-5 match pgRouting; `Geodesic` is an extension for lon/lat
/// coordinates with costs in metres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heuristic {
    Zero,
    Max,
    Min,
    SquaredEuclidean,
    Euclidean,
    Manhattan,
    Geodesic,
}

impl Heuristic {
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Zero,
            1 => Self::Max,
            2 => Self::Min,
            3 => Self::SquaredEuclidean,
            4 => Self::Euclidean,
            5 => Self::Manhattan,
            6 => Self::Geodesic,
            _ => return None,
        })
    }

    #[inline]
    fn eval(self, a: (f64, f64), b: (f64, f64)) -> f64 {
        let dx = (a.0 - b.0).abs();
        let dy = (a.1 - b.1).abs();
        match self {
            Self::Zero => 0.0,
            Self::Max => dx.max(dy),
            Self::Min => dx.min(dy),
            Self::SquaredEuclidean => dx * dx + dy * dy,
            Self::Euclidean => (dx * dx + dy * dy).sqrt(),
            Self::Manhattan => dx + dy,
            Self::Geodesic => haversine_m(a, b),
        }
    }
}

fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    // Slightly smaller than the mean earth radius keeps the heuristic admissible.
    const R: f64 = 6_356_752.0;
    let (lat1, lat2) = (a.1.to_radians(), b.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.0 - a.0).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * h.sqrt().asin()
}

/// One-to-one A* (pgr_aStar). Requires vertex coordinates; returns `None` without them.
pub fn astar(
    graph: &Graph,
    space: &mut SearchSpace,
    source: u32,
    target: u32,
    heuristic: Heuristic,
    factor: f64,
) -> Option<Path> {
    let coords = graph.coords.as_ref()?;
    let goal = coords[target as usize];
    let h = |v: u32| heuristic.eval(coords[v as usize], goal) * factor;
    space.reset(graph.num_vertices());
    space.start(source, h(source));
    while let Some(Item { key, node: u }) = space.heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        let d = space.dist[u as usize];
        if key > d + h(u) + 1e-9 {
            continue;
        }
        if u == target {
            break;
        }
        for (v, w, e) in graph.fwd.arcs(u) {
            let nd = d + w;
            space.relax(v, nd, nd + h(v), u, e, w);
        }
    }
    space.path_to(source, target)
}

/// One row of pgr_drivingDistance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReachedNode {
    pub start_vid: u32,
    pub node: u32,
    /// Predecessor vertex (equals `node` for the start vertex, as in pgRouting).
    pub pred: u32,
    pub edge: Option<u32>,
    pub cost: f64,
    pub agg_cost: f64,
}

/// All vertices reachable from `source` with `agg_cost <= max_cost` (pgr_drivingDistance).
pub fn driving_distance(
    graph: &Graph,
    space: &mut SearchSpace,
    source: u32,
    max_cost: f64,
) -> Vec<ReachedNode> {
    space.reset(graph.num_vertices());
    space.start(source, 0.0);
    let mut out = Vec::new();
    while let Some(Item { key: d, node: u }) = space.heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        if space.settled[u as usize] || d > space.dist[u as usize] {
            continue;
        }
        if d > max_cost {
            break;
        }
        space.settled[u as usize] = true;
        let (pred, edge, cost) = if u == source {
            (u, None, 0.0)
        } else {
            (
                space.pred_node[u as usize],
                Some(space.pred_edge[u as usize]),
                space.pred_cost[u as usize],
            )
        };
        out.push(ReachedNode {
            start_vid: source,
            node: u,
            pred,
            edge,
            cost,
            agg_cost: d,
        });
        for (v, w, e) in graph.fwd.arcs(u) {
            let nd = d + w;
            if nd <= max_cost {
                space.relax(v, nd, nd, u, e, w);
            }
        }
    }
    out
}
