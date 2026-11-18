//! Shortest paths under turn restrictions (pgr_trsp).
//!
//! A restriction is a sequence of edges; traversing the whole sequence consecutively adds its
//! cost, or is forbidden when the cost is infinite. The search runs over (vertex, automaton
//! state) pairs, where the automaton (Aho-Corasick over edge ids) tracks how much of any
//! restricted sequence the path is currently in. Away from restrictions the automaton stays in
//! its root state, so the search is a plain Dijkstra there.

use crate::graph::{FxMap, Graph};
use crate::search::{Path, PathStep, SearchSpace, dijkstra};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// One restricted edge sequence (pgRouting's `restrictions_sql` row: `path`, `cost`).
#[derive(Debug, Clone, PartialEq)]
pub struct Restriction {
    /// Dense edge ids, in traversal order.
    pub path: Vec<u32>,
    /// Added when the whole sequence is traversed; `f64::INFINITY` forbids it.
    pub cost: f64,
}

/// The restriction automaton. Build once per restriction set and reuse across searches.
#[derive(Debug)]
pub struct Restrictions {
    goto: FxMap<(u32, u32), u32>,
    fail: Vec<u32>,
    /// Total cost of the restrictions completed on entering a state (including suffixes).
    penalty: Vec<f64>,
    /// Whether entering a state completes at least one restriction (including suffixes).
    complete: Vec<bool>,
}

impl Default for Restrictions {
    /// No restrictions.
    fn default() -> Self {
        Self::new(&[])
    }
}

