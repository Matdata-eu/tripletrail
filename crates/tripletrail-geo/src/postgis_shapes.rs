//! PostGIS functions that build or analyse shapes: polygonizing linework, shared paths, line
//! crossing direction, clearance, inscribed and empty circles, polygon hulls, constrained
//! triangulation, TWKB and grid cells, under their `st:ST_*` names.

use crate::functions::{Func, GEOF, ST, double, func, geom, integer, num, same_space};
use crate::literal::Crs;
use crate::postgis::{int, out};
use crate::postgis_processing::{
    HEX_BINARY, from_hex, from_wkb, lines_of, node, polygons, polygons_of, segments, text, to_hex,
    typed_text,
};
use geo::line_measures::{Distance, Euclidean};
use geo::{
    BoundingRect, Contains, ConvexHull, Coord, CoordsIter, Geometry, GeometryCollection,
    InteriorPoint, Intersects, Line, LineString, MultiLineString, MultiPolygon, Orient, Point,
    Polygon, Rect, TriangulateDelaunay, orient::Direction,
};
use oxigraph::model::{Literal, NamedNodeRef, Term};
use std::collections::{BinaryHeap, HashMap, HashSet};

// --- Polygonize and build area -------------------------------------------------------------

fn key(c: Coord<f64>) -> (u64, u64) {
    ((c.x + 0.0).to_bits(), (c.y + 0.0).to_bits())
}

/// The planar graph of noded linework without dangles and cut edges.
struct Arrangement {
    points: Vec<Coord<f64>>,
    edges: Vec<(usize, usize)>,
}

impl Arrangement {
    fn new(lines: &[LineString<f64>]) -> Self {
        let mut ids: HashMap<(u64, u64), usize> = HashMap::new();
        let mut points = Vec::new();
        let mut set: HashSet<(usize, usize)> = HashSet::new();
        for l in node(lines) {
            for w in l.0.windows(2) {
                let mut id = |c: Coord<f64>| {
                    *ids.entry(key(c)).or_insert_with(|| {
                        points.push(c);
                        points.len() - 1
                    })
                };
                let (a, b) = (id(w[0]), id(w[1]));
                if a != b {
                    set.insert((a.min(b), a.max(b)));
                }
            }
        }
        let mut edges: Vec<(usize, usize)> = set.into_iter().collect();
        edges.sort_unstable();
        let mut arr = Self { points, edges };
        loop {
            arr.remove_dangles();
            let cut = arr.cut_edges();
            if cut.is_empty() {
                break;
            }
            let cut: HashSet<usize> = cut.into_iter().collect();
            arr.edges = arr
                .edges
                .iter()
                .enumerate()
                .filter(|(i, _)| !cut.contains(i))
                .map(|(_, e)| *e)
                .collect();
        }
        arr
    }

    fn remove_dangles(&mut self) {
        loop {
            let mut degree = vec![0usize; self.points.len()];
            for &(a, b) in &self.edges {
                degree[a] += 1;
                degree[b] += 1;
            }
            let before = self.edges.len();
            self.edges.retain(|&(a, b)| degree[a] > 1 && degree[b] > 1);
            if self.edges.len() == before {
                return;
            }
        }
    }

    /// Face cycles as half-edge lists (half-edge `2e` runs a→b, `2e+1` b→a).
    fn cycles(&self) -> Vec<Vec<usize>> {
        let n = self.points.len();
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
        let from = |h: usize| {
            let (a, b) = self.edges[h / 2];
            if h.is_multiple_of(2) { a } else { b }
        };
        let to = |h: usize| {
            let (a, b) = self.edges[h / 2];
            if h.is_multiple_of(2) { b } else { a }
        };
        for h in 0..self.edges.len() * 2 {
            out[from(h)].push(h);
        }
        let angle = |h: usize| {
            let (p, q) = (self.points[from(h)], self.points[to(h)]);
            (q.y - p.y).atan2(q.x - p.x)
        };
        for list in &mut out {
            list.sort_by(|&a, &b| angle(a).total_cmp(&angle(b)));
        }
        let mut position = vec![0usize; self.edges.len() * 2];
        for list in &out {
            for (i, &h) in list.iter().enumerate() {
                position[h] = i;
            }
        }
        let next = |h: usize| {
            let twin = h ^ 1;
            let list = &out[to(h)];
            list[(position[twin] + list.len() - 1) % list.len()]
        };
        let mut seen = vec![false; self.edges.len() * 2];
        let mut cycles = Vec::new();
        for h in 0..seen.len() {
            if seen[h] {
                continue;
            }
            let mut cycle = Vec::new();
            let mut cur = h;
            while !seen[cur] {
                seen[cur] = true;
                cycle.push(cur);
                cur = next(cur);
            }
            cycles.push(cycle);
        }
        cycles
    }

    fn cut_edges(&self) -> Vec<usize> {
        let mut cut = Vec::new();
        for c in self.cycles() {
            let set: HashSet<usize> = c.iter().copied().collect();
            for &h in &c {
                if h % 2 == 0 && set.contains(&(h + 1)) {
                    cut.push(h / 2);
                }
            }
        }
        cut
    }

    fn ring(&self, cycle: &[usize]) -> LineString<f64> {
        let mut pts: Vec<Coord<f64>> = cycle
            .iter()
            .map(|&h| {
                let (a, b) = self.edges[h / 2];
                self.points[if h.is_multiple_of(2) { a } else { b }]
            })
            .collect();
        pts.push(pts[0]);
        LineString::new(pts)
    }

