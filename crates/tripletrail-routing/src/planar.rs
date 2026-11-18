//! Planarity testing (pgr_isPlanar) with the left-right criterion of de Fraysseix and
//! Rosenstiehl, following Brandes' "The Left-Right Planarity Test" (2009), without
//! recursion so that long paths do not exhaust the stack.

use crate::graph::{Graph, Undirected};
use crate::search::NONE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Interval {
    low: u32,
    high: u32,
}

impl Interval {
    const EMPTY: Self = Self {
        low: NONE,
        high: NONE,
    };

    fn is_empty(&self) -> bool {
        self.low == NONE && self.high == NONE
    }
}

#[derive(Clone, Copy, Debug)]
struct ConflictPair {
    left: Interval,
    right: Interval,
}

impl ConflictPair {
    fn swap(&mut self) {
        std::mem::swap(&mut self.left, &mut self.right);
    }
}

struct State {
    /// Oriented edges: tail and head.
    from: Vec<u32>,
    to: Vec<u32>,
    height: Vec<u32>,
    parent_edge: Vec<u32>,
    lowpt: Vec<u32>,
    lowpt2: Vec<u32>,
    nesting_depth: Vec<u64>,
    lowpt_edge: Vec<u32>,
    reference: Vec<u32>,
    stack_bottom: Vec<usize>,
    stack: Vec<ConflictPair>,
}

impl State {
    fn conflicting(&self, i: &Interval, b: u32) -> bool {
        !i.is_empty() && self.lowpt[i.high as usize] > self.lowpt[b as usize]
    }

    fn lowest(&self, p: &ConflictPair) -> u32 {
        let low = |e: u32| self.lowpt.get(e as usize).copied().unwrap_or(NONE);
        low(p.left.low).min(low(p.right.low))
    }

    fn add_constraints(&mut self, ei: u32, e: u32) -> bool {
        let mut p = ConflictPair {
            left: Interval::EMPTY,
            right: Interval::EMPTY,
        };
        // Merge the return edges of ei into p.right.
        while let Some(mut q) = self.stack.pop() {
            if !q.left.is_empty() {
                q.swap();
            }
            if !q.left.is_empty() {
                return false;
            }
            if self.lowpt[q.right.low as usize] > self.lowpt[e as usize] {
                if p.right.is_empty() {
                    p.right = q.right;
                } else {
                    self.reference[p.right.low as usize] = q.right.high;
                }
                p.right.low = q.right.low;
            } else {
                self.reference[q.right.low as usize] = self.lowpt_edge[e as usize];
            }
            if self.stack.len() == self.stack_bottom[ei as usize] {
                break;
            }
        }
        // Merge the conflicting return edges of the earlier siblings into p.left.
        while let Some(top) = self.stack.last() {
            if !(self.conflicting(&top.left, ei) || self.conflicting(&top.right, ei)) {
                break;
            }
            let mut q = self.stack.pop().unwrap();
            if self.conflicting(&q.right, ei) {
                q.swap();
            }
            if self.conflicting(&q.right, ei) {
                return false;
            }
            if p.right.low != NONE {
                self.reference[p.right.low as usize] = q.right.high;
            }
            if q.right.low != NONE {
                p.right.low = q.right.low;
            }
            if p.left.is_empty() {
                p.left = q.left;
            } else {
                self.reference[p.left.low as usize] = q.left.high;
            }
            p.left.low = q.left.low;
        }
        if !(p.left.is_empty() && p.right.is_empty()) {
            self.stack.push(p);
        }
        true
    }

