//! Colourings and vertex orderings of the undirected graph: pgr_sequentialVertexColoring,
//! pgr_bipartite, pgr_edgeColoring, pgr_bandwidth, pgr_kingOrdering, pgr_sloanOrdering.
//! Vertices are taken in dense id order, as pgRouting takes them in id order.

use crate::graph::{Graph, Undirected};
use crate::search::NONE;
use std::collections::{BinaryHeap, VecDeque};

/// Greedy colouring in vertex order (pgr_sequentialVertexColoring): each vertex gets the
/// smallest colour (from 1) none of its neighbours has.
pub fn sequential_vertex_coloring(graph: &Graph) -> Vec<u32> {
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    let mut color = vec![0u32; n];
    let mut taken: Vec<u32> = Vec::new(); // taken[c] == v: colour c is used next to v
    for v in 0..n as u32 {
        taken.resize(u.degree(v) + 2, NONE);
        for &(w, _, _) in u.neighbours(v) {
            let c = color[w as usize] as usize;
            if c > 0 && c < taken.len() {
                taken[c] = v;
            }
        }
        color[v as usize] = (1..).find(|&c| taken[c as usize] != v).unwrap();
    }
    color
}

/// A two-colouring (0 and 1) if the undirected graph is bipartite (pgr_bipartite). Each
/// component starts with colour 0 at its smallest vertex.
pub fn bipartite(graph: &Graph) -> Option<Vec<u32>> {
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    // A loop makes the graph odd.
    for v in 0..n as u32 {
        if graph.fwd.arcs(v).any(|(w, _, _)| w == v) {
            return None;
        }
    }
    let mut color = vec![NONE; n];
    let mut queue = VecDeque::new();
    for s in 0..n as u32 {
        if color[s as usize] != NONE {
            continue;
        }
        color[s as usize] = 0;
        queue.push_back(s);
        while let Some(v) = queue.pop_front() {
            if crate::cancel::stopped() {
                break;
            }
            for &(w, _, _) in u.neighbours(v) {
                if color[w as usize] == NONE {
                    color[w as usize] = 1 - color[v as usize];
                    queue.push_back(w);
                } else if color[w as usize] == color[v as usize] {
                    return None;
                }
            }
        }
    }
    Some(color)
}

/// An edge colouring (pgr_edgeColoring): `(edge, colour)` by edge id, colours from 1, no two
/// edges sharing a vertex with the same colour. Greedy in edge order, so at most `2Δ - 1`
/// colours (Boost's Misra-Gries, used by pgRouting, guarantees `Δ + 1`). Loops are left out.
pub fn edge_coloring(graph: &Graph) -> Vec<(u32, u32)> {
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    // Colours used at each vertex.
    let mut used: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut out = Vec::with_capacity(u.edges.len());
    for &(a, b, e, _) in &u.edges {
        let (ua, ub) = (&used[a as usize], &used[b as usize]);
        let c = (1..).find(|c| !ua.contains(c) && !ub.contains(c)).unwrap();
        used[a as usize].push(c);
        used[b as usize].push(c);
        out.push((e, c));
    }
    out
}

/// The bandwidth of the graph in vertex order (pgr_bandwidth): the largest difference between
/// the ids of two adjacent vertices.
pub fn bandwidth(graph: &Graph) -> u32 {
    (0..graph.num_vertices() as u32)
        .flat_map(|v| graph.fwd.arcs(v).map(move |(w, _, _)| v.abs_diff(w)))
        .max()
        .unwrap_or(0)
}

/// A pseudo-peripheral vertex of the component of `start` (George and Liu): repeatedly the
/// smallest-degree vertex of the last breadth-first level, while the eccentricity grows.
fn peripheral(u: &Undirected, start: u32, level: &mut [u32]) -> u32 {
    let mut v = start;
    let mut eccentricity = 0;
    loop {
        let last = bfs_levels(u, v, level);
        let height = level[last[0] as usize];
        let next = *last.iter().min_by_key(|&&w| (u.degree(w), w)).unwrap();
        if height <= eccentricity {
            return v;
        }
        eccentricity = height;
        v = next;
    }
}