    /// Bounded faces, each with the outer boundaries of the components directly inside it.
    fn faces(&self) -> Vec<Polygon<f64>> {
        let mut shells: Vec<(LineString<f64>, f64)> = Vec::new();
        let mut holes: Vec<LineString<f64>> = Vec::new();
        for c in self.cycles() {
            let r = self.ring(&c);
            let a = signed_area(&r.0);
            if a > 0.0 {
                shells.push((r, a));
            } else if a < 0.0 {
                holes.push(r);
            }
        }
        let shell_polys: Vec<Polygon<f64>> = shells
            .iter()
            .map(|(r, _)| Polygon::new(r.clone(), Vec::new()))
            .collect();
        let mut inner: Vec<Vec<LineString<f64>>> = vec![Vec::new(); shells.len()];
        for h in holes {
            let probe = Point(h.0[0]);
            let owner = shell_polys
                .iter()
                .enumerate()
                .filter(|(_, p)| p.contains(&probe))
                .min_by(|a, b| shells[a.0].1.total_cmp(&shells[b.0].1))
                .map(|(i, _)| i);
            if let Some(i) = owner {
                inner[i].push(h);
            }
        }
        shells
            .into_iter()
            .zip(inner)
            .map(|((r, _), holes)| Polygon::new(r, holes))
            .collect()
    }
}

fn signed_area(r: &[Coord<f64>]) -> f64 {
    r.windows(2)
        .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
        .sum::<f64>()
        / 2.0
}

/// All linework of a geometry: lines and polygon rings.
fn linework(g: &Geometry<f64>) -> Vec<LineString<f64>> {
    let mut out = lines_of(g);
    for p in polygons_of(g) {
        out.push(p.exterior().clone());
        out.extend(p.interiors().iter().cloned());
    }
    out
}

/// GEOS Polygonize: the faces formed by the linework.
pub(crate) fn polygonize(g: &Geometry<f64>) -> Geometry<f64> {
    let faces = Arrangement::new(&linework(g)).faces();
    Geometry::GeometryCollection(GeometryCollection(
        faces.into_iter().map(Geometry::Polygon).collect(),
    ))
}

/// GEOS BuildArea: the faces of the linework nested inside an even number of other face
/// shells, merged (inner rings make holes, rings inside those make islands).
pub(crate) fn build_area(g: &Geometry<f64>) -> Geometry<f64> {
    let faces = Arrangement::new(&linework(g)).faces();
    let shells: Vec<Polygon<f64>> = faces
        .iter()
        .map(|f| Polygon::new(f.exterior().clone(), Vec::new()))
        .collect();
    let kept: Vec<Polygon<f64>> = faces
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            f.interior_point().is_some_and(|p| {
                shells
                    .iter()
                    .enumerate()
                    .filter(|(j, s)| j != i && s.contains(&p))
                    .count()
                    % 2
                    == 0
            })
        })
        .map(|(_, f)| f.clone())
        .collect();
    let merged: MultiPolygon<f64> = geo::algorithm::unary_union(&kept);
    match merged.0.len() {
        0 => Geometry::GeometryCollection(GeometryCollection(Vec::new())),
        1 => Geometry::Polygon(merged.0.into_iter().next().unwrap()),
        _ => Geometry::MultiPolygon(merged),
    }
}

// --- Shared paths and crossing direction ---------------------------------------------------

fn shared_paths(a: &Geometry<f64>, b: &Geometry<f64>) -> Option<Geometry<f64>> {
    let (la, lb) = (lines_of(a), lines_of(b));
    (!la.is_empty() && !lb.is_empty()).then_some(())?;
    let sb: Vec<Line<f64>> = lb.iter().flat_map(|l| l.lines()).collect();
    let mut same: Vec<Vec<Coord<f64>>> = Vec::new();
    let mut opposite: Vec<Vec<Coord<f64>>> = Vec::new();
    for l in &la {
        for s in l.lines() {
            let d = s.delta();
            let len2 = d.x * d.x + d.y * d.y;
            if len2 == 0.0 {
                continue;
            }
            let tol = 1e-12 * len2.sqrt().max(1.0);
            let mut pieces: Vec<(f64, f64, bool)> = Vec::new();
            for t in &sb {
                let side = |p: Coord<f64>| {
                    ((p.x - s.start.x) * d.y - (p.y - s.start.y) * d.x) / len2.sqrt()
                };
                if side(t.start).abs() > tol || side(t.end).abs() > tol {
                    continue;
                }
                let param =
                    |p: Coord<f64>| ((p.x - s.start.x) * d.x + (p.y - s.start.y) * d.y) / len2;
                let (u, v) = (param(t.start), param(t.end));
                let (lo, hi) = (u.min(v).max(0.0), u.max(v).min(1.0));
                if hi - lo > 1e-12 {
                    pieces.push((lo, hi, v > u));
                }
            }
            pieces.sort_by(|x, y| x.0.total_cmp(&y.0));
            for (lo, hi, fwd) in pieces {
                let at = |t: f64| Coord {
                    x: s.start.x + d.x * t,
                    y: s.start.y + d.y * t,
                };
                let target = if fwd { &mut same } else { &mut opposite };
                let (p, q) = (at(lo), at(hi));
                target.push(vec![p, q]);
            }
        }
    }
    let ml = |v: Vec<Vec<Coord<f64>>>| {
        Geometry::MultiLineString(MultiLineString(
            v.into_iter().map(LineString::new).collect(),
        ))
    };
    Some(Geometry::GeometryCollection(GeometryCollection(vec![
        ml(same),
        ml(opposite),
    ])))
}

