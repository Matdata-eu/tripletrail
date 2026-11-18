//! Contraction hierarchies: a one-off preprocessing of a graph after which shortest-path
//! queries only search "upwards" from both ends, settling a few hundred vertices instead of
//! a large part of the network. Results are the same as Dijkstra's (paths of equal cost may
//! differ when there are ties).

use crate::graph::Graph;
use crate::search::{Item, NONE, Path, PathStep};
use std::collections::BinaryHeap;

/// An arc of the hierarchy: an original arc (`via == NONE`, with its edge id) or a shortcut
/// standing for the two arcs through the contracted vertex `via`.
#[derive(Debug, Clone, Copy)]
struct Arc {
    other: u32,
    weight: f64,
    via: u32,
    edge: u32,
}

/// Adds an arc to an adjacency list, keeping only the cheapest arc to each vertex.
fn add_arc(list: &mut Vec<Arc>, arc: Arc) {
    match list.iter_mut().find(|a| a.other == arc.other) {
        Some(a) if a.weight <= arc.weight => {}
        Some(a) => *a = arc,
        None => list.push(arc),
    }
}

/// A contraction hierarchy of a [`Graph`].
#[derive(Debug)]
pub struct ContractionHierarchy {
    /// Arcs from each vertex to vertices contracted after it.
    up: Vec<Vec<Arc>>,
    /// Arcs into each vertex from vertices contracted after it (`other` is the tail).
    down: Vec<Vec<Arc>>,
    shortcuts: usize,
}

/// Local search used to find witness paths during contraction.
struct Witness {
    dist: Vec<f64>,
    touched: Vec<u32>,
    heap: BinaryHeap<Item>,
}

impl Witness {
    /// Cheapest distances from `source` to the vertices of `out` lists, avoiding `skip`, up to
    /// `max` and settling at most `limit` vertices. Distances it did not reach stay infinite,
    /// which only costs an unnecessary shortcut.
    fn search(&mut self, out: &[Vec<Arc>], source: u32, skip: u32, max: f64, limit: usize) {
        for &v in &self.touched {
            self.dist[v as usize] = f64::INFINITY;
        }
        self.touched.clear();
        self.heap.clear();
        self.dist[source as usize] = 0.0;
        self.touched.push(source);
        self.heap.push(Item {
            key: 0.0,
            node: source,
        });
        let mut settled = 0;
        while let Some(Item { key: d, node: u }) = self.heap.pop() {
            if d > self.dist[u as usize] {
                continue;
            }
            settled += 1;
            if d > max || settled > limit {
                break;
            }
            for a in &out[u as usize] {
                if a.other == skip {
                    continue;
                }
                let nd = d + a.weight;
                let o = a.other as usize;
                if nd < self.dist[o] {
                    if self.dist[o].is_infinite() {
                        self.touched.push(a.other);
                    }
                    self.dist[o] = nd;
                    self.heap.push(Item {
                        key: nd,
                        node: a.other,
                    });
                }
            }
        }
    }
}

/// Settle limits of the witness search: while ordering (estimates) and when contracting.
const SIMULATE_LIMIT: usize = 50;
const CONTRACT_LIMIT: usize = 500;

/// The shortcuts contracting `v` needs: `(tail, head, weight)`.
fn shortcuts(
    out: &[Vec<Arc>],
    inn: &[Vec<Arc>],
    witness: &mut Witness,
    v: u32,
    limit: usize,
) -> Vec<(u32, u32, f64)> {
    let mut found = Vec::new();
    let outs = &out[v as usize];
    if outs.is_empty() {
        return found;
    }
    let max_out = outs.iter().map(|a| a.weight).fold(0.0, f64::max);
    for i in &inn[v as usize] {
        let u = i.other;
        witness.search(out, u, v, i.weight + max_out, limit);
        for o in outs {
            if o.other == u {
                continue;
            }
            let through = i.weight + o.weight;
            if witness.dist[o.other as usize] > through {
                found.push((u, o.other, through));
            }
        }
    }
    found
}

