//! PostGIS functions on Z and M ordinates (constructors, editors, measures and linear
//! referencing along M, 3-D measures, trajectories), bounding-box operators and further
//! coordinate editors, under their `st:ST_*` names.

use crate::functions::{Func, GEOF, ST, boolean, double, func, geom, integer, num, same_space};
use crate::literal::{Crs, Geom, Zm, geom_literal};
use crate::postgis::{crs_of_srid, envelope, int, out, polygon, string};
use crate::postgis_processing::{
    lines_of, make_valid, map_lines, offset_curve, out_zm, polygons_of, remove_repeated,
};
use geo::line_measures::{Distance, Euclidean, Length};
use geo::{
    BooleanOps, BoundingRect, Contains, Coord, CoordsIter, Geometry, GeometryCollection, Line,
    LineString, MapCoords, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon, Rect,
};
use oxigraph::model::Term;
use std::sync::Arc;

const NONE: (f64, f64) = (f64::NAN, f64::NAN);

fn zm_at(g: &Geom, c: Coord<f64>) -> (f64, f64) {
    g.zm.as_ref().and_then(|z| z.at(c)).unwrap_or(NONE)
}

fn has_z(g: &Geom) -> bool {
    g.zm.as_ref().is_some_and(|z| z.has_z)
}

fn has_m(g: &Geom) -> bool {
    g.zm.as_ref().is_some_and(|z| z.has_m)
}

/// A geometry whose coordinates get Z and M from `f`, in coordinate order.
fn with_zm(
    geometry: Geometry<f64>,
    crs: Crs,
    z: bool,
    m: bool,
    f: impl FnMut(Coord<f64>) -> (f64, f64),
) -> Option<Term> {
    if !z && !m {
        return out(geometry, &crs);
    }
    let values: Vec<(f64, f64)> = geometry.coords_iter().map(f).collect();
    let zm = Zm::from_coords(&geometry, z, m, values);
    out_zm(geometry, crs, Some(zm))
}

/// Rewrites every coordinate with its Z and M.
fn transform(
    g: &Geom,
    z: bool,
    m: bool,
    f: impl Fn(Coord<f64>, (f64, f64)) -> (Coord<f64>, (f64, f64)),
) -> Option<Term> {
    let geometry = g.geometry.map_coords(|c| f(c, zm_at(g, c)).0);
    let values: Vec<(f64, f64)> = g
        .geometry
        .coords_iter()
        .map(|c| f(c, zm_at(g, c)).1)
        .collect();
    if !z && !m {
        return out(geometry, &g.crs);
    }
    let zm = Zm::from_coords(&geometry, z, m, values);
    out_zm(geometry, g.crs.clone(), Some(zm))
}

/// A point with optional Z and M.
pub(crate) fn point_zm(x: f64, y: f64, z: Option<f64>, m: Option<f64>, crs: Crs) -> Option<Term> {
    with_zm(
        Geometry::Point(Point::new(x, y)),
        crs,
        z.is_some(),
        m.is_some(),
        |_| (z.unwrap_or(f64::NAN), m.unwrap_or(f64::NAN)),
    )
}

fn srid_arg(t: Option<&Term>) -> Option<Crs> {
    match t {
        None => Some(Crs::Crs84),
        Some(t) => match int(t)? {
            0 => Some(Crs::Crs84),
            s => crs_of_srid(s),
        },
    }
}

// --- Normalize -----------------------------------------------------------------------------

fn sort_index(g: &Geometry<f64>) -> u8 {
    match g {
        Geometry::Point(_) => 0,
        Geometry::MultiPoint(_) => 1,
        Geometry::Line(_) | Geometry::LineString(_) => 2,
        Geometry::MultiLineString(_) => 4,
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => 5,
        Geometry::MultiPolygon(_) => 6,
        Geometry::GeometryCollection(_) => 7,
    }
}

