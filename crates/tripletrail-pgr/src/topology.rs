//! Functions on edge geometries: topology checks and analysis, building a topology from
//! geometries (pgr_createTopology, pgr_nodeNetwork), splitting crossing and touching edges
//! and finding the edges close to points. They read the edge sub-select's rows as they are
//! (duplicates and incomplete rows included), not a built network.

use crate::geometry::{self, line, locate};
use crate::service::PgrError;
use crate::tables::{Row, ranks};
use geo::{Coord, Geometry, Line, Point};
use oxigraph::model::{Literal, Term};
use oxigraph::sparql::Variable;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use std::collections::HashMap;
use tripletrail_geo::{Crs, parse_wkt_literal};

/// The rows of an evaluated edge sub-select.
pub(crate) struct Table {
    pub variables: Vec<Variable>,
    pub rows: Vec<Vec<Option<Term>>>,
}

impl Table {
    fn position(&self, name: &str) -> Option<usize> {
        self.variables.iter().position(|v| v.as_str() == name)
    }

    fn required(&self, name: &str) -> Result<usize, PgrError> {
        self.position(name)
            .ok_or_else(|| PgrError::Request(format!("the edge sub-select must project ?{name}")))
    }
}

pub(crate) const CHECK_COLUMNS: &[&str] = &["seq", "problem", "edge", "node", "value"];
pub(crate) const ANALYZE_COLUMNS: &[&str] = &[
    "edges",
    "vertices",
    "dead_ends",
    "isolated",
    "self_loops",
    "components",
    "gaps",
    "intersections",
    "ring_geometries",
];
pub(crate) const BUILD_COLUMNS: &[&str] =
    &["seq", "id", "source", "target", "x1", "y1", "x2", "y2"];
pub(crate) const NODE_COLUMNS: &[&str] = &[
    "seq|id", "old_id", "sub_id", "source", "target", "geom", "x1", "y1", "x2", "y2",
];
pub(crate) const SEPARATE_COLUMNS: &[&str] = &["seq", "id", "sub_id", "geom"];
pub(crate) const CLOSE_COLUMNS: &[&str] = &[
    "pid|point",
    "edge_id",
    "fraction",
    "side",
    "distance",
    "geom",
    "edge",
];

fn int(i: usize) -> Option<Term> {
    Some(Literal::from(i as i64).into())
}

fn float(f: f64) -> Option<Term> {
    Some(Literal::from(f).into())
}

fn dist(a: Coord, b: Coord) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// Distances below this count as zero, so that rounding does not make tiny pieces.
const EPSILON: f64 = 1e-12;

/// An edge with a line geometry.
struct GeoEdge {
    id: Term,
    line: Vec<Coord>,
    crs: Crs,
}

/// The rows with an id and a line geometry, in pgRouting's id order.
fn geo_edges(t: &Table) -> Result<Vec<GeoEdge>, PgrError> {
    let (id, geom) = (t.required("id")?, t.required("geom")?);
    let mut edges: Vec<GeoEdge> = t
        .rows
        .iter()
        .filter_map(|r| {
            let (line, crs) = line(r[geom].as_ref()?)?;
            Some(GeoEdge {
                id: r[id].clone()?,
                line,
                crs,
            })
        })
        .collect();
    let ids: Vec<Term> = edges.iter().map(|e| e.id.clone()).collect();
    let order = ranks(&ids);
    let mut slots: Vec<Option<GeoEdge>> = edges.drain(..).map(Some).collect();
    Ok(order
        .into_iter()
        .map(|i| slots[i as usize].take().unwrap())
        .collect())
}

type Indexed = GeomWithData<Rectangle<[f64; 2]>, usize>;

fn bbox(line: &[Coord], grow: f64) -> AABB<[f64; 2]> {
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for c in line {
        lo = [lo[0].min(c.x), lo[1].min(c.y)];
        hi = [hi[0].max(c.x), hi[1].max(c.y)];
    }
    AABB::from_corners([lo[0] - grow, lo[1] - grow], [hi[0] + grow, hi[1] + grow])
}

