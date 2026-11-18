//! Graph contraction as in pgr_contraction: dead-end and linear contraction, which shrink a
//! network to its junctions while keeping its costs. (Not to be confused with the contraction
//! hierarchies of [`crate::Cch`], which speed up searches.)

use crate::graph::Graph;
use crate::search::NONE;

/// A contraction method of pgr_contraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractionMethod {
    /// 1: remove vertices with a single neighbour.
    DeadEnd,
    /// 2: replace a vertex with exactly two neighbours by a shortcut between them.
    Linear,
}

/// A shortcut arc added by linear contraction.
#[derive(Debug, Clone, PartialEq)]
pub struct Shortcut {
    pub source: u32,
    pub target: u32,
    pub cost: f64,
    /// The vertices the shortcut replaces, sorted.
    pub contracted: Vec<u32>,
}

/// The result of a contraction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Contracted {
    /// Remaining vertices that absorbed dead ends, with the vertices they absorbed, sorted.
    pub vertices: Vec<(u32, Vec<u32>)>,
    /// Shortcuts left in the contracted graph, in the order they were made.
    pub shortcuts: Vec<Shortcut>,
}

struct Arc {
    a: u32,
    b: u32,
    cost: f64,
    /// Index into the shortcut list, for shortcuts.
    shortcut: u32,
    alive: bool,
}

struct Work {
    directed: bool,
    arcs: Vec<Arc>,
    /// Arcs incident to each vertex (either end); dead ones are skipped.
    incident: Vec<Vec<u32>>,
    alive: Vec<bool>,
    /// Vertices absorbed by each vertex.
    absorbed: Vec<Vec<u32>>,
    shortcuts: Vec<Shortcut>,
}

impl Work {
    /// The distinct neighbours of `v` other than itself, up to three.
    fn neighbours(&self, v: u32) -> Vec<u32> {
        let mut out = Vec::with_capacity(3);
        for &i in &self.incident[v as usize] {
            let arc = &self.arcs[i as usize];
            if !arc.alive {
                continue;
            }
            let w = if arc.a == v { arc.b } else { arc.a };
            if w != v && !out.contains(&w) {
                out.push(w);
                if out.len() == 3 {
                    break;
                }
            }
        }
        out
    }

    /// The cheapest live arc from `a` to `b` (either way round when undirected).
    fn cheapest(&self, a: u32, b: u32) -> Option<u32> {
        self.incident[a as usize]
            .iter()
            .copied()
            .filter(|&i| {
                let arc = &self.arcs[i as usize];
                arc.alive
                    && ((arc.a == a && arc.b == b) || (!self.directed && arc.a == b && arc.b == a))
            })
            .min_by(|&x, &y| {
                self.arcs[x as usize]
                    .cost
                    .total_cmp(&self.arcs[y as usize].cost)
            })
    }

    /// Kills every arc at `v`, and `v`; returns the vertices the dead shortcuts replaced.
    fn remove(&mut self, v: u32) -> Vec<u32> {
        let mut freed = Vec::new();
        for &i in &self.incident[v as usize] {
            let arc = &mut self.arcs[i as usize];
            if std::mem::replace(&mut arc.alive, false) && arc.shortcut != NONE {
                freed.extend_from_slice(&self.shortcuts[arc.shortcut as usize].contracted);
            }
        }
        self.alive[v as usize] = false;
        freed
    }

    fn dead_end(&mut self, v: u32) -> Option<u32> {
        let [u] = self.neighbours(v)[..] else {
            return None;
        };
        let mut gone = self.remove(v);
        gone.push(v);
        gone.extend(std::mem::take(&mut self.absorbed[v as usize]));
        self.absorbed[u as usize].extend(gone);
        Some(u)
    }

    fn linear(&mut self, v: u32) -> Option<[u32; 2]> {
        let [u, w] = self.neighbours(v)[..] else {
            return None;
        };
        // Each way through v must be usable in full or not at all.
        let pairs = if self.directed {
            let (uv, vw) = (self.cheapest(u, v), self.cheapest(v, w));
            let (wv, vu) = (self.cheapest(w, v), self.cheapest(v, u));
            if uv.is_some() != vw.is_some() || wv.is_some() != vu.is_some() {
                return None;
            }
            [(u, w, uv, vw), (w, u, wv, vu)]
        } else {
            [
                (u, w, self.cheapest(u, v), self.cheapest(v, w)),
                (0, 0, None, None),
            ]
        };
        if pairs.iter().all(|p| p.2.is_none()) {
            return None;
        }
        let mut new = Vec::new();
        for (from, to, first, second) in pairs {
            let (Some(first), Some(second)) = (first, second) else {
                continue;
            };
            let mut contracted = vec![v];
            contracted.extend_from_slice(&self.absorbed[v as usize]);
            for arc in [first, second] {
                let s = self.arcs[arc as usize].shortcut;
                if s != NONE {
                    contracted.extend_from_slice(&self.shortcuts[s as usize].contracted);
                }
            }
            contracted.sort_unstable();
            contracted.dedup();
            let cost = self.arcs[first as usize].cost + self.arcs[second as usize].cost;
            new.push(Shortcut {
                source: from,
                target: to,
                cost,
                contracted,
            });
        }
        self.remove(v);
        self.absorbed[v as usize].clear();
        for s in new {
            let i = self.arcs.len() as u32;
            self.arcs.push(Arc {
                a: s.source,
                b: s.target,
                cost: s.cost,
                shortcut: self.shortcuts.len() as u32,
                alive: true,
            });
            self.incident[s.source as usize].push(i);
            self.incident[s.target as usize].push(i);
            self.shortcuts.push(s);
        }
        Some([u, w])
    }
}