/// PostGIS `lw_segment_side`: -1 left, 1 right, 0 on the line.
fn side(p1: Coord<f64>, p2: Coord<f64>, q: Coord<f64>) -> i8 {
    let s = (q.x - p1.x) * (p2.y - p1.y) - (p2.x - p1.x) * (q.y - p1.y);
    if s < 0.0 {
        -1
    } else if s > 0.0 {
        1
    } else {
        0
    }
}

/// PostGIS `lwline_crossing_direction`.
fn crossing_direction(l1: &LineString<f64>, l2: &LineString<f64>) -> i64 {
    const LEFT: i8 = -1;
    const RIGHT: i8 = 1;
    let seg = |p1, p2, q1, q2| -> i8 {
        let (pq1, pq2) = (side(p1, p2, q1), side(p1, p2, q2));
        if (pq1 > 0 && pq2 > 0) || (pq1 < 0 && pq2 < 0) {
            return 0;
        }
        let (qp1, qp2) = (side(q1, q2, p1), side(q1, q2, p2));
        if (qp1 > 0 && qp2 > 0) || (qp1 < 0 && qp2 < 0) {
            return 0;
        }
        if pq1 == 0 && pq2 == 0 && qp1 == 0 && qp2 == 0 {
            return 0;
        }
        if pq2 == 0 || qp2 == 0 {
            return 0;
        }
        if pq1 == 0 {
            return if pq2 > 0 { RIGHT } else { LEFT };
        }
        if pq1 < pq2 { RIGHT } else { LEFT }
    };
    let (mut left, mut right, mut first) = (0, 0, 0i8);
    for q in l2.0.windows(2) {
        for p in l1.0.windows(2) {
            match seg(p[0], p[1], q[0], q[1]) {
                LEFT => {
                    left += 1;
                    if first == 0 {
                        first = LEFT;
                    }
                }
                RIGHT => {
                    right += 1;
                    if first == 0 {
                        first = RIGHT;
                    }
                }
                _ => {}
            }
        }
    }
    match (left, right) {
        (0, 0) => 0,
        (0, 1) => 1,
        (1, 0) => -1,
        (l, r) if l - r == 1 => -2,
        (l, r) if r - l == 1 => 2,
        (l, r) if l == r && first == LEFT => -3,
        (l, r) if l == r && first == RIGHT => 3,
        _ => 0,
    }
}

// --- Clearance and circles -----------------------------------------------------------------

/// The shortest distance between a vertex and a different vertex or a segment not ending at it.
fn minimum_clearance(g: &Geometry<f64>) -> Option<(f64, Coord<f64>, Coord<f64>)> {
    let mut verts: Vec<Coord<f64>> = g.coords_iter().collect();
    verts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    verts.dedup();
    let segs = segments(g);
    let mut best: Option<(f64, Coord<f64>, Coord<f64>)> = None;
    fn consider(
        best: &mut Option<(f64, Coord<f64>, Coord<f64>)>,
        d: f64,
        a: Coord<f64>,
        b: Coord<f64>,
    ) {
        if d > 0.0 && best.is_none_or(|x| d < x.0) {
            *best = Some((d, a, b));
        }
    }
    for (i, &v) in verts.iter().enumerate() {
        for &w in &verts[i + 1..] {
            if best.is_some_and(|b| w.x - v.x > b.0) {
                break;
            }
            consider(&mut best, Euclidean.distance(v, w), v, w);
        }
    }
    for &v in &verts {
        for s in &segs {
            if s.start == v || s.end == v {
                continue;
            }
            let c = match geo::ClosestPoint::closest_point(s, &Point(v)) {
                geo::Closest::Intersection(p) | geo::Closest::SinglePoint(p) => p.0,
                geo::Closest::Indeterminate => continue,
            };
            consider(&mut best, Euclidean.distance(v, c), v, c);
        }
    }
    best
}

#[derive(PartialEq)]
struct Cell {
    center: Coord<f64>,
    half: f64,
    value: f64,
    bound: f64,
}

impl Eq for Cell {}
impl PartialOrd for Cell {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cell {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bound.total_cmp(&other.bound)
    }
}

/// Quadtree search (polylabel) for the point of `bounds` maximising `value`, where `bound(c)` is
/// an upper estimate of `value` near `c` before adding the cell radius and `accept` says whether
/// a point may be the answer.
fn best_point(
    bounds: Rect<f64>,
    tolerance: f64,
    value: &dyn Fn(Coord<f64>) -> f64,
    accept: &dyn Fn(Coord<f64>) -> bool,
    start: Coord<f64>,
) -> (Coord<f64>, f64) {
    // The starting cells are squares as wide as the bounds' short side, but no smaller than
    // a 1024th of the long side: a very thin box (one side 1e-300 wide) would otherwise need
    // an astronomical number of them. Cells are split as the search goes, so this only
    // changes where it starts.
    let size = bounds
        .width()
        .min(bounds.height())
        .max(bounds.width().max(bounds.height()) / 1024.0);
    let make = |center: Coord<f64>, half: f64| {
        let v = value(center);
        Cell {
            center,
            half,
            value: v,
            bound: v + half * std::f64::consts::SQRT_2,
        }
    };
    let mut best = (
        start,
        if accept(start) {
            value(start)
        } else {
            f64::NEG_INFINITY
        },
    );
    if size <= 0.0 {
        return best;
    }
    let mut heap = BinaryHeap::new();
    let half = size / 2.0;
    let mut x = bounds.min().x;
    while x < bounds.max().x {
        let mut y = bounds.min().y;
        while y < bounds.max().y {
            heap.push(make(
                Coord {
                    x: x + half,
                    y: y + half,
                },
                half,
            ));
            y += size;
        }
        x += size;
    }
    let mut steps = 0;
    while let Some(cell) = heap.pop() {
        if accept(cell.center) && cell.value > best.1 {
            best = (cell.center, cell.value);
        }
        if cell.bound - best.1 <= tolerance {
            continue;
        }
        if steps > 200_000 {
            break;
        }
        steps += 1;
        let h = cell.half / 2.0;
        for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            heap.push(make(
                Coord {
                    x: cell.center.x + dx * h,
                    y: cell.center.y + dy * h,
                },
                h,
            ));
        }
    }
    best
}