fn index(edges: &[GeoEdge]) -> RTree<Indexed> {
    RTree::bulk_load(
        edges
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let b = bbox(&e.line, 0.0);
                GeomWithData::new(Rectangle::from_corners(b.lower(), b.upper()), i)
            })
            .collect(),
    )
}

fn ends(line: &[Coord]) -> [Coord; 2] {
    [line[0], line[line.len() - 1]]
}

/// Where edges cross: for each edge, the distances along it of the points where another edge
/// crosses it away from both edges' ends.
fn crossing_cuts(edges: &[GeoEdge], tree: &RTree<Indexed>, tol: f64) -> Vec<Vec<f64>> {
    let mut cuts = vec![Vec::new(); edges.len()];
    let near = tol.max(EPSILON);
    for (i, a) in edges.iter().enumerate() {
        for other in tree.locate_in_envelope_intersecting(bbox(&a.line, tol)) {
            let j = other.data;
            if j <= i {
                continue;
            }
            let b = &edges[j];
            let mut along_a = 0.0;
            for sa in a.line.windows(2) {
                let mut along_b = 0.0;
                for sb in b.line.windows(2) {
                    if let Some(geo::LineIntersection::SinglePoint {
                        intersection: p, ..
                    }) = geo::line_intersection::line_intersection(
                        Line::new(sa[0], sa[1]),
                        Line::new(sb[0], sb[1]),
                    ) && ends(&a.line).iter().all(|&e| dist(e, p) > near)
                        && ends(&b.line).iter().all(|&e| dist(e, p) > near)
                    {
                        cuts[i].push(along_a + dist(sa[0], p));
                        cuts[j].push(along_b + dist(sb[0], p));
                    }
                    along_b += dist(sb[0], sb[1]);
                }
                along_a += dist(sa[0], sa[1]);
            }
        }
    }
    cuts
}

/// Where edges touch: for each edge, the distances along it of the points where another
/// edge ends on it (within `tol`) away from its own ends.
fn touching_cuts(edges: &[GeoEdge], tree: &RTree<Indexed>, tol: f64) -> Vec<Vec<f64>> {
    let mut cuts = vec![Vec::new(); edges.len()];
    let near = tol.max(EPSILON);
    for (i, a) in edges.iter().enumerate() {
        for p in ends(&a.line) {
            let probe = AABB::from_corners([p.x - tol, p.y - tol], [p.x + tol, p.y + tol]);
            for other in tree.locate_in_envelope_intersecting(probe) {
                let j = other.data;
                if j == i {
                    continue;
                }
                let b = &edges[j];
                let (q, along, d, _) = locate(&b.line, p);
                if d <= tol && ends(&b.line).iter().all(|&e| dist(e, q) > near) {
                    cuts[j].push(along);
                }
            }
        }
    }
    cuts
}

/// A line cut at distances along it; cuts within `tol` of an end or of each other count
/// once. One piece (the line) when there is no cut.
fn split(line: &[Coord], cuts: &mut [f64], tol: f64) -> Vec<Vec<Coord>> {
    let near = tol.max(EPSILON);
    let total = geometry::length(line);
    cuts.sort_by(f64::total_cmp);
    let mut pieces = Vec::new();
    let mut from = 0.0;
    for &c in cuts.iter() {
        if c - from > near && total - c > near {
            pieces.push(geometry::between(line, from, c));
            from = c;
        }
    }
    if pieces.is_empty() {
        return vec![line.to_vec()];
    }
    pieces.push(geometry::between(line, from, total));
    pieces
}

