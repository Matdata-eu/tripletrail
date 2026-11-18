//! Shortest paths without Dijkstra's assumptions (pgr_bellmanFord, pgr_edwardMoore,
//! pgr_dagShortestPath, pgr_binaryBreadthFirstSearch) and graph traversals
//! (pgr_breadthFirstSearch, pgr_depthFirstSearch).

use crate::graph::Graph;
use crate::search::{NONE, Path, PathStep};
use std::collections::VecDeque;

/// A shortest-path tree from one source: distance and predecessor arc of every vertex.
#[derive(Debug, Clone)]
pub struct ShortestTree {
    pub source: u32,
    pub dist: Vec<f64>,
    pred: Vec<u32>,
    pred_edge: Vec<u32>,
    pred_cost: Vec<f64>,
}

impl ShortestTree {
    fn new(n: usize, source: u32) -> Self {
        let mut dist = vec![f64::INFINITY; n];
        dist[source as usize] = 0.0;
        Self {
            source,
            dist,
            pred: vec![NONE; n],
            pred_edge: vec![NONE; n],
            pred_cost: vec![0.0; n],
        }
    }

    #[inline]
    fn relax(&mut self, u: u32, v: u32, w: f64, e: u32) -> bool {
        let d = self.dist[u as usize] + w;
        if d < self.dist[v as usize] {
            self.dist[v as usize] = d;
            self.pred[v as usize] = u;
            self.pred_edge[v as usize] = e;
            self.pred_cost[v as usize] = w;
            true
        } else {
            false
        }
    }

    /// The path from the source to `t`, if `t` is reachable.
    pub fn path(&self, t: u32) -> Option<Path> {
        if self.dist[t as usize].is_infinite() {
            return None;
        }
        let mut rev = Vec::new();
        let mut v = t;
        while v != self.source {
            let p = self.pred[v as usize];
            rev.push((p, self.pred_edge[v as usize], self.pred_cost[v as usize]));
            v = p;
            if rev.len() > self.dist.len() {
                return None; // a cycle in the predecessors: no well-defined path
            }
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
            start_vid: self.source,
            end_vid: t,
            steps,
        })
    }
}

/// Bellman-Ford from `source`; negative arc costs are allowed. `None` when a negative cycle
/// is reachable from the source.
pub fn bellman_ford(graph: &Graph, source: u32) -> Option<ShortestTree> {
    let n = graph.num_vertices();
    let mut tree = ShortestTree::new(n, source);
    for _ in 0..n {
        if crate::cancel::stopped_now() {
            break;
        }
        let mut changed = false;
        for u in 0..n as u32 {
            if tree.dist[u as usize].is_infinite() {
                continue;
            }
            for (v, w, e) in graph.fwd.arcs(u) {
                changed |= tree.relax(u, v, w, e);
            }
        }
        if !changed {
            return Some(tree);
        }
    }
    // Still improving after n rounds: a negative cycle.
    None
}

/// Edward F. Moore's queue-based Bellman-Ford (also known as SPFA); negative arc costs are
/// allowed. `None` when a negative cycle is reachable from the source.
pub fn edward_moore(graph: &Graph, source: u32) -> Option<ShortestTree> {
    let n = graph.num_vertices();
    let mut tree = ShortestTree::new(n, source);
    let mut queued = vec![false; n];
    // Number of arcs on the current best path: n or more means a negative cycle.
    let mut len = vec![0u32; n];
    let mut queue = VecDeque::from([source]);
    queued[source as usize] = true;
    while let Some(u) = queue.pop_front() {
        if crate::cancel::stopped() {
            break;
        }
        queued[u as usize] = false;
        for (v, w, e) in graph.fwd.arcs(u) {
            if tree.relax(u, v, w, e) {
                len[v as usize] = len[u as usize] + 1;
                if len[v as usize] as usize >= n {
                    return None;
                }
                if !queued[v as usize] {
                    queued[v as usize] = true;
                    queue.push_back(v);
                }
            }
        }
    }
    Some(tree)
}

