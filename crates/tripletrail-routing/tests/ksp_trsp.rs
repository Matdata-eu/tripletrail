//! K shortest paths and turn restrictions, checked against brute force on random graphs.

use std::collections::{BinaryHeap, HashMap};
use tripletrail_routing::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_rows(rng: &mut Rng, n: u32, m: usize) -> Vec<EdgeRow> {
    (0..m)
        .map(|i| {
            let source = rng.below(n as u64) as u32;
            let mut target = rng.below(n as u64) as u32;
            if target == source {
                target = (target + 1) % n;
            }
            EdgeRow {
                id: i as u32,
                source,
                target,
                cost: 1.0 + rng.below(9) as f64,
                reverse_cost: if rng.below(3) == 0 {
                    Some(1.0 + rng.below(9) as f64)
                } else {
                    None
                },
            }
        })
        .collect()
}

/// Outgoing arcs (head, cost, edge) per vertex, as `Graph::build(.., directed = true)` sees them.
fn adjacency(n: u32, rows: &[EdgeRow]) -> Vec<Vec<(u32, f64, u32)>> {
    let mut adj = vec![Vec::new(); n as usize];
    for r in rows {
        adj[r.source as usize].push((r.target, r.cost, r.id));
        if let Some(rc) = r.reverse_cost.filter(|c| *c >= 0.0) {
            adj[r.target as usize].push((r.source, rc, r.id));
        }
    }
    adj
}

fn simple_path_costs(adj: &[Vec<(u32, f64, u32)>], s: u32, t: u32) -> Vec<f64> {
    fn walk(
        adj: &[Vec<(u32, f64, u32)>],
        u: u32,
        t: u32,
        cost: f64,
        on: &mut Vec<bool>,
        out: &mut Vec<f64>,
    ) {
        if u == t {
            out.push(cost);
            return;
        }
        for &(v, w, _) in &adj[u as usize] {
            if !on[v as usize] {
                on[v as usize] = true;
                walk(adj, v, t, cost + w, on, out);
                on[v as usize] = false;
            }
        }
    }
    let mut on = vec![false; adj.len()];
    on[s as usize] = true;
    let mut out = Vec::new();
    walk(adj, s, t, 0.0, &mut on, &mut out);
    out.sort_by(f64::total_cmp);
    out
}

#[test]
fn ksp_matches_enumeration() {
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut space = SearchSpace::new();
    for _ in 0..200 {
        let n = 3 + rng.below(6) as u32;
        let m = 2 + rng.below(14) as usize;
        let rows = random_rows(&mut rng, n, m);
        let g = Graph::build(n as usize, &rows, true);
        let adj = adjacency(n, &rows);
        let (s, t) = (rng.below(n as u64) as u32, rng.below(n as u64) as u32);
        if s == t {
            continue;
        }
        let k = 1 + rng.below(6) as usize;
        let expected = simple_path_costs(&adj, s, t);
        let paths = ksp(&g, &mut space, s, t, k, false);
        let got: Vec<f64> = paths.iter().map(Path::total_cost).collect();
        assert_eq!(
            got,
            expected[..expected.len().min(k)],
            "{rows:?} {s}->{t} k={k}"
        );
        for p in &paths {
            let mut seen = vec![false; n as usize];
            for st in &p.steps {
                assert!(!seen[st.node as usize], "loop in {p:?}");
                seen[st.node as usize] = true;
            }
            assert_eq!((p.steps[0].node, p.steps.last().unwrap().node), (s, t));
        }
        let with_heap = ksp(&g, &mut space, s, t, k, true);
        assert_eq!(with_heap[..paths.len()], paths[..]);
    }
}

/// Reference for restrictions of length <= 2: Dijkstra over (vertex, last edge).
fn reference_trsp(
    adj: &[Vec<(u32, f64, u32)>],
    pairs: &HashMap<(u32, u32), f64>,
    singles: &HashMap<u32, f64>,
    s: u32,
    t: u32,
) -> Option<f64> {
    let mut dist: HashMap<(u32, u32), f64> = HashMap::new();
    let mut heap = BinaryHeap::new();
    heap.push((std::cmp::Reverse(0u64), s, u32::MAX));
    dist.insert((s, u32::MAX), 0.0);
    while let Some((std::cmp::Reverse(d), u, last)) = heap.pop() {
        let d = d as f64 / 1000.0;
        if d > dist[&(u, last)] {
            continue;
        }
        if u == t {
            return Some(d);
        }
        for &(v, w, e) in &adj[u as usize] {
            let extra = singles.get(&e).copied().unwrap_or(0.0)
                + pairs.get(&(last, e)).copied().unwrap_or(0.0);
            if extra.is_infinite() {
                continue;
            }
            let nd = d + w + extra;
            if dist.get(&(v, e)).is_none_or(|&old| nd < old) {
                dist.insert((v, e), nd);
                heap.push((std::cmp::Reverse((nd * 1000.0) as u64), v, e));
            }
        }
    }
    None
}

