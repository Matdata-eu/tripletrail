//! PostGIS processing, validation, overlay, linear referencing and format functions under
//! their `st:ST_*` names. As in [`crate::postgis`], semantics follow PostGIS for `geometry`:
//! planar, in the units of the CRS.

use crate::functions::{Func, ST, boolean, double, func, geom, integer, num, same_space};
use crate::literal::{Crs, Geom, Zm, geom_literal};
use crate::postgis::{collect, crs_of_srid, envelope, int, line, out, parts, srid, string};
use geo::line_intersection::{LineIntersection, line_intersection};
use geo::line_measures::{Distance, Euclidean, InterpolateLine, Length};
use geo::{
    BooleanOps, BoundingRect, Contains, Coord, CoordsIter, Geometry, GeometryCollection,
    HasDimensions, Line, LineString, MakeValid, MinimumRotatedRect, MultiLineString, MultiPoint,
    MultiPolygon, Point, Polygon, Rect, Relate, Simplify, TriangulateDelaunayUnconstrained,
    Validation, Voronoi, Winding,
};
use oxigraph::model::{Literal, Term};
use std::f64::consts::PI;
use std::sync::Arc;

pub(crate) const HEX_BINARY: &str = "http://www.w3.org/2001/XMLSchema#hexBinary";

pub(crate) fn text(t: &Term) -> Option<&str> {
    match t {
        Term::Literal(l) => Some(l.value()),
        _ => None,
    }
}

/// A geometry with Z and M, as a literal.
pub(crate) fn out_zm(g: Geometry<f64>, crs: Crs, zm: Option<Zm>) -> Option<Term> {
    Some(geom_literal(&Geom {
        geometry: g,
        crs,
        zm: zm.map(Arc::new),
        surface: None,
        prepared: Default::default(),
    }))
}

// --- Generic coordinate and line rewriting -------------------------------------------------

/// Rewrites the points of a line or (with `true`) a ring.
pub(crate) type LineRewrite<'a> = dyn Fn(&[Coord<f64>], bool) -> Vec<Coord<f64>> + 'a;

/// Rewrites every line and ring with `f`. A line `f` empties is dropped, as is a ring left
/// with fewer than four points (a polygon whose shell collapses goes with it).
pub(crate) fn map_lines(g: &Geometry<f64>, f: &LineRewrite) -> Geometry<f64> {
    let ring = |r: &LineString<f64>| -> Option<LineString<f64>> {
        let c = f(&r.0, true);
        (c.len() >= 4).then(|| LineString::new(c))
    };
    let poly = |p: &Polygon<f64>| -> Option<Polygon<f64>> {
        Some(Polygon::new(
            ring(p.exterior())?,
            p.interiors().iter().filter_map(ring).collect(),
        ))
    };
    let line = |l: &LineString<f64>| -> Option<LineString<f64>> {
        let c = f(&l.0, false);
        (c.len() >= 2).then(|| LineString::new(c))
    };
    match g {
        Geometry::Point(_) => g.clone(),
        Geometry::MultiPoint(m) => {
            let c = f(&m.0.iter().map(|p| p.0).collect::<Vec<_>>(), false);
            Geometry::MultiPoint(MultiPoint(c.into_iter().map(Point).collect()))
        }
        Geometry::Line(l) => line(&LineString::new(vec![l.start, l.end]))
            .map_or_else(empty_collection, Geometry::LineString),
        Geometry::LineString(l) => line(l).map_or_else(empty_collection, Geometry::LineString),
        Geometry::MultiLineString(m) => {
            Geometry::MultiLineString(MultiLineString(m.0.iter().filter_map(line).collect()))
        }
        Geometry::Polygon(p) => poly(p).map_or_else(empty_collection, Geometry::Polygon),
        Geometry::Rect(r) => poly(&r.to_polygon()).map_or_else(empty_collection, Geometry::Polygon),
        Geometry::Triangle(t) => {
            poly(&t.to_polygon()).map_or_else(empty_collection, Geometry::Polygon)
        }
        Geometry::MultiPolygon(m) => {
            Geometry::MultiPolygon(MultiPolygon(m.0.iter().filter_map(poly).collect()))
        }
        Geometry::GeometryCollection(c) => Geometry::GeometryCollection(GeometryCollection(
            c.0.iter()
                .map(|g| map_lines(g, f))
                .filter(|g| !g.is_empty())
                .collect(),
        )),
    }
}

pub(crate) fn empty_collection() -> Geometry<f64> {
    Geometry::GeometryCollection(GeometryCollection(Vec::new()))
}

/// Drops consecutive points closer than `tol` (equal points when `tol` is 0). A ring keeps its
/// closing point; a multipoint drops points closer than `tol` to one it kept.
pub(crate) fn remove_repeated(c: &[Coord<f64>], tol: f64, ring: bool) -> Vec<Coord<f64>> {
    let close = |a: Coord<f64>, b: Coord<f64>| {
        if tol == 0.0 {
            a == b
        } else {
            Euclidean.distance(a, b) <= tol
        }
    };
    let mut out: Vec<Coord<f64>> = Vec::with_capacity(c.len());
    for (i, &p) in c.iter().enumerate() {
        let last = ring && i + 1 == c.len();
        match out.last() {
            Some(&q) if close(p, q) && !last => {}
            Some(&q) if last && close(p, q) && out.len() > 1 => {
                // The closing point replaces a near-duplicate before it.
                *out.last_mut().unwrap() = p;
            }
            _ => out.push(p),
        }
    }
    out
}

fn snap_coord(c: Coord<f64>, origin: Coord<f64>, size: Coord<f64>) -> Coord<f64> {
    let snap = |v: f64, o: f64, s: f64| {
        if s > 0.0 {
            o + ((v - o) / s).round() * s
        } else {
            v
        }
    };
    Coord {
        x: snap(c.x, origin.x, size.x),
        y: snap(c.y, origin.y, size.y),
    }
}

/// Segments of every line and ring of a geometry.
pub(crate) fn segments(g: &Geometry<f64>) -> Vec<Line<f64>> {
    let mut out = Vec::new();
    let mut add = |l: &LineString<f64>| out.extend(l.lines());
    match g {
        Geometry::Line(l) => out.push(*l),
        Geometry::LineString(l) => add(l),
        Geometry::MultiLineString(m) => m.0.iter().for_each(&mut add),
        Geometry::Polygon(p) => {
            add(p.exterior());
            p.interiors().iter().for_each(&mut add);
        }
        Geometry::MultiPolygon(m) => m.0.iter().for_each(|p| {
            add(p.exterior());
            p.interiors().iter().for_each(&mut add);
        }),
        Geometry::Rect(r) => out.extend(r.to_polygon().exterior().lines()),
        Geometry::Triangle(t) => out.extend(t.to_polygon().exterior().lines()),
        Geometry::GeometryCollection(c) => c.0.iter().for_each(|g| out.extend(segments(g))),
        Geometry::Point(_) | Geometry::MultiPoint(_) => {}
    }
    out
}