/// BFS levels from `s` over its component (others untouched); returns the deepest level.
fn bfs_levels(u: &Undirected, s: u32, level: &mut [u32]) -> Vec<u32> {
    let mut queue = VecDeque::from([s]);
    let mut members = vec![s];
    level[s as usize] = 0;
    while let Some(v) = queue.pop_front() {
        if crate::cancel::stopped() {
            break;
        }
        for &(w, _, _) in u.neighbours(v) {
            if level[w as usize] == NONE {
                level[w as usize] = level[v as usize] + 1;
                members.push(w);
                queue.push_back(w);
            }
        }
    }
    let top = level[*members.last().unwrap() as usize];
    let deepest = members
        .iter()
        .copied()
        .filter(|&w| level[w as usize] == top)
        .collect();
    for &w in &members {
        level[w as usize] = NONE;
    }
    deepest
}

/// The vertices of each component, smallest first, each component listed once.
fn components(u: &Undirected) -> Vec<u32> {
    // The smallest vertex of every component.
    let n = u.num_vertices();
    let mut seen = vec![false; n];
    let mut firsts = Vec::new();
    let mut stack = Vec::new();
    for s in 0..n as u32 {
        if std::mem::replace(&mut seen[s as usize], true) {
            continue;
        }
        firsts.push(s);
        stack.push(s);
        while let Some(v) = stack.pop() {
            for &(w, _, _) in u.neighbours(v) {
                if !std::mem::replace(&mut seen[w as usize], true) {
                    stack.push(w);
                }
            }
        }
    }
    firsts
}

/// King's ordering (pgr_kingOrdering), a profile-reducing variant of Cuthill-McKee: per
/// component from a pseudo-peripheral vertex, the next vertex numbered is always the queued
/// one with the fewest neighbours not yet queued.
pub fn king_ordering(graph: &Graph) -> Vec<u32> {
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    let mut level = vec![NONE; n];
    let mut queued = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for first in components(&u) {
        let s = peripheral(&u, first, &mut level);
        let mut queue: Vec<u32> = vec![s];
        queued[s as usize] = true;
        while !queue.is_empty() {
            let fresh = |v: u32| {
                u.neighbours(v)
                    .iter()
                    .filter(|x| !queued[x.0 as usize])
                    .count()
            };
            let (i, _) = queue
                .iter()
                .enumerate()
                .min_by_key(|&(i, &v)| (fresh(v), i))
                .unwrap();
            let v = queue.remove(i);
            order.push(v);
            for &(w, _, _) in u.neighbours(v) {
                if !std::mem::replace(&mut queued[w as usize], true) {
                    queue.push(w);
                }
            }
        }
    }
    order
}

