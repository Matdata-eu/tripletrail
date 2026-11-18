//! PostGIS set-returning and window functions (`ST_Dump`, `ST_DumpPoints`, `ST_HexagonGrid`,
//! `ST_ClusterDBSCAN`, ...), as plain functions over geometry literals. SPARQL functions return
//! one value, so the engine exposes these through a `SERVICE` that turns them into rows.

use crate::functions::geom;
use crate::literal::{Geom, geom_literal};
use geo::line_measures::{Distance, Euclidean};
use geo::{
    BoundingRect, Centroid, Coord, Geometry, GeometryCollection, Intersects, Line, LineString,
    Point, Polygon, Rect,
};
use oxigraph::model::Term;
use std::sync::Arc;

fn literal(src: &Geom, g: Geometry<f64>) -> Term {
    geom_literal(&Geom {
        geometry: g,
        crs: src.crs.clone(),
        zm: src.zm.clone(),
        surface: None,
        prepared: Default::default(),
    })
}

/// A PostGIS array path such as `{1,2}`.
fn path(p: &[usize]) -> String {
    let parts: Vec<String> = p.iter().map(|i| i.to_string()).collect();
    format!("{{{}}}", parts.join(","))
}

/// Which `ST_Dump*` function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpKind {
    /// `ST_Dump`: the atomic parts.
    Parts,
    /// `ST_DumpPoints`: every vertex.
    Points,
    /// `ST_DumpSegments`: every segment, as a two-point line.
    Segments,
    /// `ST_DumpRings`: the rings of a polygon, as polygons (path 0 is the shell).
    Rings,
}

fn children(g: &Geometry<f64>) -> Option<Vec<Geometry<f64>>> {
    Some(match g {
        Geometry::MultiPoint(m) => m.0.iter().map(|p| Geometry::Point(*p)).collect(),
        Geometry::MultiLineString(m) => m.0.iter().cloned().map(Geometry::LineString).collect(),
        Geometry::MultiPolygon(m) => m.0.iter().cloned().map(Geometry::Polygon).collect(),
        Geometry::GeometryCollection(c) => c.0.clone(),
        _ => return None,
    })
}

fn dump_into(
    kind: DumpKind,
    g: &Geometry<f64>,
    prefix: &mut Vec<usize>,
    out: &mut Vec<(String, Geometry<f64>)>,
) {
    if let Some(parts) = children(g) {
        for (i, p) in parts.iter().enumerate() {
            prefix.push(i + 1);
            dump_into(kind, p, prefix, out);
            prefix.pop();
        }
        return;
    }
    let mut emit = |extra: &[usize], g: Geometry<f64>| {
        let mut p = prefix.clone();
        p.extend_from_slice(extra);
        out.push((path(&p), g));
    };
    let rings = |g: &Geometry<f64>| -> Vec<LineString<f64>> {
        match g {
            Geometry::Polygon(p) => std::iter::once(p.exterior())
                .chain(p.interiors())
                .cloned()
                .collect(),
            Geometry::Rect(r) => vec![r.to_polygon().exterior().clone()],
            Geometry::Triangle(t) => vec![t.to_polygon().exterior().clone()],
            _ => Vec::new(),
        }
    };
    match kind {
        DumpKind::Parts => emit(&[], g.clone()),
        DumpKind::Points => match g {
            Geometry::Point(p) => emit(&[1], Geometry::Point(*p)),
            Geometry::Line(l) => {
                emit(&[1], Geometry::Point(Point(l.start)));
                emit(&[2], Geometry::Point(Point(l.end)));
            }
            Geometry::LineString(l) => {
                for (i, c) in l.0.iter().enumerate() {
                    emit(&[i + 1], Geometry::Point(Point(*c)));
                }
            }
            _ => {
                for (r, ring) in rings(g).iter().enumerate() {
                    for (i, c) in ring.0.iter().enumerate() {
                        emit(&[r + 1, i + 1], Geometry::Point(Point(*c)));
                    }
                }
            }
        },
        DumpKind::Segments => {
            let seg = |l: Line<f64>| Geometry::LineString(LineString::new(vec![l.start, l.end]));
            match g {
                Geometry::Line(l) => emit(&[1], seg(*l)),
                Geometry::LineString(l) => {
                    for (i, s) in l.lines().enumerate() {
                        emit(&[i + 1], seg(s));
                    }
                }
                _ => {
                    for (r, ring) in rings(g).iter().enumerate() {
                        for (i, s) in ring.lines().enumerate() {
                            emit(&[r + 1, i + 1], seg(s));
                        }
                    }
                }
            }
        }
        DumpKind::Rings => {
            for (r, ring) in rings(g).into_iter().enumerate() {
                emit(&[r], Geometry::Polygon(Polygon::new(ring, Vec::new())));
            }
        }
    }
}

