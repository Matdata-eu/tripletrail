//! K shortest loopless paths (pgr_KSP), Yen's algorithm.

use crate::graph::{FxSet, Graph};
use crate::search::{Item, Path, PathStep, SearchSpace};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Up to `k` loopless paths from `s` to `t`, cheapest first.
///
/// With `heap_paths`, the candidate paths still in Yen's heap when `k` paths are found are
/// returned too (pgRouting's `heap_paths => true`), after the `k` paths and also cheapest first.
pub fn ksp(
    graph: &Graph,
    space: &mut SearchSpace,
    s: u32,
    t: u32,
    k: usize,
    heap_paths: bool,
) -> Vec<Path> {
    if k == 0 || s == t {
        return Vec::new();
    }
    let n = graph.num_vertices();
    let mut banned_node = vec![false; n];
    let mut banned_arc = FxSet::default();
    let h = distances_to(graph, t);
    let Some(first) = restricted(graph, space, &h, s, t, &banned_node, &banned_arc) else {
        return Vec::new();
    };
    let mut found = vec![first];
    let mut seen: FxSet<Vec<(u32, u32)>> = FxSet::default();
    seen.insert(signature(&found[0]));
    let mut candidates = BinaryHeap::new();
    while found.len() < k {
        let prev = found.last().unwrap().clone();
        for i in 0..prev.steps.len() - 1 {
            let spur = prev.steps[i].node;
            let root = &prev.steps[..=i];
            for p in &found {
                if p.steps.len() > i + 1 && same_root(&p.steps, root) {
                    banned_arc.insert((p.steps[i].node, p.steps[i].edge.unwrap()));
                }
            }
            for step in &root[..i] {
                banned_node[step.node as usize] = true;
            }
            if let Some(spur_path) =
                restricted(graph, space, &h, spur, t, &banned_node, &banned_arc)
            {
                let path = join(s, t, root, &spur_path);
                if seen.insert(signature(&path)) {
                    candidates.push(Candidate(path));
                }
            }
            for step in &root[..i] {
                banned_node[step.node as usize] = false;
            }
            banned_arc.clear();
        }
        match candidates.pop() {
            Some(Candidate(p)) => found.push(p),
            None => break,
        }
    }
    if heap_paths {
        let mut rest = candidates.into_sorted_vec();
        rest.reverse(); // into_sorted_vec is ascending in `Ord`, i.e. most expensive first
        found.extend(rest.into_iter().map(|c| c.0));
    }
    found
}

fn signature(p: &Path) -> Vec<(u32, u32)> {
    p.steps
        .iter()
        .map(|s| (s.node, s.edge.unwrap_or(u32::MAX)))
        .collect()
}

fn same_root(steps: &[PathStep], root: &[PathStep]) -> bool {
    let i = root.len() - 1;
    steps[..=i]
        .iter()
        .zip(root)
        .enumerate()
        .all(|(j, (a, b))| a.node == b.node && (j == i || a.edge == b.edge))
}

/// `root` (ending at the spur node, whose `edge` is ignored) followed by `spur`.
fn join(s: u32, t: u32, root: &[PathStep], spur: &Path) -> Path {
    let base = root.last().unwrap().agg_cost;
    let mut steps: Vec<PathStep> = root[..root.len() - 1].to_vec();
    steps.extend(spur.steps.iter().map(|st| PathStep {
        agg_cost: st.agg_cost + base,
        ..*st
    }));
    Path {
        start_vid: s,
        end_vid: t,
        steps,
    }
}

/// Exact distances to `t` in the unrestricted graph (one backward Dijkstra).
///
/// Banning vertices and arcs only makes paths longer, so these stay a consistent A* heuristic
/// for every spur search, which then explores little beyond the path it returns.
fn distances_to(graph: &Graph, t: u32) -> Vec<f64> {
    let mut dist = vec![f64::INFINITY; graph.num_vertices()];
    let mut heap = BinaryHeap::new();
    dist[t as usize] = 0.0;
    heap.push(Item { key: 0.0, node: t });
    while let Some(Item { key: d, node: u }) = heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        if d > dist[u as usize] {
            continue;
        }
        for (v, w, _) in graph.bwd.arcs(u) {
            let nd = d + w;
            if nd < dist[v as usize] {
                dist[v as usize] = nd;
                heap.push(Item { key: nd, node: v });
            }
        }
    }
    dist
}

/// A* towards `t` (heuristic `h`) that skips banned vertices and banned `(tail, edge)` arcs.
fn restricted(
    graph: &Graph,
    space: &mut SearchSpace,
    h: &[f64],
    s: u32,
    t: u32,
    banned_node: &[bool],
    banned_arc: &FxSet<(u32, u32)>,
) -> Option<Path> {
    if h[s as usize].is_infinite() {
        return None;
    }
    space.reset(graph.num_vertices());
    space.start(s, h[s as usize]);
    while let Some(Item { key, node: u }) = space.heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        let d = space.dist[u as usize];
        if space.settled[u as usize] || key > d + h[u as usize] {
            continue;
        }
        space.settled[u as usize] = true;
        if u == t {
            break;
        }
        for (v, w, e) in graph.fwd.arcs(u) {
            let hv = h[v as usize];
            if hv.is_infinite()
                || banned_node[v as usize]
                || (!banned_arc.is_empty() && banned_arc.contains(&(u, e)))
            {
                continue;
            }
            let nd = d + w;
            space.relax(v, nd, nd + hv, u, e, w);
        }
    }
    space.path_to(s, t)
}

struct Candidate(Path);

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap on cost, then fewer steps.
        other
            .0
            .total_cost()
            .total_cmp(&self.0.total_cost())
            .then_with(|| other.0.steps.len().cmp(&self.0.steps.len()))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
