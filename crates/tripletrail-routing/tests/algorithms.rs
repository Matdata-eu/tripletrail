use tripletrail_routing::*;

/// Small graph:
///   0 -1-> 1 -1-> 2
///   0 ---5------> 2   (reverse_cost 1)
///   2 -1-> 3, 1 -10-> 3
fn graph(directed: bool) -> Graph {
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
            cost: 1.0,
            reverse_cost: Some(-1.0),
        },
        EdgeRow {
            id: 2,
            source: 0,
            target: 2,
            cost: 5.0,
            reverse_cost: Some(1.0),
        },
        EdgeRow {
            id: 3,
            source: 2,
            target: 3,
            cost: 1.0,
            reverse_cost: None,
        },
        EdgeRow {
            id: 4,
            source: 1,
            target: 3,
            cost: 10.0,
            reverse_cost: None,
        },
    ];
    Graph::build(4, &rows, directed).with_coords(vec![
        (0.0, 0.0),
        (1.0, 0.0),
        (2.0, 0.0),
        (3.0, 0.0),
    ])
}

fn nodes(p: &Path) -> Vec<u32> {
    p.steps.iter().map(|s| s.node).collect()
}

#[test]
fn dijkstra_directed() {
    let g = graph(true);
    let mut s = SearchSpace::new();
    let r = dijkstra(&g, &mut s, 0, &[3, 2]);
    let p = r[0].as_ref().unwrap();
    assert_eq!(nodes(p), vec![0, 1, 2, 3]);
    assert_eq!(p.total_cost(), 3.0);
    assert_eq!(p.steps[0].edge, Some(0));
    assert_eq!(p.steps[3].edge, None);
    assert_eq!(p.steps[2].agg_cost, 2.0);
    assert_eq!(r[1].as_ref().unwrap().total_cost(), 2.0);
    // 2 -> 0 only through edge 2's reverse cost.
    let back = dijkstra(&g, &mut s, 2, &[0]);
    assert_eq!(back[0].as_ref().unwrap().total_cost(), 1.0);
    // 3 has no outgoing arcs.
    assert!(dijkstra(&g, &mut s, 3, &[0])[0].is_none());
}

#[test]
fn dijkstra_undirected() {
    let g = graph(false);
    let mut s = SearchSpace::new();
    let p = dijkstra(&g, &mut s, 3, &[0]).remove(0).unwrap();
    // 3 -> 2 (1) -> 0 via edge 2 reverse cost (1)
    assert_eq!(p.total_cost(), 2.0);
    assert_eq!(nodes(&p), vec![3, 2, 0]);
}

#[test]
fn bidirectional_matches_dijkstra() {
    for directed in [true, false] {
        let g = graph(directed);
        let (mut a, mut b, mut c) = (SearchSpace::new(), SearchSpace::new(), SearchSpace::new());
        for s in 0..4 {
            for t in 0..4 {
                let d = dijkstra(&g, &mut a, s, &[t])
                    .remove(0)
                    .map(|p| p.total_cost());
                let bd = bd_dijkstra(&g, &mut b, &mut c, s, t);
                assert_eq!(
                    d,
                    bd.as_ref().map(|p| p.total_cost()),
                    "{s}->{t} directed={directed}"
                );
                if let Some(p) = bd {
                    assert_eq!(p.steps.first().unwrap().node, s);
                    assert_eq!(p.steps.last().unwrap().node, t);
                    let sum: f64 = p.steps.iter().map(|x| x.cost).sum();
                    assert_eq!(sum, p.total_cost());
                }
            }
        }
    }
}

#[test]
fn astar_matches_dijkstra() {
    let g = graph(true);
    let (mut a, mut b) = (SearchSpace::new(), SearchSpace::new());
    for s in 0..4 {
        for t in 0..4 {
            let d = dijkstra(&g, &mut a, s, &[t])
                .remove(0)
                .map(|p| p.total_cost());
            let x = astar(&g, &mut b, s, t, Heuristic::Euclidean, 1.0).map(|p| p.total_cost());
            assert_eq!(d, x, "{s}->{t}");
        }
    }
}

#[test]
fn driving_distance_limits() {
    let g = graph(true);
    let mut s = SearchSpace::new();
    let r = driving_distance(&g, &mut s, 0, 2.0);
    let mut reached: Vec<_> = r.iter().map(|n| (n.node, n.agg_cost)).collect();
    reached.sort_by_key(|x| x.0);
    assert_eq!(reached, vec![(0, 0.0), (1, 1.0), (2, 2.0)]);
    assert_eq!(r[0].pred, 0);
    assert_eq!(r[0].edge, None);
}

#[test]
fn builder_interns_keys() {
    let mut b = GraphBuilder::<String>::default();
    b.add(
        "e1".into(),
        &"a".into(),
        &"b".into(),
        2.0,
        Some(2.0),
        None,
        None,
    );
    b.add("e2".into(), &"b".into(), &"c".into(), 3.0, None, None, None);
    let ig = b.build(true);
    let (a, c) = (
        ig.vertex_id(&"a".into()).unwrap(),
        ig.vertex_id(&"c".into()).unwrap(),
    );
    let p = dijkstra(&ig.graph, &mut SearchSpace::new(), a, &[c])
        .remove(0)
        .unwrap();
    assert_eq!(p.total_cost(), 5.0);
    assert_eq!(ig.edges[p.steps[1].edge.unwrap() as usize], "e2");
    assert!(dijkstra(&ig.graph, &mut SearchSpace::new(), c, &[a])[0].is_none());
}