/// Contracts the graph (pgr_contraction): `methods` are applied in order, the whole sequence
/// `cycles` times; vertices marked in `forbidden` are kept. Each method runs until no vertex
/// qualifies. Vertices are visited in id order.
pub fn contract(
    graph: &Graph,
    directed: bool,
    methods: &[ContractionMethod],
    cycles: usize,
    forbidden: &[bool],
) -> Contracted {
    let n = graph.num_vertices();
    let mut work = Work {
        directed,
        arcs: Vec::with_capacity(graph.num_arcs()),
        incident: vec![Vec::new(); n],
        alive: vec![true; n],
        absorbed: vec![Vec::new(); n],
        shortcuts: Vec::new(),
    };
    for v in 0..n as u32 {
        for (w, cost, _) in graph.fwd.arcs(v) {
            // Undirected graphs have both arcs of every edge: keep one.
            if !directed && w < v {
                continue;
            }
            let i = work.arcs.len() as u32;
            work.arcs.push(Arc {
                a: v,
                b: w,
                cost,
                shortcut: NONE,
                alive: true,
            });
            work.incident[v as usize].push(i);
            if w != v {
                work.incident[w as usize].push(i);
            }
        }
    }
    let mut queued = vec![false; n];
    for _ in 0..cycles {
        for &method in methods {
            let mut queue: std::collections::VecDeque<u32> = (0..n as u32)
                .filter(|&v| work.alive[v as usize] && !forbidden[v as usize])
                .collect();
            for &v in &queue {
                queued[v as usize] = true;
            }
            while let Some(v) = queue.pop_front() {
                if crate::cancel::stopped() {
                    break;
                }
                queued[v as usize] = false;
                if !work.alive[v as usize] {
                    continue;
                }
                let touched: Vec<u32> = match method {
                    ContractionMethod::DeadEnd => work.dead_end(v).into_iter().collect(),
                    ContractionMethod::Linear => work.linear(v).map(Vec::from).unwrap_or_default(),
                };
                for t in touched {
                    if !forbidden[t as usize] && !std::mem::replace(&mut queued[t as usize], true) {
                        queue.push_back(t);
                    }
                }
            }
        }
    }
    let mut vertices = Vec::new();
    for v in 0..n as u32 {
        let mut a = std::mem::take(&mut work.absorbed[v as usize]);
        if work.alive[v as usize] && !a.is_empty() {
            a.sort_unstable();
            a.dedup();
            vertices.push((v, a));
        }
    }
    let mut shortcuts = Vec::new();
    for arc in &work.arcs {
        if arc.alive && arc.shortcut != NONE {
            shortcuts.push(work.shortcuts[arc.shortcut as usize].clone());
        }
    }
    Contracted {
        vertices,
        shortcuts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeRow;

    fn graph(edges: &[(u32, u32, f64)], n: usize, directed: bool) -> Graph {
        let rows: Vec<EdgeRow> = edges
            .iter()
            .enumerate()
            .map(|(i, &(s, t, c))| EdgeRow {
                id: i as u32,
                source: s,
                target: t,
                cost: c,
                reverse_cost: Some(c),
            })
            .collect();
        Graph::build(n, &rows, directed)
    }

    #[test]
    fn dead_ends_and_chains() {
        // A square 0-1-2-3-0 where 1 has a dangling branch 1-4-5, and 2-3 runs through 6.
        let g = graph(
            &[
                (0, 1, 1.0),
                (1, 2, 1.0),
                (2, 6, 1.0),
                (6, 3, 2.0),
                (3, 0, 1.0),
                (1, 4, 1.0),
                (4, 5, 1.0),
            ],
            7,
            false,
        );
        let none = vec![false; 7];
        let dead = contract(&g, false, &[ContractionMethod::DeadEnd], 1, &none);
        assert_eq!(dead.vertices, [(1, vec![4, 5])]);
        assert!(dead.shortcuts.is_empty());
        let both = contract(
            &g,
            false,
            &[ContractionMethod::DeadEnd, ContractionMethod::Linear],
            1,
            &none,
        );
        // After the dead ends the square is a 5-cycle; contracting in id order folds it up
        // until two vertices joined by two arcs are left (the shortcut carries 1's dead ends).
        assert!(both.vertices.is_empty());
        assert_eq!(
            both.shortcuts,
            [Shortcut {
                source: 6,
                target: 3,
                cost: 4.0,
                contracted: vec![0, 1, 2, 4, 5]
            }]
        );
        // Forbidding 6 keeps it.
        let mut keep = none.clone();
        keep[6] = true;
        let linear = contract(&g, false, &[ContractionMethod::Linear], 1, &keep);
        assert!(linear.shortcuts.iter().all(|s| !s.contracted.contains(&6)));
        // Directed: a one-way chain 0 -> 1 -> 2 contracts to 0 -> 2.
        let rows = [
            EdgeRow {
                id: 0,
                source: 0,
                target: 1,
                cost: 1.0,
                reverse_cost: None,
            },
            EdgeRow {
                id: 1,
                source: 1,
                target: 2,
                cost: 2.0,
                reverse_cost: None,
            },
            EdgeRow {
                id: 2,
                source: 2,
                target: 3,
                cost: 1.0,
                reverse_cost: Some(1.0),
            },
            EdgeRow {
                id: 3,
                source: 0,
                target: 3,
                cost: 1.0,
                reverse_cost: Some(1.0),
            },
        ];
        let d = Graph::build(4, &rows, true);
        let c = contract(&d, true, &[ContractionMethod::Linear], 1, &[false; 4]);
        assert_eq!(
            c.shortcuts[0],
            Shortcut {
                source: 0,
                target: 2,
                cost: 3.0,
                contracted: vec![1]
            }
        );
    }
}