pub(crate) fn lines_of(g: &Geometry<f64>) -> Vec<LineString<f64>> {
    match g {
        Geometry::Line(l) => vec![LineString::new(vec![l.start, l.end])],
        Geometry::LineString(l) => vec![l.clone()],
        Geometry::MultiLineString(m) => m.0.clone(),
        Geometry::GeometryCollection(c) => c.0.iter().flat_map(lines_of).collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn polygons_of(g: &Geometry<f64>) -> Vec<Polygon<f64>> {
    match g {
        Geometry::Polygon(p) => vec![p.clone()],
        Geometry::MultiPolygon(m) => m.0.clone(),
        Geometry::Rect(r) => vec![r.to_polygon()],
        Geometry::Triangle(t) => vec![t.to_polygon()],
        Geometry::GeometryCollection(c) => c.0.iter().flat_map(polygons_of).collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn points_of(g: &Geometry<f64>) -> Vec<Point<f64>> {
    match g {
        Geometry::Point(p) => vec![*p],
        Geometry::MultiPoint(m) => m.0.clone(),
        Geometry::GeometryCollection(c) => c.0.iter().flat_map(points_of).collect(),
        _ => Vec::new(),
    }
}

/// One polygon as a Polygon, several as a MultiPolygon.
pub(crate) fn polygons(mut p: Vec<Polygon<f64>>) -> Geometry<f64> {
    if p.len() == 1 {
        Geometry::Polygon(p.remove(0))
    } else {
        Geometry::MultiPolygon(MultiPolygon(p))
    }
}

// --- Splitting and noding ------------------------------------------------------------------

/// Cuts a line at the given points (those within `eps` of it), in order along the line.
fn split_line(l: &LineString<f64>, cuts: &[Coord<f64>], eps: f64) -> Vec<LineString<f64>> {
    let mut pieces = Vec::new();
    let mut current: Vec<Coord<f64>> = l.0.first().copied().into_iter().collect();
    for seg in l.lines() {
        let len = Euclidean.length(&seg);
        let mut on: Vec<(f64, Coord<f64>)> = cuts
            .iter()
            .filter(|&&c| Euclidean.distance(&seg, &Point(c)) <= eps)
            .map(|&c| {
                let t = if len == 0.0 {
                    0.0
                } else {
                    ((c - seg.start).x * seg.dx() + (c - seg.start).y * seg.dy()) / (len * len)
                };
                (t.clamp(0.0, 1.0), c)
            })
            .filter(|(t, _)| *t > 0.0)
            .collect();
        on.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (t, c) in on {
            let c = if t >= 1.0 { seg.end } else { c };
            if current.last() != Some(&c) {
                current.push(c);
            }
            if current.len() >= 2 {
                pieces.push(LineString::new(std::mem::take(&mut current)));
            }
            current = vec![c];
        }
        if current.last() != Some(&seg.end) {
            current.push(seg.end);
        }
    }
    if current.len() >= 2 {
        pieces.push(LineString::new(current));
    }
    pieces
}

/// Points where segments of `a` and `b` cross or touch (shared stretches give their ends).
fn crossings(a: &[Line<f64>], b: &[Line<f64>]) -> Vec<Coord<f64>> {
    let mut out = Vec::new();
    for sa in a {
        let ra = sa.bounding_rect();
        for sb in b {
            let rb = sb.bounding_rect();
            if ra.max().x < rb.min().x
                || rb.max().x < ra.min().x
                || ra.max().y < rb.min().y
                || rb.max().y < ra.min().y
            {
                continue;
            }
            match line_intersection(*sa, *sb) {
                Some(LineIntersection::SinglePoint { intersection, .. }) => out.push(intersection),
                Some(LineIntersection::Collinear { intersection }) => {
                    out.push(intersection.start);
                    out.push(intersection.end);
                }
                None => {}
            }
        }
    }
    out
}

/// PostGIS ST_Node: the lines cut wherever they cross or touch, duplicates removed.
pub(crate) fn node(lines: &[LineString<f64>]) -> Vec<LineString<f64>> {
    let segs: Vec<Vec<Line<f64>>> = lines.iter().map(|l| l.lines().collect()).collect();
    let all: Vec<Line<f64>> = segs.iter().flatten().copied().collect();
    let mut out: Vec<LineString<f64>> = Vec::new();
    for (l, own) in lines.iter().zip(&segs) {
        let mut cuts = crossings(own, &all);
        // A line's own vertices are not cuts unless another line meets them.
        cuts.retain(|c| {
            !l.0.contains(c) || all.iter().filter(|s| s.start == *c || s.end == *c).count() > 2
        });
        for piece in split_line(l, &cuts, 1e-9) {
            let reversed: Vec<Coord<f64>> = piece.0.iter().rev().copied().collect();
            if !out.iter().any(|o| o.0 == piece.0 || o.0 == reversed) {
                out.push(piece);
            }
        }
    }
    out
}

// --- Offset curve --------------------------------------------------------------------------

/// The line at `d` to its left (right when negative), with mitred joins. Bends whose mitre
/// would reach more than five times `d` are bevelled. Loops on the inner side of bends sharper
/// than the offset are not removed.
pub(crate) fn offset_curve(l: &LineString<f64>, d: f64) -> Option<LineString<f64>> {
    let mut pts: Vec<Coord<f64>> = remove_repeated(&l.0, 0.0, false);
    if pts.len() < 2 {
        return None;
    }
    let normal = |a: Coord<f64>, b: Coord<f64>| {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len = dx.hypot(dy);
        Coord {
            x: -dy / len * d,
            y: dx / len * d,
        }
    };
    let mut out = Vec::with_capacity(pts.len());
    let n0 = normal(pts[0], pts[1]);
    out.push(pts[0] + n0);
    for i in 1..pts.len() - 1 {
        let (a, b, c) = (pts[i - 1], pts[i], pts[i + 1]);
        let (n1, n2) = (normal(a, b), normal(b, c));
        let l1 = Line::new(a + n1, b + n1);
        let l2 = Line::new(b + n2, c + n2);
        let (d1, d2) = (l1.delta(), l2.delta());
        let denom = d1.x * d2.y - d1.y * d2.x;
        if denom.abs() < 1e-12 {
            out.push(b + n1);
            continue;
        }
        let w = l2.start - l1.start;
        let t = (w.x * d2.y - w.y * d2.x) / denom;
        let m = l1.start + d1 * t;
        if Euclidean.distance(m, b) > 5.0 * d.abs() {
            out.push(b + n1);
            out.push(b + n2);
        } else {
            out.push(m);
        }
    }
    let k = pts.len();
    out.push(pts[k - 1] + normal(pts[k - 2], pts[k - 1]));
    pts = out;
    Some(LineString::new(pts))
}

// --- Subdivide -----------------------------------------------------------------------------

fn clip_to(g: &Geometry<f64>, r: Rect<f64>, first: bool) -> Vec<Geometry<f64>> {
    let rp = r.to_polygon();
    let mut out = Vec::new();
    let polys = polygons_of(g);
    if !polys.is_empty() {
        let m = MultiPolygon(polys).intersection(&rp);
        out.extend(m.0.into_iter().map(Geometry::Polygon));
    }
    let lines = lines_of(g);
    if !lines.is_empty() {
        let m = rp.clip(&MultiLineString(lines), false);
        out.extend(m.0.into_iter().map(Geometry::LineString));
    }
    // A point on the line between the halves goes to the second half.
    let (min, max) = (r.min(), r.max());
    let pts: Vec<Point<f64>> = points_of(g)
        .into_iter()
        .filter(|p| {
            let inside = min.x <= p.x() && p.x() <= max.x && min.y <= p.y() && p.y() <= max.y;
            inside
                && !(first && (p.x() == max.x && max.x > min.x || p.y() == max.y && max.y > min.y))
        })
        .collect();
    if !pts.is_empty() {
        out.push(Geometry::MultiPoint(MultiPoint(pts)));
    }
    out
}

fn subdivide(g: Geometry<f64>, max: usize, depth: usize, out: &mut Vec<Geometry<f64>>) {
    if g.coords_count() <= max || depth > 50 {
        out.push(g);
        return;
    }
    let Some(r) = g.bounding_rect() else { return };
    let (lo, hi) = (r.min(), r.max());
    let halves = if r.width() >= r.height() {
        let mid = (lo.x + hi.x) / 2.0;
        [
            Rect::new(lo, Coord { x: mid, y: hi.y }),
            Rect::new(Coord { x: mid, y: lo.y }, hi),
        ]
    } else {
        let mid = (lo.y + hi.y) / 2.0;
        [
            Rect::new(lo, Coord { x: hi.x, y: mid }),
            Rect::new(Coord { x: lo.x, y: mid }, hi),
        ]
    };
    for (i, h) in halves.into_iter().enumerate() {
        for part in clip_to(&g, h, i == 0) {
            if !part.is_empty() {
                subdivide(part, max, depth + 1, out);
            }
        }
    }
}

/// The parts of `g` cut until each has at most `max` vertices.
pub(crate) fn subdivide_parts(g: &Geometry<f64>, max: usize) -> Vec<Geometry<f64>> {
    let mut pieces = Vec::new();
    subdivide(g.clone(), max, 0, &mut pieces);
    pieces.iter().flat_map(parts).collect()
}

// --- Geohash and encoded polylines ---------------------------------------------------------

const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

fn geohash(c: Coord<f64>, precision: usize) -> Option<String> {
    if !(-180.0..=180.0).contains(&c.x) || !(-90.0..=90.0).contains(&c.y) {
        return None;
    }
    let (mut lon, mut lat) = ((-180.0, 180.0), (-90.0, 90.0));
    let mut out = String::with_capacity(precision);
    let (mut bits, mut ch, mut even) = (0, 0usize, true);
    while out.len() < precision {
        let (range, v) = if even {
            (&mut lon, c.x)
        } else {
            (&mut lat, c.y)
        };
        let mid = (range.0 + range.1) / 2.0;
        ch <<= 1;
        if v >= mid {
            ch |= 1;
            range.0 = mid;
        } else {
            range.1 = mid;
        }
        even = !even;
        bits += 1;
        if bits == 5 {
            out.push(BASE32[ch] as char);
            bits = 0;
            ch = 0;
        }
    }
    Some(out)
}

/// The box of a geohash: (min, max).
fn geohash_box(hash: &str, precision: Option<usize>) -> Option<(Coord<f64>, Coord<f64>)> {
    let (mut lon, mut lat) = ((-180.0f64, 180.0f64), (-90.0f64, 90.0f64));
    let mut even = true;
    let hash = &hash[..precision.unwrap_or(hash.len()).min(hash.len())];
    (!hash.is_empty()).then_some(())?;
    for b in hash.bytes() {
        let v = BASE32.iter().position(|&x| x == b.to_ascii_lowercase())?;
        for i in (0..5).rev() {
            let range = if even { &mut lon } else { &mut lat };
            let mid = (range.0 + range.1) / 2.0;
            if v >> i & 1 == 1 {
                range.0 = mid;
            } else {
                range.1 = mid;
            }
            even = !even;
        }
    }
    Some((Coord { x: lon.0, y: lat.0 }, Coord { x: lon.1, y: lat.1 }))
}

fn encode_polyline(coords: &[Coord<f64>], precision: i32) -> String {
    let factor = 10f64.powi(precision);
    let mut out = String::new();
    let (mut plat, mut plon) = (0i64, 0i64);
    let mut put = |v: i64| {
        let mut v = if v < 0 { !(v << 1) } else { v << 1 } as u64;
        while v >= 0x20 {
            out.push((((v & 0x1f) | 0x20) as u8 + 63) as char);
            v >>= 5;
        }
        out.push((v as u8 + 63) as char);
    };
    for c in coords {
        let (lat, lon) = ((c.y * factor).round() as i64, (c.x * factor).round() as i64);
        put(lat - plat);
        put(lon - plon);
        (plat, plon) = (lat, lon);
    }
    out
}

fn decode_polyline(s: &str, precision: i32) -> Option<Vec<Coord<f64>>> {
    let factor = 10f64.powi(precision);
    let bytes = s.as_bytes();
    let mut i = 0;
    fn next(bytes: &[u8], i: &mut usize) -> Option<i64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = u64::from(bytes.get(*i)?.checked_sub(63)?);
            *i += 1;
            v |= (b & 0x1f) << shift;
            shift += 5;
            if b < 0x20 {
                break;
            }
            (shift < 64).then_some(())?;
        }
        Some(if v & 1 == 1 {
            !(v >> 1) as i64
        } else {
            (v >> 1) as i64
        })
    }
    let (mut lat, mut lon) = (0i64, 0i64);
    let mut out = Vec::new();
    while i < bytes.len() {
        lat += next(bytes, &mut i)?;
        lon += next(bytes, &mut i)?;
        out.push(Coord {
            x: lon as f64 / factor,
            y: lat as f64 / factor,
        });
    }
    Some(out)
}

// --- WKB -----------------------------------------------------------------------------------

struct WkbWriter<'a> {
    out: Vec<u8>,
    zm: Option<&'a Zm>,
    ewkb: bool,
}

impl WkbWriter<'_> {
    fn has_z(&self) -> bool {
        self.zm.is_some_and(|z| z.has_z)
    }
    fn has_m(&self) -> bool {
        self.zm.is_some_and(|z| z.has_m)
    }
    fn header(&mut self, kind: u32, srid: Option<u32>) {
        self.out.push(1);
        let t = if self.ewkb {
            kind | if self.has_z() { 0x8000_0000 } else { 0 }
                | if self.has_m() { 0x4000_0000 } else { 0 }
                | if srid.is_some() { 0x2000_0000 } else { 0 }
        } else {
            kind + if self.has_z() { 1000 } else { 0 } + if self.has_m() { 2000 } else { 0 }
        };
        self.out.extend(t.to_le_bytes());
        if let Some(s) = srid {
            self.out.extend(s.to_le_bytes());
        }
    }
    fn coord(&mut self, c: Coord<f64>) {
        self.out.extend(c.x.to_le_bytes());
        self.out.extend(c.y.to_le_bytes());
        let (z, m) = self
            .zm
            .and_then(|zm| zm.at(c))
            .unwrap_or((f64::NAN, f64::NAN));
        if self.has_z() {
            self.out.extend(z.to_le_bytes());
        }
        if self.has_m() {
            self.out.extend(m.to_le_bytes());
        }
    }
    fn coords(&mut self, l: &LineString<f64>) {
        self.out.extend((l.0.len() as u32).to_le_bytes());
        for &c in &l.0 {
            self.coord(c);
        }
    }
    fn polygon(&mut self, p: &Polygon<f64>) {
        if p.exterior().0.is_empty() {
            self.out.extend(0u32.to_le_bytes());
            return;
        }
        self.out
            .extend((1 + p.interiors().len() as u32).to_le_bytes());
        self.coords(p.exterior());
        for r in p.interiors() {
            self.coords(r);
        }
    }
    fn geometry(&mut self, g: &Geometry<f64>, srid: Option<u32>) {
        match g {
            Geometry::Point(p) => {
                self.header(1, srid);
                self.coord(p.0);
            }
            Geometry::Line(l) => {
                self.header(2, srid);
                self.coords(&LineString::new(vec![l.start, l.end]));
            }
            Geometry::LineString(l) => {
                self.header(2, srid);
                self.coords(l);
            }
            Geometry::Polygon(p) => {
                self.header(3, srid);
                self.polygon(p);
            }
            Geometry::Rect(r) => self.geometry(&Geometry::Polygon(r.to_polygon()), srid),
            Geometry::Triangle(t) => self.geometry(&Geometry::Polygon(t.to_polygon()), srid),
            Geometry::MultiPoint(m) => {
                self.header(4, srid);
                self.out.extend((m.0.len() as u32).to_le_bytes());
                m.0.iter()
                    .for_each(|p| self.geometry(&Geometry::Point(*p), None));
            }
            Geometry::MultiLineString(m) => {
                self.header(5, srid);
                self.out.extend((m.0.len() as u32).to_le_bytes());
                m.0.iter()
                    .for_each(|l| self.geometry(&Geometry::LineString(l.clone()), None));
            }
            Geometry::MultiPolygon(m) => {
                self.header(6, srid);
                self.out.extend((m.0.len() as u32).to_le_bytes());
                m.0.iter()
                    .for_each(|p| self.geometry(&Geometry::Polygon(p.clone()), None));
            }
            Geometry::GeometryCollection(c) => {
                self.header(7, srid);
                self.out.extend((c.0.len() as u32).to_le_bytes());
                c.0.iter().for_each(|g| self.geometry(g, None));
            }
        }
    }
}