    fn remove_back_edges(&mut self, e: u32) {
        let u = self.from[e as usize];
        // Drop whole conflict pairs that return to u.
        while let Some(top) = self.stack.last() {
            if self.lowest(top) != self.height[u as usize] {
                break;
            }
            self.stack.pop();
        }
        if let Some(mut p) = self.stack.pop() {
            // Trim the left interval.
            while p.left.high != NONE && self.to[p.left.high as usize] == u {
                p.left.high = self.reference[p.left.high as usize];
            }
            if p.left.high == NONE && p.left.low != NONE {
                self.reference[p.left.low as usize] = p.right.low;
                p.left.low = NONE;
            }
            // Trim the right interval.
            while p.right.high != NONE && self.to[p.right.high as usize] == u {
                p.right.high = self.reference[p.right.high as usize];
            }
            if p.right.high == NONE && p.right.low != NONE {
                self.reference[p.right.low as usize] = p.left.low;
                p.right.low = NONE;
            }
            self.stack.push(p);
        }
        // The side of e is that of a highest return edge.
        if self.lowpt[e as usize] < self.height[u as usize]
            && let Some(top) = self.stack.last()
        {
            let (hl, hr) = (top.left.high, top.right.high);
            self.reference[e as usize] = if hl != NONE
                && (hr == NONE || self.lowpt[hl as usize] > self.lowpt[hr as usize])
            {
                hl
            } else {
                hr
            };
        }
    }
}

