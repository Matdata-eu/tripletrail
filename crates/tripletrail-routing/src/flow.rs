//! Network flow: maximum flow (pgr_maxFlow, pgr_pushRelabel, pgr_edmondsKarp,
//! pgr_boykovKolmogorov), minimum-cost maximum flow (pgr_maxFlowMinCost), edge-disjoint paths
//! and the directed Chinese postman tour, which all run on a residual graph.

use crate::graph::Graph;
use crate::search::{NONE, Path, PathStep};
use std::collections::VecDeque;

/// The maximum-flow algorithm. All give a maximum flow; how it is spread over the arcs can
/// differ between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxFlowAlgorithm {
    /// FIFO push-relabel (pgr_pushRelabel, pgr_maxFlow).
    PushRelabel,
    /// Shortest augmenting paths (pgr_edmondsKarp).
    EdmondsKarp,
    /// Blocking flows on BFS levels (Dinic); used for pgr_boykovKolmogorov.
    Dinic,
}

/// One arc of a flow problem.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowArc {
    pub tail: u32,
    pub head: u32,
    pub capacity: i64,
    /// Cost per unit of flow (minimum-cost flow only).
    pub cost: f64,
}

/// A residual graph in CSR form: every arc has a twin in the opposite direction.
struct Residual {
    offsets: Vec<u32>,
    head: Vec<u32>,
    cap: Vec<i64>,
    cost: Vec<f64>,
    twin: Vec<u32>,
}