/// A topological order of the vertices (smallest id first among the ready ones), or `None`
/// if the graph has a cycle.
pub fn topological_sort(graph: &Graph) -> Option<Vec<u32>> {
    let n = graph.num_vertices();
    let mut indegree: Vec<u32> = (0..n as u32).map(|v| graph.in_degree(v) as u32).collect();
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<u32>> = (0..n as u32)
        .filter(|&v| indegree[v as usize] == 0)
        .map(std::cmp::Reverse)
        .collect();
    let mut order = Vec::with_capacity(n);
    while let Some(std::cmp::Reverse(u)) = ready.pop() {
        if crate::cancel::stopped() {
            break;
        }
        order.push(u);
        for (v, _, _) in graph.fwd.arcs(u) {
            indegree[v as usize] -= 1;
            if indegree[v as usize] == 0 {
                ready.push(std::cmp::Reverse(v));
            }
        }
    }
    (order.len() == n).then_some(order)
}

/// Shortest paths on a directed acyclic graph, relaxing arcs in topological order. `None` if
/// the graph has a cycle.
pub fn dag_shortest_paths(graph: &Graph, source: u32) -> Option<ShortestTree> {
    let order = topological_sort(graph)?;
    let mut tree = ShortestTree::new(graph.num_vertices(), source);
    for u in order {
        if tree.dist[u as usize].is_finite() {
            for (v, w, e) in graph.fwd.arcs(u) {
                tree.relax(u, v, w, e);
            }
        }
    }
    Some(tree)
}

/// 0-1 breadth-first search: shortest paths when every arc costs either 0 or one same positive
/// value. `Err` names an arc cost that breaks this.
pub fn binary_bfs(graph: &Graph, source: u32) -> Result<ShortestTree, f64> {
    let mut unit = None;
    for &w in &graph.fwd.weight {
        if w != 0.0 {
            match unit {
                None => unit = Some(w),
                Some(u) if u == w => {}
                Some(_) => return Err(w),
            }
        }
    }
    let n = graph.num_vertices();
    let mut tree = ShortestTree::new(n, source);
    let mut done = vec![false; n];
    let mut deque = VecDeque::from([source]);
    while let Some(u) = deque.pop_front() {
        if crate::cancel::stopped() {
            break;
        }
        if std::mem::replace(&mut done[u as usize], true) {
            continue;
        }
        for (v, w, e) in graph.fwd.arcs(u) {
            if !done[v as usize] && tree.relax(u, v, w, e) {
                if w == 0.0 {
                    deque.push_front(v);
                } else {
                    deque.push_back(v);
                }
            }
        }
    }
    Ok(tree)
}

/// One vertex reached by a traversal, with the tree arc it was reached by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeStep {
    pub depth: u32,
    pub start_vid: u32,
    /// The vertex it was reached from (the root itself for the root).
    pub pred: u32,
    pub node: u32,
    /// `None` for the root.
    pub edge: Option<u32>,
    pub cost: f64,
    /// Cost along the tree from the root.
    pub agg_cost: f64,
}

impl TreeStep {
    fn root(root: u32) -> Self {
        Self {
            depth: 0,
            start_vid: root,
            pred: root,
            node: root,
            edge: None,
            cost: 0.0,
            agg_cost: 0.0,
        }
    }
}

/// Breadth-first traversal from `root` down to `max_depth`, in visiting order.
pub fn breadth_first(graph: &Graph, root: u32, max_depth: u64) -> Vec<TreeStep> {
    let mut seen = vec![false; graph.num_vertices()];
    seen[root as usize] = true;
    let mut out = vec![TreeStep::root(root)];
    let mut next = 0;
    while next < out.len() {
        let at = out[next];
        next += 1;
        if u64::from(at.depth) >= max_depth {
            continue;
        }
        for (v, w, e) in graph.fwd.arcs(at.node) {
            if !std::mem::replace(&mut seen[v as usize], true) {
                out.push(TreeStep {
                    depth: at.depth + 1,
                    start_vid: root,
                    pred: at.node,
                    node: v,
                    edge: Some(e),
                    cost: w,
                    agg_cost: at.agg_cost + w,
                });
            }
        }
    }
    out
}