/// Whether the graph, taken as undirected (loops and parallel edges ignored), is planar.
pub fn is_planar(graph: &Graph) -> bool {
    let u = Undirected::new(graph);
    let n = u.num_vertices();
    // Simple undirected edges.
    let mut simple: Vec<(u32, u32)> = u.edges.iter().map(|e| (e.0, e.1)).collect();
    simple.sort_unstable();
    simple.dedup();
    let m = simple.len();
    if n > 2 && m > 3 * n - 6 {
        return false;
    }
    // Adjacency: (neighbour, simple edge index).
    let mut adj: Vec<Vec<(u32, u32)>> = vec![Vec::new(); n];
    for (i, &(a, b)) in simple.iter().enumerate() {
        adj[a as usize].push((b, i as u32));
        adj[b as usize].push((a, i as u32));
    }
    let mut s = State {
        from: vec![NONE; m],
        to: vec![NONE; m],
        height: vec![NONE; n],
        parent_edge: vec![NONE; n],
        lowpt: vec![0; m],
        lowpt2: vec![0; m],
        nesting_depth: vec![0; m],
        lowpt_edge: vec![NONE; m],
        reference: vec![NONE; m],
        stack_bottom: vec![0; m],
        stack: Vec::new(),
    };
    // Orientation: a DFS orients every edge, tree edges downwards and back edges upwards.
    let mut roots = Vec::new();
    let mut next = vec![0usize; n];
    let mut descending = vec![false; m];
    for root in 0..n as u32 {
        if s.height[root as usize] != NONE {
            continue;
        }
        s.height[root as usize] = 0;
        roots.push(root);
        let mut stack = vec![root];
        while let Some(&v) = stack.last() {
            let vi = v as usize;
            let e = s.parent_edge[vi];
            let mut descended = false;
            while next[vi] < adj[vi].len() {
                let (w, vw) = adj[vi][next[vi]];
                let vwi = vw as usize;
                if !descending[vwi] {
                    if s.from[vwi] != NONE {
                        next[vi] += 1; // already oriented from the other end
                        continue;
                    }
                    s.from[vwi] = v;
                    s.to[vwi] = w;
                    s.lowpt[vwi] = s.height[vi];
                    s.lowpt2[vwi] = s.height[vi];
                    if s.height[w as usize] == NONE {
                        // Tree edge.
                        s.parent_edge[w as usize] = vw;
                        s.height[w as usize] = s.height[vi] + 1;
                        descending[vwi] = true;
                        stack.push(w);
                        descended = true;
                        break;
                    }
                    // Back edge.
                    s.lowpt[vwi] = s.height[w as usize];
                }
                // Nesting depth, then the lowpoints of the parent edge.
                s.nesting_depth[vwi] = 2 * u64::from(s.lowpt[vwi]);
                if s.lowpt2[vwi] < s.height[vi] {
                    s.nesting_depth[vwi] += 1; // chordal
                }
                if e != NONE {
                    let ei = e as usize;
                    if s.lowpt[vwi] < s.lowpt[ei] {
                        s.lowpt2[ei] = s.lowpt[ei].min(s.lowpt2[vwi]);
                        s.lowpt[ei] = s.lowpt[vwi];
                    } else if s.lowpt[vwi] > s.lowpt[ei] {
                        s.lowpt2[ei] = s.lowpt2[ei].min(s.lowpt[vwi]);
                    } else {
                        s.lowpt2[ei] = s.lowpt2[ei].min(s.lowpt2[vwi]);
                    }
                }
                next[vi] += 1;
            }
            if !descended {
                stack.pop();
            }
        }
    }
    // Outgoing oriented edges of every vertex, by nesting depth.
    let mut ordered: Vec<Vec<u32>> = vec![Vec::new(); n];
    for e in 0..m as u32 {
        ordered[s.from[e as usize] as usize].push(e);
    }
    for list in &mut ordered {
        list.sort_by_key(|&e| (s.nesting_depth[e as usize], e));
    }
    // Testing.
    next.fill(0);
    descending.fill(false);
    for root in roots {
        let mut stack = vec![root];
        while let Some(&v) = stack.last() {
            let vi = v as usize;
            let e = s.parent_edge[vi];
            let mut descended = false;
            while next[vi] < ordered[vi].len() {
                let ei = ordered[vi][next[vi]];
                let eii = ei as usize;
                if !descending[eii] {
                    s.stack_bottom[eii] = s.stack.len();
                    let w = s.to[eii];
                    if ei == s.parent_edge[w as usize] {
                        descending[eii] = true;
                        stack.push(w);
                        descended = true;
                        break;
                    }
                    s.lowpt_edge[eii] = ei;
                    s.stack.push(ConflictPair {
                        left: Interval::EMPTY,
                        right: Interval { low: ei, high: ei },
                    });
                }
                // Integrate the new return edges.
                if s.lowpt[eii] < s.height[vi] {
                    if ei == ordered[vi][0] {
                        s.lowpt_edge[e as usize] = s.lowpt_edge[eii];
                    } else if !s.add_constraints(ei, e) {
                        return false;
                    }
                }
                next[vi] += 1;
            }
            if descended {
                continue;
            }
            if e != NONE {
                s.remove_back_edges(e);
            }
            stack.pop();
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::tests::graph;

    fn complete(n: u32) -> Vec<(u32, u32)> {
        (0..n)
            .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
            .collect()
    }

    #[test]
    fn small_graphs() {
        assert!(is_planar(&graph(&complete(4), 4, false)));
        assert!(!is_planar(&graph(&complete(5), 5, false)));
        // K5 minus one edge is planar.
        assert!(is_planar(&graph(&complete(5)[1..], 5, false)));
        let k33: Vec<(u32, u32)> = (0..3).flat_map(|a| (3..6).map(move |b| (a, b))).collect();
        assert!(!is_planar(&graph(&k33, 6, false)));
        assert!(is_planar(&graph(&k33[1..], 6, false)));
        // The Petersen graph: 10 vertices, 15 edges, not planar (but sparse enough for the
        // edge-count test to pass it on).
        let petersen = [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 4),
            (4, 0),
            (0, 5),
            (1, 6),
            (2, 7),
            (3, 8),
            (4, 9),
            (5, 7),
            (7, 9),
            (9, 6),
            (6, 8),
            (8, 5),
        ];
        assert!(!is_planar(&graph(&petersen, 10, false)));
        // A grid is planar.
        let mut grid = Vec::new();
        for r in 0..5u32 {
            for c in 0..5u32 {
                let v = r * 5 + c;
                if c < 4 {
                    grid.push((v, v + 1));
                }
                if r < 4 {
                    grid.push((v, v + 5));
                }
            }
        }
        assert!(is_planar(&graph(&grid, 25, false)));
        // A cube with both diagonals of one face is still planar; with a space diagonal not.
        let cube = [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 0),
            (4, 5),
            (5, 6),
            (6, 7),
            (7, 4),
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7),
        ];
        assert!(is_planar(&graph(&cube, 8, false)));
        let mut cube_plus = cube.to_vec();
        cube_plus.extend([(0, 2), (1, 3)]);
        assert!(!is_planar(&graph(&cube_plus, 8, false)));
        let mut one_face = cube.to_vec();
        one_face.push((0, 2));
        assert!(is_planar(&graph(&one_face, 8, false)));
        // The cube is 3-connected, so its embedding is unique and opposite corners share no
        // face.
        let mut space = cube.to_vec();
        space.push((0, 6));
        assert!(!is_planar(&graph(&space, 8, false)));
    }
}