/// pgr_separateCrossing and pgr_separateTouching: the pieces of the edges that had to be cut.
pub(crate) fn separate(t: &Table, tol: f64, crossing: bool) -> Result<Vec<Row>, PgrError> {
    let edges = geo_edges(t)?;
    let tree = index(&edges);
    let mut cuts = if crossing {
        crossing_cuts(&edges, &tree, tol)
    } else {
        touching_cuts(&edges, &tree, tol)
    };
    let mut rows = Vec::new();
    for (e, cuts) in edges.iter().zip(&mut cuts) {
        if cuts.is_empty() {
            continue;
        }
        let pieces = split(&e.line, cuts, tol);
        if pieces.len() < 2 {
            continue;
        }
        for (k, p) in pieces.into_iter().enumerate() {
            rows.push(vec![
                int(rows.len() + 1),
                Some(e.id.clone()),
                int(k + 1),
                Some(geometry::line_literal(p, &e.crs)),
            ]);
        }
    }
    Ok(rows)
}

/// Snaps points to vertices: a point within `tol` of an existing vertex is that vertex.
struct Snapper {
    tol: f64,
    cells: HashMap<(i64, i64), Vec<u32>>,
    vertices: Vec<Coord>,
}

impl Snapper {
    fn new(tol: f64) -> Self {
        Self {
            tol,
            cells: HashMap::new(),
            vertices: Vec::new(),
        }
    }

    fn cell(&self, c: Coord) -> (i64, i64) {
        let size = if self.tol > 0.0 { self.tol } else { 1.0 };
        ((c.x / size).floor() as i64, (c.y / size).floor() as i64)
    }

    fn vertex(&mut self, c: Coord) -> u32 {
        let (cx, cy) = self.cell(c);
        let mut best: Option<(f64, u32)> = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for &v in self.cells.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                    let d = dist(self.vertices[v as usize], c);
                    if d <= self.tol && best.is_none_or(|b| d < b.0) {
                        best = Some((d, v));
                    }
                }
            }
        }
        if let Some((_, v)) = best {
            return v;
        }
        let v = self.vertices.len() as u32;
        self.vertices.push(c);
        self.cells.entry((cx, cy)).or_default().push(v);
        v
    }
}

/// Source and target vertex of every line, by snapping its ends within `tol` (in order, so
/// a vertex sits where the first end that made it was), and the vertices.
fn topology(lines: &[&[Coord]], tol: f64) -> (Vec<(u32, u32)>, Vec<Coord>) {
    let mut s = Snapper::new(tol);
    let ends = lines
        .iter()
        .map(|l| {
            let [a, b] = ends(l);
            (s.vertex(a), s.vertex(b))
        })
        .collect();
    (ends, s.vertices)
}

/// rail:buildTopology (pgr_createTopology): source and target vertices (numbered from 1)
/// for every edge geometry.
pub(crate) fn build_topology(t: &Table, tol: f64) -> Result<Vec<Row>, PgrError> {
    let edges = geo_edges(t)?;
    let lines: Vec<&[Coord]> = edges.iter().map(|e| e.line.as_slice()).collect();
    let (ends, vertices) = topology(&lines, tol);
    Ok(edges
        .iter()
        .zip(ends)
        .enumerate()
        .map(|(i, (e, (s, t)))| {
            let (a, b) = (vertices[s as usize], vertices[t as usize]);
            vec![
                int(i + 1),
                Some(e.id.clone()),
                int(s as usize + 1),
                int(t as usize + 1),
                float(a.x),
                float(a.y),
                float(b.x),
                float(b.y),
            ]
        })
        .collect())
}

/// pgr_nodeNetwork: the edges cut where they cross or touch each other, each piece with its
/// source and target vertex.
pub(crate) fn node_network(t: &Table, tol: f64) -> Result<Vec<Row>, PgrError> {
    let edges = geo_edges(t)?;
    let tree = index(&edges);
    let mut cuts = crossing_cuts(&edges, &tree, tol);
    for (c, more) in cuts.iter_mut().zip(touching_cuts(&edges, &tree, tol)) {
        c.extend(more);
    }
    let mut pieces: Vec<(usize, usize, Vec<Coord>)> = Vec::new();
    for (i, (e, cuts)) in edges.iter().zip(&mut cuts).enumerate() {
        for (k, p) in split(&e.line, cuts, tol).into_iter().enumerate() {
            pieces.push((i, k + 1, p));
        }
    }
    let lines: Vec<&[Coord]> = pieces.iter().map(|p| p.2.as_slice()).collect();
    let (ends, vertices) = topology(&lines, tol);
    Ok(pieces
        .iter()
        .zip(ends)
        .enumerate()
        .map(|(n, ((i, k, line), (s, t)))| {
            let e = &edges[*i];
            let (a, b) = (vertices[s as usize], vertices[t as usize]);
            vec![
                int(n + 1),
                Some(e.id.clone()),
                int(*k),
                int(s as usize + 1),
                int(t as usize + 1),
                Some(geometry::line_literal(line.clone(), &e.crs)),
                float(a.x),
                float(a.y),
                float(b.x),
                float(b.y),
            ]
        })
        .collect())
}

