//! Edge filters, corridor searches and constrained hierarchies.

use tripletrail_routing::*;

fn row(id: u32, source: u32, target: u32, cost: f64) -> EdgeRow {
    EdgeRow {
        id,
        source,
        target,
        cost,
        reverse_cost: Some(cost),
    }
}

/// A ladder: 0-1-2-3 along the bottom (cost 1 each), 4-5-6-7 along the top (cost 2 each),
/// rungs i <-> i + 4 (cost 1). Edge ids: bottom 0, 2, 4; top 1, 3, 5; rungs 6..=9.
fn ladder() -> Vec<EdgeRow> {
    let mut rows = Vec::new();
    for i in 0..3 {
        rows.push(row(rows.len() as u32, i, i + 1, 1.0));
        rows.push(row(rows.len() as u32, i + 4, i + 5, 2.0));
    }
    for i in 0..4 {
        rows.push(row(rows.len() as u32, i, i + 4, 1.0));
    }
    rows
}

fn edges(p: &Path) -> Vec<u32> {
    p.steps.iter().filter_map(|s| s.edge).collect()
}

#[test]
fn disabled_edges_are_avoided() {
    let rows = ladder();
    let g = Graph::build(8, &rows, true);
    let mut space = SearchSpace::new();
    let p = dijkstra(&g, &mut space, 0, &[3]).pop().flatten().unwrap();
    assert_eq!(edges(&p), [0, 2, 4]);
    // Without the middle bottom edge (id 2) the route climbs to the top for a while.
    let mut disabled = vec![false; rows.len()];
    disabled[2] = true;
    let f = g.without_edges(&disabled);
    assert_eq!(f.num_vertices(), 8);
    assert_eq!(f.num_arcs(), g.num_arcs() - 2);
    let p = dijkstra(&f, &mut space, 0, &[3]).pop().flatten().unwrap();
    // 0-1, rung 1-5, top 5-6, rung 6-2, 2-3.
    assert_eq!(edges(&p), [0, 7, 3, 8, 4]);
    assert_eq!(p.total_cost(), 6.0);

    // The hierarchy of the full graph, customized for the filtered one, agrees.
    let mut cch = Cch::build(&g);
    assert!(cch.customize(&f));
    let mut cs = CchSpace::new();
    for s in 0..8 {
        for t in 0..8 {
            let a = dijkstra(&f, &mut space, s, &[t]).pop().flatten();
            let b = cch_dijkstra(&cch, &mut cs, s, &[t]).pop().flatten();
            assert_eq!(
                a.map(|p| p.total_cost()),
                b.map(|p| p.total_cost()),
                "{s}->{t}"
            );
        }
    }
}

#[test]
fn corridor_search() {
    let rows = ladder();
    let g = Graph::build(8, &rows, true);
    let mut space = SearchSpace::new();
    // Only the top edges and the outer rungs.
    let corridor = [1u32, 3, 5, 6, 9];
    let p = dijkstra_between(&g, &mut space, &[0], &[3], |e| corridor.contains(&e)).unwrap();
    assert_eq!(edges(&p), [6, 1, 3, 5, 9]);
    assert_eq!(p.total_cost(), 8.0);
    // Several sources and targets: the cheapest pair.
    let p = dijkstra_between(&g, &mut space, &[4, 0], &[7, 3], |_| true).unwrap();
    assert_eq!((p.start_vid, p.end_vid, p.total_cost()), (0, 3, 3.0));
    // Nothing in the corridor connects them.
    assert!(dijkstra_between(&g, &mut space, &[0], &[3], |e| e == 0).is_none());
}