impl Residual {
    /// The residual graph of `arcs` over `n` vertices, and the slot of every input arc.
    fn new(n: usize, arcs: &[FlowArc]) -> (Self, Vec<u32>) {
        let mut offsets = vec![0u32; n + 1];
        for a in arcs {
            offsets[a.tail as usize + 1] += 1;
            offsets[a.head as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let m = arcs.len() * 2;
        let mut cursor = offsets.clone();
        let (mut head, mut cap, mut cost, mut twin) =
            (vec![0; m], vec![0; m], vec![0.0; m], vec![0; m]);
        let mut slots = Vec::with_capacity(arcs.len());
        for a in arcs {
            let f = cursor[a.tail as usize];
            cursor[a.tail as usize] += 1;
            let b = cursor[a.head as usize];
            cursor[a.head as usize] += 1;
            (
                head[f as usize],
                cap[f as usize],
                cost[f as usize],
                twin[f as usize],
            ) = (a.head, a.capacity.max(0), a.cost, b);
            (
                head[b as usize],
                cap[b as usize],
                cost[b as usize],
                twin[b as usize],
            ) = (a.tail, 0, -a.cost, f);
            slots.push(f);
        }
        (
            Self {
                offsets,
                head,
                cap,
                cost,
                twin,
            },
            slots,
        )
    }

    fn n(&self) -> usize {
        self.offsets.len() - 1
    }

    #[inline]
    fn slots(&self, v: u32) -> std::ops::Range<usize> {
        self.offsets[v as usize] as usize..self.offsets[v as usize + 1] as usize
    }

    #[inline]
    fn push(&mut self, slot: usize, amount: i64) {
        self.cap[slot] -= amount;
        let t = self.twin[slot] as usize;
        self.cap[t] += amount;
    }

    /// BFS levels from `s` over arcs with residual capacity.
    fn levels(&self, s: u32, level: &mut [u32]) {
        level.fill(NONE);
        level[s as usize] = 0;
        let mut queue = VecDeque::from([s]);
        while let Some(u) = queue.pop_front() {
            if crate::cancel::stopped() {
                break;
            }
            for i in self.slots(u) {
                let v = self.head[i];
                if self.cap[i] > 0 && level[v as usize] == NONE {
                    level[v as usize] = level[u as usize] + 1;
                    queue.push_back(v);
                }
            }
        }
    }

    fn dinic(&mut self, s: u32, t: u32) -> i64 {
        let n = self.n();
        let mut level = vec![NONE; n];
        let mut next = vec![0usize; n];
        let mut total = 0;
        let mut path: Vec<usize> = Vec::new();
        loop {
            self.levels(s, &mut level);
            if level[t as usize] == NONE {
                return total;
            }
            for (slot, &start) in next.iter_mut().zip(&self.offsets) {
                *slot = start as usize;
            }
            path.clear();
            let mut v = s;
            loop {
                if v == t {
                    let amount = path.iter().map(|&a| self.cap[a]).min().unwrap_or(0);
                    total += amount;
                    let mut cut = path.len();
                    for (i, &a) in path.iter().enumerate() {
                        self.push(a, amount);
                        if self.cap[a] == 0 && cut == path.len() {
                            cut = i;
                        }
                    }
                    path.truncate(cut);
                    v = path.last().map_or(s, |&a| self.head[a]);
                    continue;
                }
                let end = self.offsets[v as usize + 1] as usize;
                let mut advanced = false;
                while next[v as usize] < end {
                    let a = next[v as usize];
                    let w = self.head[a];
                    if self.cap[a] > 0 && level[w as usize] == level[v as usize] + 1 {
                        path.push(a);
                        v = w;
                        advanced = true;
                        break;
                    }
                    next[v as usize] += 1;
                }
                if advanced {
                    continue;
                }
                if v == s {
                    break;
                }
                // Dead end: retreat.
                level[v as usize] = NONE;
                let a = path.pop().unwrap();
                v = self.head[self.twin[a] as usize];
                next[v as usize] += 1;
            }
        }
    }

    fn edmonds_karp(&mut self, s: u32, t: u32) -> i64 {
        let n = self.n();
        let mut via = vec![usize::MAX; n];
        let mut total = 0;
        loop {
            via.fill(usize::MAX);
            let mut queue = VecDeque::from([s]);
            let mut found = false;
            'bfs: while let Some(u) = queue.pop_front() {
                if crate::cancel::stopped() {
                    break 'bfs;
                }
                for i in self.slots(u) {
                    let v = self.head[i];
                    if self.cap[i] > 0 && v != s && via[v as usize] == usize::MAX {
                        via[v as usize] = i;
                        if v == t {
                            found = true;
                            break 'bfs;
                        }
                        queue.push_back(v);
                    }
                }
            }
            if !found {
                return total;
            }
            let mut amount = i64::MAX;
            let mut v = t;
            while v != s {
                let a = via[v as usize];
                amount = amount.min(self.cap[a]);
                v = self.head[self.twin[a] as usize];
            }
            let mut v = t;
            while v != s {
                let a = via[v as usize];
                self.push(a, amount);
                v = self.head[self.twin[a] as usize];
            }
            total += amount;
        }
    }

    fn push_relabel(&mut self, s: u32, t: u32) -> i64 {
        let n = self.n();
        let mut height = vec![0u32; n];
        let mut excess = vec![0i64; n];
        let mut current: Vec<usize> = (0..n).map(|v| self.offsets[v] as usize).collect();
        let mut active = VecDeque::new();
        height[s as usize] = n as u32;
        for a in self.slots(s) {
            let amount = self.cap[a];
            if amount > 0 {
                let v = self.head[a];
                self.push(a, amount);
                excess[v as usize] += amount;
                excess[s as usize] -= amount;
                if v != t && v != s && excess[v as usize] == amount {
                    active.push_back(v);
                }
            }
        }
        while let Some(u) = active.pop_front() {
            if crate::cancel::stopped() {
                break;
            }
            let ui = u as usize;
            while excess[ui] > 0 {
                let end = self.offsets[ui + 1] as usize;
                if current[ui] == end {
                    // Relabel.
                    let mut lowest = u32::MAX;
                    for a in self.slots(u) {
                        if self.cap[a] > 0 {
                            lowest = lowest.min(height[self.head[a] as usize]);
                        }
                    }
                    if lowest == u32::MAX {
                        break;
                    }
                    height[ui] = lowest + 1;
                    current[ui] = self.offsets[ui] as usize;
                    continue;
                }
                let a = current[ui];
                let v = self.head[a];
                if self.cap[a] > 0 && height[ui] == height[v as usize] + 1 {
                    let amount = excess[ui].min(self.cap[a]);
                    self.push(a, amount);
                    excess[ui] -= amount;
                    let was_idle = excess[v as usize] == 0;
                    excess[v as usize] += amount;
                    if was_idle && v != s && v != t {
                        active.push_back(v);
                    }
                } else {
                    current[ui] += 1;
                }
            }
        }
        excess[t as usize]
    }
}

/// Adds a super source and a super sink when there are several sources or sinks. Returns the
/// vertex count and the single source and sink.
fn single_ends(
    n: usize,
    arcs: &mut Vec<FlowArc>,
    sources: &[u32],
    sinks: &[u32],
) -> (usize, u32, u32) {
    let unbounded = arcs
        .iter()
        .map(|a| a.capacity.max(0))
        .fold(1i64, |s, c| s.saturating_add(c));
    let mut n = n;
    let mut end = |ends: &[u32], out: bool, arcs: &mut Vec<FlowArc>| {
        if let [one] = ends {
            return *one;
        }
        let v = n as u32;
        n += 1;
        for &e in ends {
            let (tail, head) = if out { (v, e) } else { (e, v) };
            arcs.push(FlowArc {
                tail,
                head,
                capacity: unbounded,
                cost: 0.0,
            });
        }
        v
    };
    let s = end(sources, true, arcs);
    let t = end(sinks, false, arcs);
    (n, s, t)
}

/// The maximum flow from `sources` to `sinks` (vertices that are both are dropped from the
/// sinks), and the flow on every arc of `arcs`.
pub fn max_flow(
    n: usize,
    arcs: &[FlowArc],
    sources: &[u32],
    sinks: &[u32],
    algorithm: MaxFlowAlgorithm,
) -> (i64, Vec<i64>) {
    let sinks: Vec<u32> = sinks
        .iter()
        .copied()
        .filter(|t| !sources.contains(t))
        .collect();
    if sources.is_empty() || sinks.is_empty() {
        return (0, vec![0; arcs.len()]);
    }
    let mut all = arcs.to_vec();
    let (n, s, t) = single_ends(n, &mut all, sources, &sinks);
    let (mut residual, slots) = Residual::new(n, &all);
    let total = match algorithm {
        MaxFlowAlgorithm::PushRelabel => residual.push_relabel(s, t),
        MaxFlowAlgorithm::EdmondsKarp => residual.edmonds_karp(s, t),
        MaxFlowAlgorithm::Dinic => residual.dinic(s, t),
    };
    let flows = arcs
        .iter()
        .zip(&slots)
        .map(|(a, &slot)| a.capacity.max(0) - residual.cap[slot as usize])
        .collect();
    (total, flows)
}

/// A maximum flow of minimum cost from `sources` to `sinks` (successive shortest paths).
/// Returns the flow, its cost and the flow on every arc of `arcs`.
pub fn min_cost_max_flow(
    n: usize,
    arcs: &[FlowArc],
    sources: &[u32],
    sinks: &[u32],
) -> (i64, f64, Vec<i64>) {
    let sinks: Vec<u32> = sinks
        .iter()
        .copied()
        .filter(|t| !sources.contains(t))
        .collect();
    if sources.is_empty() || sinks.is_empty() {
        return (0, 0.0, vec![0; arcs.len()]);
    }
    let mut all = arcs.to_vec();
    let (n, s, t) = single_ends(n, &mut all, sources, &sinks);
    let (mut r, slots) = Residual::new(n, &all);
    let (flow, cost) = r.successive_shortest_paths(s, t);
    let flows = arcs
        .iter()
        .zip(&slots)
        .map(|(a, &slot)| a.capacity.max(0) - r.cap[slot as usize])
        .collect();
    (flow, cost, flows)
}

impl Residual {
    /// Augments along cheapest paths (queue-based Bellman-Ford, so negative residual costs are
    /// fine) until the sink is unreachable.
    fn successive_shortest_paths(&mut self, s: u32, t: u32) -> (i64, f64) {
        let n = self.n();
        let mut dist = vec![f64::INFINITY; n];
        let mut via = vec![usize::MAX; n];
        let mut queued = vec![false; n];
        let (mut flow, mut cost) = (0i64, 0.0);
        loop {
            dist.fill(f64::INFINITY);
            via.fill(usize::MAX);
            dist[s as usize] = 0.0;
            let mut queue = VecDeque::from([s]);
            queued[s as usize] = true;
            while let Some(u) = queue.pop_front() {
                if crate::cancel::stopped() {
                    break;
                }
                queued[u as usize] = false;
                for a in self.slots(u) {
                    let v = self.head[a] as usize;
                    let d = dist[u as usize] + self.cost[a];
                    // A small tolerance keeps float round-off from cycling.
                    if self.cap[a] > 0 && d < dist[v] - 1e-12 {
                        dist[v] = d;
                        via[v] = a;
                        if !queued[v] {
                            queued[v] = true;
                            queue.push_back(v as u32);
                        }
                    }
                }
            }
            if dist[t as usize].is_infinite() {
                return (flow, cost);
            }
            let mut amount = i64::MAX;
            let mut v = t;
            while v != s {
                let a = via[v as usize];
                amount = amount.min(self.cap[a]);
                v = self.head[self.twin[a] as usize];
            }
            let mut v = t;
            while v != s {
                let a = via[v as usize];
                self.push(a, amount);
                v = self.head[self.twin[a] as usize];
            }
            flow += amount;
            cost += amount as f64 * dist[t as usize];
        }
    }
}

/// Splits a flow into paths from `s` to `t`: `flows[i]` units on arc `i` of `arcs`
/// (`(tail, head)`). Flow on cycles is dropped. Each path is a list of arc indices.
fn decompose(n: usize, arcs: &[(u32, u32)], flows: &mut [i64], s: u32, t: u32) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, &(a, _)) in arcs.iter().enumerate() {
        if flows[i] > 0 {
            out[a as usize].push(i);
        }
    }
    let mut next = vec![0usize; n];
    let mut on_path = vec![NONE; n];
    let mut paths = Vec::new();
    loop {
        let mut path: Vec<usize> = Vec::new();
        let mut v = s;
        on_path[s as usize] = 0;
        let mut stuck = false;
        while v != t {
            let list = &out[v as usize];
            while next[v as usize] < list.len() && flows[list[next[v as usize]]] == 0 {
                next[v as usize] += 1;
            }
            let Some(&a) = list.get(next[v as usize]) else {
                stuck = true;
                break;
            };
            let w = arcs[a].1;
            if on_path[w as usize] != NONE {
                // A cycle: drop one unit of flow around it and continue from `w`.
                let from = on_path[w as usize] as usize;
                for &c in &path[from..] {
                    flows[c] -= 1;
                    on_path[arcs[c].1 as usize] = NONE;
                }
                flows[a] -= 1;
                path.truncate(from);
                on_path[w as usize] = from as u32;
                v = w;
                continue;
            }
            path.push(a);
            on_path[w as usize] = path.len() as u32;
            v = w;
        }
        on_path[s as usize] = NONE;
        for &a in &path {
            on_path[arcs[a].1 as usize] = NONE;
        }
        if stuck {
            return paths;
        }
        for &a in &path {
            flows[a] -= 1;
        }
        paths.push(path);
    }
}

