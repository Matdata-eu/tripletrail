//! Time-dependent shortest paths: edges whose cost differs during some periods, or which are
//! closed then (possessions, temporary speed restrictions).
//!
//! A period `[from, to)` gives an edge another cost when it is entered at a time in the
//! period (the latest-starting period wins where periods overlap); an infinite or negative
//! cost closes the edge. Costs are piecewise constant in the entry time.
//!
//! With waiting allowed, a train may wait at a vertex before entering an edge, so the arrival
//! time at the edge's head is `min over t' >= t of t' + cost(t')`, which never decreases with
//! `t` (the FIFO property). Because costs are piecewise constant, the minimum is taken at `t`
//! or at the start or end of a later period. Dijkstra on arrival times is then exact. Without
//! waiting, the arrival time is `t + cost(t)`; the search is exact only when that does not
//! decrease with `t` (for instance when no period lowers a cost).

use crate::graph::Graph;
use crate::search::{Item, Path, SearchSpace};

/// One period of an edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Period {
    pub from: f64,
    pub to: f64,
    /// Cost of entering the edge at its source during the period; infinite or negative closes
    /// that direction.
    pub cost: f64,
    /// Cost from the target to the source; `None` uses `cost`.
    pub reverse_cost: Option<f64>,
}

/// The periods of every edge, by edge id.
#[derive(Debug, Clone, Default)]
pub struct TimeTable {
    offsets: Vec<u32>,
    periods: Vec<Period>,
    /// Source vertex of every edge, to tell which way an arc runs.
    edge_source: Vec<u32>,
}

