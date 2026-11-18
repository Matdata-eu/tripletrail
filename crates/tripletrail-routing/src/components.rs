//! Graph components: pgr_connectedComponents, pgr_strongComponents, pgr_articulationPoints and
//! pgr_bridges. Labels and results use the dense vertex and edge ids of the [`Graph`].

use crate::graph::Graph;

/// The weakly connected component of every vertex, labelled by its smallest vertex id. Arcs
/// count in either direction, as in pgRouting (which runs it on the undirected graph).
pub fn connected_components(graph: &Graph) -> Vec<u32> {
    let n = graph.num_vertices();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    fn find(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            let p = parent[v as usize];
            parent[v as usize] = parent[p as usize];
            v = p;
        }
        v
    }
    for v in 0..n as u32 {
        for (w, _, _) in graph.fwd.arcs(v) {
            let (a, b) = (find(&mut parent, v), find(&mut parent, w));
            // Union towards the smaller id, so roots are the smallest vertex of their set.
            if a < b {
                parent[b as usize] = a;
            } else if b < a {
                parent[a as usize] = b;
            }
        }
    }
    (0..n as u32).map(|v| find(&mut parent, v)).collect()
}

/// The strongly connected component of every vertex (following arc directions), labelled by
/// its smallest vertex id. Iterative Tarjan.
pub fn strong_components(graph: &Graph) -> Vec<u32> {
    const UNSEEN: u32 = u32::MAX;
    let n = graph.num_vertices();
    let mut index = vec![UNSEEN; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut label = vec![UNSEEN; n];
    let mut next = 0u32;
    // (vertex, position in its arc list)
    let mut call: Vec<(u32, usize)> = Vec::new();
    for root in 0..n as u32 {
        if index[root as usize] != UNSEEN {
            continue;
        }
        call.push((root, 0));
        index[root as usize] = next;
        low[root as usize] = next;
        next += 1;
        stack.push(root);
        on_stack[root as usize] = true;
        while let Some(&mut (v, ref mut pos)) = call.last_mut() {
            if let Some((w, _, _)) = graph.fwd.arcs(v).nth(*pos) {
                *pos += 1;
                let wi = w as usize;
                if index[wi] == UNSEEN {
                    index[wi] = next;
                    low[wi] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[wi] = true;
                    call.push((w, 0));
                } else if on_stack[wi] {
                    low[v as usize] = low[v as usize].min(index[wi]);
                }
                continue;
            }
            call.pop();
            if let Some(&(parent, _)) = call.last() {
                low[parent as usize] = low[parent as usize].min(low[v as usize]);
            }
            if low[v as usize] == index[v as usize] {
                let mut members = Vec::new();
                loop {
                    let w = stack.pop().unwrap();
                    on_stack[w as usize] = false;
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                let min = *members.iter().min().unwrap();
                for w in members {
                    label[w as usize] = min;
                }
            }
        }
    }
    label
}

/// Articulation points (cut vertices) and bridges (cut edges) of the undirected graph: arcs
/// count in either direction, and parallel edges between the same two vertices are not
/// bridges. Returns sorted vertex ids and sorted edge ids.
pub fn cut_vertices_and_edges(graph: &Graph) -> (Vec<u32>, Vec<u32>) {
    const UNSEEN: u32 = u32::MAX;
    let n = graph.num_vertices();
    // Undirected adjacency: (neighbour, edge id), each edge once per direction.
    let mut adj: Vec<Vec<(u32, u32)>> = vec![Vec::new(); n];
    let mut seen = std::collections::HashSet::new();
    for v in 0..n as u32 {
        for (w, _, e) in graph.fwd.arcs(v) {
            if v == w {
                continue;
            }
            let key = (v.min(w), v.max(w), e);
            if seen.insert(key) {
                adj[v as usize].push((w, e));
                adj[w as usize].push((v, e));
            }
        }
    }
    let mut disc = vec![UNSEEN; n];
    let mut low = vec![0u32; n];
    let mut is_cut = vec![false; n];
    let mut bridges = Vec::new();
    let mut time = 0u32;
    // (vertex, edge used to enter it, position in adjacency, child count)
    let mut call: Vec<(u32, u32, usize, u32)> = Vec::new();
    for root in 0..n as u32 {
        if disc[root as usize] != UNSEEN {
            continue;
        }
        disc[root as usize] = time;
        low[root as usize] = time;
        time += 1;
        call.push((root, UNSEEN, 0, 0));
        while let Some(&mut (v, via, ref mut pos, ref mut children)) = call.last_mut() {
            let vi = v as usize;
            if *pos < adj[vi].len() {
                let (w, e) = adj[vi][*pos];
                *pos += 1;
                if e == via {
                    continue; // the edge we came in on (parallel edges have other ids)
                }
                let wi = w as usize;
                if disc[wi] == UNSEEN {
                    *children += 1;
                    disc[wi] = time;
                    low[wi] = time;
                    time += 1;
                    call.push((w, e, 0, 0));
                } else {
                    low[vi] = low[vi].min(disc[wi]);
                }
                continue;
            }
            let (_, _, _, children) = call.pop().unwrap();
            match call.last() {
                Some(&(parent, _, _, _)) => {
                    let pi = parent as usize;
                    low[pi] = low[pi].min(low[vi]);
                    if low[vi] > disc[pi] {
                        bridges.push(via);
                    }
                    // A non-root parent is a cut vertex if this subtree cannot reach above it.
                    if call.len() > 1 && low[vi] >= disc[pi] {
                        is_cut[pi] = true;
                    }
                }
                None => {
                    if children > 1 {
                        is_cut[vi] = true;
                    }
                }
            }
        }
    }
    bridges.sort_unstable();
    bridges.dedup();
    let cuts = (0..n as u32).filter(|&v| is_cut[v as usize]).collect();
    (cuts, bridges)
}
