//! Graph analysis: pgr_transitiveClosure, pgr_lengauerTarjanDominatorTree,
//! pgr_biconnectedComponents, pgr_makeConnected, pgr_hawickCircuits, pgr_maxCardinalityMatch,
//! pgr_stoerWagner. Results use the dense vertex and edge ids of the [`Graph`]; where pgRouting
//! breaks ties by id, these break them by dense id.

use crate::components::connected_components;
use crate::graph::{Graph, Undirected};
use crate::search::{NONE, Path, PathStep};
use std::collections::{BinaryHeap, VecDeque};

/// The vertices reachable from each vertex by one or more arcs (pgr_transitiveClosure),
/// sorted. A vertex is listed as reaching itself only when it is on a cycle.
pub fn transitive_closure(graph: &Graph) -> Vec<Vec<u32>> {
    let n = graph.num_vertices();
    let mut mark = vec![NONE; n];
    let mut stack = Vec::new();
    (0..n as u32)
        .map(|s| {
            let mut reached = Vec::new();
            stack.clear();
            stack.push(s);
            while let Some(u) = stack.pop() {
                for (v, _, _) in graph.fwd.arcs(u) {
                    if mark[v as usize] != s {
                        mark[v as usize] = s;
                        reached.push(v);
                        stack.push(v);
                    }
                }
            }
            reached.sort_unstable();
            reached
        })
        .collect()
}