/// pgr_findCloseEdges: for every point, the edges within `tol`, closest first, at most `cap`.
pub(crate) fn find_close_edges(
    t: &Table,
    points: &[(Term, Term)],
    tol: f64,
    cap: usize,
) -> Result<Vec<Row>, PgrError> {
    let edges = geo_edges(t)?;
    let tree = index(&edges);
    let mut rows = Vec::new();
    for (pid, geom) in points {
        let Some(p) = parse_wkt_literal(geom).and_then(|g| match &g.geometry {
            Geometry::Point(p) => Some(p.0),
            _ => None,
        }) else {
            continue;
        };
        let probe = AABB::from_corners([p.x - tol, p.y - tol], [p.x + tol, p.y + tol]);
        let mut found: Vec<(f64, usize, Coord, f64, char)> = tree
            .locate_in_envelope_intersecting(probe)
            .filter_map(|o| {
                let (q, along, d, side) = locate(&edges[o.data].line, p);
                (d <= tol).then_some((d, o.data, q, along, side))
            })
            .collect();
        found.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        for (d, i, q, along, side) in found.into_iter().take(cap) {
            let e = &edges[i];
            let total = geometry::length(&e.line);
            rows.push(vec![
                Some(pid.clone()),
                Some(e.id.clone()),
                float(if total > 0.0 { along / total } else { 0.0 }),
                Some(Literal::from(side.to_string()).into()),
                float(d),
                Some(geometry::literal(&Point(q).into(), &e.crs)),
                Some(geometry::line_literal(vec![p, q], &e.crs)),
            ]);
        }
    }
    Ok(rows)
}

/// The source and target end coordinates of a row: `?x1 ?y1` / `?x2 ?y2`, or else the ends
/// of `?geom`.
fn row_ends(
    r: &[Option<Term>],
    xy: Option<[usize; 4]>,
    geom: Option<usize>,
) -> (Option<Coord>, Option<Coord>) {
    let num = |p: usize| r[p].as_ref().and_then(crate::network::number);
    if let Some([x1, y1, x2, y2]) = xy {
        let a = num(x1).zip(num(y1)).map(|(x, y)| Coord { x, y });
        let b = num(x2).zip(num(y2)).map(|(x, y)| Coord { x, y });
        if a.is_some() || b.is_some() {
            return (a, b);
        }
    }
    match geom.and_then(|g| r[g].as_ref()).and_then(line) {
        Some((l, _)) => (Some(l[0]), Some(l[l.len() - 1])),
        None => (None, None),
    }
}

fn coords_columns(t: &Table) -> Option<[usize; 4]> {
    Some([
        t.position("x1")?,
        t.position("y1")?,
        t.position("x2")?,
        t.position("y2")?,
    ])
}

