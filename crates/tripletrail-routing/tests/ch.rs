//! Contraction hierarchy queries checked against Dijkstra on random graphs.

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

fn random_rows(rng: &mut Rng, n: u32, m: usize) -> Vec<EdgeRow> {
    (0..m)
        .map(|i| EdgeRow {
            id: i as u32,
            source: rng.below(n as u64) as u32,
            target: rng.below(n as u64) as u32,
            cost: if rng.below(10) == 0 {
                -1.0
            } else {
                rng.below(20) as f64
            },
            reverse_cost: match rng.below(3) {
                0 => None,
                1 => Some(-1.0),
                _ => Some(rng.below(20) as f64),
            },
        })
        .collect()
}

/// A path is a chain of real arcs adding up to its costs.
fn check_path(rows: &[EdgeRow], directed: bool, p: &Path) {
    let mut agg = 0.0;
    for w in p.steps.windows(2) {
        let (a, b) = (w[0], w[1]);
        assert_eq!(a.agg_cost, agg);
        let e = rows[a.edge.unwrap() as usize];
        let mut ok = false;
        for (from, to, c) in [
            (e.source, e.target, Some(e.cost)),
            (e.target, e.source, e.reverse_cost),
        ] {
            let Some(c) = c.filter(|c| *c >= 0.0) else {
                continue;
            };
            let fits =
                (from == a.node && to == b.node) || (!directed && from == b.node && to == a.node);
            ok |= fits && c == a.cost;
        }
        assert!(ok, "step {a:?} -> {b:?} does not follow edge {e:?}");
        agg += a.cost;
    }
    assert_eq!(p.total_cost(), agg);
}

#[test]
fn hierarchy_matches_dijkstra() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut space = SearchSpace::new();
    let mut ch_space = ChSpace::new();
    for round in 0..200 {
        let n = 2 + rng.below(40) as u32;
        let m = rng.below(120) as usize;
        let rows = random_rows(&mut rng, n, m);
        let directed = round % 3 != 0;
        let g = Graph::build(n as usize, &rows, directed);
        let ch = ContractionHierarchy::build(&g);
        let targets: Vec<u32> = (0..n).collect();
        for s in 0..n {
            let expected = dijkstra(&g, &mut space, s, &targets);
            let got = ch_dijkstra(&ch, &mut ch_space, s, &targets);
            for (e, c) in expected.iter().zip(&got) {
                assert_eq!(
                    e.as_ref().map(Path::total_cost),
                    c.as_ref().map(Path::total_cost),
                    "round {round}, from {s}"
                );
                if let Some(c) = c {
                    assert_eq!(c.start_vid, s);
                    check_path(&rows, directed, c);
                }
            }
        }
    }
}

#[test]
fn customizable_hierarchy_matches_dijkstra() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut space = SearchSpace::new();
    let mut cch_space = CchSpace::new();
    for round in 0..200 {
        let n = 2 + rng.below(40) as u32;
        let m = rng.below(120) as usize;
        let rows = random_rows(&mut rng, n, m);
        let directed = round % 3 != 0;
        let g = Graph::build(n as usize, &rows, directed);
        let mut cch = Cch::build(&g);
        // Same topology, new costs: customizing again matches Dijkstra on the new graph.
        let mut rows2 = rows.clone();
        for r in &mut rows2 {
            if r.cost >= 0.0 {
                r.cost = rng.below(20) as f64;
            }
        }
        let g2 = Graph::build(n as usize, &rows2, directed);
        let targets: Vec<u32> = (0..n).collect();
        for (graph, rows) in [(&g, &rows), (&g2, &rows2)] {
            assert!(cch.customize(graph));
            for s in 0..n {
                let expected = dijkstra(graph, &mut space, s, &targets);
                let got = cch_dijkstra(&cch, &mut cch_space, s, &targets);
                for (e, c) in expected.iter().zip(&got) {
                    assert_eq!(
                        e.as_ref().map(Path::total_cost),
                        c.as_ref().map(Path::total_cost),
                        "round {round}, from {s}"
                    );
                    if let Some(c) = c {
                        assert_eq!(c.start_vid, s);
                        check_path(rows, directed, c);
                    }
                }
            }
        }
    }
}