fn cmp_coords(a: &[Coord<f64>], b: &[Coord<f64>]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = x.x.total_cmp(&y.x).then(x.y.total_cmp(&y.y));
        if o.is_ne() {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

fn cmp_geometry(a: &Geometry<f64>, b: &Geometry<f64>) -> std::cmp::Ordering {
    sort_index(a)
        .cmp(&sort_index(b))
        .then_with(|| match (a, b) {
            (Geometry::Polygon(p), Geometry::Polygon(q)) => {
                cmp_coords(&p.exterior().0, &q.exterior().0).then_with(|| {
                    for (r, s) in p.interiors().iter().zip(q.interiors()) {
                        let o = cmp_coords(&r.0, &s.0);
                        if o.is_ne() {
                            return o;
                        }
                    }
                    p.interiors().len().cmp(&q.interiors().len())
                })
            }
            (Geometry::GeometryCollection(p), Geometry::GeometryCollection(q)) => {
                for (r, s) in p.0.iter().zip(&q.0) {
                    let o = cmp_geometry(r, s);
                    if o.is_ne() {
                        return o;
                    }
                }
                p.0.len().cmp(&q.0.len())
            }
            _ => cmp_coords(
                &a.coords_iter().collect::<Vec<_>>(),
                &b.coords_iter().collect::<Vec<_>>(),
            ),
        })
}

/// A ring starting at its smallest vertex, clockwise (or counter-clockwise).
fn normalize_ring(r: &LineString<f64>, clockwise: bool) -> LineString<f64> {
    use geo::Winding;
    if r.0.len() < 4 {
        return r.clone();
    }
    let mut pts: Vec<Coord<f64>> = r.0[..r.0.len() - 1].to_vec();
    let min = (0..pts.len())
        .min_by(|&i, &j| {
            pts[i]
                .x
                .total_cmp(&pts[j].x)
                .then(pts[i].y.total_cmp(&pts[j].y))
        })
        .unwrap_or(0);
    pts.rotate_left(min);
    pts.push(pts[0]);
    let mut ring = LineString::new(pts);
    if ring.is_cw() != clockwise {
        ring.0.reverse();
    }
    ring
}

/// GEOS `normalize`: a canonical form for comparing geometries.
fn normalize(g: &Geometry<f64>) -> Geometry<f64> {
    match g {
        Geometry::LineString(l) => {
            let rev: Vec<Coord<f64>> = l.0.iter().rev().copied().collect();
            if cmp_coords(&rev, &l.0).is_lt() {
                Geometry::LineString(LineString::new(rev))
            } else {
                g.clone()
            }
        }
        Geometry::Polygon(p) => {
            let mut holes: Vec<LineString<f64>> = p
                .interiors()
                .iter()
                .map(|r| normalize_ring(r, false))
                .collect();
            holes.sort_by(|a, b| cmp_coords(&b.0, &a.0));
            Geometry::Polygon(Polygon::new(normalize_ring(p.exterior(), true), holes))
        }
        Geometry::MultiPoint(m) => {
            let mut v = m.0.clone();
            v.sort_by(|a, b| cmp_coords(&[b.0], &[a.0]));
            Geometry::MultiPoint(MultiPoint(v))
        }
        Geometry::MultiLineString(m) => {
            let mut v: Vec<LineString<f64>> =
                m.0.iter()
                    .map(|l| match normalize(&Geometry::LineString(l.clone())) {
                        Geometry::LineString(l) => l,
                        _ => unreachable!(),
                    })
                    .collect();
            v.sort_by(|a, b| cmp_coords(&b.0, &a.0));
            Geometry::MultiLineString(MultiLineString(v))
        }
        Geometry::MultiPolygon(m) => {
            let mut v: Vec<Geometry<f64>> =
                m.0.iter()
                    .map(|p| normalize(&Geometry::Polygon(p.clone())))
                    .collect();
            v.sort_by(|a, b| cmp_geometry(b, a));
            Geometry::MultiPolygon(MultiPolygon(
                v.into_iter()
                    .filter_map(|g| Polygon::try_from(g).ok())
                    .collect(),
            ))
        }
        Geometry::GeometryCollection(c) => {
            let mut v: Vec<Geometry<f64>> = c.0.iter().map(normalize).collect();
            v.sort_by(|a, b| cmp_geometry(b, a));
            Geometry::GeometryCollection(GeometryCollection(v))
        }
        Geometry::Rect(r) => normalize(&Geometry::Polygon(r.to_polygon())),
        Geometry::Triangle(t) => normalize(&Geometry::Polygon(t.to_polygon())),
        other => other.clone(),
    }
}

// --- Homogenize, summary, memory size --------------------------------------------------------

fn flatten(g: &Geometry<f64>, out: &mut Vec<Geometry<f64>>) {
    match g {
        Geometry::GeometryCollection(c) => c.0.iter().for_each(|g| flatten(g, out)),
        Geometry::MultiPoint(m) => out.extend(m.0.iter().map(|p| Geometry::Point(*p))),
        Geometry::MultiLineString(m) => out.extend(m.0.iter().cloned().map(Geometry::LineString)),
        Geometry::MultiPolygon(m) => out.extend(m.0.iter().cloned().map(Geometry::Polygon)),
        other => out.push(other.clone()),
    }
}

fn homogenize(g: &Geometry<f64>) -> Geometry<f64> {
    let Geometry::GeometryCollection(_) = g else {
        return g.clone();
    };
    let mut parts = Vec::new();
    flatten(g, &mut parts);
    match parts.len() {
        0 => g.clone(),
        1 => parts.remove(0),
        _ => crate::postgis::collect(parts),
    }
}

fn summary(g: &Geometry<f64>, flags: &str, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    let kind = match g {
        Geometry::Point(_) => "Point",
        Geometry::Line(_) | Geometry::LineString(_) => "LineString",
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => "Polygon",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
    };
    out.push_str(&format!("{pad}{kind}[{flags}]"));
    match g {
        Geometry::Point(_) => {}
        Geometry::Line(_) => out.push_str(" with 2 points"),
        Geometry::LineString(l) => out.push_str(&format!(" with {} points", l.0.len())),
        Geometry::Polygon(p) => {
            out.push_str(&format!(" with {} rings", 1 + p.interiors().len()));
            for (i, r) in std::iter::once(p.exterior())
                .chain(p.interiors())
                .enumerate()
            {
                out.push_str(&format!("\n{pad}   ring {i} has {} points", r.0.len()));
            }
        }
        Geometry::Rect(_) | Geometry::Triangle(_) => {
            out.push_str(" with 1 rings");
        }
        Geometry::MultiPoint(_)
        | Geometry::MultiLineString(_)
        | Geometry::MultiPolygon(_)
        | Geometry::GeometryCollection(_) => {
            let parts = crate::postgis::parts(g);
            out.push_str(&format!(" with {} elements", parts.len()));
            for p in &parts {
                out.push('\n');
                summary(p, flags, indent + 2, out);
            }
        }
    }
}

/// The size of a geometry in PostGIS's serialised form (header, box and coordinates).
fn mem_size(g: &Geometry<f64>, dims: usize) -> usize {
    fn body(g: &Geometry<f64>, dims: usize) -> usize {
        let ring = |l: &LineString<f64>| 4 + l.0.len() * dims * 8;
        match g {
            Geometry::Point(_) => 8 + dims * 8,
            Geometry::Line(_) => 8 + 2 * dims * 8,
            Geometry::LineString(l) => 4 + ring(l),
            Geometry::Polygon(p) => {
                let n = 1 + p.interiors().len();
                8 + 4 * n
                    + (n % 2) * 4
                    + std::iter::once(p.exterior())
                        .chain(p.interiors())
                        .map(|r| r.0.len() * dims * 8)
                        .sum::<usize>()
            }
            Geometry::Rect(r) => body(&Geometry::Polygon(r.to_polygon()), dims),
            Geometry::Triangle(t) => body(&Geometry::Polygon(t.to_polygon()), dims),
            _ => {
                8 + crate::postgis::parts(g)
                    .iter()
                    .map(|p| body(p, dims))
                    .sum::<usize>()
            }
        }
    }
    let bbox = if matches!(g, Geometry::Point(_)) {
        0
    } else {
        dims * 8
    };
    8 + bbox + body(g, dims)
}

// --- Measures (M) along lines --------------------------------------------------------------

/// The vertices of a line with their Z and M.
fn vertices(g: &Geom, l: &LineString<f64>) -> Vec<(Coord<f64>, f64, f64)> {
    l.0.iter()
        .map(|&c| {
            let (z, m) = zm_at(g, c);
            (c, z, m)
        })
        .collect()
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn add_measure(g: &Geom, start: f64, end: f64) -> Option<Term> {
    let lines = lines_of(&g.geometry);
    (!lines.is_empty() && lines.len() == crate::postgis::parts(&g.geometry).len()).then_some(())?;
    let total: f64 = lines.iter().map(|l| Euclidean.length(l)).sum();
    let mut along = 0.0;
    let mut values = Vec::new();
    for l in &lines {
        for (i, &c) in l.0.iter().enumerate() {
            if i > 0 {
                along += Euclidean.distance(l.0[i - 1], c);
            }
            let t = if total > 0.0 { along / total } else { 0.0 };
            values.push((zm_at(g, c).0, lerp(start, end, t)));
        }
    }
    let mut values = values.into_iter();
    with_zm(g.geometry.clone(), g.crs.clone(), has_z(g), true, |_| {
        values.next().unwrap_or(NONE)
    })
}

/// The point (with Z and M) at ordinate value `v` (index 0 = Z, 1 = M) on each segment where
/// the ordinate passes `v`.
fn locate_along(g: &Geom, value: f64, offset: f64) -> Option<Term> {
    let mut pts: Vec<(Coord<f64>, f64, f64)> = Vec::new();
    match &g.geometry {
        Geometry::Point(_) | Geometry::MultiPoint(_) => {
            for c in g.geometry.coords_iter() {
                let (z, m) = zm_at(g, c);
                if m == value {
                    pts.push((c, z, m));
                }
            }
        }
        _ => {
            for l in lines_of(&g.geometry) {
                let v = vertices(g, &l);
                let first = pts.len();
                for w in v.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if a.2.is_nan() || b.2.is_nan() {
                        continue;
                    }
                    let (lo, hi) = (a.2.min(b.2), a.2.max(b.2));
                    if value < lo || value > hi {
                        continue;
                    }
                    let t = if a.2 == b.2 {
                        0.0
                    } else {
                        (value - a.2) / (b.2 - a.2)
                    };
                    let mut c = Coord {
                        x: lerp(a.0.x, b.0.x, t),
                        y: lerp(a.0.y, b.0.y, t),
                    };
                    if offset != 0.0 {
                        let (dx, dy) = (b.0.x - a.0.x, b.0.y - a.0.y);
                        let len = dx.hypot(dy);
                        if len > 0.0 {
                            c.x -= dy / len * offset;
                            c.y += dx / len * offset;
                        }
                    }
                    if !pts[first..].iter().any(|p| p.0 == c) {
                        pts.push((c, lerp(a.1, b.1, t), value));
                    }
                }
            }
        }
    }
    let geometry = Geometry::MultiPoint(MultiPoint(pts.iter().map(|p| Point(p.0)).collect()));
    let mut it = pts.into_iter();
    with_zm(geometry, g.crs.clone(), has_z(g), true, |_| {
        it.next().map_or(NONE, |p| (p.1, p.2))
    })
}

/// The parts of the lines whose ordinate (0 = Z, 1 = M) lies in `[lo, hi]`.
fn locate_between(g: &Geom, ordinate: usize, lo: f64, hi: f64, offset: f64) -> Option<Term> {
    let (lo, hi) = (lo.min(hi), lo.max(hi));
    type V = (Coord<f64>, f64, f64);
    let ord = |v: &V| if ordinate == 0 { v.1 } else { v.2 };
    let mut pieces: Vec<Vec<V>> = Vec::new();
    let mut points: Vec<V> = Vec::new();
    for l in lines_of(&g.geometry) {
        let v = vertices(g, &l);
        let mut current: Vec<V> = Vec::new();
        let at = |a: &V, b: &V, target: f64| -> V {
            let t = (target - ord(a)) / (ord(b) - ord(a));
            (
                Coord {
                    x: lerp(a.0.x, b.0.x, t),
                    y: lerp(a.0.y, b.0.y, t),
                },
                lerp(a.1, b.1, t),
                lerp(a.2, b.2, t),
            )
        };
        for (i, p) in v.iter().enumerate() {
            let inside = (lo..=hi).contains(&ord(p));
            if i > 0 {
                let q = &v[i - 1];
                let q_in = (lo..=hi).contains(&ord(q));
                // Entering, leaving or passing through the range on this segment.
                let mut cuts: Vec<V> = Vec::new();
                for bound in [lo, hi] {
                    let (a, b) = (ord(q), ord(p));
                    if a != b && (bound - a) * (bound - b) < 0.0 {
                        cuts.push(at(q, p, bound));
                    }
                }
                cuts.sort_by(|x, y| {
                    Euclidean
                        .distance(q.0, x.0)
                        .total_cmp(&Euclidean.distance(q.0, y.0))
                });
                let mut state = q_in;
                for c in cuts {
                    if state {
                        current.push(c);
                        pieces.push(std::mem::take(&mut current));
                    } else {
                        current = vec![c];
                    }
                    state = !state;
                }
            }
            if inside {
                current.push(*p);
            } else if !current.is_empty() {
                pieces.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            pieces.push(current);
        }
    }
    for p in points_of_geom(g) {
        if (lo..=hi).contains(&ord(&p)) {
            points.push(p);
        }
    }
    let mut parts: Vec<Geometry<f64>> = Vec::new();
    let mut values: Vec<(f64, f64)> = Vec::new();
    for mut piece in pieces {
        piece.dedup_by(|a, b| a.0 == b.0);
        if piece.len() == 1 {
            values.push((piece[0].1, piece[0].2));
            parts.push(Geometry::Point(Point(piece[0].0)));
        } else if !piece.is_empty() {
            let line = LineString::new(piece.iter().map(|p| p.0).collect());
            if offset != 0.0 {
                if let Some(o) = offset_curve(&line, offset) {
                    parts.push(Geometry::LineString(o));
                }
                continue;
            }
            values.extend(piece.iter().map(|p| (p.1, p.2)));
            parts.push(Geometry::LineString(line));
        }
    }
    for p in points {
        values.push((p.1, p.2));
        parts.push(Geometry::Point(Point(p.0)));
    }
    let geometry = match &g.geometry {
        Geometry::Point(_) | Geometry::MultiPoint(_) => Geometry::MultiPoint(MultiPoint(
            parts
                .into_iter()
                .filter_map(|p| Point::try_from(p).ok())
                .collect(),
        )),
        _ if parts.iter().all(|p| matches!(p, Geometry::LineString(_))) => {
            Geometry::MultiLineString(MultiLineString(
                parts
                    .into_iter()
                    .filter_map(|p| LineString::try_from(p).ok())
                    .collect(),
            ))
        }
        _ => Geometry::GeometryCollection(GeometryCollection(parts)),
    };
    if offset != 0.0 {
        return out(geometry, &g.crs);
    }
    let mut it = values.into_iter();
    with_zm(geometry, g.crs.clone(), has_z(g), has_m(g), |_| {
        it.next().unwrap_or(NONE)
    })
}

fn points_of_geom(g: &Geom) -> Vec<(Coord<f64>, f64, f64)> {
    fn collect(geometry: &Geometry<f64>, out: &mut Vec<Coord<f64>>) {
        match geometry {
            Geometry::Point(p) => out.push(p.0),
            Geometry::MultiPoint(m) => out.extend(m.0.iter().map(|p| p.0)),
            Geometry::GeometryCollection(c) => c.0.iter().for_each(|g| collect(g, out)),
            _ => {}
        }
    }
    let mut cs = Vec::new();
    collect(&g.geometry, &mut cs);
    cs.into_iter()
        .map(|c| {
            let (z, m) = zm_at(g, c);
            (c, z, m)
        })
        .collect()
}

/// M at the point of the line closest to `p`.
fn interpolate_point(g: &Geom, p: Coord<f64>) -> Option<f64> {
    has_m(g).then_some(())?;
    let mut best: Option<(f64, f64)> = None;
    for l in lines_of(&g.geometry) {
        let v = vertices(g, &l);
        for w in v.windows(2) {
            let (a, b) = (w[0], w[1]);
            let seg = Line::new(a.0, b.0);
            let len2 = seg.dx() * seg.dx() + seg.dy() * seg.dy();
            let t = if len2 == 0.0 {
                0.0
            } else {
                (((p.x - a.0.x) * seg.dx() + (p.y - a.0.y) * seg.dy()) / len2).clamp(0.0, 1.0)
            };
            let c = Coord {
                x: lerp(a.0.x, b.0.x, t),
                y: lerp(a.0.y, b.0.y, t),
            };
            let d = Euclidean.distance(c, p);
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, lerp(a.2, b.2, t)));
            }
        }
    }
    best.map(|(_, m)| m).filter(|m| !m.is_nan())
}