/// The largest set of arc-disjoint paths from `source` to `target` (pgr_edgeDisjointPaths):
/// every edge id is used at most once in each direction, and a path never uses an edge
/// in one direction while another uses it in the other.
pub fn edge_disjoint_paths(graph: &Graph, source: u32, target: u32) -> Vec<Path> {
    if source == target {
        return Vec::new();
    }
    let n = graph.num_vertices();
    let mut arcs = Vec::with_capacity(graph.num_arcs());
    let mut info = Vec::with_capacity(graph.num_arcs());
    for v in 0..n as u32 {
        for (w, c, e) in graph.fwd.arcs(v) {
            arcs.push(FlowArc {
                tail: v,
                head: w,
                capacity: 1,
                cost: 0.0,
            });
            info.push((v, w, c, e));
        }
    }
    let (_, mut flows) = max_flow(n, &arcs, &[source], &[target], MaxFlowAlgorithm::Dinic);
    // Cancel flow on both directions of the same edge.
    let mut by_edge: crate::graph::FxMap<(u32, u32, u32), usize> = Default::default();
    for (i, &(v, w, _, e)) in info.iter().enumerate() {
        if flows[i] == 0 {
            continue;
        }
        if let Some(&j) = by_edge.get(&(w, v, e))
            && flows[j] > 0
        {
            flows[i] -= 1;
            flows[j] -= 1;
            continue;
        }
        by_edge.insert((v, w, e), i);
    }
    let ends: Vec<(u32, u32)> = info.iter().map(|&(v, w, _, _)| (v, w)).collect();
    decompose(n, &ends, &mut flows, source, target)
        .into_iter()
        .map(|arcs| {
            let mut steps = Vec::with_capacity(arcs.len() + 1);
            let mut agg = 0.0;
            for a in arcs {
                let (v, _, c, e) = info[a];
                steps.push(PathStep {
                    node: v,
                    edge: Some(e),
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
            Path {
                start_vid: source,
                end_vid: target,
                steps,
            }
        })
        .collect()
}

/// The cheapest closed walk that traverses every arc at least once (pgr_chinesePostman),
/// starting at the tail of the first arc of the smallest vertex that has one. `None` when the
/// arcs do not all lie in one strongly connected component. An empty graph gives an empty
/// path.
pub fn chinese_postman(graph: &Graph) -> Option<Path> {
    let n = graph.num_vertices();
    let Some(start) = (0..n as u32).find(|&v| graph.out_degree(v) > 0) else {
        return Some(Path {
            start_vid: 0,
            end_vid: 0,
            steps: Vec::new(),
        });
    };
    // Every vertex with an arc must be in the start's strong component.
    let reach = |csr: &crate::graph::Csr| {
        let mut seen = vec![false; n];
        let mut stack = vec![start];
        seen[start as usize] = true;
        while let Some(u) = stack.pop() {
            for (v, _, _) in csr.arcs(u) {
                if !std::mem::replace(&mut seen[v as usize], true) {
                    stack.push(v);
                }
            }
        }
        seen
    };
    let (fwd, bwd) = (reach(&graph.fwd), reach(&graph.bwd));
    for v in 0..n as u32 {
        let has_arcs = graph.out_degree(v) + graph.in_degree(v) > 0;
        if has_arcs && !(fwd[v as usize] && bwd[v as usize]) {
            return None;
        }
    }
    // Duplicate arcs along cheapest paths from vertices short of out-arcs to vertices short of
    // in-arcs: a minimum-cost flow.
    let mut arcs = Vec::with_capacity(graph.num_arcs() + n);
    let mut info = Vec::with_capacity(graph.num_arcs());
    let mut need = 0i64;
    for v in 0..n as u32 {
        for (w, c, e) in graph.fwd.arcs(v) {
            info.push((v, w, c, e));
        }
    }
    let unbounded = (graph.num_arcs() as i64 + 1) * (n as i64 + 1);
    for &(v, w, c, _) in &info {
        arcs.push(FlowArc {
            tail: v,
            head: w,
            capacity: unbounded,
            cost: c,
        });
    }
    let (s, t) = (n as u32, n as u32 + 1);
    for v in 0..n as u32 {
        let balance = graph.in_degree(v) as i64 - graph.out_degree(v) as i64;
        if balance > 0 {
            arcs.push(FlowArc {
                tail: s,
                head: v,
                capacity: balance,
                cost: 0.0,
            });
            need += balance;
        } else if balance < 0 {
            arcs.push(FlowArc {
                tail: v,
                head: t,
                capacity: -balance,
                cost: 0.0,
            });
        }
    }
    let mut copies = vec![1i64; info.len()];
    if need > 0 {
        let (mut r, slots) = Residual::new(n + 2, &arcs);
        r.successive_shortest_paths(s, t);
        for (i, &slot) in slots.iter().take(info.len()).enumerate() {
            copies[i] += unbounded - r.cap[slot as usize];
        }
    }
    // Hierholzer's algorithm over the arcs with their copies.
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, &(v, _, _, _)) in info.iter().enumerate() {
        out[v as usize].push(i);
    }
    for list in &mut out {
        list.reverse(); // popped from the back, so arcs leave in their original order
    }
    let mut stack: Vec<(u32, usize)> = vec![(start, usize::MAX)];
    let mut circuit: Vec<usize> = Vec::new();
    while let Some(&(v, via)) = stack.last() {
        let list = &mut out[v as usize];
        match list.last().copied() {
            Some(a) => {
                copies[a] -= 1;
                if copies[a] == 0 {
                    list.pop();
                }
                stack.push((info[a].1, a));
            }
            None => {
                stack.pop();
                if via != usize::MAX {
                    circuit.push(via);
                }
            }
        }
    }
    circuit.reverse();
    let mut steps = Vec::with_capacity(circuit.len() + 1);
    let mut agg = 0.0;
    for a in circuit {
        let (v, _, c, e) = info[a];
        steps.push(PathStep {
            node: v,
            edge: Some(e),
            cost: c,
            agg_cost: agg,
        });
        agg += c;
    }
    steps.push(PathStep {
        node: start,
        edge: None,
        cost: 0.0,
        agg_cost: agg,
    });
    Some(Path {
        start_vid: start,
        end_vid: start,
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeRow;

    fn arc(tail: u32, head: u32, capacity: i64, cost: f64) -> FlowArc {
        FlowArc {
            tail,
            head,
            capacity,
            cost,
        }
    }

    #[test]
    fn max_flow_algorithms_agree() {
        // The classic CLRS network: max flow 23.
        let arcs = [
            arc(0, 1, 16, 0.0),
            arc(0, 2, 13, 0.0),
            arc(2, 1, 4, 0.0),
            arc(1, 3, 12, 0.0),
            arc(3, 2, 9, 0.0),
            arc(2, 4, 14, 0.0),
            arc(4, 3, 7, 0.0),
            arc(3, 5, 20, 0.0),
            arc(4, 5, 4, 0.0),
        ];
        for algorithm in [
            MaxFlowAlgorithm::PushRelabel,
            MaxFlowAlgorithm::EdmondsKarp,
            MaxFlowAlgorithm::Dinic,
        ] {
            let (total, flows) = max_flow(6, &arcs, &[0], &[5], algorithm);
            assert_eq!(total, 23, "{algorithm:?}");
            // Conservation and capacities.
            let mut balance = [0i64; 6];
            for (a, f) in arcs.iter().zip(&flows) {
                assert!((0..=a.capacity).contains(f));
                balance[a.tail as usize] -= f;
                balance[a.head as usize] += f;
            }
            assert_eq!(balance, [-23, 0, 0, 0, 0, 23]);
        }
        // Several sources and sinks.
        let (total, _) = max_flow(6, &arcs, &[0, 2], &[3, 5], MaxFlowAlgorithm::PushRelabel);
        assert_eq!(total, 12 + 7 + 4);
    }

    #[test]
    fn min_cost_flow() {
        // Two routes of capacity 1 and 2, costing 1 and 3 per unit.
        let arcs = [
            arc(0, 1, 1, 1.0),
            arc(1, 3, 5, 0.0),
            arc(0, 2, 2, 3.0),
            arc(2, 3, 5, 0.0),
        ];
        let (flow, cost, flows) = min_cost_max_flow(4, &arcs, &[0], &[3]);
        assert_eq!((flow, cost), (3, 7.0));
        assert_eq!(flows, [1, 1, 2, 2]);
    }

    fn graph(edges: &[(u32, u32, f64)], n: usize, directed: bool) -> Graph {
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
        Graph::build(n, &rows, directed)
    }

    #[test]
    fn disjoint_paths() {
        // Two ways from 0 to 3, sharing nothing; a third arc 0 -> 1 runs into the same edge.
        let g = graph(
            &[
                (0, 1, 1.0),
                (1, 3, 1.0),
                (0, 2, 1.0),
                (2, 3, 1.0),
                (0, 1, 5.0),
            ],
            4,
            true,
        );
        let paths = edge_disjoint_paths(&g, 0, 3);
        assert_eq!(paths.len(), 2);
        for p in &paths {
            assert_eq!(p.steps.last().unwrap().node, 3);
        }
    }

    #[test]
    fn postman_tour() {
        // A directed triangle plus a chord 0 -> 2: the chord forces 2 -> 0 to repeat... via 1.
        let g = graph(
            &[(0, 1, 1.0), (1, 2, 1.0), (2, 0, 1.0), (0, 2, 1.0)],
            3,
            true,
        );
        let p = chinese_postman(&g).unwrap();
        // 4 arcs plus the cheapest way back from 2 to 0 (2 -> 0, cost 1).
        assert_eq!(p.total_cost(), 5.0);
        assert_eq!(p.steps.first().unwrap().node, 0);
        assert_eq!(p.steps.last().unwrap().node, 0);
        let not_strong = graph(&[(0, 1, 1.0)], 2, true);
        assert!(chinese_postman(&not_strong).is_none());
    }
}
