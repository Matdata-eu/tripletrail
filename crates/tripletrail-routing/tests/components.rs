//! Components, cut vertices and bridges, checked against brute force on random graphs.

use tripletrail_routing::*;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Sparse random rows: some one-way, some two-way, some parallel, some unusable.
fn rows(rng: &mut Rng, n: u32, m: usize) -> Vec<EdgeRow> {
    (0..m)
        .map(|i| EdgeRow {
            id: i as u32,
            source: rng.below(n as u64) as u32,
            target: rng.below(n as u64) as u32,
            cost: if rng.below(8) == 0 { -1.0 } else { 1.0 },
            reverse_cost: (rng.below(3) == 0).then_some(1.0),
        })
        .collect()
}

/// Directed reachability over usable arcs, skipping a vertex and/or an edge.
fn reach(
    n: u32,
    rows: &[EdgeRow],
    from: u32,
    skip_v: Option<u32>,
    skip_e: Option<u32>,
    undirected: bool,
) -> Vec<bool> {
    let mut seen = vec![false; n as usize];
    if Some(from) == skip_v {
        return seen;
    }
    seen[from as usize] = true;
    let mut todo = vec![from];
    while let Some(v) = todo.pop() {
        for r in rows {
            if Some(r.id) == skip_e {
                continue;
            }
            let fwd = r.cost >= 0.0;
            let bwd = r.reverse_cost.is_some_and(|c| c >= 0.0);
            let mut next = Vec::new();
            if r.source == v && (fwd || (undirected && bwd)) {
                next.push(r.target);
            }
            if r.target == v && (bwd || (undirected && fwd)) {
                next.push(r.source);
            }
            for w in next {
                if Some(w) != skip_v && !seen[w as usize] {
                    seen[w as usize] = true;
                    todo.push(w);
                }
            }
        }
    }
    seen
}

fn count_components(n: u32, rows: &[EdgeRow], skip_v: Option<u32>, skip_e: Option<u32>) -> usize {
    let mut label = vec![false; n as usize];
    let mut count = 0;
    for v in 0..n {
        if Some(v) == skip_v || label[v as usize] {
            continue;
        }
        count += 1;
        for (w, r) in reach(n, rows, v, skip_v, skip_e, true)
            .into_iter()
            .enumerate()
        {
            label[w] |= r;
        }
    }
    count
}

#[test]
fn match_brute_force() {
    let mut rng = Rng(0x9e3779b97f4a7c15);
    for case in 0..300 {
        let n = 2 + rng.below(12) as u32;
        let m = rng.below(2 * n as u64 + 1) as usize;
        let rows = rows(&mut rng, n, m);
        let g = Graph::build(n as usize, &rows, true);

        let cc = connected_components(&g);
        let sc = strong_components(&g);
        for v in 0..n {
            let und = reach(n, &rows, v, None, None, true);
            let fwd = reach(n, &rows, v, None, None, false);
            for w in 0..n {
                assert_eq!(
                    cc[v as usize] == cc[w as usize],
                    und[w as usize],
                    "case {case}: cc {v} {w}"
                );
                let back = reach(n, &rows, w, None, None, false)[v as usize];
                assert_eq!(
                    sc[v as usize] == sc[w as usize],
                    fwd[w as usize] && back,
                    "case {case}: scc {v} {w}"
                );
            }
            // Labels are the smallest member.
            assert!(cc[v as usize] <= v && cc[cc[v as usize] as usize] == cc[v as usize]);
            assert!(sc[v as usize] <= v && sc[sc[v as usize] as usize] == sc[v as usize]);
        }

        let (cuts, bridges) = cut_vertices_and_edges(&g);
        let base = count_components(n, &rows, None, None);
        for v in 0..n {
            // Removing a cut vertex splits its component (isolated vertices excluded).
            let expected = count_components(n, &rows, Some(v), None)
                > base - usize::from(cc_size(&cc, v) == 1);
            assert_eq!(
                cuts.contains(&v),
                expected,
                "case {case}: cut vertex {v} {rows:?}"
            );
        }
        for r in &rows {
            let usable = r.cost >= 0.0 || r.reverse_cost.is_some_and(|c| c >= 0.0);
            let expected = usable
                && r.source != r.target
                && count_components(n, &rows, None, Some(r.id)) > base;
            assert_eq!(
                bridges.contains(&r.id),
                expected,
                "case {case}: bridge {} {rows:?}",
                r.id
            );
        }
    }
}

fn cc_size(cc: &[u32], v: u32) -> usize {
    cc.iter().filter(|&&c| c == cc[v as usize]).count()
}

/// Prim's algorithm on a dense matrix of cheapest undirected costs, per component.
fn prim_weight(n: u32, rows: &[EdgeRow]) -> f64 {
    let n = n as usize;
    let mut w = vec![vec![f64::INFINITY; n]; n];
    for r in rows {
        let (a, b) = (r.source as usize, r.target as usize);
        for c in [Some(r.cost), r.reverse_cost].into_iter().flatten() {
            if c >= 0.0 && a != b && c < w[a][b] {
                w[a][b] = c;
                w[b][a] = c;
            }
        }
    }
    let mut done = vec![false; n];
    let mut total = 0.0;
    for root in 0..n {
        if done[root] {
            continue;
        }
        let mut best = vec![f64::INFINITY; n];
        best[root] = 0.0;
        loop {
            let next = (0..n)
                .filter(|&v| !done[v] && best[v].is_finite())
                .min_by(|&x, &y| best[x].total_cmp(&best[y]));
            let Some(v) = next else { break };
            done[v] = true;
            total += best[v];
            for u in 0..n {
                if !done[u] && w[v][u] < best[u] {
                    best[u] = w[v][u];
                }
            }
        }
    }
    total
}

#[test]
fn spanning_forest_matches_prim() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..300 {
        let n = 2 + rng.below(12) as u32;
        let m = rng.below(30) as usize;
        let mut rows = rows(&mut rng, n, m);
        for r in &mut rows {
            if r.cost >= 0.0 {
                r.cost = rng.below(10) as f64;
            }
            if let Some(c) = &mut r.reverse_cost {
                *c = rng.below(10) as f64;
            }
        }
        let g = Graph::build(n as usize, &rows, true);
        let forest = minimum_spanning_forest(&g);
        let total: f64 = forest.iter().map(|f| f.1).sum();
        assert_eq!(total, prim_weight(n, &rows));
        let mut labels = connected_components(&g);
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(forest.len(), n as usize - labels.len());
    }
}