fn to_wkb(g: &Geom, ewkb: bool) -> Vec<u8> {
    if let Some(s) = &g.surface {
        let id = srid(&g.crs);
        return s.to_wkb(ewkb, (ewkb && id > 0).then_some(id as u32));
    }
    let mut w = WkbWriter {
        out: Vec::new(),
        zm: g.zm.as_deref(),
        ewkb,
    };
    let s = srid(&g.crs);
    w.geometry(&g.geometry, (ewkb && s > 0).then_some(s as u32));
    w.out
}

struct WkbReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    zm: Vec<(f64, f64)>,
    has_z: bool,
    has_m: bool,
    srid: Option<u32>,
}

impl WkbReader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.bytes.get(self.pos..self.pos + N)?.try_into().ok()?;
        self.pos += N;
        Some(b)
    }
    fn u32(&mut self, le: bool) -> Option<u32> {
        let b = self.take::<4>()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }
    fn f64(&mut self, le: bool) -> Option<f64> {
        let b = self.take::<8>()?;
        Some(if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }
    fn coord(&mut self, le: bool, z: bool, m: bool) -> Option<Coord<f64>> {
        let c = Coord {
            x: self.f64(le)?,
            y: self.f64(le)?,
        };
        let zv = if z { self.f64(le)? } else { f64::NAN };
        let mv = if m { self.f64(le)? } else { f64::NAN };
        self.zm.push((zv, mv));
        Some(c)
    }
    fn ring(&mut self, le: bool, z: bool, m: bool) -> Option<LineString<f64>> {
        let n = self.u32(le)? as usize;
        (n <= self.bytes.len()).then_some(())?;
        (0..n)
            .map(|_| self.coord(le, z, m))
            .collect::<Option<Vec<_>>>()
            .map(LineString::new)
    }
    fn geometry(&mut self, depth: usize) -> Option<Geometry<f64>> {
        (depth < 32).then_some(())?;
        let le = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let raw = self.u32(le)?;
        let (mut z, mut m) = (raw & 0x8000_0000 != 0, raw & 0x4000_0000 != 0);
        if raw & 0x2000_0000 != 0 {
            self.srid = Some(self.u32(le)?);
        }
        let mut kind = raw & 0x0fff_ffff;
        match kind / 1000 {
            1 => z = true,
            2 => m = true,
            3 => (z, m) = (true, true),
            _ => {}
        }
        kind %= 1000;
        self.has_z |= z;
        self.has_m |= m;
        let count = |r: &mut Self| -> Option<usize> {
            let n = r.u32(le)? as usize;
            (n <= r.bytes.len()).then_some(n)
        };
        Some(match kind {
            1 => {
                let c = self.coord(le, z, m)?;
                if c.x.is_nan() && c.y.is_nan() {
                    self.zm.pop();
                    Geometry::MultiPoint(MultiPoint(Vec::new()))
                } else {
                    Geometry::Point(Point(c))
                }
            }
            2 => Geometry::LineString(self.ring(le, z, m)?),
            3 => {
                let n = count(self)?;
                let mut rings = (0..n)
                    .map(|_| self.ring(le, z, m))
                    .collect::<Option<Vec<_>>>()?
                    .into_iter();
                match rings.next() {
                    Some(shell) => Geometry::Polygon(Polygon::new(shell, rings.collect())),
                    None => {
                        Geometry::Polygon(Polygon::new(LineString::new(Vec::new()), Vec::new()))
                    }
                }
            }
            4..=7 => {
                let n = count(self)?;
                let parts = (0..n)
                    .map(|_| self.geometry(depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                match kind {
                    4 => Geometry::MultiPoint(MultiPoint(
                        parts
                            .into_iter()
                            .map(Point::try_from)
                            .collect::<Result<_, _>>()
                            .ok()?,
                    )),
                    5 => Geometry::MultiLineString(MultiLineString(
                        parts
                            .into_iter()
                            .map(LineString::try_from)
                            .collect::<Result<_, _>>()
                            .ok()?,
                    )),
                    6 => Geometry::MultiPolygon(MultiPolygon(
                        parts
                            .into_iter()
                            .map(Polygon::try_from)
                            .collect::<Result<_, _>>()
                            .ok()?,
                    )),
                    _ => Geometry::GeometryCollection(GeometryCollection(parts)),
                }
            }
            _ => return None,
        })
    }
}

pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().strip_prefix("\\x").unwrap_or(s.trim());
    (s.len().is_multiple_of(2)).then_some(())?;
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

pub(crate) fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// A geometry from (E)WKB in hex; `srid` overrides the one in the data.
pub(crate) fn from_wkb(hex: &str, srid: Option<i64>) -> Option<Term> {
    let crs = match srid {
        None => None,
        Some(0) => Some(Crs::Crs84),
        Some(s) => Some(crs_of_srid(s)?),
    };
    wkb_literal(&from_hex(hex)?, crs)
}

/// The ISO WKB of a geometry (Z and M ordinates included), without an SRID.
pub fn wkb(g: &Geom) -> Vec<u8> {
    to_wkb(g, false)
}

/// The geometry literal of ISO WKB or EWKB bytes. `crs` overrides the SRID of EWKB data; with
/// neither, the geometry is CRS84.
pub fn wkb_literal(bytes: &[u8], crs: Option<Crs>) -> Option<Term> {
    let crs_of = |data_srid: Option<u32>| match crs.clone() {
        Some(c) => Some(c),
        None => match data_srid.map_or(4326, i64::from) {
            0 => Some(Crs::Crs84),
            s => crs_of_srid(s),
        },
    };
    if let Some(parsed) = crate::solid::parse_wkb(bytes) {
        let (s, data_srid) = parsed?;
        return Some(geom_literal(&Geom::from_surface(s, crs_of(data_srid)?)));
    }
    let mut r = WkbReader {
        bytes,
        pos: 0,
        zm: Vec::new(),
        has_z: false,
        has_m: false,
        srid: None,
    };
    let g = r.geometry(0)?;
    (r.pos == bytes.len()).then_some(())?;
    let crs = crs_of(r.srid)?;
    let zm = (r.has_z || r.has_m).then(|| Zm::from_coords(&g, r.has_z, r.has_m, r.zm));
    out_zm(g, crs, zm)
}

// --- SVG and lat/lon text ------------------------------------------------------------------

fn fmt_num(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    };
    if s == "-0" { "0".into() } else { s }
}

fn svg(g: &Geometry<f64>, rel: bool, dec: usize) -> String {
    let n = |v: f64| fmt_num(v, dec);
    let path = |c: &[Coord<f64>], close: bool| -> String {
        let pts = if close && c.len() > 1 {
            &c[..c.len() - 1]
        } else {
            c
        };
        let mut s = String::new();
        if let Some(first) = pts.first() {
            if rel {
                s.push_str(&format!("m {} {} l", n(first.x), n(-first.y)));
                for w in pts.windows(2) {
                    s.push_str(&format!(
                        " {} {}",
                        n(w[1].x - w[0].x),
                        n(-(w[1].y - w[0].y))
                    ));
                }
                if close {
                    s.push_str(" z");
                }
            } else {
                s.push_str(&format!("M {} {} L", n(first.x), n(-first.y)));
                for c in &pts[1..] {
                    s.push_str(&format!(" {} {}", n(c.x), n(-c.y)));
                }
                if close {
                    s.push_str(" Z");
                }
            }
        }
        s
    };
    let point = |p: &Point<f64>| {
        if rel {
            format!("x=\"{}\" y=\"{}\"", n(p.x()), n(-p.y()))
        } else {
            format!("cx=\"{}\" cy=\"{}\"", n(p.x()), n(-p.y()))
        }
    };
    let poly = |p: &Polygon<f64>| {
        std::iter::once(p.exterior())
            .chain(p.interiors())
            .map(|r| path(&r.0, true))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match g {
        Geometry::Point(p) => point(p),
        Geometry::MultiPoint(m) => m.0.iter().map(point).collect::<Vec<_>>().join(","),
        Geometry::Line(l) => path(&[l.start, l.end], false),
        Geometry::LineString(l) => path(&l.0, false),
        Geometry::MultiLineString(m) => {
            m.0.iter()
                .map(|l| path(&l.0, false))
                .collect::<Vec<_>>()
                .join(" ")
        }
        Geometry::Polygon(p) => poly(p),
        Geometry::Rect(r) => poly(&r.to_polygon()),
        Geometry::Triangle(t) => poly(&t.to_polygon()),
        Geometry::MultiPolygon(m) => m.0.iter().map(poly).collect::<Vec<_>>().join(" "),
        Geometry::GeometryCollection(c) => {
            c.0.iter()
                .map(|g| svg(g, rel, dec))
                .collect::<Vec<_>>()
                .join(";")
        }
    }
}

/// One coordinate in a PostGIS ST_AsLatLonText format: `D`, `M` and `S` runs (with `.` and
/// more letters for decimals) and `C` for the cardinal letter; other characters are copied.
fn lat_lon_text(v: f64, format: &str, pos: char, neg: char) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum F {
        D,
        M,
        S,
    }
    let chars: Vec<char> = format.chars().collect();
    let has = |c: char| chars.contains(&c);
    let last = if has('S') {
        F::S
    } else if has('M') {
        F::M
    } else {
        F::D
    };
    let a = v.abs();
    let (mut d, mut m, mut s) = (a, 0.0, 0.0);
    if last != F::D {
        d = a.trunc();
        m = (a - d) * 60.0;
        if last == F::S {
            let mi = m.trunc();
            s = (m - mi) * 60.0;
            m = mi;
        }
    }
    let mut out = String::new();
    if !has('C') && v < 0.0 {
        out.push('-');
    }
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let f = match c {
            'D' => Some(F::D),
            'M' => Some(F::M),
            'S' => Some(F::S),
            _ => None,
        };
        if let Some(f) = f {
            while i < chars.len() && chars[i] == c {
                i += 1;
            }
            let mut decimals = 0;
            if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1] == c {
                i += 1;
                while i < chars.len() && chars[i] == c {
                    decimals += 1;
                    i += 1;
                }
            }
            let val = match f {
                F::D => d,
                F::M => m,
                F::S => s,
            };
            let val = if f == last { val } else { val.trunc() };
            out.push_str(&format!("{val:.decimals$}"));
            continue;
        }
        if c == 'C' {
            out.push(if v < 0.0 { neg } else { pos });
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

// --- Small numeric helpers -----------------------------------------------------------------

/// Clockwise angle from north of the vector a -> b, in [0, 2π).
fn azimuth(a: Coord<f64>, b: Coord<f64>) -> f64 {
    (b.x - a.x).atan2(b.y - a.y).rem_euclid(2.0 * PI)
}

fn point_arg(t: &Term) -> Option<Coord<f64>> {
    match geom(t)?.geometry {
        Geometry::Point(p) => Some(p.0),
        _ => None,
    }
}

/// Deterministic pseudo-random numbers (xorshift64*), so seeded calls repeat.
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn is_oriented(g: &Geometry<f64>, cw_shell: bool) -> bool {
    polygons_of(g).iter().all(|p| {
        let ok = |r: &LineString<f64>, cw: bool| {
            if cw { r.is_cw() } else { r.is_ccw() }
        };
        ok(p.exterior(), cw_shell) && p.interiors().iter().all(|r| ok(r, !cw_shell))
    })
}

pub(crate) fn make_valid(g: &Geometry<f64>) -> Option<Geometry<f64>> {
    if g.is_valid() {
        return Some(g.clone());
    }
    Some(match g {
        Geometry::Polygon(p) => polygons(p.make_valid().ok()?.0),
        Geometry::MultiPolygon(m) => polygons(m.make_valid().ok()?.0),
        Geometry::Rect(_) | Geometry::Triangle(_) => {
            polygons(MultiPolygon(polygons_of(g)).make_valid().ok()?.0)
        }
        Geometry::LineString(l) => {
            let c = remove_repeated(&l.0, 0.0, false);
            match c.len() {
                0 => empty_collection(),
                1 => Geometry::Point(Point(c[0])),
                _ => Geometry::LineString(LineString::new(c)),
            }
        }
        Geometry::MultiLineString(m) => {
            let mut lines = Vec::new();
            let mut pts = Vec::new();
            for l in &m.0 {
                match make_valid(&Geometry::LineString(l.clone()))? {
                    Geometry::LineString(l) => lines.push(Geometry::LineString(l)),
                    Geometry::Point(p) => pts.push(Geometry::Point(p)),
                    _ => {}
                }
            }
            lines.extend(pts);
            collect(lines)
        }
        Geometry::GeometryCollection(c) => Geometry::GeometryCollection(GeometryCollection(
            c.0.iter().map(make_valid).collect::<Option<Vec<_>>>()?,
        )),
        // Points, lines and multipoints with finite coordinates are valid.
        other => other.clone(),
    })
}

/// Douglas-Peucker per ring and line, keeping a part as it is where simplifying would make it
/// invalid or collapse it.
fn simplify_preserve(g: &Geometry<f64>, tol: f64) -> Geometry<f64> {
    let poly = |p: &Polygon<f64>| {
        let s = p.simplify(tol);
        if s.exterior().0.len() >= 4 && s.is_valid() && s.interiors().len() == p.interiors().len() {
            s
        } else {
            p.clone()
        }
    };
    match g {
        Geometry::LineString(l) => Geometry::LineString(l.simplify(tol)),
        Geometry::MultiLineString(m) => Geometry::MultiLineString(m.simplify(tol)),
        Geometry::Polygon(p) => Geometry::Polygon(poly(p)),
        Geometry::MultiPolygon(m) => {
            let s = MultiPolygon(m.0.iter().map(poly).collect());
            if s.is_valid() {
                Geometry::MultiPolygon(s)
            } else {
                g.clone()
            }
        }
        Geometry::GeometryCollection(c) => Geometry::GeometryCollection(GeometryCollection(
            c.0.iter().map(|g| simplify_preserve(g, tol)).collect(),
        )),
        other => other.clone(),
    }
}

/// Snaps the vertices of `g` to the vertices of `to` within `tol`, else to its segments.
fn snap(g: &Geometry<f64>, to: &Geometry<f64>, tol: f64) -> Geometry<f64> {
    let targets: Vec<Coord<f64>> = to.coords_iter().collect();
    let segs = segments(to);
    let one = |c: Coord<f64>| -> Coord<f64> {
        let best = targets
            .iter()
            .map(|&t| (Euclidean.distance(c, t), t))
            .filter(|(d, _)| *d <= tol)
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, t)) = best {
            return t;
        }
        segs.iter()
            .filter_map(|s| match geo::ClosestPoint::closest_point(s, &Point(c)) {
                geo::Closest::Intersection(p) | geo::Closest::SinglePoint(p) => Some(p.0),
                geo::Closest::Indeterminate => None,
            })
            .map(|p| (Euclidean.distance(c, p), p))
            .filter(|(d, _)| *d <= tol)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map_or(c, |(_, p)| p)
    };
    let snapped = map_lines(g, &|c, ring| {
        remove_repeated(&c.iter().map(|&c| one(c)).collect::<Vec<_>>(), 0.0, ring)
    });
    match snapped {
        Geometry::Point(p) => Geometry::Point(Point(one(p.0))),
        other => other,
    }
}