impl ContractionHierarchy {
    /// Contracts every vertex, least important first (edge difference plus the number of
    /// contracted neighbours, updated lazily).
    pub fn build(graph: &Graph) -> Self {
        let n = graph.num_vertices();
        let mut out: Vec<Vec<Arc>> = vec![Vec::new(); n];
        let mut inn: Vec<Vec<Arc>> = vec![Vec::new(); n];
        for v in 0..n as u32 {
            for (w, weight, edge) in graph.fwd.arcs(v) {
                if v == w {
                    continue; // a loop is never part of a shortest path
                }
                let arc = |other| Arc {
                    other,
                    weight,
                    via: NONE,
                    edge,
                };
                add_arc(&mut out[v as usize], arc(w));
                add_arc(&mut inn[w as usize], arc(v));
            }
        }
        let mut witness = Witness {
            dist: vec![f64::INFINITY; n],
            touched: Vec::new(),
            heap: BinaryHeap::new(),
        };
        let mut deleted = vec![0u32; n];
        let priority =
            |out: &[Vec<Arc>], inn: &[Vec<Arc>], deleted: &[u32], witness: &mut Witness, v: u32| {
                let added = shortcuts(out, inn, witness, v, SIMULATE_LIMIT).len() as f64;
                let removed = (out[v as usize].len() + inn[v as usize].len()) as f64;
                added - removed + deleted[v as usize] as f64
            };
        let mut queue = BinaryHeap::with_capacity(n);
        for v in 0..n as u32 {
            let key = priority(&out, &inn, &deleted, &mut witness, v);
            queue.push(Item { key, node: v });
        }
        let mut up: Vec<Vec<Arc>> = vec![Vec::new(); n];
        let mut down: Vec<Vec<Arc>> = vec![Vec::new(); n];
        let mut contracted = vec![false; n];
        let mut total_shortcuts = 0;
        while let Some(Item { node: v, .. }) = queue.pop() {
            if contracted[v as usize] {
                continue;
            }
            // Lazy update: contract only if still the least important.
            let key = priority(&out, &inn, &deleted, &mut witness, v);
            if let Some(next) = queue.peek()
                && key > next.key
            {
                queue.push(Item { key, node: v });
                continue;
            }
            let added = shortcuts(&out, &inn, &mut witness, v, CONTRACT_LIMIT);
            contracted[v as usize] = true;
            let vi = v as usize;
            up[vi] = std::mem::take(&mut out[vi]);
            down[vi] = std::mem::take(&mut inn[vi]);
            for a in &up[vi] {
                inn[a.other as usize].retain(|b| b.other != v);
                deleted[a.other as usize] += 1;
            }
            for a in &down[vi] {
                out[a.other as usize].retain(|b| b.other != v);
                deleted[a.other as usize] += 1;
            }
            total_shortcuts += added.len();
            for (u, x, weight) in added {
                let arc = |other| Arc {
                    other,
                    weight,
                    via: v,
                    edge: NONE,
                };
                add_arc(&mut out[u as usize], arc(x));
                add_arc(&mut inn[x as usize], arc(u));
            }
        }
        Self {
            up,
            down,
            shortcuts: total_shortcuts,
        }
    }

    pub fn num_vertices(&self) -> usize {
        self.up.len()
    }

    /// Shortcuts added during contraction.
    pub fn num_shortcuts(&self) -> usize {
        self.shortcuts
    }

    /// Expands an arc into original `(tail, edge, cost)` steps, appending to `steps`.
    fn unpack(&self, tail: u32, arc: Arc, steps: &mut Vec<(u32, u32, f64)>) {
        let mut stack = vec![(tail, arc)];
        while let Some((a, arc)) = stack.pop() {
            if arc.via == NONE {
                steps.push((a, arc.edge, arc.weight));
                continue;
            }
            let m = arc.via as usize;
            // The shortcut a -> b stands for a -> m (stored at m as an incoming arc) and
            // m -> b (an outgoing arc of m), the cheapest of each when m was contracted.
            let first = self.down[m]
                .iter()
                .find(|x| x.other == a)
                .map(|x| Arc {
                    other: arc.via,
                    ..*x
                })
                .expect("shortcut tail arc");
            let second = self.up[m]
                .iter()
                .find(|x| x.other == arc.other)
                .copied()
                .expect("shortcut head arc");
            stack.push((arc.via, second));
            stack.push((a, first));
        }
    }
}

/// Reusable state for hierarchy queries.
#[derive(Debug, Default)]
pub struct ChSpace {
    fwd: Side,
    bwd: Side,
}