fn distance_to_segments(p: Coord<f64>, segs: &[Line<f64>]) -> f64 {
    segs.iter()
        .map(|s| Euclidean.distance(s, &Point(p)))
        .fold(f64::INFINITY, f64::min)
}

/// The centre of the largest circle inside the polygons, and its radius.
fn maximum_inscribed_circle(
    g: &Geometry<f64>,
    tolerance: Option<f64>,
) -> Option<(Coord<f64>, f64)> {
    let polys = polygons_of(g);
    (!polys.is_empty()).then_some(())?;
    let mp = MultiPolygon(polys);
    let segs = segments(&Geometry::MultiPolygon(mp.clone()));
    let r = mp.bounding_rect()?;
    let tol = tolerance
        .filter(|t| *t > 0.0)
        .unwrap_or(r.width().max(r.height()) / 1000.0);
    let value = |c: Coord<f64>| {
        let d = distance_to_segments(c, &segs);
        if mp.contains(&Point(c)) { d } else { -d }
    };
    let start = mp.interior_point()?.0;
    let (c, d) = best_point(r, tol, &value, &|c| mp.contains(&Point(c)), start);
    Some((c, d))
}

/// The centre of the largest circle with no part of `g` inside, centred within `boundary`.
fn largest_empty_circle(
    g: &Geometry<f64>,
    tolerance: Option<f64>,
    boundary: Option<&Geometry<f64>>,
) -> Option<(Coord<f64>, f64)> {
    let polys = MultiPolygon(polygons_of(g));
    let segs = segments(g);
    let pts: Vec<Coord<f64>> = crate::postgis_processing::points_of(g)
        .iter()
        .map(|p| p.0)
        .collect();
    let boundary: Geometry<f64> = match boundary {
        Some(b) => b.clone(),
        None => Geometry::Polygon(g.convex_hull()),
    };
    let area = MultiPolygon(polygons_of(&boundary));
    (!area.0.is_empty()).then_some(())?;
    let r = area.bounding_rect()?;
    let tol = tolerance
        .filter(|t| *t > 0.0)
        .unwrap_or(r.width().max(r.height()) / 1000.0);
    let value = |c: Coord<f64>| {
        if !polys.0.is_empty() && polys.contains(&Point(c)) {
            return 0.0;
        }
        let mut d = distance_to_segments(c, &segs);
        for p in &pts {
            d = d.min(Euclidean.distance(*p, c));
        }
        d
    };
    let accept = |c: Coord<f64>| area.contains(&Point(c)) || area.intersects(&Point(c));
    let start = area.interior_point()?.0;
    Some(best_point(r, tol, &value, &accept, start))
}

// --- Polygon hull simplification -----------------------------------------------------------

fn segments_cross(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>, d: Coord<f64>) -> bool {
    use geo::line_intersection::{LineIntersection, line_intersection};
    match line_intersection(Line::new(a, b), Line::new(c, d)) {
        Some(LineIntersection::SinglePoint { is_proper, .. }) => is_proper,
        Some(LineIntersection::Collinear { .. }) => true,
        None => false,
    }
}

/// Removes ring vertices whose removal only grows (`outer`) or only shrinks the polygon,
/// smallest change first, until `target` vertices are left or no removal keeps it simple.
fn hull_ring(
    ring: &LineString<f64>,
    others: &[Line<f64>],
    target: usize,
    outer: bool,
) -> LineString<f64> {
    let mut pts: Vec<Coord<f64>> = ring.0[..ring.0.len().saturating_sub(1)].to_vec();
    if pts.len() < 3 {
        // An empty or degenerate ring: nothing to simplify.
        return ring.clone();
    }
    let target = target.max(3);
    while pts.len() > target {
        let n = pts.len();
        let mut best: Option<(f64, usize)> = None;
        for i in 0..n {
            let (a, v, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
            let turn = (v.x - a.x) * (c.y - v.y) - (v.y - a.y) * (c.x - v.x);
            // With the polygon on the left of the ring, a right turn is a notch.
            let grows = turn < 0.0;
            if turn != 0.0 && grows != outer {
                continue;
            }
            let area = turn.abs() / 2.0;
            if best.is_some_and(|(b, _)| area >= b) {
                continue;
            }
            let crosses = (0..n).any(|j| {
                let (p, q) = (pts[j], pts[(j + 1) % n]);
                j != (i + n - 1) % n
                    && j != i
                    && j != (i + n - 2) % n
                    && j != (i + 1) % n
                    && segments_cross(a, c, p, q)
            }) || others.iter().any(|s| segments_cross(a, c, s.start, s.end));
            if !crosses {
                best = Some((area, i));
            }
        }
        let Some((_, i)) = best else { break };
        pts.remove(i);
    }
    pts.push(pts[0]);
    LineString::new(pts)
}

fn polygon_hull(g: &Geometry<f64>, fraction: f64, outer: bool) -> Geometry<f64> {
    let one = |p: &Polygon<f64>| {
        let p = p.orient(Direction::Default);
        let rings: Vec<&LineString<f64>> =
            std::iter::once(p.exterior()).chain(p.interiors()).collect();
        let mut out: Vec<LineString<f64>> = Vec::new();
        for (k, r) in rings.iter().enumerate() {
            let others: Vec<Line<f64>> = rings
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != k)
                .flat_map(|(_, r)| r.lines())
                .collect();
            let target = ((r.0.len().saturating_sub(1)) as f64 * fraction).ceil() as usize;
            out.push(hull_ring(r, &others, target, outer));
        }
        let mut it = out.into_iter();
        Polygon::new(it.next().unwrap(), it.collect())
    };
    polygons(polygons_of(g).iter().map(one).collect())
}