/// The immediate dominator of every vertex reachable from `root` (pgr_lengauerTarjanDominatorTree);
/// `None` for the root and for unreachable vertices. Uses the iterative algorithm of Cooper,
/// Harvey and Kennedy, which gives the same tree as Lengauer-Tarjan.
pub fn dominator_tree(graph: &Graph, root: u32) -> Vec<Option<u32>> {
    let n = graph.num_vertices();
    // Reverse postorder of a DFS from the root.
    let mut post = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack = vec![(root, graph.fwd.offsets[root as usize] as usize)];
    seen[root as usize] = true;
    while let Some(&mut (v, ref mut slot)) = stack.last_mut() {
        if *slot < graph.fwd.offsets[v as usize + 1] as usize {
            let w = graph.fwd.head[*slot];
            *slot += 1;
            if !std::mem::replace(&mut seen[w as usize], true) {
                stack.push((w, graph.fwd.offsets[w as usize] as usize));
            }
        } else {
            post.push(v);
            stack.pop();
        }
    }
    let mut order = vec![NONE; n]; // postorder number
    for (i, &v) in post.iter().enumerate() {
        order[v as usize] = i as u32;
    }
    let mut idom = vec![NONE; n];
    idom[root as usize] = root;
    let intersect = |idom: &[u32], mut a: u32, mut b: u32| {
        while a != b {
            while order[a as usize] < order[b as usize] {
                a = idom[a as usize];
            }
            while order[b as usize] < order[a as usize] {
                b = idom[b as usize];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &v in post.iter().rev() {
            if v == root {
                continue;
            }
            let mut new = NONE;
            for (p, _, _) in graph.bwd.arcs(v) {
                if idom[p as usize] == NONE {
                    continue;
                }
                new = if new == NONE {
                    p
                } else {
                    intersect(&idom, p, new)
                };
            }
            if new != idom[v as usize] {
                idom[v as usize] = new;
                changed = true;
            }
        }
    }
    idom.iter()
        .enumerate()
        .map(|(v, &d)| (d != NONE && v as u32 != root).then_some(d))
        .collect()
}

/// The biconnected component of every edge of the undirected graph
/// (pgr_biconnectedComponents), labelled by its smallest edge id; loops are left out.
/// Returns `(edge, component)` sorted by edge.
pub fn biconnected_components(graph: &Graph) -> Vec<(u32, u32)> {
    const UNSEEN: u32 = u32::MAX;
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    let mut disc = vec![UNSEEN; n];
    let mut low = vec![0u32; n];
    let mut time = 0;
    let mut edge_stack: Vec<u32> = Vec::new();
    let mut out = Vec::new();
    // (vertex, edge used to enter it, position in adjacency)
    let mut call: Vec<(u32, u32, usize)> = Vec::new();
    for root in 0..n as u32 {
        if disc[root as usize] != UNSEEN {
            continue;
        }
        disc[root as usize] = time;
        low[root as usize] = time;
        time += 1;
        call.push((root, UNSEEN, 0));
        while let Some(&mut (v, via, ref mut pos)) = call.last_mut() {
            let vi = v as usize;
            if let Some(&(w, e, _)) = u.neighbours(v).get(*pos) {
                *pos += 1;
                if e == via {
                    continue;
                }
                let wi = w as usize;
                if disc[wi] == UNSEEN {
                    edge_stack.push(e);
                    disc[wi] = time;
                    low[wi] = time;
                    time += 1;
                    call.push((w, e, 0));
                } else if disc[wi] < disc[vi] {
                    edge_stack.push(e);
                    low[vi] = low[vi].min(disc[wi]);
                }
                continue;
            }
            call.pop();
            if let Some(&(parent, _, _)) = call.last() {
                let pi = parent as usize;
                low[pi] = low[pi].min(low[vi]);
                if low[vi] >= disc[pi] {
                    // `parent` separates this subtree: its edges form a component.
                    let mut members = Vec::new();
                    while let Some(e) = edge_stack.pop() {
                        members.push(e);
                        if e == via {
                            break;
                        }
                    }
                    let label = *members.iter().min().unwrap();
                    out.extend(members.into_iter().map(|e| (e, label)));
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// The arcs to add so that the undirected graph becomes connected (pgr_makeConnected): one
/// between the smallest vertices of consecutive components.
pub fn make_connected(graph: &Graph) -> Vec<(u32, u32)> {
    let labels = connected_components(graph);
    let roots: Vec<u32> = (0..labels.len() as u32)
        .filter(|&v| labels[v as usize] == v)
        .collect();
    roots.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Every elementary circuit of the directed graph (pgr_hawickCircuits), parallel arcs and
/// loops included, each starting and ending at its smallest vertex. Johnson's algorithm, with
/// the arcs (not just the vertices) enumerated so that parallel arcs give distinct circuits.
pub fn circuits(graph: &Graph) -> Vec<Path> {
    let n = graph.num_vertices();
    let mut out = Vec::new();
    let mut blocked = vec![false; n];
    let mut blocked_by: Vec<Vec<u32>> = vec![Vec::new(); n];
    // (vertex, next arc slot, found a circuit)
    let mut stack: Vec<(u32, usize, bool)> = Vec::new();
    // Arc slots of the current path.
    let mut path: Vec<usize> = Vec::new();
    let mut unblock_stack = Vec::new();
    for s in 0..n as u32 {
        for v in s as usize..n {
            blocked[v] = false;
            blocked_by[v].clear();
        }
        blocked[s as usize] = true;
        stack.push((s, graph.fwd.offsets[s as usize] as usize, false));
        while let Some(&mut (v, ref mut slot, ref mut found)) = stack.last_mut() {
            if crate::cancel::stopped() {
                break;
            }
            let end = graph.fwd.offsets[v as usize + 1] as usize;
            if *slot < end {
                let a = *slot;
                *slot += 1;
                let w = graph.fwd.head[a];
                if w < s {
                    continue;
                }
                if w == s {
                    *found = true;
                    path.push(a);
                    out.push(circuit_path(graph, s, &path));
                    path.pop();
                } else if !blocked[w as usize] {
                    blocked[w as usize] = true;
                    path.push(a);
                    stack.push((w, graph.fwd.offsets[w as usize] as usize, false));
                }
                continue;
            }
            let (v, _, found) = stack.pop().unwrap();
            if found {
                unblock_stack.push(v);
                while let Some(x) = unblock_stack.pop() {
                    if std::mem::replace(&mut blocked[x as usize], false) {
                        unblock_stack.append(&mut blocked_by[x as usize]);
                    }
                }
            } else {
                for (w, _, _) in graph.fwd.arcs(v) {
                    if w >= s && !blocked_by[w as usize].contains(&v) {
                        blocked_by[w as usize].push(v);
                    }
                }
            }
            if let Some(parent) = stack.last_mut() {
                parent.2 |= found;
                path.pop();
            }
        }
    }
    out
}

fn circuit_path(graph: &Graph, s: u32, arcs: &[usize]) -> Path {
    let mut steps = Vec::with_capacity(arcs.len() + 1);
    let mut node = s;
    let mut agg = 0.0;
    for &a in arcs {
        let cost = graph.fwd.weight[a];
        steps.push(PathStep {
            node,
            edge: Some(graph.fwd.edge[a]),
            cost,
            agg_cost: agg,
        });
        agg += cost;
        node = graph.fwd.head[a];
    }
    steps.push(PathStep {
        node: s,
        edge: None,
        cost: 0.0,
        agg_cost: agg,
    });
    Path {
        start_vid: s,
        end_vid: s,
        steps,
    }
}

/// A maximum matching of the undirected graph (pgr_maxCardinalityMatch): edge ids, sorted.
/// Edmonds' blossom algorithm; between two matched vertices the smallest edge id is used.
pub fn max_cardinality_matching(graph: &Graph) -> Vec<u32> {
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    let mut mate = vec![NONE; n];
    // Greedy start.
    for v in 0..n as u32 {
        if mate[v as usize] == NONE
            && let Some(&(w, _, _)) = u.neighbours(v).iter().find(|x| mate[x.0 as usize] == NONE)
        {
            mate[v as usize] = w;
            mate[w as usize] = v;
        }
    }
    let mut parent = vec![NONE; n];
    let mut base: Vec<u32> = (0..n as u32).collect();
    let mut used = vec![false; n];
    let mut blossom = vec![false; n];
    let mut queue = VecDeque::new();
    let lca = |base: &[u32], mate: &[u32], parent: &[u32], mut a: u32, mut b: u32| {
        let mut on_path = vec![false; n];
        loop {
            a = base[a as usize];
            on_path[a as usize] = true;
            if mate[a as usize] == NONE {
                break;
            }
            a = parent[mate[a as usize] as usize];
        }
        loop {
            b = base[b as usize];
            if on_path[b as usize] {
                return b;
            }
            b = parent[mate[b as usize] as usize];
        }
    };
    fn mark_path(
        base: &[u32],
        mate: &[u32],
        parent: &mut [u32],
        blossom: &mut [bool],
        mut v: u32,
        b: u32,
        mut child: u32,
    ) {
        while base[v as usize] != b {
            let m = mate[v as usize];
            blossom[base[v as usize] as usize] = true;
            blossom[base[m as usize] as usize] = true;
            parent[v as usize] = child;
            child = m;
            v = parent[m as usize];
        }
    }
    for root in 0..n as u32 {
        if mate[root as usize] != NONE {
            continue;
        }
        // Search for an augmenting path from `root`.
        parent.fill(NONE);
        used.fill(false);
        for (i, b) in base.iter_mut().enumerate() {
            *b = i as u32;
        }
        used[root as usize] = true;
        queue.clear();
        queue.push_back(root);
        let mut end = NONE;
        'search: while let Some(v) = queue.pop_front() {
            if crate::cancel::stopped() {
                break 'search;
            }
            for &(to, _, _) in u.neighbours(v) {
                if base[v as usize] == base[to as usize] || mate[v as usize] == to {
                    continue;
                }
                if to == root
                    || (mate[to as usize] != NONE && parent[mate[to as usize] as usize] != NONE)
                {
                    let b = lca(&base, &mate, &parent, v, to);
                    blossom.fill(false);
                    mark_path(&base, &mate, &mut parent, &mut blossom, v, b, to);
                    mark_path(&base, &mate, &mut parent, &mut blossom, to, b, v);
                    for i in 0..n {
                        if blossom[base[i] as usize] {
                            base[i] = b;
                            if !used[i] {
                                used[i] = true;
                                queue.push_back(i as u32);
                            }
                        }
                    }
                } else if parent[to as usize] == NONE {
                    parent[to as usize] = v;
                    if mate[to as usize] == NONE {
                        end = to;
                        break 'search;
                    }
                    let m = mate[to as usize];
                    used[m as usize] = true;
                    queue.push_back(m);
                }
            }
        }
        // Augment.
        let mut v = end;
        while v != NONE {
            let pv = parent[v as usize];
            let ppv = mate[pv as usize];
            mate[v as usize] = pv;
            mate[pv as usize] = v;
            v = ppv;
        }
    }
    let mut out: Vec<u32> = Vec::new();
    for v in 0..n as u32 {
        let w = mate[v as usize];
        if w != NONE && v < w {
            let e = u
                .neighbours(v)
                .iter()
                .filter(|x| x.0 == w)
                .map(|x| x.1)
                .min()
                .unwrap();
            out.push(e);
        }
    }
    out.sort_unstable();
    out
}

/// A minimum cut of the undirected graph (pgr_stoerWagner), each edge weighted by its cheapest
/// arc: the total weight and the edges crossing the cut, as `(edge, weight)` sorted by edge.
/// `None` for a graph with fewer than two vertices.
pub fn stoer_wagner(graph: &Graph) -> Option<(f64, Vec<(u32, f64)>)> {
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    if n < 2 {
        return None;
    }
    let mut adj: Vec<crate::graph::FxMap<u32, f64>> = vec![Default::default(); n];
    for &(a, b, _, c) in &u.edges {
        *adj[a as usize].entry(b).or_default() += c;
        *adj[b as usize].entry(a).or_default() += c;
    }
    let mut members: Vec<Vec<u32>> = (0..n as u32).map(|v| vec![v]).collect();
    let mut alive: Vec<u32> = (0..n as u32).collect();
    let mut best = (f64::INFINITY, Vec::new());
    let mut weight = vec![0.0f64; n];
    let mut added = vec![false; n];
    #[derive(PartialEq)]
    struct Key(f64, u32);
    impl Eq for Key {}
    impl PartialOrd for Key {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(o))
        }
    }
    impl Ord for Key {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering {
            self.0.total_cmp(&o.0).then(o.1.cmp(&self.1))
        }
    }
    while alive.len() > 1 {
        // Maximum adjacency order.
        for &v in &alive {
            weight[v as usize] = 0.0;
            added[v as usize] = false;
        }
        let mut heap: BinaryHeap<Key> = alive.iter().map(|&v| Key(0.0, v)).collect();
        let (mut prev, mut last) = (NONE, NONE);
        let mut count = 0;
        while let Some(Key(w, v)) = heap.pop() {
            if crate::cancel::stopped() {
                break;
            }
            if added[v as usize] || w != weight[v as usize] {
                continue;
            }
            added[v as usize] = true;
            count += 1;
            prev = last;
            last = v;
            if count == alive.len() {
                break;
            }
            for (&x, &c) in &adj[v as usize] {
                if !added[x as usize] {
                    weight[x as usize] += c;
                    heap.push(Key(weight[x as usize], x));
                }
            }
        }
        if weight[last as usize] < best.0 {
            best = (weight[last as usize], members[last as usize].clone());
        }
        // Merge `last` into `prev`.
        let moved = std::mem::take(&mut members[last as usize]);
        members[prev as usize].extend(moved);
        let edges = std::mem::take(&mut adj[last as usize]);
        for (x, c) in edges {
            adj[x as usize].remove(&last);
            if x != prev {
                *adj[prev as usize].entry(x).or_default() += c;
                *adj[x as usize].entry(prev).or_default() += c;
            }
        }
        alive.retain(|&v| v != last);
    }
    let mut side = vec![false; n];
    for &v in &best.1 {
        side[v as usize] = true;
    }
    let cut = u
        .edges
        .iter()
        .filter(|e| side[e.0 as usize] != side[e.1 as usize])
        .map(|e| (e.2, e.3))
        .collect();
    Some((best.0, cut))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::graph::EdgeRow;

    pub(crate) fn graph(edges: &[(u32, u32)], n: usize, directed: bool) -> Graph {
        let rows: Vec<EdgeRow> = edges
            .iter()
            .enumerate()
            .map(|(i, &(s, t))| EdgeRow {
                id: i as u32,
                source: s,
                target: t,
                cost: 1.0,
                reverse_cost: None,
            })
            .collect();
        Graph::build(n, &rows, directed)
    }

    #[test]
    fn closure_and_dominators() {
        // 0 -> 1 -> 2 -> 1, 0 -> 3
        let g = graph(&[(0, 1), (1, 2), (2, 1), (0, 3)], 5, true);
        assert_eq!(
            transitive_closure(&g),
            [vec![1, 2, 3], vec![1, 2], vec![1, 2], vec![], vec![]]
        );
        // Diamond 0 -> {1, 2} -> 3 -> 4.
        let d = graph(&[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)], 6, true);
        assert_eq!(
            dominator_tree(&d, 0),
            [None, Some(0), Some(0), Some(0), Some(3), None]
        );
    }

    #[test]
    fn biconnected_and_connecting() {
        // Triangle 0-1-2 plus a pendant edge 2-3, and an isolated pair 4-5.
        let g = graph(&[(0, 1), (1, 2), (2, 0), (2, 3), (4, 5)], 6, false);
        assert_eq!(
            biconnected_components(&g),
            [(0, 0), (1, 0), (2, 0), (3, 3), (4, 4)]
        );
        assert_eq!(make_connected(&g), [(0, 4)]);
    }

    #[test]
    fn circuits_with_parallel_arcs_and_loops() {
        // 0 <-> 1 (twice 0 -> 1), loop at 2, 1 -> 2 -> 0.
        let g = graph(&[(0, 1), (0, 1), (1, 0), (2, 2), (1, 2), (2, 0)], 3, true);
        let mut found: Vec<Vec<u32>> = circuits(&g)
            .iter()
            .map(|p| p.steps.iter().filter_map(|s| s.edge).collect())
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                vec![0, 2],
                vec![0, 4, 5],
                vec![1, 2],
                vec![1, 4, 5],
                vec![3]
            ]
        );
    }

    #[test]
    fn matching_and_min_cut() {
        // A path 0-1-2-3 matches 2 edges; a 5-cycle matches 2.
        let path = graph(&[(0, 1), (1, 2), (2, 3)], 4, false);
        assert_eq!(max_cardinality_matching(&path), [0, 2]);
        let cycle = graph(&[(0, 1), (1, 2), (2, 3), (3, 4), (4, 0)], 5, false);
        assert_eq!(max_cardinality_matching(&cycle).len(), 2);
        // A blossom: triangle 0-1-2 with tails 0-3 and 1-4 and 2-5.
        let b = graph(&[(0, 1), (1, 2), (2, 0), (0, 3), (1, 4), (2, 5)], 6, false);
        assert_eq!(max_cardinality_matching(&b).len(), 3);
        // Two triangles joined by one edge: the cut is that edge.
        let t = graph(
            &[(0, 1), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3), (2, 3)],
            6,
            false,
        );
        assert_eq!(stoer_wagner(&t), Some((1.0, vec![(6, 1.0)])));
    }
}