#[test]
fn trsp_matches_reference() {
    let mut rng = Rng(0x2545f4914f6cdd1d);
    for _ in 0..300 {
        let n = 3 + rng.below(8) as u32;
        let m = 3 + rng.below(20) as usize;
        let rows = random_rows(&mut rng, n, m);
        let g = Graph::build(n as usize, &rows, true);
        let adj = adjacency(n, &rows);
        let mut list = Vec::new();
        let mut pairs = HashMap::new();
        let mut singles = HashMap::new();
        for _ in 0..rng.below(6) {
            let cost = if rng.below(2) == 0 {
                f64::INFINITY
            } else {
                1.0 + rng.below(20) as f64
            };
            let a = rng.below(m as u64) as u32;
            if rng.below(4) == 0 {
                *singles.entry(a).or_insert(0.0) += cost;
                list.push(Restriction {
                    path: vec![a],
                    cost,
                });
            } else {
                let b = rng.below(m as u64) as u32;
                *pairs.entry((a, b)).or_insert(0.0) += cost;
                list.push(Restriction {
                    path: vec![a, b],
                    cost,
                });
            }
        }
        let r = Restrictions::new(&list);
        let s = rng.below(n as u64) as u32;
        let targets: Vec<u32> = (0..n).filter(|&t| t != s).collect();
        let got = trsp(&g, &mut SearchSpace::new(), &r, s, &targets);
        for (t, p) in targets.iter().zip(got) {
            let expected = reference_trsp(&adj, &pairs, &singles, s, *t);
            assert_eq!(
                p.as_ref().map(Path::total_cost),
                expected,
                "{rows:?} {list:?} {s}->{t}"
            );
            if let Some(p) = p {
                // The reported steps are a real walk whose costs add up.
                let mut agg = 0.0;
                for w in p.steps.windows(2) {
                    let e = w[0].edge.unwrap();
                    assert!(
                        adj[w[0].node as usize]
                            .iter()
                            .any(|&(v, _, id)| v == w[1].node && id == e)
                    );
                    assert_eq!(w[0].agg_cost, agg);
                    agg += w[0].cost;
                }
            }
        }
    }
}

#[test]
fn trsp_without_restrictions_is_dijkstra() {
    let mut rng = Rng(7);
    let mut space = SearchSpace::new();
    for _ in 0..100 {
        let n = 2 + rng.below(30) as u32;
        let m = 1 + rng.below(80) as usize;
        let rows = random_rows(&mut rng, n, m);
        let directed = rng.below(2) == 0;
        let g = Graph::build(n as usize, &rows, directed);
        let targets: Vec<u32> = (1..n).collect();
        // Force the restricted search with a restriction on an edge id that does not exist.
        let unused = Restrictions::new(&[Restriction {
            path: vec![u32::MAX],
            cost: 1.0,
        }]);
        let a = trsp(&g, &mut SearchSpace::new(), &unused, 0, &targets);
        let b = dijkstra(&g, &mut space, 0, &targets);
        let cost = |v: Vec<Option<Path>>| {
            v.iter()
                .map(|p| p.as_ref().map(Path::total_cost))
                .collect::<Vec<_>>()
        };
        assert_eq!(cost(a), cost(b));
    }
}

#[test]
fn longer_restriction_sequences() {
    // A straight line 0-1-2-3 (edges 0,1,2) with a detour 1-4-2 (edges 3,4), all cost 1.
    let row = |id, source, target| EdgeRow {
        id,
        source,
        target,
        cost: 1.0,
        reverse_cost: None,
    };
    let rows = [
        row(0, 0, 1),
        row(1, 1, 2),
        row(2, 2, 3),
        row(3, 1, 4),
        row(4, 4, 2),
    ];
    let g = Graph::build(5, &rows, true);
    let only = |list: &[Restriction]| {
        let p = trsp(
            &g,
            &mut SearchSpace::new(),
            &Restrictions::new(list),
            0,
            &[3],
        )
        .remove(0)
        .unwrap();
        p.steps.iter().filter_map(|s| s.edge).collect::<Vec<_>>()
    };
    assert_eq!(only(&[]), vec![0, 1, 2]);
    // Forbidding 0,1,2 forces the detour; forbidding 1,2 alone does too.
    assert_eq!(
        only(&[Restriction {
            path: vec![0, 1, 2],
            cost: f64::INFINITY
        }]),
        vec![0, 3, 4, 2]
    );
    assert_eq!(
        only(&[Restriction {
            path: vec![1, 2],
            cost: f64::INFINITY
        }]),
        vec![0, 3, 4, 2]
    );
    // A cheap penalty is paid rather than detouring.
    let p = trsp(
        &g,
        &mut SearchSpace::new(),
        &Restrictions::new(&[Restriction {
            path: vec![0, 1, 2],
            cost: 0.5,
        }]),
        0,
        &[3],
    );
    assert_eq!(p[0].as_ref().unwrap().total_cost(), 3.5);
    // Overlapping sequences: 0,1 is fine, 3,4,2 is forbidden, 1,2 is forbidden: unreachable.
    let r = [
        Restriction {
            path: vec![1, 2],
            cost: f64::INFINITY,
        },
        Restriction {
            path: vec![3, 4, 2],
            cost: f64::INFINITY,
        },
    ];
    assert!(trsp(&g, &mut SearchSpace::new(), &Restrictions::new(&r), 0, &[3])[0].is_none());
}