#[test]
fn customizing_rejects_a_new_connection() {
    let rows = vec![
        EdgeRow {
            id: 0,
            source: 0,
            target: 1,
            cost: 1.0,
            reverse_cost: None,
        },
        EdgeRow {
            id: 1,
            source: 2,
            target: 3,
            cost: 1.0,
            reverse_cost: None,
        },
    ];
    let mut cch = Cch::build(&Graph::build(4, &rows, true));
    let mut more = rows.clone();
    more.push(EdgeRow {
        id: 2,
        source: 1,
        target: 2,
        cost: 1.0,
        reverse_cost: None,
    });
    assert!(!cch.customize(&Graph::build(4, &more, true)));
    assert!(cch.customize(&Graph::build(4, &rows, true)));
}

#[test]
fn graph_and_hierarchy_survive_a_round_trip() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut b = GraphBuilder::<String>::default();
    for (i, r) in random_rows(&mut rng, 30, 80).iter().enumerate() {
        b.add(
            format!("e{i}"),
            &format!("v{}", r.source),
            &format!("v{}", r.target),
            r.cost,
            r.reverse_cost,
            Some((r.source as f64, 0.0)),
            Some((r.target as f64, 1.0)),
        );
    }
    let g = b.build(true);
    let ch = Cch::build(&g.graph);
    let mut bytes = Vec::new();
    g.write(&mut bytes, true, Some(&ch), Clone::clone);
    let (back, directed, back_ch) =
        InternedGraph::<String>::read(&bytes, |s| Some(s.to_owned())).unwrap();
    assert!(directed);
    assert_eq!(back.vertices, g.vertices);
    assert_eq!(back.edges, g.edges);
    assert_eq!(back.rows, g.rows);
    assert_eq!(back.graph.has_coords(), g.graph.has_coords());
    let back_ch = back_ch.unwrap();
    let (mut s1, mut s2) = (CchSpace::new(), CchSpace::new());
    let targets: Vec<u32> = (0..g.graph.num_vertices() as u32).collect();
    for s in 0..g.graph.num_vertices() as u32 {
        assert_eq!(
            cch_dijkstra(&ch, &mut s1, s, &targets),
            cch_dijkstra(&back_ch, &mut s2, s, &targets)
        );
    }
    // Truncated or foreign bytes read as nothing.
    assert!(
        InternedGraph::<String>::read(&bytes[..bytes.len() - 1], |s| Some(s.to_owned())).is_none()
    );
    assert!(InternedGraph::<String>::read(b"not a graph", |s| Some(s.to_owned())).is_none());
}

#[test]
fn relabelled_hierarchy_matches_dijkstra() {
    let mut rng = Rng(0x5151_7a7a_0101_9e9e);
    let mut space = SearchSpace::new();
    let mut cch_space = CchSpace::new();
    for round in 0..50 {
        let n = 2 + rng.below(30) as u32;
        let m = rng.below(80) as usize;
        let rows = random_rows(&mut rng, n, m);
        let cch = Cch::build(&Graph::build(n as usize, &rows, true));
        // The same graph with its vertices numbered in reverse.
        let flip = |v: u32| n - 1 - v;
        let flipped: Vec<EdgeRow> = rows
            .iter()
            .map(|r| EdgeRow {
                source: flip(r.source),
                target: flip(r.target),
                ..*r
            })
            .collect();
        let g = Graph::build(n as usize, &flipped, true);
        let old_id: Vec<u32> = (0..n).map(flip).collect();
        let mut moved = cch.relabel(&old_id).unwrap();
        assert!(moved.customize(&g), "round {round}");
        let targets: Vec<u32> = (0..n).collect();
        for s in 0..n {
            let expected = dijkstra(&g, &mut space, s, &targets);
            let got = cch_dijkstra(&moved, &mut cch_space, s, &targets);
            for (e, c) in expected.iter().zip(&got) {
                assert_eq!(
                    e.as_ref().map(Path::total_cost),
                    c.as_ref().map(Path::total_cost)
                );
            }
        }
    }
}