fn geometric_median(pts: &[Coord<f64>], tol: f64, max_iter: usize) -> Option<Coord<f64>> {
    (!pts.is_empty()).then_some(())?;
    let n = pts.len() as f64;
    let mut y = Coord {
        x: pts.iter().map(|p| p.x).sum::<f64>() / n,
        y: pts.iter().map(|p| p.y).sum::<f64>() / n,
    };
    for _ in 0..max_iter {
        if crate::cancel::stopped_now() {
            return None;
        }
        let (mut num, mut den) = (Coord { x: 0.0, y: 0.0 }, 0.0);
        let mut at_point = false;
        for &p in pts {
            let d = Euclidean.distance(p, y);
            if d < 1e-15 {
                at_point = true;
                continue;
            }
            num = num + p / d;
            den += 1.0 / d;
        }
        if den == 0.0 {
            break;
        }
        let next = num / den;
        let moved = Euclidean.distance(next, y);
        y = next;
        if moved <= tol || at_point && moved < tol * 10.0 {
            break;
        }
    }
    Some(y)
}

pub(crate) fn typed_text(text: &str, expected: &[&str], srid_arg: Option<&Term>) -> Option<Term> {
    let t: Term = Literal::new_typed_literal(text, crate::literal::WKT_LITERAL).into();
    let g = geom(&t)?;
    let kind = crate::postgis::postgis_type(&g.geometry);
    (expected.is_empty() || expected.contains(&kind)).then_some(())?;
    let crs = match srid_arg {
        Some(s) => crs_of_srid(int(s)?)?,
        None => g.crs.clone(),
    };
    Some(geom_literal(&Geom {
        geometry: g.geometry.clone(),
        crs,
        zm: g.zm.clone(),
        surface: None,
        prepared: Default::default(),
    }))
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    // --- Constructors from text and binary ----------------------------------------------
    for (name, kinds) in [
        ("ST_GeometryFromText", &[][..]),
        ("ST_WKTToSQL", &[][..]),
        ("ST_PointFromText", &["POINT"][..]),
        ("ST_LineFromText", &["LINESTRING"][..]),
        ("ST_PolygonFromText", &["POLYGON"][..]),
        ("ST_MPointFromText", &["MULTIPOINT"][..]),
        ("ST_MLineFromText", &["MULTILINESTRING"][..]),
        ("ST_MPolyFromText", &["MULTIPOLYGON"][..]),
        ("ST_GeomCollFromText", &["GEOMETRYCOLLECTION"][..]),
    ] {
        add(
            ST,
            name,
            func(move |args| match args {
                [t] => typed_text(text(t)?, kinds, None),
                [t, s] => typed_text(text(t)?, kinds, Some(s)),
                _ => None,
            }),
        );
    }
    add(
        ST,
        "ST_GeomFromEWKT",
        func(|args| {
            let [t] = args else { return None };
            let t = text(t)?.trim();
            match t.split_once(';') {
                Some((s, wkt)) if s.trim().to_ascii_uppercase().starts_with("SRID=") => {
                    let s: Term = Literal::from(s.trim()[5..].trim().parse::<i64>().ok()?).into();
                    typed_text(wkt, &[], Some(&s))
                }
                _ => typed_text(t, &[], None),
            }
        }),
    );
    add(
        ST,
        "ST_AsEWKT",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let body = match &g.surface {
                Some(s) => crate::literal::surface_wkt(s, &g.crs, true),
                None => crate::wkt_out::string(&g.geometry),
            };
            string(match srid(&g.crs) {
                0 => body,
                s => format!("SRID={s};{body}"),
            })
        }),
    );
    for (name, ewkb) in [
        ("ST_AsBinary", false),
        ("ST_AsWKB", false),
        ("ST_AsEWKB", true),
    ] {
        add(
            ST,
            name,
            func(move |args| {
                let [a] = args else { return None };
                let g = geom(a)?;
                Some(
                    Literal::new_typed_literal(
                        to_hex(&to_wkb(&g, ewkb)),
                        oxigraph::model::NamedNodeRef::new_unchecked(HEX_BINARY),
                    )
                    .into(),
                )
            }),
        );
    }
    add(
        ST,
        "ST_AsHEXEWKB",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            string(to_hex(&to_wkb(&g, true)))
        }),
    );
    for name in ["ST_GeomFromWKB", "ST_GeomFromEWKB", "ST_WKBToSQL"] {
        add(
            ST,
            name,
            func(|args| match args {
                [h] => from_wkb(text(h)?, None),
                [h, s] => from_wkb(text(h)?, Some(int(s)?)),
                _ => None,
            }),
        );
    }
    add(
        ST,
        "ST_MakeBox2D",
        func(|args| {
            let [a, b] = args else { return None };
            let (ga, gb) = (geom(a)?, geom(b)?);
            same_space(&ga, &gb).then_some(())?;
            let (Geometry::Point(p), Geometry::Point(q)) = (&ga.geometry, &gb.geometry) else {
                return None;
            };
            let r = Rect::new(p.0, q.0);
            out(envelope(r.min(), r.max()), &ga.crs)
        }),
    );
    add(
        ST,
        "ST_Polygon",
        func(|args| {
            let [l, s] = args else { return None };
            let g = geom(l)?;
            let ring = line(&g.geometry)?;
            (ring.is_closed() && ring.0.len() >= 4).then_some(())?;
            out(
                Geometry::Polygon(Polygon::new(ring.clone(), Vec::new())),
                &crs_of_srid(int(s)?)?,
            )
        }),
    );
    add(
        ST,
        "ST_TileEnvelope",
        func(|args| {
            let (z, x, y) = match args {
                [z, x, y] => (int(z)?, int(x)?, int(y)?),
                _ => return None,
            };
            (0..=32).contains(&z).then_some(())?;
            let tiles = 1i64 << z;
            ((0..tiles).contains(&x) && (0..tiles).contains(&y)).then_some(())?;
            const HALF: f64 = 20_037_508.342_789_244;
            let size = 2.0 * HALF / tiles as f64;
            let min = Coord {
                x: -HALF + x as f64 * size,
                y: HALF - (y + 1) as f64 * size,
            };
            let max = Coord {
                x: min.x + size,
                y: min.y + size,
            };
            out(envelope(min, max), &crs_of_srid(3857)?)
        }),
    );
    // --- Geohash and encoded polylines -----------------------------------------------------
    add(
        ST,
        "ST_GeoHash",
        func(|args| {
            let (a, precision) = match args {
                [a] => (a, None),
                [a, p] => (a, Some(int(p)?)),
                _ => return None,
            };
            let g = geom(a)?;
            g.crs.is_geographic().then_some(())?;
            let r = g.geometry.bounding_rect()?;
            if let Some(p) = precision {
                (1..=20).contains(&p).then_some(())?;
                return string(geohash(r.center(), p as usize)?);
            }
            if r.width() == 0.0 && r.height() == 0.0 {
                return string(geohash(r.min(), 20)?);
            }
            // The longest hash whose cell holds the whole box.
            let (a, b) = (geohash(r.min(), 20)?, geohash(r.max(), 20)?);
            let common = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
            string(&a[..common])
        }),
    );
    add(
        ST,
        "ST_PointFromGeoHash",
        func(|args| {
            let (h, p) = match args {
                [h] => (h, None),
                [h, p] => (h, Some(int(p)? as usize)),
                _ => return None,
            };
            let (min, max) = geohash_box(text(h)?, p)?;
            out(
                Geometry::Point(Point(Coord {
                    x: (min.x + max.x) / 2.0,
                    y: (min.y + max.y) / 2.0,
                })),
                &Crs::Crs84,
            )
        }),
    );
    for name in ["ST_GeomFromGeoHash", "ST_Box2dFromGeoHash"] {
        add(
            ST,
            name,
            func(|args| {
                let (h, p) = match args {
                    [h] => (h, None),
                    [h, p] => (h, Some(int(p)? as usize)),
                    _ => return None,
                };
                let (min, max) = geohash_box(text(h)?, p)?;
                out(envelope(min, max), &Crs::Crs84)
            }),
        );
    }
    add(
        ST,
        "ST_AsEncodedPolyline",
        func(|args| {
            let (a, p) = match args {
                [a] => (a, 5),
                [a, p] => (a, int(p)?),
                _ => return None,
            };
            (0..=10).contains(&p).then_some(())?;
            let g = geom(a)?;
            g.crs.is_geographic().then_some(())?;
            string(encode_polyline(&line(&g.geometry)?.0, p as i32))
        }),
    );
    add(
        ST,
        "ST_LineFromEncodedPolyline",
        func(|args| {
            let (s, p) = match args {
                [s] => (s, 5),
                [s, p] => (s, int(p)?),
                _ => return None,
            };
            (0..=10).contains(&p).then_some(())?;
            let c = decode_polyline(text(s)?, p as i32)?;
            (c.len() >= 2).then_some(())?;
            out(Geometry::LineString(LineString::new(c)), &Crs::Crs84)
        }),
    );
    // --- Output formats ------------------------------------------------------------------
    add(
        ST,
        "ST_AsSVG",
        func(|args| {
            let (a, rel, dec) = match args {
                [a] => (a, 0, 15),
                [a, r] => (a, int(r)?, 15),
                [a, r, d] => (a, int(r)?, int(d)?),
                _ => return None,
            };
            (0..=15).contains(&dec).then_some(())?;
            string(svg(&geom(a)?.geometry, rel != 0, dec as usize))
        }),
    );
    add(
        ST,
        "ST_AsLatLonText",
        func(|args| {
            let (a, f) = match args {
                [a] => (a, "D°M'S.SSS\"C"),
                [a, f] => (a, text(f)?),
                _ => return None,
            };
            let g = geom(a)?;
            g.crs.is_geographic().then_some(())?;
            let Geometry::Point(p) = g.geometry else {
                return None;
            };
            // Longitudes are normalised to [-180, 180] and latitudes to [-90, 90] as in PostGIS.
            let mut lon = (p.x() + 180.0).rem_euclid(360.0) - 180.0;
            let mut lat = p.y();
            if !(-90.0..=90.0).contains(&lat) {
                lat = lat.clamp(-270.0, 270.0);
                lat = if lat > 90.0 {
                    180.0 - lat
                } else {
                    -180.0 - lat
                };
                lon += 180.0;
                lon = (lon + 180.0).rem_euclid(360.0) - 180.0;
            }
            string(format!(
                "{} {}",
                lat_lon_text(lat, f, 'N', 'S'),
                lat_lon_text(lon, f, 'E', 'W')
            ))
        }),
    );
    // --- Accessors -----------------------------------------------------------------------
    add(
        ST,
        "ST_Z",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let Geometry::Point(p) = g.geometry else {
                return None;
            };
            let zm = g.zm.as_ref().filter(|z| z.has_z)?;
            double(zm.at(p.0)?.0)
        }),
    );
    add(
        ST,
        "ST_M",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let Geometry::Point(p) = g.geometry else {
                return None;
            };
            let zm = g.zm.as_ref().filter(|z| z.has_m)?;
            double(zm.at(p.0)?.1)
        }),
    );
    add(
        ST,
        "ST_Zmflag",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let (z, m) =
                g.zm.as_ref()
                    .map_or((false, false), |zm| (zm.has_z, zm.has_m));
            integer(i64::from(m) + 2 * i64::from(z))
        }),
    );
    add(
        ST,
        "ST_HasArc",
        func(|args| {
            let [a] = args else { return None };
            geom(a)?;
            boolean(false)
        }),
    );
    add(
        ST,
        "ST_NRings",
        func(|args| {
            let [a] = args else { return None };
            let p = polygons_of(&geom(a)?.geometry);
            integer(p.iter().map(|p| 1 + p.interiors().len() as i64).sum())
        }),
    );
    add(
        ST,
        "ST_IsPolygonCW",
        func(|args| {
            let [a] = args else { return None };
            boolean(is_oriented(&geom(a)?.geometry, true))
        }),
    );
    add(
        ST,
        "ST_IsPolygonCCW",
        func(|args| {
            let [a] = args else { return None };
            boolean(is_oriented(&geom(a)?.geometry, false))
        }),
    );
    // --- Relationships and measures ------------------------------------------------------
    add(
        ST,
        "ST_ContainsProperly",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            boolean(a.geometry.relate(&b.geometry).matches("T**FF*FF*").ok()?)
        }),
    );
    add(
        ST,
        "ST_RelateMatch",
        func(|args| {
            let [m, p] = args else { return None };
            let (m, p) = (text(m)?, text(p)?);
            (m.len() == 9 && p.len() == 9).then_some(())?;
            let ok = m
                .chars()
                .zip(p.chars())
                .all(|(m, p)| match p.to_ascii_uppercase() {
                    '*' => true,
                    'T' => matches!(m, '0' | '1' | '2'),
                    'F' => m.eq_ignore_ascii_case(&'F'),
                    d => m == d,
                });
            boolean(ok)
        }),
    );
    add(
        ST,
        "ST_PointInsideCircle",
        func(|args| {
            let [p, x, y, r] = args else { return None };
            let c = point_arg(p)?;
            boolean(
                Euclidean.distance(
                    c,
                    Coord {
                        x: num(x)?,
                        y: num(y)?,
                    },
                ) <= num(r)?,
            )
        }),
    );
    add(
        ST,
        "ST_Angle",
        func(|args| {
            let a = match args {
                [l1, l2] if !matches!(geom(l1)?.geometry, Geometry::Point(_)) => {
                    let (g1, g2) = (geom(l1)?, geom(l2)?);
                    let (a, b) = (line(&g1.geometry)?, line(&g2.geometry)?);
                    azimuth(*b.0.first()?, *b.0.last()?) - azimuth(*a.0.first()?, *a.0.last()?)
                }
                [p1, p2, p3] => {
                    let (p1, p2, p3) = (point_arg(p1)?, point_arg(p2)?, point_arg(p3)?);
                    azimuth(p2, p3) - azimuth(p2, p1)
                }
                [p1, p2, p3, p4] => {
                    let (p1, p2, p3, p4) = (
                        point_arg(p1)?,
                        point_arg(p2)?,
                        point_arg(p3)?,
                        point_arg(p4)?,
                    );
                    azimuth(p3, p4) - azimuth(p1, p2)
                }
                _ => return None,
            };
            double(a.rem_euclid(2.0 * PI))
        }),
    );
    add(
        ST,
        "ST_LongestLine",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            let mut best: Option<(f64, Coord<f64>, Coord<f64>)> = None;
            for p in a.geometry.coords_iter() {
                for q in b.geometry.coords_iter() {
                    let d = Euclidean.distance(p, q);
                    if best.is_none_or(|(bd, _, _)| d > bd) {
                        best = Some((d, p, q));
                    }
                }
            }
            let (_, p, q) = best?;
            out(Geometry::LineString(LineString::new(vec![p, q])), &a.crs)
        }),
    );
    // --- Editors and affine transforms ---------------------------------------------------
    add(
        ST,
        "ST_ForceCollection",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let c = match &g.geometry {
                Geometry::GeometryCollection(_) => g.geometry.clone(),
                other => Geometry::GeometryCollection(GeometryCollection(vec![other.clone()])),
            };
            out(c, &g.crs)
        }),
    );
    add(
        ST,
        "ST_RemoveRepeatedPoints",
        func(|args| {
            let (a, tol) = match args {
                [a] => (a, 0.0),
                [a, t] => (a, num(t)?),
                _ => return None,
            };
            (tol >= 0.0).then_some(())?;
            let g = geom(a)?;
            out(
                map_lines(&g.geometry, &|c, ring| remove_repeated(c, tol, ring)),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_SnapToGrid",
        func(|args| {
            let (a, origin, size) = match args {
                [a, s] => (
                    a,
                    Coord { x: 0.0, y: 0.0 },
                    Coord {
                        x: num(s)?,
                        y: num(s)?,
                    },
                ),
                [a, sx, sy] => (
                    a,
                    Coord { x: 0.0, y: 0.0 },
                    Coord {
                        x: num(sx)?,
                        y: num(sy)?,
                    },
                ),
                [a, ox, oy, sx, sy] => (
                    a,
                    Coord {
                        x: num(ox)?,
                        y: num(oy)?,
                    },
                    Coord {
                        x: num(sx)?,
                        y: num(sy)?,
                    },
                ),
                _ => return None,
            };
            let g = geom(a)?;
            let snapped = match &g.geometry {
                Geometry::Point(p) => Geometry::Point(Point(snap_coord(p.0, origin, size))),
                other => map_lines(other, &|c, ring| {
                    let c: Vec<_> = c.iter().map(|&c| snap_coord(c, origin, size)).collect();
                    remove_repeated(&c, 0.0, ring)
                }),
            };
            out(snapped, &g.crs)
        }),
    );
    add(
        ST,
        "ST_Snap",
        func(|args| {
            let [a, b, tol] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            out(snap(&a.geometry, &b.geometry, num(tol)?), &a.crs)
        }),
    );
    add(
        ST,
        "ST_Affine",
        func(|args| {
            let v: Vec<f64> = args.get(1..)?.iter().map(num).collect::<Option<_>>()?;
            // 2-D form (a, b, d, e, xoff, yoff) or the 3-D one, whose Z terms are dropped.
            let (a, b, d, e, xo, yo) = match v.as_slice() {
                [a, b, d, e, xo, yo] => (*a, *b, *d, *e, *xo, *yo),
                [a, b, _c, d, e, _f, _g, _h, _i, xo, yo, _zo] => (*a, *b, *d, *e, *xo, *yo),
                _ => return None,
            };
            let g = geom(args.first()?)?;
            out(
                geo::MapCoords::map_coords(&g.geometry, |c| Coord {
                    x: a * c.x + b * c.y + xo,
                    y: d * c.x + e * c.y + yo,
                }),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_TransScale",
        func(|args| {
            let [a, dx, dy, sx, sy] = args else {
                return None;
            };
            let (dx, dy, sx, sy) = (num(dx)?, num(dy)?, num(sx)?, num(sy)?);
            let g = geom(a)?;
            out(
                geo::MapCoords::map_coords(&g.geometry, |c| Coord {
                    x: (c.x + dx) * sx,
                    y: (c.y + dy) * sy,
                }),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_ShiftLongitude",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(
                geo::MapCoords::map_coords(&g.geometry, |c| Coord {
                    x: if c.x < 0.0 {
                        c.x + 360.0
                    } else if c.x > 180.0 {
                        c.x - 360.0
                    } else {
                        c.x
                    },
                    y: c.y,
                }),
                &g.crs,
            )
        }),
    );
    // --- Validation ----------------------------------------------------------------------
    add(
        ST,
        "ST_MakeValid",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(make_valid(&g.geometry)?, &g.crs)
        }),
    );
    // --- Processing ----------------------------------------------------------------------
    add(
        ST,
        "ST_SimplifyPreserveTopology",
        func(|args| {
            let [a, t] = args else { return None };
            let g = geom(a)?;
            out(simplify_preserve(&g.geometry, num(t)?), &g.crs)
        }),
    );
    add(
        ST,
        "ST_OrientedEnvelope",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let r = g.geometry.bounding_rect()?;
            if r.width() == 0.0 && r.height() == 0.0 {
                return out(Geometry::Point(Point(r.min())), &g.crs);
            }
            match g.geometry.minimum_rotated_rect() {
                Some(p) if geo::Area::unsigned_area(&p) > 0.0 => out(Geometry::Polygon(p), &g.crs),
                // Collinear input: the line between its extreme points.
                _ => {
                    let pts: Vec<Coord<f64>> = g.geometry.coords_iter().collect();
                    let first = *pts.first()?;
                    let far = |from: Coord<f64>| {
                        pts.iter()
                            .copied()
                            .max_by(|a, b| {
                                Euclidean
                                    .distance(*a, from)
                                    .total_cmp(&Euclidean.distance(*b, from))
                            })
                            .unwrap_or(from)
                    };
                    let p = far(first);
                    let q = far(p);
                    let (p, q) = if (p.x, p.y) <= (q.x, q.y) {
                        (p, q)
                    } else {
                        (q, p)
                    };
                    out(Geometry::LineString(LineString::new(vec![p, q])), &g.crs)
                }
            }
        }),
    );
    add(
        ST,
        "ST_DelaunayTriangles",
        func(|args| {
            let (a, tol, flags) = match args {
                [a] => (a, 0.0, 0),
                [a, t] => (a, num(t)?, 0),
                [a, t, f] => (a, num(t)?, int(f)?),
                _ => return None,
            };
            let g = geom(a)?;
            let mut pts: Vec<Coord<f64>> = g.geometry.coords_iter().collect();
            if tol > 0.0 {
                let size = Coord { x: tol, y: tol };
                pts.iter_mut()
                    .for_each(|c| *c = snap_coord(*c, Coord { x: 0.0, y: 0.0 }, size));
            }
            pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
            pts.dedup();
            let mp = MultiPoint(pts.into_iter().map(Point).collect());
            let tris = mp.unconstrained_triangulation().ok()?;
            match flags {
                0 => out(
                    Geometry::GeometryCollection(GeometryCollection(
                        tris.into_iter()
                            .map(|t| Geometry::Polygon(t.to_polygon()))
                            .collect(),
                    )),
                    &g.crs,
                ),
                1 => {
                    let mut edges: Vec<(Coord<f64>, Coord<f64>)> = Vec::new();
                    for t in &tris {
                        for (p, q) in [(t.v1(), t.v2()), (t.v2(), t.v3()), (t.v3(), t.v1())] {
                            let (p, q) = if (p.x, p.y) <= (q.x, q.y) {
                                (p, q)
                            } else {
                                (q, p)
                            };
                            if !edges.contains(&(p, q)) {
                                edges.push((p, q));
                            }
                        }
                    }
                    out(
                        Geometry::MultiLineString(MultiLineString(
                            edges
                                .into_iter()
                                .map(|(p, q)| LineString::new(vec![p, q]))
                                .collect(),
                        )),
                        &g.crs,
                    )
                }
                _ => None,
            }
        }),
    );
    for (name, cells) in [("ST_VoronoiPolygons", true), ("ST_VoronoiLines", false)] {
        add(
            ST,
            name,
            func(move |args| {
                let (a, tol, extend) = match args {
                    [a] => (a, 0.0, None),
                    [a, t] => (a, num(t)?, None),
                    [a, t, e] => (a, num(t)?, Some(geom(e)?)),
                    _ => return None,
                };
                let g = geom(a)?;
                let mut pts: Vec<Coord<f64>> = g.geometry.coords_iter().collect();
                pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
                pts.dedup();
                (pts.len() >= 2).then_some(())?;
                let mp = MultiPoint(pts.into_iter().map(Point).collect());
                let clip_poly;
                let mut params = geo::VoronoiParams::new().tolerance(tol);
                if let Some(e) = &extend {
                    let r = g.geometry.bounding_rect()?;
                    let er = e.geometry.bounding_rect()?;
                    let u = Rect::new(
                        Coord {
                            x: r.min().x.min(er.min().x),
                            y: r.min().y.min(er.min().y),
                        },
                        Coord {
                            x: r.max().x.max(er.max().x),
                            y: r.max().y.max(er.max().y),
                        },
                    );
                    clip_poly = u.to_polygon();
                    params = params.clip(geo::VoronoiClip::Polygon(&clip_poly));
                }
                if cells {
                    let c = mp.voronoi_cells_with_params(params).ok()?;
                    out(
                        Geometry::GeometryCollection(GeometryCollection(
                            c.into_iter().map(Geometry::Polygon).collect(),
                        )),
                        &g.crs,
                    )
                } else {
                    let e = mp.voronoi_edges_with_params(params).ok()?;
                    out(
                        Geometry::MultiLineString(MultiLineString(
                            e.into_iter()
                                .map(|l| LineString::new(vec![l.start, l.end]))
                                .collect(),
                        )),
                        &g.crs,
                    )
                }
            }),
        );
    }
    add(
        ST,
        "ST_OffsetCurve",
        func(|args| {
            let (a, d) = match args {
                [a, d] | [a, d, _] => (a, num(d)?),
                _ => return None,
            };
            let g = geom(a)?;
            if d == 0.0 {
                return out(g.geometry.clone(), &g.crs);
            }
            let lines = lines_of(&g.geometry);
            (!lines.is_empty() && polygons_of(&g.geometry).is_empty()).then_some(())?;
            let mut offs: Vec<LineString<f64>> =
                lines.iter().filter_map(|l| offset_curve(l, d)).collect();
            if offs.len() == 1 {
                out(Geometry::LineString(offs.remove(0)), &g.crs)
            } else {
                out(Geometry::MultiLineString(MultiLineString(offs)), &g.crs)
            }
        }),
    );
    add(
        ST,
        "ST_LineInterpolatePoints",
        func(|args| {
            let (l, f, repeat) = match args {
                [l, f] => (l, num(f)?, true),
                [l, f, r] => (
                    l,
                    num(f)?,
                    match r {
                        Term::Literal(x) => x.value() == "true" || x.value() == "1",
                        _ => return None,
                    },
                ),
                _ => return None,
            };
            (0.0..=1.0).contains(&f).then_some(())?;
            let g = geom(l)?;
            let ls = line(&g.geometry)?;
            let mut fractions = vec![f];
            if repeat && f > 0.0 {
                // As many points as ST_GeneratePoints allows, not 10^12 for a tiny fraction.
                (1.0 / f <= 1_000_000.0).then_some(())?;
                let mut k = 2.0;
                while k * f <= 1.0 + 1e-12 {
                    fractions.push((k * f).min(1.0));
                    k += 1.0;
                }
            }
            let pts: Vec<Point<f64>> = fractions
                .into_iter()
                .filter_map(|f| Euclidean.point_at_ratio_from_start(ls, f))
                .collect();
            if pts.len() == 1 {
                out(Geometry::Point(pts[0]), &g.crs)
            } else {
                out(Geometry::MultiPoint(MultiPoint(pts)), &g.crs)
            }
        }),
    );
    // --- Overlay -------------------------------------------------------------------------
    add(
        ST,
        "ST_Split",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            let lines = lines_of(&a.geometry);
            (!lines.is_empty() && polygons_of(&a.geometry).is_empty()).then_some(())?;
            let cuts: Vec<Coord<f64>> = match &b.geometry {
                Geometry::Point(_) | Geometry::MultiPoint(_) => b.geometry.coords_iter().collect(),
                other => {
                    let blade = segments(other);
                    (!blade.is_empty()).then_some(())?;
                    let own: Vec<Line<f64>> = lines.iter().flat_map(|l| l.lines()).collect();
                    crossings(&own, &blade)
                }
            };
            let eps = 1e-9
                * a.geometry
                    .bounding_rect()
                    .map_or(1.0, |r| r.width().max(r.height()).max(1.0));
            let pieces: Vec<Geometry<f64>> = lines
                .iter()
                .flat_map(|l| split_line(l, &cuts, eps))
                .map(Geometry::LineString)
                .collect();
            out(
                Geometry::GeometryCollection(GeometryCollection(pieces)),
                &a.crs,
            )
        }),
    );
    add(
        ST,
        "ST_Node",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let lines = lines_of(&g.geometry);
            (!lines.is_empty()).then_some(())?;
            out(
                Geometry::MultiLineString(MultiLineString(node(&lines))),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_UnaryUnion",
        func(|args| {
            let a = match args {
                [a] | [a, _] => a,
                _ => return None,
            };
            let g = geom(a)?;
            let polys = polygons_of(&g.geometry);
            let area: MultiPolygon<f64> = geo::algorithm::unary_union(&polys);
            let mut out_parts: Vec<Geometry<f64>> =
                area.0.iter().cloned().map(Geometry::Polygon).collect();
            let lines = lines_of(&g.geometry);
            if !lines.is_empty() {
                let noded = node(&lines);
                let kept = if area.0.is_empty() {
                    noded
                } else {
                    MultiPolygon(area.0.clone())
                        .clip(&MultiLineString(noded), true)
                        .0
                };
                out_parts.extend(kept.into_iter().map(Geometry::LineString));
            }
            let mut seen: Vec<Coord<f64>> = Vec::new();
            for p in points_of(&g.geometry) {
                let covered = area
                    .0
                    .iter()
                    .any(|a| a.contains(&p) || a.relate(&p).is_covers())
                    || out_parts
                        .iter()
                        .any(|g| matches!(g, Geometry::LineString(l) if l.relate(&p).is_covers()));
                if !covered && !seen.contains(&p.0) {
                    seen.push(p.0);
                    out_parts.push(Geometry::Point(p));
                }
            }
            let result = match out_parts.len() {
                0 => empty_collection(),
                1 => out_parts.remove(0),
                _ => collect(out_parts),
            };
            out(result, &g.crs)
        }),
    );
    add(
        ST,
        "ST_Subdivide",
        func(|args| {
            let (a, max) = match args {
                [a] => (a, 256),
                [a, m] => (a, int(m)?),
                _ => return None,
            };
            (max >= 5).then_some(())?;
            let g = geom(a)?;
            let mut pieces = Vec::new();
            subdivide(g.geometry.clone(), max as usize, 0, &mut pieces);
            let flat: Vec<Geometry<f64>> = pieces.iter().flat_map(parts).collect();
            out(
                Geometry::GeometryCollection(GeometryCollection(flat)),
                &g.crs,
            )
        }),
    );
    // --- Generators ----------------------------------------------------------------------
    add(
        ST,
        "ST_GeneratePoints",
        func(|args| {
            let (a, n, seed) = match args {
                [a, n] => (a, int(n)?, 0x9E37_79B9_7F4A_7C15),
                [a, n, s] => (a, int(n)?, int(s)? as u64),
                _ => return None,
            };
            (0..=1_000_000).contains(&n).then_some(())?;
            let g = geom(a)?;
            let polys = MultiPolygon(polygons_of(&g.geometry));
            (!polys.0.is_empty()).then_some(())?;
            let r = polys.bounding_rect()?;
            let mut rng = Rng(seed.max(1));
            let mut pts = Vec::with_capacity(n as usize);
            let mut tries = 0usize;
            while pts.len() < n as usize && tries < 1000 * n as usize + 1000 {
                if crate::cancel::stopped() {
                    return None;
                }
                tries += 1;
                let p = Point::new(
                    r.min().x + rng.next_f64() * r.width(),
                    r.min().y + rng.next_f64() * r.height(),
                );
                if polys.contains(&p) {
                    pts.push(p);
                }
            }
            out(Geometry::MultiPoint(MultiPoint(pts)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_GeometricMedian",
        func(|args| {
            let (a, tol, iter) = match args {
                [a] => (a, None, 10_000),
                [a, t] => (a, Some(num(t)?), 10_000),
                [a, t, i] | [a, t, i, _] => (a, Some(num(t)?), int(i)?),
                _ => return None,
            };
            let g = geom(a)?;
            let pts: Vec<Coord<f64>> = g.geometry.coords_iter().collect();
            let r = g.geometry.bounding_rect()?;
            let tol = tol.unwrap_or(1e-6 * r.width().max(r.height()).max(f64::MIN_POSITIVE));
            // A NaN or negative tolerance is never reached, so every iteration would run.
            if !(tol.is_finite() && tol >= 0.0) {
                return None;
            }
            let m = geometric_median(&pts, tol, iter.max(1) as usize)?;
            out(Geometry::Point(Point(m)), &g.crs)
        }),
    );
}