impl Restrictions {
    pub fn new(restrictions: &[Restriction]) -> Self {
        let mut goto = FxMap::default();
        let mut penalty = vec![0.0];
        let mut ends = std::collections::HashSet::new();
        for r in restrictions {
            if r.path.is_empty() {
                continue;
            }
            let mut state = 0;
            for &e in &r.path {
                state = *goto.entry((state, e)).or_insert_with(|| {
                    penalty.push(0.0);
                    penalty.len() as u32 - 1
                });
            }
            penalty[state as usize] += r.cost;
            ends.insert(state);
        }
        let mut complete: Vec<bool> = (0..penalty.len() as u32)
            .map(|s| ends.contains(&s))
            .collect();
        // Breadth-first failure links, as in Aho-Corasick.
        let mut children: Vec<Vec<(u32, u32)>> = vec![Vec::new(); penalty.len()];
        for (&(from, e), &to) in &goto {
            children[from as usize].push((e, to));
        }
        let mut fail = vec![0u32; penalty.len()];
        let mut queue: std::collections::VecDeque<u32> =
            children[0].iter().map(|&(_, to)| to).collect();
        while let Some(state) = queue.pop_front() {
            for &(e, to) in &children[state as usize] {
                let mut f = fail[state as usize];
                let next = loop {
                    if let Some(&n) = goto.get(&(f, e)) {
                        break n;
                    }
                    if f == 0 {
                        break 0;
                    }
                    f = fail[f as usize];
                };
                fail[to as usize] = next;
                penalty[to as usize] += penalty[next as usize];
                complete[to as usize] |= complete[next as usize];
                queue.push_back(to);
            }
        }
        Self {
            goto,
            fail,
            penalty,
            complete,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.goto.is_empty()
    }

    /// Whether the edge sequence traverses any restricted sequence consecutively.
    pub fn violated_by(&self, edges: impl IntoIterator<Item = u32>) -> bool {
        let mut state = 0;
        for e in edges {
            state = self.step(state, e);
            if self.complete[state as usize] {
                return true;
            }
        }
        false
    }

    #[inline]
    fn step(&self, mut state: u32, edge: u32) -> u32 {
        loop {
            if let Some(&n) = self.goto.get(&(state, edge)) {
                return n;
            }
            if state == 0 {
                return 0;
            }
            state = self.fail[state as usize];
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Item {
    key: f64,
    vertex: u32,
    state: u32,
}

impl Eq for Item {}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.total_cmp(&self.key)
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Label {
    dist: f64,
    pred: Option<(u32, u32)>,
    edge: u32,
    cost: f64,
    settled: bool,
}

/// One-to-many shortest paths under `restrictions`. Returns one entry per target, `None` when
/// unreachable. The `cost` of a step includes any restriction cost completed by that step.
/// Without restrictions this is [`dijkstra`].
pub fn trsp(
    graph: &Graph,
    space: &mut SearchSpace,
    restrictions: &Restrictions,
    source: u32,
    targets: &[u32],
) -> Vec<Option<Path>> {
    trsp_avoiding(graph, space, restrictions, source, targets, None)
}

/// [`trsp`], except that the path may not leave `source` along edge `avoid` (pgr_trspVia with
/// `U_turn_on_edge` false: a leg does not go back along the edge the previous leg arrived by).
pub fn trsp_avoiding(
    graph: &Graph,
    space: &mut SearchSpace,
    restrictions: &Restrictions,
    source: u32,
    targets: &[u32],
    avoid: Option<u32>,
) -> Vec<Option<Path>> {
    if restrictions.is_empty() && avoid.is_none() {
        return dijkstra(graph, space, source, targets);
    }
    let mut labels: FxMap<(u32, u32), Label> = FxMap::default();
    let mut heap = BinaryHeap::new();
    let mut best: FxMap<u32, (u32, u32)> = FxMap::default();
    let mut remaining: FxMap<u32, ()> = targets.iter().map(|&t| (t, ())).collect();
    labels.insert(
        (source, 0),
        Label {
            dist: 0.0,
            pred: None,
            edge: u32::MAX,
            cost: 0.0,
            settled: false,
        },
    );
    heap.push(Item {
        key: 0.0,
        vertex: source,
        state: 0,
    });
    while let Some(Item {
        key: d,
        vertex: u,
        state,
    }) = heap.pop()
    {
        let label = labels.get_mut(&(u, state)).unwrap();
        if label.settled || d > label.dist {
            continue;
        }
        label.settled = true;
        if remaining.remove(&u).is_some() {
            best.insert(u, (u, state));
            if remaining.is_empty() {
                break;
            }
        }
        let first = u == source && state == 0 && label.pred.is_none();
        for (v, w, e) in graph.fwd.arcs(u) {
            if first && avoid == Some(e) {
                continue;
            }
            let next = restrictions.step(state, e);
            let w = w + restrictions.penalty[next as usize];
            if w.is_infinite() {
                continue;
            }
            let nd = d + w;
            let entry = labels.entry((v, next)).or_insert(Label {
                dist: f64::INFINITY,
                pred: None,
                edge: u32::MAX,
                cost: 0.0,
                settled: false,
            });
            if nd < entry.dist {
                *entry = Label {
                    dist: nd,
                    pred: Some((u, state)),
                    edge: e,
                    cost: w,
                    settled: false,
                };
                heap.push(Item {
                    key: nd,
                    vertex: v,
                    state: next,
                });
            }
        }
    }
    targets
        .iter()
        .map(|t| {
            let &end = best.get(t)?;
            let mut rev = Vec::new();
            let mut at = end;
            while let Some(pred) = labels[&at].pred {
                rev.push((pred.0, labels[&at].edge, labels[&at].cost));
                at = pred;
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
                node: *t,
                edge: None,
                cost: 0.0,
                agg_cost: agg,
            });
            Some(Path {
                start_vid: source,
                end_vid: *t,
                steps,
            })
        })
        .collect()
}

/// pgr_turnRestrictedPath: Yen's `k` shortest loopless paths from `s` to `t`, checked against
/// `restrictions`, which here only forbid (their costs are not added).
///
/// With `stop_on_first`, the first path that traverses no restricted sequence is the answer.
/// Otherwise all `k` paths (plus Yen's remaining candidates with `heap_paths`) are returned;
/// with `strict`, only those that traverse no restricted sequence. When no path complies and
/// `strict` is false, the paths found are returned as they are.
#[allow(clippy::too_many_arguments)]
pub fn turn_restricted_path(
    graph: &Graph,
    space: &mut SearchSpace,
    restrictions: &Restrictions,
    s: u32,
    t: u32,
    k: usize,
    heap_paths: bool,
    stop_on_first: bool,
    strict: bool,
) -> Vec<Path> {
    let paths = crate::ksp::ksp(graph, space, s, t, k, heap_paths);
    let complies = |p: &Path| !restrictions.violated_by(p.steps.iter().filter_map(|st| st.edge));
    if stop_on_first {
        if let Some(p) = paths.iter().find(|p| complies(p)) {
            return vec![p.clone()];
        }
        return if strict { Vec::new() } else { paths };
    }
    if strict {
        paths.into_iter().filter(complies).collect()
    } else {
        paths
    }
}

#[cfg(test)]
mod turn_restricted_tests {
    use super::*;
    use crate::graph::EdgeRow;

    #[test]
    fn violation_follows_consecutive_edges() {
        let r = Restrictions::new(&[
            Restriction {
                path: vec![1, 2],
                cost: 100.0,
            },
            Restriction {
                path: vec![5],
                cost: 0.0,
            },
        ]);
        assert!(r.violated_by([0, 1, 2, 3]));
        assert!(!r.violated_by([1, 3, 2]));
        assert!(r.violated_by([3, 5]));
        assert!(!r.violated_by([]));
    }

    #[test]
    fn first_compliant_path() {
        // 0 -e0-> 1 -e1-> 3 (cost 2), 0 -e2-> 2 -e3-> 3 (cost 4); e0 then e1 is forbidden.
        let row = |id, source, target, cost| EdgeRow {
            id,
            source,
            target,
            cost,
            reverse_cost: None,
        };
        let g = Graph::build(
            4,
            &[
                row(0, 0, 1, 1.0),
                row(1, 1, 3, 1.0),
                row(2, 0, 2, 2.0),
                row(3, 2, 3, 2.0),
            ],
            true,
        );
        let mut space = SearchSpace::new();
        let r = Restrictions::new(&[Restriction {
            path: vec![0, 1],
            cost: f64::INFINITY,
        }]);
        let p = turn_restricted_path(&g, &mut space, &r, 0, 3, 2, false, true, false);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].total_cost(), 4.0);
        let all = turn_restricted_path(&g, &mut space, &r, 0, 3, 2, false, false, false);
        assert_eq!(all.len(), 2);
        let strict = turn_restricted_path(&g, &mut space, &r, 0, 3, 2, false, false, true);
        assert_eq!(strict.len(), 1);
        let none = turn_restricted_path(&g, &mut space, &r, 0, 3, 1, false, true, true);
        assert!(none.is_empty());
        let loose = turn_restricted_path(&g, &mut space, &r, 0, 3, 1, false, true, false);
        assert_eq!(loose[0].total_cost(), 2.0);
    }
}
