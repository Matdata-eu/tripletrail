//! Multi-criteria shortest paths: the Pareto-optimal paths between two vertices under several
//! additive, non-negative costs, by Martins' label-setting algorithm.
//!
//! Every vertex keeps a bag of labels (cost vectors with a predecessor) none of which dominates
//! another; labels are settled in lexicographic order of their costs, so a settled label is
//! never dominated later. Labels dominated by one already at the target are dropped (target
//! pruning). The number of Pareto-optimal paths can grow exponentially with the network, so
//! the search is capped: at most `max_paths` labels per vertex (a full bag gives up its
//! lexicographically largest label for a smaller one), and it stops once `max_paths` paths have
//! reached the target. The paths returned are Pareto-optimal and the first is always the
//! lexicographic optimum; when the cap is hit, the set may be incomplete. Paths with equal
//! costs are reported once.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// One row of a multi-criteria edge set, with ids already made dense.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiEdge {
    pub id: u32,
    pub source: u32,
    pub target: u32,
    /// The costs `source -> target`, one per criterion; `None` for no arc.
    pub forward: Option<Vec<f64>>,
    /// The costs `target -> source`; `None` for no arc.
    pub backward: Option<Vec<f64>>,
}

/// A graph whose arcs carry one cost per criterion.
#[derive(Debug, Clone)]
pub struct MultiGraph {
    criteria: usize,
    offsets: Vec<u32>,
    head: Vec<u32>,
    edge: Vec<u32>,
    /// `costs[arc * criteria + c]`.
    costs: Vec<f64>,
}

impl MultiGraph {
    /// Builds the graph. With `directed = false` every arc is usable both ways (pgRouting's
    /// semantics). Arcs with a negative or NaN cost, or with the wrong number of costs, are
    /// left out.
    pub fn build(
        num_vertices: usize,
        criteria: usize,
        edges: &[MultiEdge],
        directed: bool,
    ) -> Self {
        let mut arcs: Vec<(u32, u32, u32, &[f64])> = Vec::with_capacity(edges.len() * 2);
        fn usable(c: &Option<Vec<f64>>, criteria: usize) -> Option<&[f64]> {
            c.as_deref()
                .filter(|c| c.len() == criteria && c.iter().all(|v| *v >= 0.0))
        }
        for e in edges {
            if let Some(c) = usable(&e.forward, criteria) {
                arcs.push((e.source, e.target, e.id, c));
                if !directed {
                    arcs.push((e.target, e.source, e.id, c));
                }
            }
            if let Some(c) = usable(&e.backward, criteria) {
                arcs.push((e.target, e.source, e.id, c));
                if !directed {
                    arcs.push((e.source, e.target, e.id, c));
                }
            }
        }
        arcs.sort_by_key(|a| a.0);
        let mut offsets = vec![0u32; num_vertices + 1];
        for a in &arcs {
            offsets[a.0 as usize + 1] += 1;
        }
        for i in 0..num_vertices {
            offsets[i + 1] += offsets[i];
        }
        Self {
            criteria,
            offsets,
            head: arcs.iter().map(|a| a.1).collect(),
            edge: arcs.iter().map(|a| a.2).collect(),
            costs: arcs.iter().flat_map(|a| a.3.iter().copied()).collect(),
        }
    }

    pub fn num_vertices(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn criteria(&self) -> usize {
        self.criteria
    }
}

/// One step of a Pareto path: the vertex, the edge taken from it (`None` on the last step),
/// that edge's costs (zeros on the last step) and the costs so far, before the edge.
#[derive(Debug, Clone, PartialEq)]
pub struct ParetoStep {
    pub node: u32,
    pub edge: Option<u32>,
    pub cost: Vec<f64>,
    pub agg_cost: Vec<f64>,
}

/// A Pareto-optimal path and its total costs.
#[derive(Debug, Clone, PartialEq)]
pub struct ParetoPath {
    pub start_vid: u32,
    pub end_vid: u32,
    pub steps: Vec<ParetoStep>,
    pub total: Vec<f64>,
}

struct Label {
    node: u32,
    /// Index of the predecessor label and the arc taken from it.
    pred: Option<(usize, usize)>,
    alive: bool,
}

/// Lexicographically ordered cost vector (smallest first in a max-heap).
struct Key(Vec<f64>, usize);

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Key {}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed: BinaryHeap is a max-heap.
        lex_cmp(&self.0, &other.0)
            .then(self.1.cmp(&other.1))
            .reverse()
    }
}