/// Sloan's ordering (pgr_sloanOrdering), which reduces profile and wavefront: per component
/// from one end of a pseudo-diameter to the other, by priority
/// `W1 * distance-to-end - W2 * (current degree + 1)` with Boost's weights 1 and 2.
pub fn sloan_ordering(graph: &Graph) -> Vec<u32> {
    const W1: i64 = 1;
    const W2: i64 = 2;
    #[derive(Clone, Copy, PartialEq)]
    enum Status {
        Inactive,
        Preactive,
        Active,
        Numbered,
    }
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    let mut level = vec![NONE; n];
    let mut dist = vec![0u32; n];
    let mut status = vec![Status::Inactive; n];
    let mut priority = vec![0i64; n];
    let mut order = Vec::with_capacity(n);
    for first in components(&u) {
        let s = peripheral(&u, first, &mut level);
        // The end: the smallest-degree vertex furthest from the start.
        let e = *bfs_levels(&u, s, &mut level)
            .iter()
            .min_by_key(|&&w| (u.degree(w), w))
            .unwrap();
        // Distances to the end.
        let mut queue = VecDeque::from([e]);
        let mut members = vec![e];
        level[e as usize] = 0;
        while let Some(v) = queue.pop_front() {
            if crate::cancel::stopped() {
                break;
            }
            for &(w, _, _) in u.neighbours(v) {
                if level[w as usize] == NONE {
                    level[w as usize] = level[v as usize] + 1;
                    members.push(w);
                    queue.push_back(w);
                }
            }
        }
        for &v in &members {
            dist[v as usize] = level[v as usize];
            level[v as usize] = NONE;
            priority[v as usize] = W1 * dist[v as usize] as i64 - W2 * (u.degree(v) as i64 + 1);
        }
        let mut heap: BinaryHeap<(i64, std::cmp::Reverse<u32>)> = BinaryHeap::new();
        status[s as usize] = Status::Preactive;
        heap.push((priority[s as usize], std::cmp::Reverse(s)));
        while let Some((p, std::cmp::Reverse(v))) = heap.pop() {
            if crate::cancel::stopped() {
                break;
            }
            let vi = v as usize;
            if status[vi] == Status::Numbered || p != priority[vi] {
                continue;
            }
            if status[vi] == Status::Preactive {
                for &(w, _, _) in u.neighbours(v) {
                    let wi = w as usize;
                    if status[wi] == Status::Numbered {
                        continue;
                    }
                    priority[wi] += W2;
                    if status[wi] == Status::Inactive {
                        status[wi] = Status::Preactive;
                    }
                    heap.push((priority[wi], std::cmp::Reverse(w)));
                }
            }
            status[vi] = Status::Numbered;
            order.push(v);
            for &(w, _, _) in u.neighbours(v) {
                let wi = w as usize;
                if status[wi] != Status::Preactive {
                    continue;
                }
                status[wi] = Status::Active;
                priority[wi] += W2;
                heap.push((priority[wi], std::cmp::Reverse(w)));
                for &(x, _, _) in u.neighbours(w) {
                    let xi = x as usize;
                    if status[xi] == Status::Numbered {
                        continue;
                    }
                    priority[xi] += W2;
                    if status[xi] == Status::Inactive {
                        status[xi] = Status::Preactive;
                    }
                    heap.push((priority[xi], std::cmp::Reverse(x)));
                }
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::tests::graph;

    #[test]
    fn colourings() {
        // Square 0-1-2-3-0 with a diagonal 0-2.
        let g = graph(&[(0, 1), (1, 2), (2, 3), (3, 0), (0, 2)], 4, false);
        assert_eq!(sequential_vertex_coloring(&g), [1, 2, 3, 2]);
        assert_eq!(bipartite(&g), None);
        let square = graph(&[(0, 1), (1, 2), (2, 3), (3, 0)], 4, false);
        assert_eq!(bipartite(&square), Some(vec![0, 1, 0, 1]));
        let colours = edge_coloring(&g);
        for &(e1, c1) in &colours {
            for &(e2, c2) in &colours {
                let ends = |e: u32| [(0, 1), (1, 2), (2, 3), (3, 0), (0, 2)][e as usize];
                let (a, b) = (ends(e1), ends(e2));
                let share = a.0 == b.0 || a.0 == b.1 || a.1 == b.0 || a.1 == b.1;
                assert!(e1 == e2 || !share || c1 != c2);
            }
        }
    }

    #[test]
    fn orderings() {
        // A path numbered badly: 0-3-1-4-2.
        let g = graph(&[(0, 3), (3, 1), (1, 4), (4, 2)], 5, false);
        assert_eq!(bandwidth(&g), 3);
        let king = king_ordering(&g);
        let sloan = sloan_ordering(&g);
        for order in [king, sloan] {
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, [0, 1, 2, 3, 4]);
            // Along the path, from one end to the other.
            let path = [0, 3, 1, 4, 2];
            assert!(order == path || order.iter().rev().eq(path.iter()));
        }
    }
}