impl TimeTable {
    /// `edge_source[e]` is edge `e`'s source vertex; `periods` are `(edge, period)` pairs in
    /// any order (periods with an empty interval are dropped).
    pub fn new(edge_source: Vec<u32>, periods: impl IntoIterator<Item = (u32, Period)>) -> Self {
        let n = edge_source.len();
        let mut list: Vec<(u32, Period)> = periods
            .into_iter()
            .filter(|(e, p)| (*e as usize) < n && p.from < p.to)
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.from.total_cmp(&b.1.from)));
        let mut offsets = vec![0u32; n + 1];
        for (e, _) in &list {
            offsets[*e as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        Self {
            offsets,
            periods: list.into_iter().map(|p| p.1).collect(),
            edge_source,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.periods.is_empty()
    }

    fn of(&self, edge: u32) -> &[Period] {
        match self.offsets.get(edge as usize + 1) {
            Some(&b) => &self.periods[self.offsets[edge as usize] as usize..b as usize],
            None => &[],
        }
    }

    /// The cost of entering an edge (given its periods) at time `t`.
    #[inline]
    fn cost_at(periods: &[Period], forward: bool, base: f64, t: f64) -> f64 {
        let mut cost = base;
        for p in periods {
            if p.from > t {
                break;
            }
            if t < p.to {
                let c = if forward {
                    p.cost
                } else {
                    p.reverse_cost.unwrap_or(p.cost)
                };
                cost = if c >= 0.0 { c } else { f64::INFINITY };
            }
        }
        cost
    }

    /// The earliest arrival at the head of the arc `tail -> head` of `edge` (cost `base`
    /// outside the periods) when reaching `tail` at `t`.
    #[inline]
    pub fn arrival(&self, edge: u32, tail: u32, base: f64, t: f64, wait: bool) -> f64 {
        let periods = self.of(edge);
        if periods.is_empty() {
            return t + base;
        }
        let forward = self.edge_source.get(edge as usize) == Some(&tail);
        let mut best = t + Self::cost_at(periods, forward, base, t);
        if wait {
            // Later departures can only be better at a point where the cost changes.
            for p in periods {
                for b in [p.from, p.to] {
                    if b > t && b < best {
                        best = best.min(b + Self::cost_at(periods, forward, base, b));
                    }
                }
            }
        }
        best
    }
}

/// One-to-many time-dependent Dijkstra leaving `source` at `departure`. Each path's
/// `agg_cost` is the time elapsed since the departure and each step's `cost` the time from
/// reaching its vertex to reaching the next (waiting included).
pub fn td_dijkstra(
    graph: &Graph,
    space: &mut SearchSpace,
    table: &TimeTable,
    source: u32,
    targets: &[u32],
    departure: f64,
    wait: bool,
) -> Vec<Option<Path>> {
    space.reset(graph.num_vertices());
    space.start(source, 0.0);
    let mut remaining = targets.len();
    while let Some(Item { key: d, node: u }) = space.heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        if space.settled[u as usize] || d > space.dist[u as usize] {
            continue;
        }
        space.settled[u as usize] = true;
        if targets.contains(&u) {
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
        let t = departure + d;
        for (v, w, e) in graph.fwd.arcs(u) {
            let arrive = table.arrival(e, u, w, t, wait);
            if arrive.is_finite() {
                let nd = arrive - departure;
                space.relax(v, nd, nd, u, e, nd - d);
            }
        }
    }
    targets.iter().map(|&t| space.path_to(source, t)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeRow;

    fn row(id: u32, source: u32, target: u32, cost: f64) -> EdgeRow {
        EdgeRow {
            id,
            source,
            target,
            cost,
            reverse_cost: None,
        }
    }

    fn period(from: f64, to: f64, cost: f64) -> Period {
        Period {
            from,
            to,
            cost,
            reverse_cost: None,
        }
    }

    #[test]
    fn arrival_with_and_without_waiting() {
        let t = TimeTable::new(vec![0], [(0, period(10.0, 20.0, -1.0))]);
        // Before the closure: as normal.
        assert_eq!(t.arrival(0, 0, 5.0, 0.0, true), 5.0);
        // During it: wait until 20, or not at all.
        assert_eq!(t.arrival(0, 0, 5.0, 12.0, true), 25.0);
        assert!(t.arrival(0, 0, 5.0, 12.0, false).is_infinite());
        // A slow period that ends soon: waiting for its end is faster.
        let t = TimeTable::new(vec![0], [(0, period(0.0, 10.0, 100.0))]);
        assert_eq!(t.arrival(0, 0, 5.0, 8.0, true), 15.0);
        assert_eq!(t.arrival(0, 0, 5.0, 8.0, false), 108.0);
        // The reverse direction uses reverse_cost when given.
        let p = Period {
            reverse_cost: Some(50.0),
            ..period(0.0, 10.0, 1.0)
        };
        let t = TimeTable::new(vec![0], [(0, p)]);
        assert_eq!(t.arrival(0, 0, 5.0, 0.0, false), 1.0);
        assert_eq!(t.arrival(0, 1, 5.0, 0.0, false), 50.0);
    }

    #[test]
    fn closure_detours_or_waits() {
        // 0 -> 1 -> 3 costs 2; 0 -> 2 -> 3 costs 10. Edge 1 (1 -> 3) is closed in [0, 5).
        let rows = [
            row(0, 0, 1, 1.0),
            row(1, 1, 3, 1.0),
            row(2, 0, 2, 5.0),
            row(3, 2, 3, 5.0),
        ];
        let g = Graph::build(4, &rows, true);
        let sources: Vec<u32> = rows.iter().map(|r| r.source).collect();
        let mut space = SearchSpace::new();
        let closed = TimeTable::new(sources.clone(), [(1, period(0.0, 5.0, -1.0))]);
        // Waiting at 1 until 5 then arriving at 6 beats the detour (10).
        let p = td_dijkstra(&g, &mut space, &closed, 0, &[3], 0.0, true)[0]
            .clone()
            .unwrap();
        assert_eq!(p.total_cost(), 6.0);
        assert_eq!(p.steps[1].cost, 5.0);
        // Without waiting the detour is the only way.
        let p = td_dijkstra(&g, &mut space, &closed, 0, &[3], 0.0, false)[0]
            .clone()
            .unwrap();
        assert_eq!(p.total_cost(), 10.0);
        // A long closure makes the detour better even with waiting.
        let long = TimeTable::new(sources, [(1, period(0.0, 50.0, -1.0))]);
        let p = td_dijkstra(&g, &mut space, &long, 0, &[3], 0.0, true)[0]
            .clone()
            .unwrap();
        assert_eq!(p.total_cost(), 10.0);
        // Departing after the closure: the direct path.
        let p = td_dijkstra(&g, &mut space, &long, 0, &[3], 100.0, true)[0]
            .clone()
            .unwrap();
        assert_eq!(p.total_cost(), 2.0);
    }
}