/// Lexicographic order of cost vectors.
fn lex_cmp(a: &[f64], b: &[f64]) -> Ordering {
    a.iter()
        .zip(b)
        .map(|(x, y)| x.total_cmp(y))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Lexicographic order of labels `a` and `b` by their costs.
fn lex(costs: &[f64], k: usize, a: usize, b: usize) -> Ordering {
    lex_cmp(&costs[a * k..(a + 1) * k], &costs[b * k..(b + 1) * k])
}

/// Whether `a` dominates or equals `b` (no worse in every criterion).
fn covers(a: &[f64], b: &[f64]) -> bool {
    a.iter().zip(b).all(|(x, y)| x <= y)
}

/// The Pareto-optimal paths from `source` to `target`, at most `max_paths` of them, in
/// lexicographic order of their costs. Empty when the target is unreachable or the two are
/// the same vertex.
pub fn pareto_paths(
    graph: &MultiGraph,
    source: u32,
    target: u32,
    max_paths: usize,
) -> Vec<ParetoPath> {
    let n = graph.num_vertices();
    let k = graph.criteria;
    if source as usize >= n || target as usize >= n || source == target || max_paths == 0 {
        return Vec::new();
    }
    let mut labels: Vec<Label> = Vec::new();
    let mut costs: Vec<f64> = Vec::new();
    let cost_of = |costs: &[f64], l: usize| costs[l * k..(l + 1) * k].to_vec();
    let mut bags: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut heap = BinaryHeap::new();
    labels.push(Label {
        node: source,
        pred: None,
        alive: true,
    });
    costs.extend(std::iter::repeat_n(0.0, k));
    bags[source as usize].push(0);
    heap.push(Key(vec![0.0; k], 0));
    let mut settled_at_target: Vec<usize> = Vec::new();
    while let Some(Key(current, l)) = heap.pop() {
        if crate::cancel::stopped() {
            break;
        }
        if !labels[l].alive {
            continue;
        }
        let v = labels[l].node;
        if v == target {
            settled_at_target.push(l);
            if settled_at_target.len() >= max_paths {
                break;
            }
            continue;
        }
        let (a, b) = (
            graph.offsets[v as usize] as usize,
            graph.offsets[v as usize + 1] as usize,
        );
        for arc in a..b {
            let w = graph.head[arc];
            let next: Vec<f64> = current
                .iter()
                .zip(&graph.costs[arc * k..(arc + 1) * k])
                .map(|(x, c)| x + c)
                .collect();
            // Target pruning, then the head's bag.
            if bags[target as usize]
                .iter()
                .any(|&t| covers(&costs[t * k..(t + 1) * k], &next))
            {
                continue;
            }
            let bag = &mut bags[w as usize];
            if bag
                .iter()
                .any(|&o| covers(&costs[o * k..(o + 1) * k], &next))
            {
                continue;
            }
            bag.retain(|&o| {
                let dominated = covers(&next, &costs[o * k..(o + 1) * k]);
                if dominated {
                    labels[o].alive = false;
                }
                !dominated
            });
            if bag.len() >= max_paths {
                // A full bag keeps its lexicographically smallest labels, so the paths
                // returned under the cap are the lexicographically first. The label given up
                // is never settled: settled labels are no greater than `next`.
                let (slot, &largest) = bag
                    .iter()
                    .enumerate()
                    .max_by(|a, b| lex(&costs, k, *a.1, *b.1))
                    .expect("max_paths >= 1");
                if lex_cmp(&next, &costs[largest * k..(largest + 1) * k]).is_ge() {
                    continue;
                }
                labels[largest].alive = false;
                bag.swap_remove(slot);
            }
            let id = labels.len();
            labels.push(Label {
                node: w,
                pred: Some((l, arc)),
                alive: true,
            });
            costs.extend_from_slice(&next);
            bag.push(id);
            heap.push(Key(next, id));
        }
    }
    settled_at_target
        .into_iter()
        .map(|l| {
            let total = cost_of(&costs, l);
            let mut steps = vec![ParetoStep {
                node: target,
                edge: None,
                cost: vec![0.0; k],
                agg_cost: total.clone(),
            }];
            let mut at = l;
            while let Some((p, arc)) = labels[at].pred {
                steps.push(ParetoStep {
                    node: labels[p].node,
                    edge: Some(graph.edge[arc]),
                    cost: graph.costs[arc * k..(arc + 1) * k].to_vec(),
                    agg_cost: cost_of(&costs, p),
                });
                at = p;
            }
            steps.reverse();
            ParetoPath {
                start_vid: source,
                end_vid: target,
                steps,
                total,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(id: u32, s: u32, t: u32, c: &[f64]) -> MultiEdge {
        MultiEdge {
            id,
            source: s,
            target: t,
            forward: Some(c.to_vec()),
            backward: None,
        }
    }

    /// 0 -> 3 three ways: fast and costly, slow and cheap, and a dominated middle one; plus a
    /// balanced two-edge route.
    fn sample() -> MultiGraph {
        let edges = [
            edge(0, 0, 3, &[1.0, 10.0]),
            edge(1, 0, 3, &[10.0, 1.0]),
            edge(2, 0, 3, &[6.0, 6.0]),
            edge(3, 0, 1, &[2.0, 2.0]),
            edge(4, 1, 3, &[2.0, 2.0]),
            edge(5, 0, 2, &[7.0, 7.0]),
            edge(6, 2, 3, &[0.0, 0.0]),
        ];
        MultiGraph::build(4, 2, &edges, true)
    }

    #[test]
    fn finds_the_pareto_front() {
        let paths = pareto_paths(&sample(), 0, 3, 100);
        let totals: Vec<Vec<f64>> = paths.iter().map(|p| p.total.clone()).collect();
        assert_eq!(totals, [vec![1.0, 10.0], vec![4.0, 4.0], vec![10.0, 1.0]]);
        let p = &paths[1];
        let nodes: Vec<u32> = p.steps.iter().map(|s| s.node).collect();
        assert_eq!(nodes, [0, 1, 3]);
        assert_eq!(p.steps[0].edge, Some(3));
        assert_eq!(p.steps[1].agg_cost, vec![2.0, 2.0]);
        assert_eq!(p.steps[2].edge, None);
    }

    #[test]
    fn caps_and_degenerate_cases() {
        let g = sample();
        assert_eq!(pareto_paths(&g, 0, 3, 2).len(), 2);
        // With room for one label per vertex, the lexicographic optimum still wins, although
        // other labels reach the target first.
        let edges = [
            edge(0, 0, 2, &[10.0, 1.0]),
            edge(1, 0, 1, &[1.0, 5.0]),
            edge(2, 1, 2, &[1.0, 5.0]),
        ];
        let g = MultiGraph::build(3, 2, &edges, true);
        let one = pareto_paths(&g, 0, 2, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].total, vec![2.0, 10.0]);
        assert_eq!(pareto_paths(&g, 0, 2, 5).len(), 2);
        assert!(pareto_paths(&g, 3, 0, 10).is_empty());
        assert!(pareto_paths(&g, 0, 0, 10).is_empty());
        // Undirected, the reverse direction works too.
        let edges = [edge(0, 0, 1, &[1.0, 2.0])];
        let u = MultiGraph::build(2, 2, &edges, false);
        assert_eq!(pareto_paths(&u, 1, 0, 10)[0].total, vec![1.0, 2.0]);
    }

    #[test]
    fn one_criterion_is_the_shortest_path() {
        let edges = [
            edge(0, 0, 1, &[1.0]),
            edge(1, 1, 2, &[1.0]),
            edge(2, 0, 2, &[3.0]),
        ];
        let g = MultiGraph::build(3, 1, &edges, true);
        let paths = pareto_paths(&g, 0, 2, 100);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].total, vec![2.0]);
    }
}