// --- TWKB ----------------------------------------------------------------------------------

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct TwkbWriter {
    out: Vec<u8>,
    factor: f64,
    last: [i64; 2],
}

impl TwkbWriter {
    fn coords(&mut self, cs: &[Coord<f64>]) {
        for c in cs {
            for (i, v) in [c.x, c.y].into_iter().enumerate() {
                let q = (v * self.factor).round() as i64;
                put_varint(&mut self.out, zigzag(q - self.last[i]));
                self.last[i] = q;
            }
        }
    }
    fn geometry(&mut self, g: &Geometry<f64>, precision: i64) {
        let kind: u8 = match g {
            Geometry::Point(_) => 1,
            Geometry::Line(_) | Geometry::LineString(_) => 2,
            Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => 3,
            Geometry::MultiPoint(_) => 4,
            Geometry::MultiLineString(_) => 5,
            Geometry::MultiPolygon(_) => 6,
            Geometry::GeometryCollection(_) => 7,
        };
        self.out.push(kind | ((zigzag(precision) as u8) << 4));
        let empty = g.coords_iter().next().is_none();
        self.out.push(if empty { 0x10 } else { 0 });
        if empty {
            return;
        }
        self.last = [0, 0];
        let ring = |w: &mut Self, l: &LineString<f64>| {
            put_varint(&mut w.out, l.0.len() as u64);
            w.coords(&l.0);
        };
        let poly = |w: &mut Self, p: &Polygon<f64>| {
            put_varint(&mut w.out, 1 + p.interiors().len() as u64);
            ring(w, p.exterior());
            p.interiors().iter().for_each(|r| ring(w, r));
        };
        match g {
            Geometry::Point(p) => self.coords(&[p.0]),
            Geometry::Line(l) => ring(self, &LineString::new(vec![l.start, l.end])),
            Geometry::LineString(l) => ring(self, l),
            Geometry::Polygon(p) => poly(self, p),
            Geometry::Rect(r) => poly(self, &r.to_polygon()),
            Geometry::Triangle(t) => poly(self, &t.to_polygon()),
            Geometry::MultiPoint(m) => {
                put_varint(&mut self.out, m.0.len() as u64);
                let cs: Vec<Coord<f64>> = m.0.iter().map(|p| p.0).collect();
                self.coords(&cs);
            }
            Geometry::MultiLineString(m) => {
                put_varint(&mut self.out, m.0.len() as u64);
                m.0.iter().for_each(|l| ring(self, l));
            }
            Geometry::MultiPolygon(m) => {
                put_varint(&mut self.out, m.0.len() as u64);
                m.0.iter().for_each(|p| poly(self, p));
            }
            Geometry::GeometryCollection(c) => {
                put_varint(&mut self.out, c.0.len() as u64);
                for g in &c.0 {
                    self.geometry(g, precision);
                }
            }
        }
    }
}

pub(crate) fn to_twkb(g: &Geometry<f64>, precision: i64) -> Vec<u8> {
    let mut w = TwkbWriter {
        out: Vec::new(),
        factor: 10f64.powi(precision as i32),
        last: [0, 0],
    };
    w.geometry(g, precision);
    w.out
}