/// Depth-first traversal from `root` down to `max_depth`, in discovery order.
pub fn depth_first(graph: &Graph, root: u32, max_depth: u64) -> Vec<TreeStep> {
    let mut seen = vec![false; graph.num_vertices()];
    seen[root as usize] = true;
    let mut out = vec![TreeStep::root(root)];
    // (index in `out` of the vertex, next arc slot)
    let mut stack = vec![(0usize, graph.fwd.offsets[root as usize] as usize)];
    while let Some(&mut (i, ref mut slot)) = stack.last_mut() {
        let at = out[i];
        let end = graph.fwd.offsets[at.node as usize + 1] as usize;
        if u64::from(at.depth) >= max_depth || *slot >= end {
            stack.pop();
            continue;
        }
        let (v, w, e) = (
            graph.fwd.head[*slot],
            graph.fwd.weight[*slot],
            graph.fwd.edge[*slot],
        );
        *slot += 1;
        if !std::mem::replace(&mut seen[v as usize], true) {
            out.push(TreeStep {
                depth: at.depth + 1,
                start_vid: root,
                pred: at.node,
                node: v,
                edge: Some(e),
                cost: w,
                agg_cost: at.agg_cost + w,
            });
            stack.push((out.len() - 1, graph.fwd.offsets[v as usize] as usize));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeRow;

    fn graph(edges: &[(u32, u32, f64)], n: usize, negative: bool) -> Graph {
        let rows: Vec<EdgeRow> = edges
            .iter()
            .enumerate()
            .map(|(i, &(s, t, c))| EdgeRow {
                id: i as u32,
                source: s,
                target: t,
                cost: c,
                reverse_cost: None,
            })
            .collect();
        if negative {
            Graph::build_keeping_negative(n, &rows, true)
        } else {
            Graph::build(n, &rows, true)
        }
    }

    #[test]
    fn negative_arcs() {
        // 0 -> 1 (4), 0 -> 2 (5), 2 -> 1 (-3), 1 -> 3 (1)
        let g = graph(
            &[(0, 1, 4.0), (0, 2, 5.0), (2, 1, -3.0), (1, 3, 1.0)],
            4,
            true,
        );
        for tree in [bellman_ford(&g, 0).unwrap(), edward_moore(&g, 0).unwrap()] {
            let p = tree.path(3).unwrap();
            assert_eq!(p.total_cost(), 3.0);
            assert_eq!(
                p.steps.iter().map(|s| s.node).collect::<Vec<_>>(),
                [0, 2, 1, 3]
            );
        }
        let cycle = graph(&[(0, 1, 1.0), (1, 2, -2.0), (2, 1, 1.0)], 3, true);
        assert!(bellman_ford(&cycle, 0).is_none());
        assert!(edward_moore(&cycle, 0).is_none());
    }

    #[test]
    fn dag_and_binary_bfs() {
        let g = graph(
            &[(0, 1, 1.0), (0, 2, 0.0), (2, 1, 0.0), (1, 3, 1.0)],
            4,
            false,
        );
        assert_eq!(topological_sort(&g).unwrap(), [0, 2, 1, 3]);
        assert_eq!(
            dag_shortest_paths(&g, 0).unwrap().dist,
            [0.0, 0.0, 0.0, 1.0]
        );
        assert_eq!(binary_bfs(&g, 0).unwrap().dist, [0.0, 0.0, 0.0, 1.0]);
        let cyclic = graph(&[(0, 1, 1.0), (1, 0, 1.0)], 2, false);
        assert!(dag_shortest_paths(&cyclic, 0).is_none());
        let mixed = graph(&[(0, 1, 1.0), (1, 2, 2.0)], 3, false);
        assert_eq!(binary_bfs(&mixed, 0).err(), Some(2.0));
    }

    #[test]
    fn traversals() {
        // 0 -> 1 -> 3, 0 -> 2 -> 3
        let g = graph(
            &[(0, 1, 1.0), (0, 2, 2.0), (1, 3, 1.0), (2, 3, 1.0)],
            4,
            false,
        );
        let bfs: Vec<_> = breadth_first(&g, 0, u64::MAX)
            .iter()
            .map(|s| (s.node, s.depth, s.agg_cost))
            .collect();
        assert_eq!(bfs, [(0, 0, 0.0), (1, 1, 1.0), (2, 1, 2.0), (3, 2, 2.0)]);
        let dfs: Vec<_> = depth_first(&g, 0, u64::MAX)
            .iter()
            .map(|s| (s.node, s.depth))
            .collect();
        assert_eq!(dfs, [(0, 0), (1, 1), (3, 2), (2, 1)]);
        assert_eq!(depth_first(&g, 0, 1).len(), 3);
        assert_eq!(breadth_first(&g, 0, 0).len(), 1);
    }
}