/// A problem found by rail:checkTopology: its name, edge, vertex and value.
type Problem = (&'static str, Option<Term>, Option<Term>, Option<f64>);

/// rail:checkTopology: one row per problem found in an edge set.
pub(crate) fn check_topology(t: &Table, tol: f64) -> Result<Vec<Row>, PgrError> {
    let (id, source, target) = (
        t.required("id")?,
        t.required("source")?,
        t.required("target")?,
    );
    let (cost, reverse_cost, geom) = (
        t.position("cost"),
        t.position("reverse_cost"),
        t.position("geom"),
    );
    let xy = coords_columns(t);
    let mut problems: Vec<Problem> = Vec::new();
    let mut seen: HashMap<&Term, usize> = HashMap::new();
    // Per vertex: the edge ends at it, and where they are.
    let mut at: HashMap<&Term, Vec<(&Term, Option<Coord>)>> = HashMap::new();
    for r in &t.rows {
        let (Some(i), Some(s), Some(g)) = (&r[id], &r[source], &r[target]) else {
            problems.push(("incomplete", r[id].clone(), None, None));
            continue;
        };
        *seen.entry(i).or_default() += 1;
        if s == g {
            problems.push(("self_loop", Some(i.clone()), Some(s.clone()), None));
        }
        let num = |p: Option<usize>| {
            p.and_then(|p| r[p].as_ref())
                .and_then(crate::network::number)
        };
        if let Some(c) = cost {
            match num(Some(c)) {
                None => problems.push(("missing_cost", Some(i.clone()), None, None)),
                Some(c) if c < 0.0 && num(reverse_cost).is_none_or(|rc| rc < 0.0) => {
                    problems.push(("no_direction", Some(i.clone()), None, None))
                }
                _ => {}
            }
        }
        if let Some(g) = geom
            && r[g].as_ref().and_then(line).is_none()
        {
            problems.push(("missing_geometry", Some(i.clone()), None, None));
        }
        let (a, b) = row_ends(r, xy, geom);
        at.entry(s).or_default().push((i, a));
        at.entry(g).or_default().push((i, b));
    }
    for (i, n) in &seen {
        if *n > 1 {
            problems.push(("duplicate_id", Some((*i).clone()), None, Some(*n as f64)));
        }
    }
    for (v, list) in &at {
        if list.len() == 1 {
            problems.push((
                "dangling_vertex",
                Some(list[0].0.clone()),
                Some((*v).clone()),
                None,
            ));
        }
        // Ends measured from the first edge (in id order) with coordinates there.
        let mut located: Vec<(&Term, Coord)> =
            list.iter().filter_map(|(e, c)| Some((*e, (*c)?))).collect();
        if located.len() < 2 {
            continue;
        }
        let keys: Vec<Term> = located.iter().map(|l| l.0.clone()).collect();
        let order = ranks(&keys);
        let reference = located[order[0] as usize].1;
        located.sort_by_key(|l| l.0.to_string());
        for (e, c) in located {
            let d = dist(c, reference);
            if d > tol {
                problems.push((
                    "ends_not_coincident",
                    Some(e.clone()),
                    Some((*v).clone()),
                    Some(d),
                ));
            }
        }
    }
    let key = |t: &Option<Term>| t.as_ref().map(Term::to_string).unwrap_or_default();
    problems.sort_by_cached_key(|p| (p.0, key(&p.1), key(&p.2)));
    problems.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
    Ok(problems
        .into_iter()
        .enumerate()
        .map(|(n, (problem, edge, node, value))| {
            vec![
                int(n + 1),
                Some(Literal::from(problem).into()),
                edge,
                node,
                value.and_then(float),
            ]
        })
        .collect())
}

fn find(parent: &mut [u32], mut v: u32) -> u32 {
    while parent[v as usize] != v {
        parent[v as usize] = parent[parent[v as usize] as usize];
        v = parent[v as usize];
    }
    v
}

/// rail:analyzeGraph (pgr_analyzeGraph): counts describing an edge set. Without
/// `?source ?target` the topology is built from the geometries first.
pub(crate) fn analyze_graph(t: &Table, tol: f64) -> Result<Vec<Row>, PgrError> {
    let geom = t.position("geom");
    let edges = if geom.is_some() && t.position("id").is_some() {
        geo_edges(t)?
    } else {
        Vec::new()
    };
    // Vertex ids of each counted edge, and their number.
    let (edge_ends, vertex_count): (Vec<(u32, u32)>, usize) =
        match (t.position("source"), t.position("target")) {
            (Some(s), Some(g)) => {
                let mut ids: HashMap<&Term, u32> = HashMap::new();
                let mut ends = Vec::new();
                for r in &t.rows {
                    let (Some(a), Some(b)) = (&r[s], &r[g]) else {
                        continue;
                    };
                    let mut id = |v| {
                        let n = ids.len() as u32;
                        *ids.entry(v).or_insert(n)
                    };
                    ends.push((id(a), id(b)));
                }
                (ends, ids.len())
            }
            _ if geom.is_some() => {
                let lines: Vec<&[Coord]> = edges.iter().map(|e| e.line.as_slice()).collect();
                let (ends, vertices) = topology(&lines, tol);
                (ends, vertices.len())
            }
            _ => {
                return Err(PgrError::Request(
                    "rail:analyzeGraph needs ?source ?target or ?geom".into(),
                ));
            }
        };
    let mut degree = vec![0usize; vertex_count];
    let mut parent: Vec<u32> = (0..vertex_count as u32).collect();
    let mut loops = 0;
    for &(a, b) in &edge_ends {
        degree[a as usize] += 1;
        degree[b as usize] += 1;
        loops += usize::from(a == b);
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        parent[ra as usize] = rb;
    }
    let dead_ends = degree.iter().filter(|&&d| d == 1).count();
    let isolated = edge_ends
        .iter()
        .filter(|&&(a, b)| degree[a as usize] == 1 && degree[b as usize] == 1)
        .count();
    let components = (0..vertex_count as u32)
        .filter(|&v| find(&mut parent, v) == v)
        .count();
    let (mut gaps, mut intersections, mut rings) = (None, None, None);
    if geom.is_some() {
        let tree = index(&edges);
        let crossings: usize = crossing_cuts(&edges, &tree, tol).iter().map(Vec::len).sum();
        intersections = Some(crossings / 2);
        // A gap: a line end within `tol` of another line it does not share a vertex with,
        // where it is the only line ending there.
        let lines: Vec<&[Coord]> = edges.iter().map(|e| e.line.as_slice()).collect();
        let (own, vertices) = topology(&lines, tol);
        let mut own_degree = vec![0usize; vertices.len()];
        for &(a, b) in &own {
            own_degree[a as usize] += 1;
            own_degree[b as usize] += 1;
        }
        let mut g = 0;
        for (i, e) in edges.iter().enumerate() {
            for (k, p) in ends(&e.line).into_iter().enumerate() {
                let v = if k == 0 { own[i].0 } else { own[i].1 };
                if own_degree[v as usize] != 1 {
                    continue;
                }
                let probe = AABB::from_corners([p.x - tol, p.y - tol], [p.x + tol, p.y + tol]);
                let close = tree.locate_in_envelope_intersecting(probe).any(|o| {
                    let j = o.data;
                    j != i && own[j].0 != v && own[j].1 != v && locate(&edges[j].line, p).2 <= tol
                });
                g += usize::from(close);
            }
        }
        gaps = Some(g);
        rings = Some(
            edges
                .iter()
                .filter(|e| e.line.len() > 2 && e.line[0] == e.line[e.line.len() - 1])
                .count(),
        );
    }
    Ok(vec![vec![
        int(edge_ends.len()),
        int(vertex_count),
        int(dead_ends),
        int(isolated),
        int(loops),
        int(components),
        gaps.and_then(int),
        intersections.and_then(int),
        rings.and_then(int),
    ]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tripletrail_geo::WKT_LITERAL;

    fn table(rows: &[(&str, &str)]) -> Table {
        Table {
            variables: vec![
                Variable::new_unchecked("id"),
                Variable::new_unchecked("geom"),
            ],
            rows: rows
                .iter()
                .map(|(id, wkt)| {
                    vec![
                        Some(Literal::from(*id).into()),
                        Some(Literal::new_typed_literal(*wkt, WKT_LITERAL).into()),
                    ]
                })
                .collect(),
        }
    }

    fn text(t: &Option<Term>) -> String {
        match t {
            Some(Term::Literal(l)) => l.value().to_owned(),
            Some(other) => other.to_string(),
            None => "-".into(),
        }
    }

    fn texts(rows: &[Row]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(text).collect()).collect()
    }

    #[test]
    fn crossing_and_touching() {
        // A cross, and a third line ending on the first one.
        let t = table(&[
            ("a", "LINESTRING(0 0, 4 0)"),
            ("b", "LINESTRING(2 -2, 2 2)"),
            ("c", "LINESTRING(3 3, 3 0)"),
        ]);
        let r = texts(&separate(&t, 0.0, true).unwrap());
        assert_eq!(
            r,
            [
                ["1", "a", "1", "LINESTRING(0 0,2 0)"],
                ["2", "a", "2", "LINESTRING(2 0,4 0)"],
                ["3", "b", "1", "LINESTRING(2 -2,2 0)"],
                ["4", "b", "2", "LINESTRING(2 0,2 2)"],
            ]
        );
        let r = texts(&separate(&t, 0.0, false).unwrap());
        assert_eq!(
            r,
            [
                ["1", "a", "1", "LINESTRING(0 0,3 0)"],
                ["2", "a", "2", "LINESTRING(3 0,4 0)"],
            ]
        );
        // Node the network: a in three pieces, b in two, c whole: 6 pieces, and the
        // vertices (0 0) (2 0) (3 0) (4 0) (2 -2) (2 2) (3 3).
        let r = node_network(&t, 0.0).unwrap();
        assert_eq!(r.len(), 6);
        let vertices: std::collections::HashSet<String> =
            r.iter().flat_map(|r| [text(&r[3]), text(&r[4])]).collect();
        assert_eq!(vertices.len(), 7);
    }

    #[test]
    fn topology_with_tolerance() {
        let t = table(&[
            ("1", "LINESTRING(0 0, 1 0)"),
            ("2", "LINESTRING(1.05 0, 2 0)"),
            ("3", "LINESTRING(2 0, 2 1)"),
        ]);
        let exact = texts(&build_topology(&t, 0.0).unwrap());
        assert_eq!(exact[1][2], "3", "1.05 is its own vertex without tolerance");
        let snapped = texts(&build_topology(&t, 0.1).unwrap());
        assert_eq!(
            snapped
                .iter()
                .map(|r| (r[1].clone(), r[2].clone(), r[3].clone()))
                .collect::<Vec<_>>(),
            [
                ("1".into(), "1".into(), "2".into()),
                ("2".into(), "2".into(), "3".into()),
                ("3".into(), "3".into(), "4".into())
            ]
        );
        // The snapped vertex keeps the first end's coordinates.
        assert_eq!(snapped[1][4], "1");
        let a = texts(&analyze_graph(&t, 0.0).unwrap());
        // edges, vertices, dead ends, isolated, loops, components, gaps, crossings, rings
        assert_eq!(a[0], ["3", "5", "4", "1", "0", "2", "0", "0", "0"]);
        let a = texts(&analyze_graph(&t, 0.1).unwrap());
        assert_eq!(a[0], ["3", "4", "2", "0", "0", "1", "0", "0", "0"]);
    }

    #[test]
    fn close_edges() {
        let t = table(&[("a", "LINESTRING(0 0, 4 0)"), ("b", "LINESTRING(0 2, 4 2)")]);
        let p: Term = Literal::new_typed_literal("POINT(1 0.5)", WKT_LITERAL).into();
        let r = texts(&find_close_edges(&t, &[(p.clone(), p.clone())], 2.0, 5).unwrap());
        assert_eq!(r.len(), 2);
        assert_eq!(r[0][1..6], ["a", "0.25", "l", "0.5", "POINT(1 0)"]);
        assert_eq!(r[1][1..5], ["b", "0.25", "r", "1.5"]);
        let r = find_close_edges(&t, &[(p.clone(), p)], 1.0, 5).unwrap();
        assert_eq!(r.len(), 1);
    }
}