#[derive(Debug, Default)]
struct Side {
    dist: Vec<f64>,
    /// The vertex the best arc came from, and that arc.
    pred: Vec<(u32, Option<Arc>)>,
    touched: Vec<u32>,
    heap: BinaryHeap<Item>,
}

impl Side {
    fn reset(&mut self, n: usize) {
        if self.dist.len() != n {
            self.dist = vec![f64::INFINITY; n];
            self.pred = vec![(NONE, None); n];
            self.touched.clear();
        } else {
            for &v in &self.touched {
                self.dist[v as usize] = f64::INFINITY;
                self.pred[v as usize] = (NONE, None);
            }
            self.touched.clear();
        }
        self.heap.clear();
    }

    /// Full search over `arcs` from `root`: the upward search space is small.
    fn search(&mut self, arcs: &[Vec<Arc>], root: u32) {
        self.reset(arcs.len());
        self.dist[root as usize] = 0.0;
        self.touched.push(root);
        self.heap.push(Item {
            key: 0.0,
            node: root,
        });
        while let Some(Item { key: d, node: u }) = self.heap.pop() {
            if crate::cancel::stopped() {
                break;
            }
            if d > self.dist[u as usize] {
                continue;
            }
            for a in &arcs[u as usize] {
                let nd = d + a.weight;
                let o = a.other as usize;
                if nd < self.dist[o] {
                    if self.dist[o].is_infinite() {
                        self.touched.push(a.other);
                    }
                    self.dist[o] = nd;
                    self.pred[o] = (u, Some(*a));
                    self.heap.push(Item {
                        key: nd,
                        node: a.other,
                    });
                }
            }
        }
    }
}

impl ChSpace {
    pub fn new() -> Self {
        Self::default()
    }
}

/// One-to-many shortest paths on a hierarchy (same results as [`crate::dijkstra`]).
pub fn ch_dijkstra(
    ch: &ContractionHierarchy,
    space: &mut ChSpace,
    source: u32,
    targets: &[u32],
) -> Vec<Option<Path>> {
    let ChSpace { fwd, bwd } = space;
    fwd.search(&ch.up, source);
    targets
        .iter()
        .map(|&t| {
            bwd.search(&ch.down, t);
            // The cheapest vertex both searches reached.
            let (meet, cost) = bwd
                .touched
                .iter()
                .map(|&v| (v, fwd.dist[v as usize] + bwd.dist[v as usize]))
                .min_by(|a, b| a.1.total_cmp(&b.1))?;
            if cost.is_infinite() {
                return None;
            }
            // Arcs source -> meet, then meet -> t, each expanded to original edges.
            let mut arcs = Vec::new();
            let mut v = meet;
            while let (p, Some(arc)) = fwd.pred[v as usize] {
                arcs.push((p, arc));
                v = p;
            }
            arcs.reverse();
            let mut v = meet;
            while let (p, Some(arc)) = bwd.pred[v as usize] {
                // Backward arcs are stored at their head with `other` = tail.
                arcs.push((v, Arc { other: p, ..arc }));
                v = p;
            }
            let mut raw = Vec::new();
            for (tail, arc) in arcs {
                ch.unpack(tail, arc, &mut raw);
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
                node: t,
                edge: None,
                cost: 0.0,
                agg_cost: agg,
            });
            Some(Path {
                start_vid: source,
                end_vid: t,
                steps,
            })
        })
        .collect()
}

impl ContractionHierarchy {
    /// Reads a hierarchy saved by an earlier version (files now hold a [`crate::Cch`]).
    pub(crate) fn read(r: &mut crate::persist::Reader, n: usize) -> Option<Self> {
        let shortcuts = usize::try_from(r.u64()?).ok()?;
        let mut read_lists = || -> Option<Vec<Vec<Arc>>> {
            (0..n)
                .map(|_| {
                    let len = r.len(20)?;
                    (0..len)
                        .map(|_| {
                            let arc = Arc {
                                other: r.u32()?,
                                weight: r.f64()?,
                                via: r.u32()?,
                                edge: r.u32()?,
                            };
                            let valid = (arc.other as usize) < n
                                && (arc.via == NONE || (arc.via as usize) < n);
                            valid.then_some(arc)
                        })
                        .collect()
                })
                .collect()
        };
        let up = read_lists()?;
        let down = read_lists()?;
        Some(Self {
            up,
            down,
            shortcuts,
        })
    }
}