/// The rows of an `ST_Dump*` function: (path, geometry).
pub fn dump(kind: DumpKind, g: &Term) -> Option<Vec<(String, Term)>> {
    let src = geom(g)?;
    if kind == DumpKind::Rings
        && !matches!(
            src.geometry,
            Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_)
        )
    {
        return None;
    }
    let mut out = Vec::new();
    dump_into(kind, &src.geometry, &mut Vec::new(), &mut out);
    Some(
        out.into_iter()
            .map(|(p, g)| (p, literal(&src, g)))
            .collect(),
    )
}

/// `ST_HexagonGrid` / `ST_SquareGrid`: the cells (geometry, i, j) touching the bounding box of
/// `bounds`.
pub fn grid(hexagons: bool, size: f64, bounds: &Term) -> Option<Vec<(Term, i64, i64)>> {
    (size > 0.0).then_some(())?;
    let src = geom(bounds)?;
    let r = src.geometry.bounding_rect()?;
    let origin = Coord { x: 0.0, y: 0.0 };
    let cell = |i: i64, j: i64| {
        if hexagons {
            crate::postgis_shapes::hexagon(size, i, j, origin)
        } else {
            crate::postgis_shapes::square(size, i, j, origin)
        }
    };
    let (i0, i1, j0, j1) = if hexagons {
        let h = size * 3f64.sqrt();
        (
            ((r.min().x - size) / (1.5 * size)).floor() as i64,
            ((r.max().x + size) / (1.5 * size)).ceil() as i64,
            ((r.min().y - h) / h).floor() as i64,
            ((r.max().y + h) / h).ceil() as i64,
        )
    } else {
        (
            (r.min().x / size).floor() as i64,
            (r.max().x / size).floor() as i64,
            (r.min().y / size).floor() as i64,
            (r.max().y / size).floor() as i64,
        )
    };
    let cells = (i1 - i0 + 1).saturating_mul(j1 - j0 + 1);
    (cells <= 10_000_000).then_some(())?;
    let mut out = Vec::new();
    for i in i0..=i1 {
        for j in j0..=j1 {
            let p = cell(i, j);
            let b = p.bounding_rect()?;
            let touches = b.min().x < r.max().x
                && r.min().x < b.max().x
                && b.min().y < r.max().y
                && r.min().y < b.max().y
                || (r.width() == 0.0 || r.height() == 0.0) && p.intersects(&r.to_polygon());
            if touches {
                let g = Geometry::Polygon(p);
                out.push((geom_literal(&Geom::new(g, src.crs.clone())), i, j));
            }
        }
    }
    Some(out)
}