// --- 3-D measures --------------------------------------------------------------------------

type P3 = [f64; 3];

fn sub(a: P3, b: P3) -> P3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: P3, b: P3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn norm(a: P3) -> f64 {
    dot(a, a).sqrt()
}
fn at3(a: P3, b: P3, t: f64) -> P3 {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

/// The parts of a geometry in 3-D (missing Z is 0): points, segments and polygons.
#[derive(Default)]
struct Shape3 {
    points: Vec<P3>,
    segments: Vec<(P3, P3)>,
    polygons: Vec<(Polygon<f64>, Vec<P3>)>,
}

fn shape3(g: &Geom) -> Shape3 {
    let p3 = |c: Coord<f64>| {
        let z = zm_at(g, c).0;
        [c.x, c.y, if z.is_nan() { 0.0 } else { z }]
    };
    let mut s = Shape3::default();
    fn walk(geometry: &Geometry<f64>, s: &mut Shape3, p3: &dyn Fn(Coord<f64>) -> P3) {
        let line = |l: &LineString<f64>, s: &mut Shape3| {
            if l.0.len() == 1 {
                s.points.push(p3(l.0[0]));
            }
            for w in l.0.windows(2) {
                s.segments.push((p3(w[0]), p3(w[1])));
            }
        };
        match geometry {
            Geometry::Point(p) => s.points.push(p3(p.0)),
            Geometry::MultiPoint(m) => s.points.extend(m.0.iter().map(|p| p3(p.0))),
            Geometry::Line(l) => s.segments.push((p3(l.start), p3(l.end))),
            Geometry::LineString(l) => line(l, s),
            Geometry::MultiLineString(m) => m.0.iter().for_each(|l| line(l, s)),
            Geometry::Polygon(p) => {
                line(p.exterior(), s);
                p.interiors().iter().for_each(|r| line(r, s));
                s.polygons
                    .push((p.clone(), p.exterior().0.iter().map(|&c| p3(c)).collect()));
            }
            Geometry::MultiPolygon(m) => {
                m.0.iter()
                    .for_each(|p| walk(&Geometry::Polygon(p.clone()), s, p3))
            }
            Geometry::Rect(r) => walk(&Geometry::Polygon(r.to_polygon()), s, p3),
            Geometry::Triangle(t) => walk(&Geometry::Polygon(t.to_polygon()), s, p3),
            Geometry::GeometryCollection(c) => c.0.iter().for_each(|g| walk(g, s, p3)),
        }
    }
    walk(&g.geometry, &mut s, &p3);
    s
}

fn closest_on_segment(p: P3, a: P3, b: P3) -> P3 {
    let ab = sub(b, a);
    let l2 = dot(ab, ab);
    if l2 == 0.0 {
        return a;
    }
    at3(a, b, (dot(sub(p, a), ab) / l2).clamp(0.0, 1.0))
}

/// Closest points of two segments.
fn segment_segment(a0: P3, a1: P3, b0: P3, b1: P3) -> (P3, P3) {
    let (d1, d2, r) = (sub(a1, a0), sub(b1, b0), sub(a0, b0));
    let (a, e, f) = (dot(d1, d1), dot(d2, d2), dot(d2, r));
    let (s, t);
    if a <= f64::EPSILON && e <= f64::EPSILON {
        return (a0, b0);
    }
    if a <= f64::EPSILON {
        s = 0.0;
        t = (f / e).clamp(0.0, 1.0);
    } else {
        let c = dot(d1, r);
        if e <= f64::EPSILON {
            t = 0.0;
            s = (-c / a).clamp(0.0, 1.0);
        } else {
            let b = dot(d1, d2);
            let denom = a * e - b * b;
            let mut s0 = if denom != 0.0 {
                ((b * f - c * e) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mut t0 = (b * s0 + f) / e;
            if t0 < 0.0 {
                t0 = 0.0;
                s0 = (-c / a).clamp(0.0, 1.0);
            } else if t0 > 1.0 {
                t0 = 1.0;
                s0 = ((b - c) / a).clamp(0.0, 1.0);
            }
            s = s0;
            t = t0;
        }
    }
    (at3(a0, a1, s), at3(b0, b1, t))
}

/// The plane of a polygon ring (Newell's method): unit normal and a point on it.
fn plane(ring: &[P3]) -> Option<(P3, P3)> {
    let mut n = [0.0; 3];
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    let len = norm(n);
    (len > 0.0).then(|| ([n[0] / len, n[1] / len, n[2] / len], ring[0]))
}

/// The projection of `p` on the polygon's plane, if it falls inside the polygon.
fn onto_polygon(p: P3, poly: &Polygon<f64>, ring: &[P3]) -> Option<P3> {
    let (n, o) = plane(ring)?;
    // Only planes that are not vertical can use the 2-D footprint for the inside test.
    (n[2].abs() > 1e-12).then_some(())?;
    let d = dot(sub(p, o), n);
    let q = [p[0] - d * n[0], p[1] - d * n[1], p[2] - d * n[2]];
    poly.contains(&Point::new(q[0], q[1])).then_some(q)
}

/// Where a segment meets a polygon's interior, if it does.
fn segment_through_polygon(a: P3, b: P3, poly: &Polygon<f64>, ring: &[P3]) -> Option<P3> {
    let (n, o) = plane(ring)?;
    let (da, db) = (dot(sub(a, o), n), dot(sub(b, o), n));
    if da * db > 0.0 || da == db {
        return None;
    }
    let q = at3(a, b, da / (da - db));
    poly.contains(&Point::new(q[0], q[1])).then_some(q)
}

/// The closest points of two shapes in 3-D: on `a`, on `b`.
fn closest3(a: &Shape3, b: &Shape3) -> Option<(P3, P3)> {
    let mut best: Option<(f64, P3, P3)> = None;
    let mut consider = |p: P3, q: P3| {
        let d = norm(sub(p, q));
        if best.is_none_or(|(bd, _, _)| d < bd) {
            best = Some((d, p, q));
        }
    };
    for &p in &a.points {
        for &q in &b.points {
            consider(p, q);
        }
        for &(s, t) in &b.segments {
            consider(p, closest_on_segment(p, s, t));
        }
        for (poly, ring) in &b.polygons {
            if let Some(q) = onto_polygon(p, poly, ring) {
                consider(p, q);
            }
        }
    }
    for &(s, t) in &a.segments {
        for &q in &b.points {
            consider(closest_on_segment(q, s, t), q);
        }
        for &(u, v) in &b.segments {
            let (p, q) = segment_segment(s, t, u, v);
            consider(p, q);
        }
        for (poly, ring) in &b.polygons {
            if let Some(q) = segment_through_polygon(s, t, poly, ring) {
                consider(q, q);
            }
            for e in [s, t] {
                if let Some(q) = onto_polygon(e, poly, ring) {
                    consider(e, q);
                }
            }
        }
    }
    for (poly, ring) in &a.polygons {
        for &q in &b.points {
            if let Some(p) = onto_polygon(q, poly, ring) {
                consider(p, q);
            }
        }
        for &(u, v) in &b.segments {
            if let Some(p) = segment_through_polygon(u, v, poly, ring) {
                consider(p, p);
            }
            for e in [u, v] {
                if let Some(p) = onto_polygon(e, poly, ring) {
                    consider(p, e);
                }
            }
        }
    }
    best.map(|(_, p, q)| (p, q))
}

fn vertices3(s: &Shape3) -> Vec<P3> {
    let mut v = s.points.clone();
    for &(a, b) in &s.segments {
        v.push(a);
        v.push(b);
    }
    v
}

fn farthest3(a: &Shape3, b: &Shape3) -> Option<(P3, P3)> {
    let (va, vb) = (vertices3(a), vertices3(b));
    let mut best: Option<(f64, P3, P3)> = None;
    for &p in &va {
        for &q in &vb {
            let d = norm(sub(p, q));
            if best.is_none_or(|(bd, _, _)| d > bd) {
                best = Some((d, p, q));
            }
        }
    }
    best.map(|(_, p, q)| (p, q))
}

fn point3(p: P3, crs: &Crs) -> Option<Term> {
    point_zm(p[0], p[1], Some(p[2]), None, crs.clone())
}

fn line3(a: P3, b: P3, crs: &Crs) -> Option<Term> {
    let l = Geometry::LineString(LineString::new(vec![
        Coord { x: a[0], y: a[1] },
        Coord { x: b[0], y: b[1] },
    ]));
    let mut zs = [a[2], b[2]].into_iter();
    with_zm(l, crs.clone(), true, false, |_| {
        (zs.next().unwrap_or(0.0), f64::NAN)
    })
}

/// PostGIS's text form of a box, `BOX(xmin ymin,xmax ymax)` or
/// `BOX3D(xmin ymin zmin,xmax ymax zmax)` (Z 0 when not given), as its lowest and highest
/// corners.
fn box_text(s: &str) -> Option<(P3, P3)> {
    let s = s.trim();
    let head = s.get(..5).unwrap_or(s).to_ascii_uppercase();
    let body = if head.starts_with("BOX3D") {
        &s[5..]
    } else if head.starts_with("BOX") {
        &s[3..]
    } else {
        return None;
    };
    let body = body.trim().strip_prefix('(')?.strip_suffix(')')?;
    let (a, b) = body.split_once(',')?;
    let corner = |c: &str| -> Option<P3> {
        let v: Vec<f64> = c
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        match v[..] {
            [x, y] => Some([x, y, 0.0]),
            [x, y, z] => Some([x, y, z]),
            _ => None,
        }
    };
    let (a, b) = (corner(a)?, corner(b)?);
    Some((
        [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])],
        [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])],
    ))
}

