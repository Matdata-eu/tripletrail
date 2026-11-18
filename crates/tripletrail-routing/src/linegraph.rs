//! Line graphs: pgr_lineGraph (edges become vertices, joined where one edge can follow
//! another) and pgr_lineGraphFull (every vertex split into one vertex per arc end, so that
//! each turn is an arc of its own).

use crate::graph::{FxMap, Graph};
use crate::search::NONE;

/// One row of pgr_lineGraph: two edges, and the cost of going from `source` to `target`
/// (the cost of the `source` edge's arc into the shared vertex) and back. A negative cost means
/// that way is not possible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineEdge {
    pub source: u32,
    pub target: u32,
    pub cost: f64,
    pub reverse_cost: f64,
}

/// The line graph (pgr_lineGraph): one row per pair of edges where one can follow the other
/// through a shared vertex, following arc directions. An edge never follows itself. Rows are
/// sorted by `(source, target)` with `source < target`.
pub fn line_graph(graph: &Graph) -> Vec<LineEdge> {
    let mut pairs: FxMap<(u32, u32), (f64, f64)> = FxMap::default();
    for v in 0..graph.num_vertices() as u32 {
        for (_, c_in, e_in) in graph.bwd.arcs(v) {
            for (_, _, e_out) in graph.fwd.arcs(v) {
                if e_in == e_out {
                    continue;
                }
                let (key, forward) = if e_in < e_out {
                    ((e_in, e_out), true)
                } else {
                    ((e_out, e_in), false)
                };
                let entry = pairs.entry(key).or_insert((f64::INFINITY, f64::INFINITY));
                let slot = if forward { &mut entry.0 } else { &mut entry.1 };
                *slot = slot.min(c_in);
            }
        }
    }
    let mut out: Vec<LineEdge> = pairs
        .into_iter()
        .map(|((source, target), (c, r))| LineEdge {
            source,
            target,
            cost: if c.is_finite() { c } else { -1.0 },
            reverse_cost: if r.is_finite() { r } else { -1.0 },
        })
        .collect();
    out.sort_by_key(|e| (e.source, e.target));
    out
}

/// The full line graph (pgr_lineGraphFull).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LineGraphFull {
    /// The original vertex of every new vertex.
    pub vertex: Vec<u32>,
    /// `(source, target, cost, edge)` over the new vertices: `edge` is the original edge for
    /// an arc along it, `None` for a turn (cost 0) inside an original vertex.
    pub arcs: Vec<(u32, u32, f64, Option<u32>)>,
}

/// Splits every vertex into one new vertex per arc end: an arc `u -> v` along edge `e` runs
/// from its own vertex at `u` to its own vertex at `v`, and inside every original vertex each
/// arriving arc's vertex is joined to each leaving arc's vertex (U-turns included).
pub fn line_graph_full(graph: &Graph) -> LineGraphFull {
    let n = graph.num_vertices();
    let mut out = LineGraphFull::default();
    // The new vertex at the tail and at the head of every forward arc slot.
    let m = graph.num_arcs();
    let mut at_tail = vec![NONE; m];
    let mut at_head = vec![NONE; m];
    // Slot of each backward arc in the forward arrays: (tail, edge, nth) matching.
    let mut arriving: Vec<Vec<usize>> = vec![Vec::new(); n];
    for v in 0..n as u32 {
        let range =
            graph.fwd.offsets[v as usize] as usize..graph.fwd.offsets[v as usize + 1] as usize;
        for slot in range {
            arriving[graph.fwd.head[slot] as usize].push(slot);
        }
    }
    for v in 0..n as u32 {
        let range =
            graph.fwd.offsets[v as usize] as usize..graph.fwd.offsets[v as usize + 1] as usize;
        let leaving: Vec<usize> = range.collect();
        for &slot in &arriving[v as usize] {
            at_head[slot] = out.vertex.len() as u32;
            out.vertex.push(v);
        }
        for &slot in &leaving {
            at_tail[slot] = out.vertex.len() as u32;
            out.vertex.push(v);
        }
        for &a in &arriving[v as usize] {
            for &b in &leaving {
                out.arcs.push((at_head[a], at_tail[b], 0.0, None));
            }
        }
    }
    for slot in 0..m {
        out.arcs.push((
            at_tail[slot],
            at_head[slot],
            graph.fwd.weight[slot],
            Some(graph.fwd.edge[slot]),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeRow;

    #[test]
    fn line_graphs() {
        // 0 -> 1 (edge 0), 1 <-> 2 (edge 1, costs 2 and 3), 1 -> 3 (edge 2).
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
                reverse_cost: Some(3.0),
            },
            EdgeRow {
                id: 2,
                source: 1,
                target: 3,
                cost: 4.0,
                reverse_cost: None,
            },
        ];
        let g = Graph::build(4, &rows, true);
        assert_eq!(
            line_graph(&g),
            [
                LineEdge {
                    source: 0,
                    target: 1,
                    cost: 1.0,
                    reverse_cost: -1.0
                },
                LineEdge {
                    source: 0,
                    target: 2,
                    cost: 1.0,
                    reverse_cost: -1.0
                },
                LineEdge {
                    source: 1,
                    target: 2,
                    cost: 3.0,
                    reverse_cost: -1.0
                },
            ]
        );
        let full = line_graph_full(&g);
        // 4 arcs, so 8 arc ends; at 1: 2 arriving (edges 0, 1) x 2 leaving (edges 1, 2) turns;
        // at 2: 1 arriving x 1 leaving (the U-turn).
        assert_eq!(full.vertex.len(), 8);
        let turns = full.arcs.iter().filter(|a| a.3.is_none()).count();
        assert_eq!(turns, 5);
        assert_eq!(full.arcs.len(), 5 + 4);
    }
}