fn parsed(geoms: &[Term]) -> Vec<Option<Arc<Geom>>> {
    geoms.iter().map(geom).collect()
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// Numbers groups 0, 1, ... in order of their first member; `None` stays `None`.
fn number(groups: Vec<Option<usize>>) -> Vec<Option<i64>> {
    let mut ids = std::collections::HashMap::new();
    groups
        .into_iter()
        .map(|g| {
            g.map(|g| {
                let n = ids.len() as i64;
                *ids.entry(g).or_insert(n)
            })
        })
        .collect()
}

/// `ST_ClusterWithinWin` (`distance` > 0) or `ST_ClusterIntersectingWin` (`None`): a cluster id
/// per input, geometries linked when within `distance` (or intersecting).
pub fn cluster_within(geoms: &[Term], distance: Option<f64>) -> Vec<Option<i64>> {
    let g = parsed(geoms);
    let valid: Vec<Geometry<f64>> = g
        .iter()
        .map(|g| {
            g.as_ref().map_or(
                Geometry::GeometryCollection(GeometryCollection(Vec::new())),
                |g| g.geometry.clone(),
            )
        })
        .collect();
    let reach = distance.unwrap_or(0.0);
    let groups = crate::postgis_shapes::clusters(
        &valid,
        |a, b| match distance {
            None => a.intersects(b),
            Some(d) => Euclidean.distance(a, b) <= d,
        },
        reach,
    );
    let mut out = vec![None; geoms.len()];
    for (k, members) in groups.iter().enumerate() {
        for &i in members {
            if g[i].is_some() {
                out[i] = Some(k);
            }
        }
    }
    number(out)
}

/// `ST_ClusterDBSCAN`: a cluster id per input, `None` for noise. A geometry with at least
/// `min_points` geometries (itself included) within `eps` is a core; clusters are the cores
/// linked within `eps` plus the geometries within `eps` of one of them.
pub fn cluster_dbscan(geoms: &[Term], eps: f64, min_points: usize) -> Vec<Option<i64>> {
    let g = parsed(geoms);
    let n = g.len();
    let boxes: Vec<Option<Rect<f64>>> = g
        .iter()
        .map(|g| g.as_ref().and_then(|g| g.geometry.bounding_rect()))
        .collect();
    let mut order: Vec<usize> = (0..n).filter(|&i| boxes[i].is_some()).collect();
    order.sort_by(|&a, &b| {
        boxes[a]
            .unwrap()
            .min()
            .x
            .total_cmp(&boxes[b].unwrap().min().x)
    });
    let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (k, &i) in order.iter().enumerate() {
        let bi = boxes[i].unwrap();
        for &j in &order[k + 1..] {
            let bj = boxes[j].unwrap();
            if bj.min().x > bi.max().x + eps {
                break;
            }
            if bj.min().y > bi.max().y + eps || bi.min().y > bj.max().y + eps {
                continue;
            }
            let (a, b) = (g[i].as_ref().unwrap(), g[j].as_ref().unwrap());
            if Euclidean.distance(&a.geometry, &b.geometry) <= eps {
                neighbours[i].push(j);
                neighbours[j].push(i);
            }
        }
    }
    let core: Vec<bool> = (0..n)
        .map(|i| g[i].is_some() && neighbours[i].len() + 1 >= min_points)
        .collect();
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n {
        if core[i] {
            for &j in &neighbours[i] {
                if core[j] {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut out: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        if core[i] {
            out[i] = Some(find(&mut parent, i));
        }
    }
    for i in 0..n {
        if !core[i] && g[i].is_some() {
            // A border geometry joins the cluster of its first core neighbour.
            if let Some(&j) = neighbours[i].iter().filter(|&&j| core[j]).min() {
                out[i] = Some(find(&mut parent, j));
            }
        }
    }
    number(out)
}

/// `ST_ClusterKMeans`: `k` clusters of the geometries' centroids (k-means++ from a fixed seed).
/// With `max_radius`, `k` grows until no cluster is wider than it.
pub fn cluster_kmeans(geoms: &[Term], k: usize, max_radius: Option<f64>) -> Vec<Option<i64>> {
    let pts: Vec<Option<Coord<f64>>> = parsed(geoms)
        .iter()
        .map(|g| g.as_ref().and_then(|g| g.geometry.centroid()).map(|p| p.0))
        .collect();
    let valid: Vec<usize> = (0..pts.len()).filter(|&i| pts[i].is_some()).collect();
    let xs: Vec<Coord<f64>> = valid.iter().map(|&i| pts[i].unwrap()).collect();
    if xs.is_empty() || k == 0 {
        return vec![None; geoms.len()];
    }
    let mut k = k.min(xs.len());
    loop {
        let (assign, centers) = kmeans(&xs, k);
        let fits = max_radius.is_none_or(|r| {
            xs.iter()
                .zip(&assign)
                .all(|(p, &c)| Euclidean.distance(*p, centers[c]) <= r)
        });
        if fits || k >= xs.len() {
            let mut out = vec![None; geoms.len()];
            for (slot, &i) in valid.iter().enumerate() {
                out[i] = Some(assign[slot] as i64);
            }
            return out;
        }
        k += 1;
    }
}

fn kmeans(xs: &[Coord<f64>], k: usize) -> (Vec<usize>, Vec<Coord<f64>>) {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rand = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let d2 = |a: Coord<f64>, b: Coord<f64>| (a.x - b.x).powi(2) + (a.y - b.y).powi(2);
    let mut centers = vec![xs[0]];
    while centers.len() < k {
        let w: Vec<f64> = xs
            .iter()
            .map(|&p| {
                centers
                    .iter()
                    .map(|&c| d2(p, c))
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        let total: f64 = w.iter().sum();
        if total == 0.0 {
            break;
        }
        let mut target = rand() * total;
        let mut pick = xs.len() - 1;
        for (i, wi) in w.iter().enumerate() {
            target -= wi;
            if target <= 0.0 {
                pick = i;
                break;
            }
        }
        centers.push(xs[pick]);
    }
    let mut assign = vec![0usize; xs.len()];
    for _ in 0..100 {
        let mut changed = false;
        for (i, &p) in xs.iter().enumerate() {
            let best = (0..centers.len())
                .min_by(|&a, &b| d2(p, centers[a]).total_cmp(&d2(p, centers[b])))
                .unwrap();
            if best != assign[i] {
                assign[i] = best;
                changed = true;
            }
        }
        let mut sum = vec![(0.0, 0.0, 0usize); centers.len()];
        for (i, &p) in xs.iter().enumerate() {
            let s = &mut sum[assign[i]];
            s.0 += p.x;
            s.1 += p.y;
            s.2 += 1;
        }
        for (c, s) in centers.iter_mut().zip(&sum) {
            if s.2 > 0 {
                *c = Coord {
                    x: s.0 / s.2 as f64,
                    y: s.1 / s.2 as f64,
                };
            }
        }
        if !changed {
            break;
        }
    }
    (assign, centers)
}

/// The members of each cluster as one geometry collection (`ST_ClusterWithin`,
/// `ST_ClusterIntersecting` as sets).
pub fn collect_clusters(geoms: &[Term], ids: &[Option<i64>]) -> Vec<Term> {
    let g = parsed(geoms);
    let n = ids.iter().flatten().max().map_or(0, |m| *m as usize + 1);
    let mut parts: Vec<Vec<Geometry<f64>>> = vec![Vec::new(); n];
    let mut crs = vec![None; n];
    for (i, id) in ids.iter().enumerate() {
        if let (Some(id), Some(g)) = (id, &g[i]) {
            parts[*id as usize].push(g.geometry.clone());
            crs[*id as usize].get_or_insert_with(|| g.crs.clone());
        }
    }
    parts
        .into_iter()
        .zip(crs)
        .filter_map(|(p, c)| {
            Some(geom_literal(&Geom::new(
                Geometry::GeometryCollection(GeometryCollection(p)),
                c?,
            )))
        })
        .collect()
}

/// `ST_Subdivide` as rows.
pub fn subdivide(g: &Term, max_vertices: usize) -> Option<Vec<Term>> {
    let src = geom(g)?;
    let all = crate::postgis_processing::subdivide_parts(&src.geometry, max_vertices.max(5));
    Some(all.into_iter().map(|p| literal(&src, p)).collect())
}

/// Which coverage window function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageKind {
    /// `ST_CoverageInvalidEdges(geom, tolerance)`.
    InvalidEdges,
    /// `ST_CoverageSimplify(geom, tolerance)`.
    Simplify,
    /// `ST_CoverageClean(geom, gap_width)`.
    Clean,
}

/// One result per input of a coverage window function (`None` for an invalid input, or for a
/// polygon without invalid edges).
pub fn coverage(kind: CoverageKind, geoms: &[Term], param: f64) -> Vec<Option<Term>> {
    let g = parsed(geoms);
    let present: Vec<usize> = (0..g.len()).filter(|&i| g[i].is_some()).collect();
    let parts: Vec<Geometry<f64>> = present
        .iter()
        .map(|&i| g[i].as_ref().unwrap().geometry.clone())
        .collect();
    let results: Vec<Option<Geometry<f64>>> = match kind {
        CoverageKind::InvalidEdges => crate::postgis_p2::coverage_invalid_edges(&parts, param),
        CoverageKind::Simplify => crate::postgis_p2::coverage_simplify(&parts, param)
            .into_iter()
            .map(Some)
            .collect(),
        CoverageKind::Clean => crate::postgis_p2::coverage_clean(&parts, param)
            .into_iter()
            .map(Some)
            .collect(),
    };
    let mut out = vec![None; geoms.len()];
    for (slot, r) in present.iter().zip(results) {
        out[*slot] = r.map(|r| geom_literal(&Geom::new(r, g[*slot].as_ref().unwrap().crs.clone())));
    }
    out
}

// --- ST_IsValidDetail --------------------------------------------------------------------------

use geo::algorithm::validation::{
    InvalidGeometry, InvalidGeometryCollection, InvalidLineString, InvalidMultiPolygon,
    InvalidPolygon, RingRole,
};

/// The segments of a ring.
fn ring_lines(r: &LineString<f64>) -> Vec<Line<f64>> {
    r.lines().collect()
}

/// Where two sets of segments meet: an overlap or a proper crossing first, then any contact.
/// Within one ring (`same`), neighbouring segments are skipped.
fn contact(a: &[Line<f64>], b: &[Line<f64>], same: bool) -> Option<Coord<f64>> {
    use geo::line_intersection::{LineIntersection, line_intersection};
    let mut any = None;
    for (i, la) in a.iter().enumerate() {
        for (j, lb) in b.iter().enumerate() {
            if same && (j <= i + 1 || (i == 0 && j + 1 == b.len())) {
                continue;
            }
            match line_intersection(*la, *lb) {
                Some(LineIntersection::Collinear { intersection }) => {
                    return Some(intersection.start);
                }
                Some(LineIntersection::SinglePoint {
                    intersection,
                    is_proper,
                }) => {
                    if is_proper || same {
                        return Some(intersection);
                    }
                    any.get_or_insert(intersection);
                }
                None => {}
            }
        }
    }
    any
}

fn ring(p: &Polygon<f64>, role: RingRole) -> Option<&LineString<f64>> {
    match role {
        RingRole::Exterior => Some(p.exterior()),
        RingRole::Interior(i) => p.interiors().get(i),
    }
}

fn polygon_location(p: &Polygon<f64>, e: &InvalidPolygon) -> Option<Coord<f64>> {
    let first = |r: Option<&LineString<f64>>| r.and_then(|r| r.0.first().copied());
    match e {
        InvalidPolygon::TooFewPointsInRing(r) => first(ring(p, *r)),
        InvalidPolygon::SelfIntersection(r) => {
            let lines = ring_lines(ring(p, *r)?);
            contact(&lines, &lines, true).or_else(|| first(ring(p, *r)))
        }
        InvalidPolygon::NonFiniteCoord(..) => None,
        InvalidPolygon::InteriorRingNotContainedInExteriorRing(r) => {
            let hole = ring_lines(ring(p, *r)?);
            contact(&ring_lines(p.exterior()), &hole, false).or_else(|| first(ring(p, *r)))
        }
        InvalidPolygon::IntersectingRingsOnALine(a, b)
        | InvalidPolygon::IntersectingRingsOnAnArea(a, b) => {
            let (ra, rb) = (ring(p, *a)?, ring(p, *b)?);
            contact(&ring_lines(ra), &ring_lines(rb), false).or_else(|| first(Some(rb)))
        }
    }
}

fn polygon_lines(p: &Polygon<f64>) -> Vec<Line<f64>> {
    std::iter::once(p.exterior())
        .chain(p.interiors())
        .flat_map(ring_lines)
        .collect()
}

fn location(g: &Geometry<f64>, e: &InvalidGeometry) -> Option<Coord<f64>> {
    use geo::CoordsIter;
    let first = g.coords_iter().next();
    match (g, e) {
        (Geometry::Polygon(p), InvalidGeometry::InvalidPolygon(e)) => polygon_location(p, e),
        (Geometry::MultiPolygon(m), InvalidGeometry::InvalidMultiPolygon(e)) => match e {
            InvalidMultiPolygon::InvalidPolygon(i, e) => polygon_location(m.0.get(i.0)?, e),
            InvalidMultiPolygon::ElementsOverlaps(i, j)
            | InvalidMultiPolygon::ElementsTouchOnALine(i, j) => {
                let (a, b) = (m.0.get(i.0)?, m.0.get(j.0)?);
                contact(&polygon_lines(a), &polygon_lines(b), false)
                    .or_else(|| b.exterior().0.first().copied())
            }
        },
        (
            Geometry::GeometryCollection(c),
            InvalidGeometry::InvalidGeometryCollection(InvalidGeometryCollection::InvalidGeometry(
                i,
                e,
            )),
        ) => location(c.0.get(i.0)?, e),
        (_, InvalidGeometry::InvalidLineString(InvalidLineString::NonFiniteCoord(_))) => None,
        _ => first,
    }
}

/// PostGIS `ST_IsValidDetail`: whether a geometry is valid, and if not, why (the text of
/// `ST_IsValidReason`) and where (a point, when one can be given).
pub fn is_valid_detail(g: &Term) -> Option<(bool, Option<String>, Option<Term>)> {
    use geo::Validation;
    let g = geom(g)?;
    let errors = g.geometry.validation_errors();
    let Some(e) = errors.first() else {
        return Some((true, None, None));
    };
    let at = location(&g.geometry, e)
        .filter(|c| c.x.is_finite() && c.y.is_finite())
        .map(|c| geom_literal(&Geom::new(Geometry::Point(Point(c)), g.crs.clone())));
    Some((false, Some(e.to_string()), at))
}