fn length3(g: &Geom, rings: bool) -> f64 {
    let s = shape3(g);
    let is_poly = !s.polygons.is_empty();
    if rings != is_poly {
        return 0.0;
    }
    s.segments.iter().map(|&(a, b)| norm(sub(b, a))).sum()
}

fn pair(a: &Term, b: &Term) -> Option<(Arc<Geom>, Arc<Geom>)> {
    let (a, b) = (geom(a)?, geom(b)?);
    same_space(&a, &b).then_some((a, b))
}

// --- Trajectories --------------------------------------------------------------------------

/// The samples (m, x, y, z) of a trajectory: a LineString with increasing M.
fn trajectory(g: &Geom) -> Option<Vec<(f64, P3)>> {
    let Geometry::LineString(l) = &g.geometry else {
        return None;
    };
    has_m(g).then_some(())?;
    let v: Vec<(f64, P3)> =
        l.0.iter()
            .map(|&c| {
                let (z, m) = zm_at(g, c);
                (m, [c.x, c.y, if z.is_nan() { 0.0 } else { z }])
            })
            .collect();
    (!v.is_empty() && v.windows(2).all(|w| w[1].0 > w[0].0) && v.iter().all(|p| !p.0.is_nan()))
        .then_some(v)
}

fn position(t: &[(f64, P3)], m: f64) -> P3 {
    let i = t.partition_point(|p| p.0 <= m);
    if i == 0 {
        return t[0].1;
    }
    if i >= t.len() {
        return t[t.len() - 1].1;
    }
    let (a, b) = (t[i - 1], t[i]);
    at3(a.1, b.1, (m - a.0) / (b.0 - a.0))
}

/// Time and distance of the closest point of approach of two trajectories.
fn cpa(a: &[(f64, P3)], b: &[(f64, P3)]) -> Option<(f64, f64)> {
    let start = a[0].0.max(b[0].0);
    let end = a[a.len() - 1].0.min(b[b.len() - 1].0);
    (start <= end).then_some(())?;
    let mut times: Vec<f64> = a
        .iter()
        .chain(b)
        .map(|p| p.0)
        .filter(|&m| m > start && m < end)
        .collect();
    times.push(start);
    times.push(end);
    times.sort_by(f64::total_cmp);
    times.dedup();
    let mut best = (start, norm(sub(position(a, start), position(b, start))));
    for w in times.windows(2) {
        let (t0, t1) = (w[0], w[1]);
        let r0 = sub(position(a, t0), position(b, t0));
        let r1 = sub(position(a, t1), position(b, t1));
        let v = sub(r1, r0);
        let vv = dot(v, v);
        let s = if vv == 0.0 {
            0.0
        } else {
            (-dot(r0, v) / vv).clamp(0.0, 1.0)
        };
        let d = norm(at3(r0, r1, s));
        if d < best.1 {
            best = (lerp(t0, t1, s), d);
        }
    }
    let d1 = norm(sub(position(a, end), position(b, end)));
    if d1 < best.1 {
        best = (end, d1);
    }
    Some(best)
}