struct TwkbReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl TwkbReader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    fn count(&mut self) -> Option<usize> {
        let n = self.varint()? as usize;
        (n <= self.bytes.len() * 2).then_some(n)
    }
    #[allow(clippy::type_complexity)]
    fn geometry(&mut self, depth: usize) -> Option<Geometry<f64>> {
        (depth < 32).then_some(())?;
        let head = self.byte()?;
        let kind = head & 0x0f;
        let precision = unzigzag(u64::from(head >> 4));
        let meta = self.byte()?;
        let (has_box, has_size, has_ids, extended, empty) = (
            meta & 1 != 0,
            meta & 2 != 0,
            meta & 4 != 0,
            meta & 8 != 0,
            meta & 16 != 0,
        );
        let mut dims = 2;
        if extended {
            let e = self.byte()?;
            dims += usize::from(e & 1 != 0) + usize::from(e & 2 != 0);
        }
        if has_size {
            self.varint()?;
        }
        if empty {
            return Some(match kind {
                1 | 4 => Geometry::MultiPoint(geo::MultiPoint(Vec::new())),
                2 => Geometry::LineString(LineString::new(Vec::new())),
                3 => Geometry::Polygon(Polygon::new(LineString::new(Vec::new()), Vec::new())),
                5 => Geometry::MultiLineString(MultiLineString(Vec::new())),
                6 => Geometry::MultiPolygon(MultiPolygon(Vec::new())),
                _ => Geometry::GeometryCollection(GeometryCollection(Vec::new())),
            });
        }
        if has_box {
            for _ in 0..dims * 2 {
                self.varint()?;
            }
        }
        let factor = 10f64.powi(precision as i32);
        let mut last = vec![0i64; dims];
        let mut coords = |r: &mut Self, n: usize| -> Option<Vec<Coord<f64>>> {
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                for v in last.iter_mut() {
                    *v += unzigzag(r.varint()?);
                }
                out.push(Coord {
                    x: last[0] as f64 / factor,
                    y: last[1] as f64 / factor,
                });
            }
            Some(out)
        };
        let ring =
            |r: &mut Self, c: &mut dyn FnMut(&mut Self, usize) -> Option<Vec<Coord<f64>>>| {
                let n = r.count()?;
                c(r, n).map(LineString::new)
            };
        let poly =
            |r: &mut Self, c: &mut dyn FnMut(&mut Self, usize) -> Option<Vec<Coord<f64>>>| {
                let n = r.count()?;
                let mut rings = Vec::new();
                for _ in 0..n {
                    let k = r.count()?;
                    rings.push(LineString::new(c(r, k)?));
                }
                let mut it = rings.into_iter();
                Some(Polygon::new(
                    it.next().unwrap_or_else(|| LineString::new(Vec::new())),
                    it.collect(),
                ))
            };
        let skip_ids = |r: &mut Self, n: usize| -> Option<()> {
            if has_ids {
                for _ in 0..n {
                    r.varint()?;
                }
            }
            Some(())
        };
        Some(match kind {
            1 => Geometry::Point(Point(coords(self, 1)?[0])),
            2 => Geometry::LineString(ring(self, &mut coords)?),
            3 => Geometry::Polygon(poly(self, &mut coords)?),
            4 => {
                let n = self.count()?;
                skip_ids(self, n)?;
                Geometry::MultiPoint(geo::MultiPoint(
                    coords(self, n)?.into_iter().map(Point).collect(),
                ))
            }
            5 => {
                let n = self.count()?;
                skip_ids(self, n)?;
                let mut v = Vec::new();
                for _ in 0..n {
                    v.push(ring(self, &mut coords)?);
                }
                Geometry::MultiLineString(MultiLineString(v))
            }
            6 => {
                let n = self.count()?;
                skip_ids(self, n)?;
                let mut v = Vec::new();
                for _ in 0..n {
                    v.push(poly(self, &mut coords)?);
                }
                Geometry::MultiPolygon(MultiPolygon(v))
            }
            7 => {
                let n = self.count()?;
                skip_ids(self, n)?;
                let mut v = Vec::new();
                for _ in 0..n {
                    v.push(self.geometry(depth + 1)?);
                }
                Geometry::GeometryCollection(GeometryCollection(v))
            }
            _ => return None,
        })
    }
}

pub(crate) fn from_twkb(bytes: &[u8]) -> Option<Geometry<f64>> {
    let mut r = TwkbReader { bytes, pos: 0 };
    let g = r.geometry(0)?;
    (r.pos == bytes.len()).then_some(g)
}

// --- Grid cells ----------------------------------------------------------------------------

/// PostGIS ST_Hexagon: flat-topped hexagons with edge `size`, odd columns shifted up.
pub(crate) fn hexagon(size: f64, i: i64, j: i64, origin: Coord<f64>) -> Polygon<f64> {
    const HX: [f64; 7] = [-1.0, -0.5, 0.5, 1.0, 0.5, -0.5, -1.0];
    const HY: [f64; 7] = [0.0, -0.5, -0.5, 0.0, 0.5, 0.5, 0.0];
    let height = size * 3f64.sqrt();
    let ox = origin.x + 1.5 * size * i as f64;
    let mut oy = origin.y + height * j as f64;
    if i % 2 != 0 {
        oy += 0.5 * height;
    }
    Polygon::new(
        LineString::new(
            (0..7)
                .map(|k| Coord {
                    x: ox + size * HX[k],
                    y: oy + height * HY[k],
                })
                .collect(),
        ),
        Vec::new(),
    )
}

pub(crate) fn square(size: f64, i: i64, j: i64, origin: Coord<f64>) -> Polygon<f64> {
    let (x, y) = (origin.x + size * i as f64, origin.y + size * j as f64);
    Rect::new(
        Coord { x, y },
        Coord {
            x: x + size,
            y: y + size,
        },
    )
    .to_polygon()
}

