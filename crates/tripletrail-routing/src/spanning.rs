//! Minimum spanning forest (pgr_kruskal, pgr_prim). Results use the dense edge ids of the
//! [`Graph`].

use crate::graph::Graph;

/// The edges of a minimum spanning forest of the undirected graph, with the cost each is
/// used at, in the order Kruskal's algorithm takes them (cheapest first). An edge usable in
/// both directions at different costs counts at the cheaper one; loops never take part.
pub fn minimum_spanning_forest(graph: &Graph) -> Vec<(u32, f64)> {
    minimum_spanning_forest_arcs(graph)
        .into_iter()
        .map(|(e, c, _, _)| (e, c))
        .collect()
}

/// [`minimum_spanning_forest`] with the two vertices each edge joins: `(edge, cost, a, b)`.
pub fn minimum_spanning_forest_arcs(graph: &Graph) -> Vec<(u32, f64, u32, u32)> {
    let n = graph.num_vertices();
    // (cost, edge, a, b), one entry per edge id at its cheapest arc.
    let mut best: std::collections::HashMap<u32, (f64, u32, u32)> = Default::default();
    for v in 0..n as u32 {
        for (w, cost, e) in graph.fwd.arcs(v) {
            if v == w {
                continue;
            }
            let entry = best.entry(e).or_insert((cost, v, w));
            if cost < entry.0 {
                *entry = (cost, v, w);
            }
        }
    }
    let mut edges: Vec<(f64, u32, u32, u32)> = best
        .into_iter()
        .map(|(e, (c, a, b))| (c, e, a, b))
        .collect();
    edges.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
    let mut parent: Vec<u32> = (0..n as u32).collect();
    fn find(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            let p = parent[v as usize];
            parent[v as usize] = parent[p as usize];
            v = p;
        }
        v
    }
    let mut forest = Vec::new();
    for (cost, e, a, b) in edges {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent[ra.max(rb) as usize] = ra.min(rb);
            forest.push((e, cost, a, b));
        }
    }
    forest
}