// --- Editors -------------------------------------------------------------------------------

fn line_extend(l: &LineString<f64>, forward: f64, backward: f64) -> Option<LineString<f64>> {
    let pts = remove_repeated(&l.0, 0.0, false);
    (pts.len() >= 2).then_some(())?;
    let ext = |a: Coord<f64>, b: Coord<f64>, d: f64| {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len = dx.hypot(dy);
        Coord {
            x: b.x + dx / len * d,
            y: b.y + dy / len * d,
        }
    };
    let mut out = l.0.clone();
    if forward > 0.0 {
        let n = pts.len();
        out.push(ext(pts[n - 2], pts[n - 1], forward));
    }
    if backward > 0.0 {
        out.insert(0, ext(pts[1], pts[0], backward));
    }
    Some(LineString::new(out))
}

fn small(l: &LineString<f64>, min_x: f64, min_y: f64) -> bool {
    l.bounding_rect()
        .is_none_or(|r| r.width() < min_x && r.height() < min_y)
}

fn remove_small_parts(g: &Geometry<f64>, min_x: f64, min_y: f64) -> Geometry<f64> {
    let poly = |p: &Polygon<f64>| {
        (!small(p.exterior(), min_x, min_y)).then(|| {
            Polygon::new(
                p.exterior().clone(),
                p.interiors()
                    .iter()
                    .filter(|r| !small(r, min_x, min_y))
                    .cloned()
                    .collect(),
            )
        })
    };
    match g {
        Geometry::LineString(l) if small(l, min_x, min_y) => {
            Geometry::LineString(LineString::new(Vec::new()))
        }
        Geometry::MultiLineString(m) => Geometry::MultiLineString(MultiLineString(
            m.0.iter()
                .filter(|l| !small(l, min_x, min_y))
                .cloned()
                .collect(),
        )),
        Geometry::Polygon(p) => poly(p).map_or_else(
            || Geometry::Polygon(Polygon::new(LineString::new(Vec::new()), Vec::new())),
            Geometry::Polygon,
        ),
        Geometry::MultiPolygon(m) => {
            Geometry::MultiPolygon(MultiPolygon(m.0.iter().filter_map(poly).collect()))
        }
        Geometry::GeometryCollection(c) => Geometry::GeometryCollection(GeometryCollection(
            c.0.iter()
                .map(|g| remove_small_parts(g, min_x, min_y))
                .collect(),
        )),
        other => other.clone(),
    }
}

/// Drops vertices that lie, with both neighbours, beyond the same side of `r`.
fn irrelevant_for_view(g: &Geometry<f64>, r: Rect<f64>) -> Geometry<f64> {
    let side = |c: Coord<f64>| -> u8 {
        (u8::from(c.x < r.min().x))
            | (u8::from(c.x > r.max().x) << 1)
            | (u8::from(c.y < r.min().y) << 2)
            | (u8::from(c.y > r.max().y) << 3)
    };
    map_lines(g, &|pts: &[Coord<f64>], ring: bool| {
        let n = pts.len();
        if n < 3 {
            return pts.to_vec();
        }
        let mut out = vec![pts[0]];
        for i in 1..n - 1 {
            let s = side(pts[i]) & side(*out.last().unwrap()) & side(pts[i + 1]);
            if s == 0 {
                out.push(pts[i]);
            }
        }
        out.push(pts[n - 1]);
        if ring && out.len() < 4 {
            return pts.to_vec();
        }
        out
    })
}

/// Zeroes the mantissa bits of `v` below what `digits` decimal places need (as PostGIS
/// `ST_QuantizeCoordinates`).
fn quantize(v: f64, digits: i64) -> f64 {
    if v == 0.0 || !v.is_finite() {
        return v;
    }
    let left = (1.0 + v.abs().log10()) as i64;
    let bits = ((digits.saturating_add(left) as f64) / std::f64::consts::LOG10_2).ceil() as i64;
    let bits = bits.clamp(1, 52) as u32;
    if bits >= 52 {
        return v;
    }
    let mask = u64::MAX << (52 - bits);
    f64::from_bits(v.to_bits() & mask)
}

fn wrap_x(g: &Geometry<f64>, wrap: f64, shift: f64) -> Geometry<f64> {
    let Some(r) = g.bounding_rect() else {
        return g.clone();
    };
    let pad = 1.0 + r.width() + r.height() + (wrap - r.center().x).abs();
    let (lo, hi) = (r.min().y - pad, r.max().y + pad);
    let left = Rect::new(
        Coord {
            x: r.min().x - pad,
            y: lo,
        },
        Coord { x: wrap, y: hi },
    )
    .to_polygon();
    let right = Rect::new(
        Coord { x: wrap, y: lo },
        Coord {
            x: r.max().x + pad,
            y: hi,
        },
    )
    .to_polygon();
    // Parts on the side `shift` moves away from.
    let (moving, staying) = if shift > 0.0 {
        (&left, &right)
    } else {
        (&right, &left)
    };
    let mv = |g: Geometry<f64>| {
        g.map_coords(|c| Coord {
            x: c.x + shift,
            y: c.y,
        })
    };
    let polys = polygons_of(g);
    let lines: Vec<LineString<f64>> = lines_of(g);
    let mut out: Vec<Geometry<f64>> = Vec::new();
    if !polys.is_empty() {
        let mp = MultiPolygon(polys);
        let a = mp.intersection(&MultiPolygon(vec![moving.clone()]));
        let b = mp.intersection(&MultiPolygon(vec![staying.clone()]));
        let moved = match mv(Geometry::MultiPolygon(a)) {
            Geometry::MultiPolygon(m) => m,
            _ => unreachable!(),
        };
        let u = moved.union(&b);
        out.extend(u.0.into_iter().map(Geometry::Polygon));
    }
    if !lines.is_empty() {
        let ml = MultiLineString(lines);
        let a = moving.clip(&ml, false);
        let b = staying.clip(&ml, false);
        out.extend(b.0.into_iter().map(Geometry::LineString));
        out.extend(a.0.into_iter().map(|l| mv(Geometry::LineString(l))));
    }
    for c in crate::postgis_processing::points_of(g) {
        out.push(if (c.x() < wrap) == (shift > 0.0) {
            Geometry::Point(Point::new(c.x() + shift, c.y()))
        } else {
            Geometry::Point(c)
        });
    }
    match out.len() {
        1 => out.remove(0),
        _ => crate::postgis::collect(out),
    }
}

/// Visvalingam-Whyatt effective areas of the vertices of a line (ends get infinity).
fn effective_areas(pts: &[Coord<f64>]) -> Vec<f64> {
    let n = pts.len();
    let mut area = vec![f64::INFINITY; n];
    if n < 3 {
        return area;
    }
    let tri = |a: Coord<f64>, b: Coord<f64>, c: Coord<f64>| {
        ((b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y)).abs() / 2.0
    };
    let mut prev: Vec<usize> = (0..n).map(|i| i.saturating_sub(1)).collect();
    let mut next: Vec<usize> = (0..n).map(|i| (i + 1).min(n - 1)).collect();
    let mut alive = vec![true; n];
    let mut current: Vec<f64> = (0..n)
        .map(|i| {
            if i == 0 || i == n - 1 {
                f64::INFINITY
            } else {
                tri(pts[i - 1], pts[i], pts[i + 1])
            }
        })
        .collect();
    let mut floor = 0.0f64;
    for _ in 0..n - 2 {
        let i = (1..n - 1)
            .filter(|&i| alive[i])
            .min_by(|&a, &b| current[a].total_cmp(&current[b]))
            .unwrap();
        // An area never drops below that of a vertex removed before it.
        floor = floor.max(current[i]);
        area[i] = floor;
        alive[i] = false;
        let (p, q) = (prev[i], next[i]);
        next[p] = q;
        prev[q] = p;
        for j in [p, q] {
            if j != 0 && j != n - 1 {
                current[j] = tri(pts[prev[j]], pts[j], pts[next[j]]);
            }
        }
    }
    area
}