// --- Clusters (aggregate) ------------------------------------------------------------------

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// Groups of geometries connected by `linked`, in input order.
pub(crate) fn clusters(
    geoms: &[Geometry<f64>],
    linked: impl Fn(&Geometry<f64>, &Geometry<f64>) -> bool,
    reach: f64,
) -> Vec<Vec<usize>> {
    let n = geoms.len();
    let boxes: Vec<Option<Rect<f64>>> = geoms.iter().map(|g| g.bounding_rect()).collect();
    let mut order: Vec<usize> = (0..n).filter(|&i| boxes[i].is_some()).collect();
    order.sort_by(|&a, &b| {
        boxes[a]
            .unwrap()
            .min()
            .x
            .total_cmp(&boxes[b].unwrap().min().x)
    });
    let mut parent: Vec<usize> = (0..n).collect();
    for (k, &i) in order.iter().enumerate() {
        let bi = boxes[i].unwrap();
        for &j in &order[k + 1..] {
            let bj = boxes[j].unwrap();
            if bj.min().x > bi.max().x + reach {
                break;
            }
            if bj.min().y > bi.max().y + reach || bi.min().y > bj.max().y + reach {
                continue;
            }
            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
            if ri != rj && linked(&geoms[i], &geoms[j]) {
                parent[ri.max(rj)] = ri.min(rj);
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut index: HashMap<usize, usize> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let k = *index.entry(r).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[k].push(i);
    }
    groups
}

pub(crate) fn cluster_intersecting(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
    let groups = clusters(&geoms, |a, b| a.intersects(b), 0.0);
    Geometry::GeometryCollection(GeometryCollection(
        groups
            .into_iter()
            .map(|g| {
                Geometry::GeometryCollection(GeometryCollection(
                    g.into_iter().map(|i| geoms[i].clone()).collect(),
                ))
            })
            .collect(),
    ))
}

fn hex(bytes: &[u8]) -> Option<Term> {
    Some(Literal::new_typed_literal(to_hex(bytes), NamedNodeRef::new_unchecked(HEX_BINARY)).into())
}

fn typed_wkb(hex: &str, srid: Option<&Term>, kinds: &[&str]) -> Option<Term> {
    let t = from_wkb(
        hex,
        match srid {
            Some(s) => Some(int(s)?),
            None => None,
        },
    )?;
    let g = geom(&t)?;
    (kinds.is_empty() || kinds.contains(&crate::postgis::postgis_type(&g.geometry))).then_some(t)
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    for (name, kinds) in [
        ("ST_PointFromWKB", &["POINT"][..]),
        ("ST_LineFromWKB", &["LINESTRING"][..]),
        ("ST_LinestringFromWKB", &["LINESTRING"][..]),
    ] {
        add(
            ST,
            name,
            func(move |args| match args {
                [h] => typed_wkb(text(h)?, None, kinds),
                [h, s] => typed_wkb(text(h)?, Some(s), kinds),
                _ => None,
            }),
        );
    }
    add(
        ST,
        "ST_GeogFromWKB",
        func(|args| {
            let [h] = args else { return None };
            typed_wkb(text(h)?, None, &[])
        }),
    );
    for name in ["ST_GeogFromText", "ST_GeographyFromText"] {
        add(
            ST,
            name,
            func(|args| {
                let [t] = args else { return None };
                let t = text(t)?.trim();
                let wkt = match t.split_once(';') {
                    Some((s, w)) if s.trim().to_ascii_uppercase().starts_with("SRID=") => w,
                    _ => t,
                };
                typed_text(wkt, &[], None)
            }),
        );
    }
    for (name, multi) in [("ST_BdPolyFromText", false), ("ST_BdMPolyFromText", true)] {
        add(
            ST,
            name,
            func(move |args| {
                let [t, s] = args else { return None };
                let lines = geom(&typed_text(text(t)?, &["MULTILINESTRING"], Some(s))?)?;
                let area = build_area(&lines.geometry);
                let area = match (area, multi) {
                    (Geometry::Polygon(p), false) => Geometry::Polygon(p),
                    (Geometry::Polygon(p), true) => Geometry::MultiPolygon(MultiPolygon(vec![p])),
                    (Geometry::MultiPolygon(m), true) => Geometry::MultiPolygon(m),
                    _ => return None,
                };
                out(area, &lines.crs)
            }),
        );
    }
    add(
        ST,
        "ST_BuildArea",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(build_area(&g.geometry), &g.crs)
        }),
    );
    add(
        ST,
        "ST_Polygonize",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(polygonize(&g.geometry), &g.crs)
        }),
    );
    add(
        ST,
        "ST_SharedPaths",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            out(shared_paths(&a.geometry, &b.geometry)?, &a.crs)
        }),
    );
    add(
        ST,
        "ST_LineCrossingDirection",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            let (Geometry::LineString(l1), Geometry::LineString(l2)) = (&a.geometry, &b.geometry)
            else {
                return None;
            };
            integer(crossing_direction(l1, l2))
        }),
    );
    add(
        ST,
        "ST_MinimumClearance",
        func(|args| {
            let [a] = args else { return None };
            double(minimum_clearance(&geom(a)?.geometry)?.0)
        }),
    );
    add(
        ST,
        "ST_MinimumClearanceLine",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let line = match minimum_clearance(&g.geometry) {
                Some((_, p, q)) => LineString::new(vec![p, q]),
                None => LineString::new(Vec::new()),
            };
            out(Geometry::LineString(line), &g.crs)
        }),
    );
    add(
        ST,
        "ST_MaximumInscribedCircle",
        func(|args| {
            let g = geom(args.first()?)?;
            let tol = args.get(1).and_then(num);
            let (c, _) = maximum_inscribed_circle(&g.geometry, tol)?;
            out(Geometry::Point(Point(c)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_LargestEmptyCircle",
        func(|args| {
            let g = geom(args.first()?)?;
            let tol = args.get(1).and_then(num);
            let boundary = match args.get(2) {
                Some(b) => Some(geom(b)?),
                None => None,
            };
            let (c, _) =
                largest_empty_circle(&g.geometry, tol, boundary.as_deref().map(|b| &b.geometry))?;
            out(Geometry::Point(Point(c)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_MinimumBoundingRadius",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let pts: Vec<Coord<f64>> = g.geometry.coords_iter().collect();
            match crate::geosparql11::bounding_circle(&pts)? {
                Geometry::Polygon(p) => {
                    let ring = &p.exterior().0[..p.exterior().0.len() - 1];
                    let n = ring.len() as f64;
                    let c = Coord {
                        x: ring.iter().map(|c| c.x).sum::<f64>() / n,
                        y: ring.iter().map(|c| c.y).sum::<f64>() / n,
                    };
                    double(Euclidean.distance(c, ring[0]) * (std::f64::consts::PI / n).cos())
                }
                _ => double(0.0),
            }
        }),
    );
    add(
        ST,
        "ST_SimplifyPolygonHull",
        func(|args| {
            let g = geom(args.first()?)?;
            let f = num(args.get(1)?)?;
            (0.0..=1.0).contains(&f).then_some(())?;
            let outer = match args.get(2) {
                Some(Term::Literal(l)) => l.value() != "false",
                _ => true,
            };
            out(polygon_hull(&g.geometry, f, outer), &g.crs)
        }),
    );
    add(
        ST,
        "ST_TriangulatePolygon",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let mp = MultiPolygon(polygons_of(&g.geometry));
            let tris = mp.constrained_triangulation(Default::default()).ok()?;
            out(
                Geometry::GeometryCollection(GeometryCollection(
                    tris.into_iter()
                        .map(|t| Geometry::Polygon(t.to_polygon()))
                        .collect(),
                )),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_AsTWKB",
        func(|args| {
            let g = geom(args.first()?)?;
            let precision = args.get(1).map_or(Some(0), int)?;
            (-7..=7).contains(&precision).then_some(())?;
            // TWKB stores coordinates as integers scaled by 10^precision; infinite ones, or
            // ones too large for that, have no encoding.
            let factor = 10f64.powi(precision as i32);
            g.geometry
                .coords_iter()
                .all(|c| (c.x * factor).abs() < 1e15 && (c.y * factor).abs() < 1e15)
                .then_some(())?;
            hex(&to_twkb(&g.geometry, precision))
        }),
    );
    add(
        ST,
        "ST_GeomFromTWKB",
        func(|args| {
            let [h] = args else { return None };
            out(from_twkb(&from_hex(text(h)?)?)?, &Crs::Crs84)
        }),
    );
    let origin = |t: Option<&Term>| -> Option<Coord<f64>> {
        match t {
            None => Some(Coord { x: 0.0, y: 0.0 }),
            Some(t) => match geom(t)?.geometry {
                Geometry::Point(p) => Some(p.0),
                _ => None,
            },
        }
    };
    add(
        ST,
        "ST_Hexagon",
        func(move |args| {
            let size = num(args.first()?)?;
            (size > 0.0).then_some(())?;
            let (i, j) = (int(args.get(1)?)?, int(args.get(2)?)?);
            out(
                Geometry::Polygon(hexagon(size, i, j, origin(args.get(3))?)),
                &Crs::Crs84,
            )
        }),
    );
    add(
        ST,
        "ST_Square",
        func(move |args| {
            let size = num(args.first()?)?;
            (size > 0.0).then_some(())?;
            let (i, j) = (int(args.get(1)?)?, int(args.get(2)?)?);
            out(
                Geometry::Polygon(square(size, i, j, origin(args.get(3))?)),
                &Crs::Crs84,
            )
        }),
    );
    // GeoSPARQL 1.1 drafts named these; geof:distance and geof:buffer default to metres.
    add(
        GEOF,
        "metricDistance",
        func(|args| {
            let [a, b] = args else { return None };
            crate::functions::function(&format!("{GEOF}distance"))?(&[a.clone(), b.clone()])
        }),
    );
    add(
        GEOF,
        "metricBuffer",
        func(|args| {
            let [a, r] = args else { return None };
            crate::functions::function(&format!("{GEOF}buffer"))?(&[a.clone(), r.clone()])
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    fn ls(pts: &[(f64, f64)]) -> LineString<f64> {
        LineString::new(pts.iter().map(|&(x, y)| Coord { x, y }).collect())
    }

    #[test]
    fn polygonize_two_squares_and_a_hole() {
        let lines = Geometry::MultiLineString(MultiLineString(vec![
            ls(&[(0.0, 0.0), (2.0, 0.0), (2.0, 1.0), (0.0, 1.0), (0.0, 0.0)]),
            ls(&[(1.0, 0.0), (1.0, 1.0)]),
            ls(&[(5.0, 5.0), (9.0, 5.0), (9.0, 9.0), (5.0, 9.0), (5.0, 5.0)]),
            ls(&[(6.0, 6.0), (7.0, 6.0), (7.0, 7.0), (6.0, 7.0), (6.0, 6.0)]),
            // A dangle.
            ls(&[(2.0, 1.0), (3.0, 3.0)]),
        ]));
        let Geometry::GeometryCollection(c) = polygonize(&lines) else {
            panic!()
        };
        assert_eq!(c.0.len(), 4);
        let mut areas: Vec<f64> = c.0.iter().map(|g| g.unsigned_area()).collect();
        areas.sort_by(f64::total_cmp);
        assert_eq!(areas, vec![1.0, 1.0, 1.0, 15.0]);
        let area = build_area(&lines);
        assert_eq!(area.unsigned_area(), 2.0 + 15.0);
    }

    #[test]
    fn crossing_direction_matches_postgis_docs() {
        let l1 = ls(&[(25.0, 169.0), (89.0, 114.0), (40.0, 70.0), (86.0, 43.0)]);
        let l2 = ls(&[(171.0, 154.0), (20.0, 140.0), (71.0, 74.0), (161.0, 53.0)]);
        assert_eq!(crossing_direction(&l1, &l2), 3);
        assert_eq!(crossing_direction(&l2, &l1), -3);
    }

    #[test]
    fn twkb_round_trip() {
        let g = Geometry::LineString(ls(&[(1.0, 2.0), (3.5, 4.25), (-1.0, 0.0)]));
        let b = to_twkb(&g, 2);
        assert_eq!(from_twkb(&b).unwrap(), g);
        // PostGIS doc example: ST_AsTWKB('LINESTRING(1 1,5 5)') = \x02000202020808
        let l = Geometry::LineString(ls(&[(1.0, 1.0), (5.0, 5.0)]));
        assert_eq!(to_hex(&to_twkb(&l, 0)), "02000202020808");
    }

    #[test]
    fn inscribed_circle_of_a_square() {
        let sq = Geometry::Polygon(Polygon::new(
            ls(&[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ]),
            Vec::new(),
        ));
        let (c, r) = maximum_inscribed_circle(&sq, Some(0.001)).unwrap();
        assert!((c.x - 5.0).abs() < 0.01 && (c.y - 5.0).abs() < 0.01 && (r - 5.0).abs() < 0.01);
    }
}