fn bbox(g: &Geom) -> Option<Rect<f64>> {
    g.geometry.bounding_rect()
}

fn z_range(g: &Geom) -> Option<(f64, f64)> {
    has_z(g).then_some(())?;
    let mut r: Option<(f64, f64)> = None;
    for c in g.geometry.coords_iter() {
        let z = zm_at(g, c).0;
        if !z.is_nan() {
            r = Some(r.map_or((z, z), |(lo, hi)| (lo.min(z), hi.max(z))));
        }
    }
    r
}

fn box_distance(a: Rect<f64>, b: Rect<f64>) -> f64 {
    let dx = (b.min().x - a.max().x).max(a.min().x - b.max().x).max(0.0);
    let dy = (b.min().y - a.max().y).max(a.min().y - b.max().y).max(0.0);
    dx.hypot(dy)
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    // --- Constructors ---------------------------------------------------------------------
    add(
        ST,
        "ST_MakePointM",
        func(|args| {
            let [x, y, m] = args else { return None };
            point_zm(num(x)?, num(y)?, None, Some(num(m)?), Crs::Crs84)
        }),
    );
    add(
        ST,
        "ST_PointZ",
        func(|args| {
            let [x, y, z, rest @ ..] = args else {
                return None;
            };
            point_zm(
                num(x)?,
                num(y)?,
                Some(num(z)?),
                None,
                srid_arg(rest.first())?,
            )
        }),
    );
    add(
        ST,
        "ST_PointM",
        func(|args| {
            let [x, y, m, rest @ ..] = args else {
                return None;
            };
            point_zm(
                num(x)?,
                num(y)?,
                None,
                Some(num(m)?),
                srid_arg(rest.first())?,
            )
        }),
    );
    add(
        ST,
        "ST_PointZM",
        func(|args| {
            let [x, y, z, m, rest @ ..] = args else {
                return None;
            };
            point_zm(
                num(x)?,
                num(y)?,
                Some(num(z)?),
                Some(num(m)?),
                srid_arg(rest.first())?,
            )
        }),
    );
    add(
        ST,
        "ST_3DMakeBox",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = pair(a, b)?;
            let (Geometry::Point(p), Geometry::Point(q)) = (&a.geometry, &b.geometry) else {
                return None;
            };
            out(
                envelope(
                    Coord {
                        x: p.x().min(q.x()),
                        y: p.y().min(q.y()),
                    },
                    Coord {
                        x: p.x().max(q.x()),
                        y: p.y().max(q.y()),
                    },
                ),
                &a.crs,
            )
        }),
    );
    // PostGIS's box2d and box3d casts. A box2d is the envelope polygon (as `ST_MakeBox2D`
    // writes it), a box3d the `LINESTRING Z` from the lowest to the highest corner (as
    // `ST_3DExtent`). Both also read PostGIS's text forms `BOX(...)` and `BOX3D(...)`.
    add(
        ST,
        "Box2D",
        func(|args| {
            let [a] = args else { return None };
            if let Some(g) = geom(a) {
                let r = g.geometry.bounding_rect()?;
                return out(envelope(r.min(), r.max()), &g.crs);
            }
            let (min, max) = box_text(crate::postgis_processing::text(a)?)?;
            out(
                envelope(
                    Coord {
                        x: min[0],
                        y: min[1],
                    },
                    Coord {
                        x: max[0],
                        y: max[1],
                    },
                ),
                &Crs::Crs84,
            )
        }),
    );
    add(
        ST,
        "Box3D",
        func(|args| {
            let [a] = args else { return None };
            if let Some(g) = geom(a) {
                let zm: Vec<Arc<Geom>> = if has_z(&g) {
                    vec![Arc::clone(&g)]
                } else {
                    Vec::new()
                };
                return crate::geosparql11::extent_3d(
                    std::slice::from_ref(&g.geometry),
                    &zm,
                    &g.crs,
                );
            }
            let (min, max) = box_text(crate::postgis_processing::text(a)?)?;
            line3(min, max, &Crs::Crs84)
        }),
    );
    for name in ["latitude", "longitude"] {
        add(
            GEOF,
            name,
            func(move |args| {
                let [a] = args else { return None };
                let g = geom(a)?;
                let Geometry::Point(p) = crate::crs::to_lon_lat(&g.geometry, &g.crs)? else {
                    return None;
                };
                double(if name == "latitude" { p.y() } else { p.x() })
            }),
        );
    }

    // --- Accessors ------------------------------------------------------------------------
    add(
        ST,
        "ST_HasZ",
        func(|args| {
            let [a] = args else { return None };
            boolean(has_z(&*geom(a)?))
        }),
    );
    add(
        ST,
        "ST_HasM",
        func(|args| {
            let [a] = args else { return None };
            boolean(has_m(&*geom(a)?))
        }),
    );
    add(
        ST,
        "ST_NumInteriorRing",
        func(|args| {
            let [a] = args else { return None };
            integer(polygon(&geom(a)?.geometry)?.interiors().len() as i64)
        }),
    );
    add(
        ST,
        "ST_MemSize",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let dims = 2 + usize::from(has_z(&g)) + usize::from(has_m(&g));
            integer(mem_size(&g.geometry, dims) as i64)
        }),
    );
    add(
        ST,
        "ST_Summary",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let mut flags = String::new();
            if has_z(&g) {
                flags.push('Z');
            }
            if has_m(&g) {
                flags.push('M');
            }
            if !matches!(g.geometry, Geometry::Point(_)) {
                flags.push('B');
            }
            if crate::postgis::srid(&g.crs) != 0 {
                flags.push('S');
            }
            let mut s = String::new();
            summary(&g.geometry, &flags, 0, &mut s);
            string(s)
        }),
    );
    add(
        ST,
        "ST_IsValidTrajectory",
        func(|args| {
            let [a] = args else { return None };
            boolean(trajectory(&*geom(a)?).is_some())
        }),
    );

    // --- Editors --------------------------------------------------------------------------
    add(
        ST,
        "ST_CollectionHomogenize",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let h = homogenize(&g.geometry);
            Some(geom_literal(&Geom {
                geometry: h,
                crs: g.crs.clone(),
                zm: g.zm.clone(),
                surface: None,
                prepared: Default::default(),
            }))
        }),
    );
    add(
        ST,
        "ST_ForceSFS",
        func(|args| {
            let [a, ..] = args else { return None };
            Some(geom_literal(&*geom(a)?))
        }),
    );
    for (name, z, m) in [
        ("ST_Force3D", true, false),
        ("ST_Force3DZ", true, false),
        ("ST_Force3DM", false, true),
        ("ST_Force4D", true, true),
    ] {
        add(
            ST,
            name,
            func(move |args| {
                let g = geom(args.first()?)?;
                let fill: Vec<f64> = args[1..].iter().map(num).collect::<Option<_>>()?;
                let (fz, fm) = match (z, m) {
                    (true, true) => (
                        fill.first().copied().unwrap_or(0.0),
                        fill.get(1).copied().unwrap_or(0.0),
                    ),
                    (true, false) => (fill.first().copied().unwrap_or(0.0), f64::NAN),
                    _ => (f64::NAN, fill.first().copied().unwrap_or(0.0)),
                };
                let keep_z = has_z(&g);
                let keep_m = has_m(&g);
                transform(&g, z, m, |c, (oz, om)| {
                    (
                        c,
                        (
                            if keep_z && !oz.is_nan() { oz } else { fz },
                            if keep_m && !om.is_nan() { om } else { fm },
                        ),
                    )
                })
            }),
        );
    }
    add(
        ST,
        "ST_Scroll",
        func(|args| {
            let [a, p] = args else { return None };
            let g = geom(a)?;
            let Geometry::Point(start) = geom(p)?.geometry else {
                return None;
            };
            let Geometry::LineString(l) = &g.geometry else {
                return None;
            };
            (l.is_closed() && l.0.len() >= 4).then_some(())?;
            let mut pts = l.0[..l.0.len() - 1].to_vec();
            let i = pts.iter().position(|&c| c == start.0)?;
            pts.rotate_left(i);
            pts.push(pts[0]);
            Some(geom_literal(&Geom {
                geometry: Geometry::LineString(LineString::new(pts)),
                crs: g.crs.clone(),
                zm: g.zm.clone(),
                surface: None,
                prepared: Default::default(),
            }))
        }),
    );
    add(
        ST,
        "ST_Normalize",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            Some(geom_literal(&Geom {
                geometry: normalize(&g.geometry),
                crs: g.crs.clone(),
                zm: g.zm.clone(),
                surface: None,
                prepared: Default::default(),
            }))
        }),
    );
    add(
        ST,
        "ST_LineExtend",
        func(|args| {
            let g = geom(args.first()?)?;
            let forward = num(args.get(1)?)?;
            let backward = args.get(2).map_or(Some(0.0), num)?;
            let Geometry::LineString(l) = &g.geometry else {
                return None;
            };
            out(
                Geometry::LineString(line_extend(l, forward, backward)?),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_RemoveSmallParts",
        func(|args| {
            let [a, x, y] = args else { return None };
            let g = geom(a)?;
            out(remove_small_parts(&g.geometry, num(x)?, num(y)?), &g.crs)
        }),
    );
    add(
        ST,
        "ST_RemoveIrrelevantPointsForView",
        func(|args| {
            let [a, b, ..] = args else { return None };
            let g = geom(a)?;
            let r = geom(b)?.geometry.bounding_rect()?;
            out(irrelevant_for_view(&g.geometry, r), &g.crs)
        }),
    );
    add(
        ST,
        "ST_QuantizeCoordinates",
        func(|args| {
            let g = geom(args.first()?)?;
            let px = int(args.get(1)?)?;
            let py = args.get(2).map_or(Some(px), int)?;
            let pz = args.get(3).map_or(Some(px), int)?;
            let pm = args.get(4).map_or(Some(px), int)?;
            transform(&g, has_z(&g), has_m(&g), |c, (z, m)| {
                (
                    Coord {
                        x: quantize(c.x, px),
                        y: quantize(c.y, py),
                    },
                    (quantize(z, pz), quantize(m, pm)),
                )
            })
        }),
    );
    add(
        ST,
        "ST_ReducePrecision",
        func(|args| {
            let [a, size] = args else { return None };
            let g = geom(a)?;
            let s = num(size)?;
            (s > 0.0).then_some(())?;
            let scale = 1.0 / s;
            let snapped = g.geometry.map_coords(|c| Coord {
                x: (c.x * scale).round() / scale,
                y: (c.y * scale).round() / scale,
            });
            let cleaned = map_lines(&snapped, &|pts: &[Coord<f64>], ring: bool| {
                remove_repeated(pts, 0.0, ring)
            });
            out(make_valid(&cleaned)?, &g.crs)
        }),
    );
    add(
        ST,
        "ST_WrapX",
        func(|args| {
            let [a, w, m] = args else { return None };
            let g = geom(a)?;
            out(wrap_x(&g.geometry, num(w)?, num(m)?), &g.crs)
        }),
    );
    for (name, axis) in [("ST_RotateX", 0), ("ST_RotateY", 1), ("ST_RotateZ", 2)] {
        add(
            ST,
            name,
            func(move |args| {
                let [a, angle] = args else { return None };
                let g = geom(a)?;
                let (s, c) = num(angle)?.sin_cos();
                let z_out = has_z(&g) || axis != 2;
                transform(&g, z_out, has_m(&g), move |p, (z, m)| {
                    let z0 = if z.is_nan() { 0.0 } else { z };
                    let (x, y, z) = match axis {
                        0 => (p.x, p.y * c - z0 * s, p.y * s + z0 * c),
                        1 => (p.x * c + z0 * s, p.y, -p.x * s + z0 * c),
                        _ => (p.x * c - p.y * s, p.x * s + p.y * c, z),
                    };
                    (Coord { x, y }, (z, m))
                })
            }),
        );
    }
    add(
        ST,
        "ST_FilterByM",
        func(|args| {
            let g = geom(args.first()?)?;
            has_m(&g).then_some(())?;
            let lo = num(args.get(1)?)?;
            let hi = match args.get(2) {
                Some(t) => num(t).unwrap_or(f64::INFINITY),
                None => f64::INFINITY,
            };
            let keep_m = match args.get(3) {
                Some(oxigraph::model::Term::Literal(l)) => l.value() == "true",
                _ => false,
            };
            let gg = Arc::clone(&g);
            let filtered = map_lines(&g.geometry, &move |pts: &[Coord<f64>], ring: bool| {
                let kept: Vec<Coord<f64>> = pts
                    .iter()
                    .copied()
                    .filter(|&c| {
                        let m = zm_at(&gg, c).1;
                        m >= lo && m <= hi
                    })
                    .collect();
                if !ring && kept.len() < 2 {
                    Vec::new()
                } else {
                    kept
                }
            });
            let gz = has_z(&g);
            let src = Arc::clone(&g);
            with_zm(filtered, g.crs.clone(), gz, keep_m, move |c| zm_at(&src, c))
        }),
    );
    add(
        ST,
        "ST_SetEffectiveArea",
        func(|args| {
            let g = geom(args.first()?)?;
            let threshold = args.get(1).map_or(Some(0.0), num)?;
            let gz = has_z(&g);
            let src = Arc::clone(&g);
            let mut areas: std::collections::HashMap<(u64, u64), f64> = Default::default();
            let filtered = map_lines(&g.geometry, &|pts: &[Coord<f64>], ring: bool| {
                let a = effective_areas(pts);
                let kept: Vec<Coord<f64>> = pts
                    .iter()
                    .zip(&a)
                    .filter(|(_, a)| **a >= threshold)
                    .map(|(c, _)| *c)
                    .collect();
                if ring && kept.len() < 4 {
                    pts.to_vec()
                } else {
                    kept
                }
            });
            let mut rings: Vec<LineString<f64>> = lines_of(&g.geometry);
            for p in polygons_of(&g.geometry) {
                rings.push(p.exterior().clone());
                rings.extend(p.interiors().iter().cloned());
            }
            for l in &rings {
                for (c, a) in l.0.iter().zip(effective_areas(&l.0)) {
                    areas.entry((c.x.to_bits(), c.y.to_bits())).or_insert(a);
                }
            }
            with_zm(filtered, g.crs.clone(), gz, true, move |c| {
                let a = areas
                    .get(&(c.x.to_bits(), c.y.to_bits()))
                    .copied()
                    .unwrap_or(f64::INFINITY);
                (zm_at(&src, c).0, if a.is_finite() { a } else { f64::MAX })
            })
        }),
    );

    // --- Measures (M) ---------------------------------------------------------------------
    add(
        ST,
        "ST_AddMeasure",
        func(|args| {
            let [a, s, e] = args else { return None };
            add_measure(&*geom(a)?, num(s)?, num(e)?)
        }),
    );
    add(
        ST,
        "ST_InterpolatePoint",
        func(|args| {
            let [a, p] = args else { return None };
            let (g, p) = pair(a, p)?;
            let Geometry::Point(p) = p.geometry else {
                return None;
            };
            double(interpolate_point(&g, p.0)?)
        }),
    );
    add(
        ST,
        "ST_LocateAlong",
        func(|args| {
            let g = geom(args.first()?)?;
            has_m(&g).then_some(())?;
            let m = num(args.get(1)?)?;
            let offset = args.get(2).map_or(Some(0.0), num)?;
            locate_along(&g, m, offset)
        }),
    );
    add(
        ST,
        "ST_LocateBetween",
        func(|args| {
            let g = geom(args.first()?)?;
            has_m(&g).then_some(())?;
            let lo = num(args.get(1)?)?;
            let hi = num(args.get(2)?)?;
            let offset = args.get(3).map_or(Some(0.0), num)?;
            locate_between(&g, 1, lo, hi, offset)
        }),
    );
    add(
        ST,
        "ST_LocateBetweenElevations",
        func(|args| {
            let [a, lo, hi] = args else { return None };
            let g = geom(a)?;
            has_z(&g).then_some(())?;
            locate_between(&g, 0, num(lo)?, num(hi)?, 0.0)
        }),
    );

    // --- 3-D ------------------------------------------------------------------------------
    let distance3 = |a: &Term, b: &Term| -> Option<f64> {
        let (a, b) = pair(a, b)?;
        let (p, q) = closest3(&shape3(&a), &shape3(&b))?;
        Some(norm(sub(p, q)))
    };
    add(
        ST,
        "ST_3DDistance",
        func(move |args| {
            let [a, b] = args else { return None };
            double(distance3(a, b)?)
        }),
    );
    add(
        ST,
        "ST_3DIntersects",
        func(move |args| {
            let [a, b] = args else { return None };
            boolean(distance3(a, b)? <= 1e-12)
        }),
    );
    add(
        ST,
        "ST_3DDWithin",
        func(move |args| {
            let [a, b, d] = args else { return None };
            boolean(distance3(a, b)? <= num(d)?)
        }),
    );
    let max3 = |a: &Term, b: &Term| -> Option<(P3, P3, Crs)> {
        let (a, b) = pair(a, b)?;
        let (p, q) = farthest3(&shape3(&a), &shape3(&b))?;
        Some((p, q, a.crs.clone()))
    };
    add(
        ST,
        "ST_3DDFullyWithin",
        func(move |args| {
            let [a, b, d] = args else { return None };
            let (p, q, _) = max3(a, b)?;
            boolean(norm(sub(p, q)) <= num(d)?)
        }),
    );
    add(
        ST,
        "ST_3DMaxDistance",
        func(move |args| {
            let [a, b] = args else { return None };
            let (p, q, _) = max3(a, b)?;
            double(norm(sub(p, q)))
        }),
    );
    add(
        ST,
        "ST_3DLongestLine",
        func(move |args| {
            let [a, b] = args else { return None };
            let (p, q, crs) = max3(a, b)?;
            line3(p, q, &crs)
        }),
    );
    add(
        ST,
        "ST_3DClosestPoint",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = pair(a, b)?;
            let (p, _) = closest3(&shape3(&a), &shape3(&b))?;
            point3(p, &a.crs)
        }),
    );
    add(
        ST,
        "ST_3DShortestLine",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = pair(a, b)?;
            let (p, q) = closest3(&shape3(&a), &shape3(&b))?;
            line3(p, q, &a.crs)
        }),
    );
    add(
        ST,
        "ST_3DLength",
        func(|args| {
            let [a] = args else { return None };
            double(length3(&*geom(a)?, false))
        }),
    );
    add(
        ST,
        "ST_3DPerimeter",
        func(|args| {
            let [a] = args else { return None };
            double(length3(&*geom(a)?, true))
        }),
    );
    add(
        ST,
        "ST_3DLineInterpolatePoint",
        func(|args| {
            let [a, f] = args else { return None };
            let g = geom(a)?;
            let f = num(f)?;
            (0.0..=1.0).contains(&f).then_some(())?;
            let Geometry::LineString(_) = &g.geometry else {
                return None;
            };
            let s = shape3(&g);
            let total: f64 = s.segments.iter().map(|&(a, b)| norm(sub(b, a))).sum();
            let mut target = f * total;
            for &(a, b) in &s.segments {
                let len = norm(sub(b, a));
                if target <= len || len == total {
                    let p = at3(a, b, if len > 0.0 { target / len } else { 0.0 });
                    return point3(p, &g.crs);
                }
                target -= len;
            }
            s.segments.last().and_then(|&(_, b)| point3(b, &g.crs))
        }),
    );

    // --- Trajectories ---------------------------------------------------------------------
    let cpa_of = |a: &Term, b: &Term| -> Option<(f64, f64)> {
        let (a, b) = pair(a, b)?;
        cpa(&trajectory(&a)?, &trajectory(&b)?)
    };
    add(
        ST,
        "ST_ClosestPointOfApproach",
        func(move |args| {
            let [a, b] = args else { return None };
            double(cpa_of(a, b)?.0)
        }),
    );
    add(
        ST,
        "ST_DistanceCPA",
        func(move |args| {
            let [a, b] = args else { return None };
            double(cpa_of(a, b)?.1)
        }),
    );
    add(
        ST,
        "ST_CPAWithin",
        func(move |args| {
            let [a, b, d] = args else { return None };
            boolean(cpa_of(a, b)?.1 <= num(d)?)
        }),
    );
    add(
        ST,
        "cpaDistance",
        func(move |args| {
            let [a, b] = args else { return None };
            double(cpa_of(a, b)?.1)
        }),
    );

    // --- Bounding-box operators (PostGIS &&, ~, @, ~=, =, <<, &<, ... , <#>) ----------------
    type BoxTest = fn(Rect<f64>, Rect<f64>) -> bool;
    let tests: [(&str, BoxTest); 12] = [
        ("bboxIntersects", |a, b| {
            a.min().x <= b.max().x
                && b.min().x <= a.max().x
                && a.min().y <= b.max().y
                && b.min().y <= a.max().y
        }),
        ("bboxContains", |a, b| {
            a.min().x <= b.min().x
                && a.min().y <= b.min().y
                && a.max().x >= b.max().x
                && a.max().y >= b.max().y
        }),
        ("bboxWithin", |a, b| {
            b.min().x <= a.min().x
                && b.min().y <= a.min().y
                && b.max().x >= a.max().x
                && b.max().y >= a.max().y
        }),
        ("bboxEquals", |a, b| a == b),
        ("bboxLeftOf", |a, b| a.max().x < b.min().x),
        ("bboxOverLeft", |a, b| a.max().x <= b.max().x),
        ("bboxRightOf", |a, b| a.min().x > b.max().x),
        ("bboxOverRight", |a, b| a.min().x >= b.min().x),
        ("bboxBelow", |a, b| a.max().y < b.min().y),
        ("bboxOverBelow", |a, b| a.max().y <= b.max().y),
        ("bboxAbove", |a, b| a.min().y > b.max().y),
        ("bboxOverAbove", |a, b| a.min().y >= b.min().y),
    ];
    for (name, test) in tests {
        add(
            ST,
            name,
            func(move |args| {
                let [a, b] = args else { return None };
                let (a, b) = pair(a, b)?;
                boolean(test(bbox(&a)?, bbox(&b)?))
            }),
        );
    }
    add(
        ST,
        "bboxIntersects3D",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = pair(a, b)?;
            let (ra, rb) = (bbox(&a)?, bbox(&b)?);
            let flat = ra.min().x <= rb.max().x
                && rb.min().x <= ra.max().x
                && ra.min().y <= rb.max().y
                && rb.min().y <= ra.max().y;
            let z = match (z_range(&a), z_range(&b)) {
                (Some((al, ah)), Some((bl, bh))) => al <= bh && bl <= ah,
                _ => true,
            };
            boolean(flat && z)
        }),
    );
    add(
        ST,
        "bboxDistance",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = pair(a, b)?;
            double(box_distance(bbox(&a)?, bbox(&b)?))
        }),
    );
    add(
        ST,
        "geomEquals",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = pair(a, b)?;
            boolean(a.geometry == b.geometry)
        }),
    );
    add(
        ST,
        "distanceKnn",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = pair(a, b)?;
            double(Euclidean.distance(&a.geometry, &b.geometry))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_keeps_the_asked_precision() {
        for v in [1.234_567_89, 123_456.789_012, -0.000_123_456] {
            let q = quantize(v, 3);
            assert!((q - v).abs() < 1e-3, "{v} -> {q}");
            assert!(q.to_bits().trailing_zeros() > 10, "{v}");
        }
    }

    #[test]
    fn cpa_of_crossing_tracks() {
        let a = vec![(0.0, [0.0, 0.0, 0.0]), (10.0, [10.0, 0.0, 0.0])];
        let b = vec![(0.0, [10.0, 1.0, 0.0]), (10.0, [0.0, 1.0, 0.0])];
        let (t, d) = cpa(&a, &b).unwrap();
        assert!((t - 5.0).abs() < 1e-9 && (d - 1.0).abs() < 1e-9);
    }

    #[test]
    fn effective_areas_of_a_triangle_wave() {
        let pts: Vec<Coord<f64>> = [(0.0, 0.0), (1.0, 1.0), (2.0, 0.0), (3.0, 2.0), (4.0, 0.0)]
            .iter()
            .map(|&(x, y)| Coord { x, y })
            .collect();
        let a = effective_areas(&pts);
        assert!(a[0].is_infinite() && a[4].is_infinite());
        assert_eq!(a[1], 1.0);
    }
}
