//! `rail:` functions (`http://w3id.org/tripletrail/rail#`): linear referencing along lines with
//! kilometre tables, the alpha shape of pgRouting's `pgr_alphaShape`, and line conflation.
//!
//! They work on geometry literals and numbers given by the query, whatever data model the
//! lines come from: a line is a `LINESTRING`, or a `MULTILINESTRING` whose parts are elements in
//! order and direction, measured as one line (gaps between parts add no length). Positions
//! along it are intrinsic coordinates, 0 at the start and 1 at the end, proportional to the
//! geodesic length on CRS84/EPSG:4326 and to the planar length otherwise.
//!
//! A kilometre table maps intrinsic coordinates to kilometres. It is either a string of
//! `intrinsic:km` anchors, `"0:12.300 0.5:12.950 1:13.600"` (separated by spaces, commas or
//! semicolons, intrinsic coordinates not decreasing), interpolated linearly in between and
//! extrapolated beyond the ends; or a line with M ordinates, whose vertices are anchors with
//! their M as kilometre (at their position along the line, found by projection unless the line
//! is the referenced line itself). Kilometres may decrease along the line. Two anchors at the same
//! intrinsic coordinate are a discontinuity (a jump in kilometres): at that position, `kmAt`
//! gives the value after the jump, and `intrinsicAtKm` finds no position for a kilometre inside
//! the jump. A kilometre that occurs more than once (an overlap) gives its first position.

use crate::functions::{Func, double, func, geom, num, same_space};
use crate::literal::{Crs, Geom, WKT_LITERAL, geom_literal, wkt_literal};
use crate::postgis_processing::{lines_of, offset_curve, polygons};
use geo::line_measures::{Distance, Euclidean, FrechetDistance, Geodesic, Length};
use geo::{
    BoundingRect, Coord, CoordsIter, Geometry, HausdorffDistance, LineString, MapCoords,
    MultiLineString, MultiPoint, Point, Polygon, TriangulateDelaunayUnconstrained,
};
use oxigraph::model::Term;
use std::sync::Arc;

/// Namespace of the `rail:` functions.
pub const RAIL: &str = "http://w3id.org/tripletrail/rail#";

const EARTH_RADIUS: f64 = 6_371_008.8;

// --- Measured lines ------------------------------------------------------------------------

struct Seg {
    a: Coord<f64>,
    b: Coord<f64>,
    /// Distance from the start of the line to `a`.
    start: f64,
    len: f64,
}

/// A line measured along its length.
struct Measured {
    parts: Vec<Vec<Coord<f64>>>,
    /// Segments of every part, in order; parts with one point add none.
    segs: Vec<Seg>,
    /// Distance of every vertex of every part from the start.
    vertex_at: Vec<Vec<f64>>,
    total: f64,
    geographic: bool,
}

impl Measured {
    fn new(g: &Geom) -> Option<Self> {
        let parts: Vec<Vec<Coord<f64>>> = lines_of(&g.geometry)
            .into_iter()
            .map(|l| l.0)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty()
            || !matches!(
                g.geometry,
                Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_)
            )
        {
            return None;
        }
        let geographic = g.crs.is_geographic();
        let mut segs = Vec::new();
        let mut vertex_at = Vec::new();
        let mut walked = 0.0;
        for p in &parts {
            let mut at = vec![walked];
            for w in p.windows(2) {
                let len = if geographic {
                    Geodesic.distance(Point(w[0]), Point(w[1]))
                } else {
                    Euclidean.distance(w[0], w[1])
                };
                segs.push(Seg {
                    a: w[0],
                    b: w[1],
                    start: walked,
                    len,
                });
                walked += len;
                at.push(walked);
            }
            vertex_at.push(at);
        }
        Some(Self {
            parts,
            segs,
            vertex_at,
            total: walked,
            geographic,
        })
    }

    /// The point `d` along the line, and the direction of its segment.
    fn point_at(&self, d: f64) -> (Coord<f64>, Coord<f64>) {
        let Some(last) = self.segs.last() else {
            return (self.parts[0][0], Coord { x: 0.0, y: 0.0 });
        };
        let d = d.clamp(0.0, self.total);
        let i = self
            .segs
            .partition_point(|s| s.start + s.len < d)
            .min(self.segs.len() - 1);
        let s = if self.segs[i].len == 0.0 && i + 1 == self.segs.len() {
            last
        } else {
            &self.segs[i]
        };
        let t = if s.len > 0.0 {
            ((d - s.start) / s.len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (s.a + (s.b - s.a) * t, s.b - s.a)
    }

    /// Scale of x relative to y in a local metric frame (longitudes shrink with latitude).
    fn kx(&self) -> f64 {
        if !self.geographic {
            return 1.0;
        }
        let lat = self.parts.iter().flatten().map(|c| c.y).sum::<f64>()
            / self.parts.iter().map(Vec::len).sum::<usize>() as f64;
        lat.to_radians().cos()
    }

    /// Distance along the line of the point of it nearest to `c`.
    fn locate(&self, c: Coord<f64>) -> f64 {
        if self.segs.is_empty() {
            return 0.0;
        }
        let kx = self.kx();
        let scale = |p: Coord<f64>| Coord {
            x: p.x * kx,
            y: p.y,
        };
        let p = scale(c);
        let mut best = (f64::INFINITY, 0.0);
        for s in &self.segs {
            let (a, b) = (scale(s.a), scale(s.b));
            let ab = b - a;
            let l2 = ab.x * ab.x + ab.y * ab.y;
            let t = if l2 > 0.0 {
                (((p - a).x * ab.x + (p - a).y * ab.y) / l2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let q = a + ab * t;
            let dist = (p - q).x.hypot((p - q).y);
            if dist < best.0 {
                best = (dist, s.start + t * s.len);
            }
        }
        best.1
    }

    /// The pieces of the line between distances `d1 <= d2`, one per part touched, joined
    /// where one ends at the start of the next.
    fn substring(&self, d1: f64, d2: f64) -> Vec<Vec<Coord<f64>>> {
        let mut pieces: Vec<Vec<Coord<f64>>> = Vec::new();
        for (p, part) in self.parts.iter().enumerate() {
            let at = &self.vertex_at[p];
            let (s, e) = (at[0], at[at.len() - 1]);
            if part.len() < 2 || e < d1 || s > d2 || (e == d1 && d1 < d2 && e < self.total) {
                continue;
            }
            let mut piece = vec![self.part_point(p, d1.max(s))];
            for (i, &c) in part.iter().enumerate() {
                if at[i] > d1 && at[i] < d2 {
                    piece.push(c);
                }
            }
            piece.push(self.part_point(p, d2.min(e)));
            piece.dedup();
            match pieces.last_mut() {
                Some(prev) if prev.last() == piece.first() => {
                    prev.extend(piece.into_iter().skip(1))
                }
                _ => pieces.push(piece),
            }
        }
        pieces
    }

    /// The point `d` along part `p` (clamped to it).
    fn part_point(&self, p: usize, d: f64) -> Coord<f64> {
        let at = &self.vertex_at[p];
        let part = &self.parts[p];
        for i in 0..part.len() - 1 {
            if d <= at[i + 1] || i + 2 == part.len() {
                let len = at[i + 1] - at[i];
                let t = if len > 0.0 {
                    ((d - at[i]) / len).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                return part[i] + (part[i + 1] - part[i]) * t;
            }
        }
        part[0]
    }
}

// --- Kilometre tables ----------------------------------------------------------------------

/// A vertex and its M.
type Measure = (Coord<f64>, f64);

/// The vertices of a WKT line with M, in order, per part: (x, y) and M.
fn measured_vertices(t: &Term) -> Option<Vec<Vec<Measure>>> {
    use geo_traits::{CoordTrait, GeometryTrait};
    let Term::Literal(l) = t else { return None };
    if l.datatype() != WKT_LITERAL {
        return None;
    }
    let g = geom(t)?;
    let text = l.value().trim();
    let text = match text.strip_prefix('<') {
        Some(rest) => rest[rest.find('>')? + 1..].trim_start(),
        None => text,
    };
    let parsed: wkt::Wkt<f64> = text.parse().ok()?;
    let m_index = match parsed.dim() {
        geo_traits::Dimensions::Xym => 2,
        geo_traits::Dimensions::Xyzm => 3,
        _ => return None,
    };
    let swap = g.crs == Crs::Epsg4326;
    let vertices = |l: &wkt::types::LineString<f64>| -> Vec<Measure> {
        l.coords()
            .iter()
            .map(|c| {
                let (x, y) = (c.x(), c.y());
                let xy = if swap {
                    Coord { x: y, y: x }
                } else {
                    Coord { x, y }
                };
                (xy, c.nth(m_index).unwrap_or(f64::NAN))
            })
            .collect()
    };
    match &parsed {
        wkt::Wkt::LineString(l) => Some(vec![vertices(l)]),
        wkt::Wkt::MultiLineString(m) => Some(m.line_strings().iter().map(vertices).collect()),
        _ => None,
    }
}

/// Anchors (intrinsic, km), by intrinsic coordinate.
struct KmTable {
    anchors: Vec<(f64, f64)>,
    /// Kilometres per unit of intrinsic coordinate where the table gives no slope.
    default_slope: f64,
}

impl KmTable {
    fn parse(t: &Term, line: &Measured) -> Option<Self> {
        let anchors = match measured_vertices(t) {
            Some(parts) => {
                let total = line.total;
                (total > 0.0).then_some(())?;
                let same_line = parts.len() == line.parts.len()
                    && parts.iter().zip(&line.parts).all(|(a, b)| {
                        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.0 == *y)
                    });
                let mut anchors = Vec::new();
                for (p, part) in parts.iter().enumerate() {
                    for (i, &(c, m)) in part.iter().enumerate() {
                        if !m.is_finite() {
                            continue;
                        }
                        let d = if same_line {
                            line.vertex_at[p][i]
                        } else {
                            line.locate(c)
                        };
                        anchors.push((d / total, m));
                    }
                }
                anchors.sort_by(|a, b| a.0.total_cmp(&b.0));
                anchors
            }
            None => {
                let Term::Literal(l) = t else { return None };
                let mut anchors: Vec<(f64, f64)> = Vec::new();
                for token in l
                    .value()
                    .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
                    .filter(|s| !s.is_empty())
                {
                    let (f, km) = token.split_once(':')?;
                    let (f, km): (f64, f64) = (f.trim().parse().ok()?, km.trim().parse().ok()?);
                    (f.is_finite() && km.is_finite()).then_some(())?;
                    if anchors.last().is_some_and(|a| a.0 > f) {
                        return None;
                    }
                    anchors.push((f, km));
                }
                anchors
            }
        };
        (!anchors.is_empty()).then_some(())?;
        Some(Self {
            anchors,
            default_slope: line.total / 1000.0,
        })
    }

    /// Slope (km per unit intrinsic) of the first (or last) stretch of the table.
    fn end_slope(&self, first: bool) -> f64 {
        let pairs: Vec<_> = self
            .anchors
            .windows(2)
            .filter(|w| w[1].0 > w[0].0)
            .collect();
        let w = if first { pairs.first() } else { pairs.last() };
        match w {
            Some(w) => (w[1].1 - w[0].1) / (w[1].0 - w[0].0),
            None => self.default_slope,
        }
    }

    fn km_at(&self, f: f64) -> f64 {
        let a = &self.anchors;
        let (f0, k0) = a[0];
        if f < f0 {
            return k0 + (f - f0) * self.end_slope(true);
        }
        let i = a.partition_point(|x| x.0 <= f) - 1;
        if i + 1 == a.len() {
            let (fl, kl) = a[i];
            return kl + (f - fl) * self.end_slope(false);
        }
        let ((fa, ka), (fb, kb)) = (a[i], a[i + 1]);
        ka + (kb - ka) * (f - fa) / (fb - fa)
    }

    fn intrinsic_at(&self, km: f64) -> Option<f64> {
        const EPS: f64 = 1e-9;
        let a = &self.anchors;
        let in_range = |f: f64| (-EPS..=1.0 + EPS).contains(&f);
        let (f0, k0) = a[0];
        let s = self.end_slope(true);
        if s != 0.0 {
            let f = f0 + (km - k0) / s;
            if f < f0 && in_range(f) {
                return Some(f.clamp(0.0, 1.0));
            }
        }
        for w in a.windows(2) {
            let ((fa, ka), (fb, kb)) = (w[0], w[1]);
            if fb <= fa {
                continue;
            }
            if (ka.min(kb)..=ka.max(kb)).contains(&km) {
                let f = if kb == ka {
                    fa
                } else {
                    fa + (km - ka) / (kb - ka) * (fb - fa)
                };
                return in_range(f).then(|| f.clamp(0.0, 1.0));
            }
        }
        // Exactly at an anchor (a table of one anchor, or the end of the table).
        if let Some(&(f, _)) = a.iter().find(|x| x.1 == km) {
            return in_range(f).then(|| f.clamp(0.0, 1.0));
        }
        let (fl, kl) = a[a.len() - 1];
        let s = self.end_slope(false);
        if s != 0.0 {
            let f = fl + (km - kl) / s;
            if f > fl && in_range(f) {
                return Some(f.clamp(0.0, 1.0));
            }
        }
        None
    }
}

// --- Results -------------------------------------------------------------------------------

fn lines_result(pieces: Vec<Vec<Coord<f64>>>) -> Option<Geometry<f64>> {
    let mut lines: Vec<LineString<f64>> = Vec::new();
    let mut point = None;
    for p in pieces {
        if p.len() >= 2 {
            lines.push(LineString::new(p));
        } else if let Some(&c) = p.first() {
            point.get_or_insert(c);
        }
    }
    match lines.len() {
        0 => point.map(|c| Geometry::Point(Point(c))),
        1 => lines.pop().map(Geometry::LineString),
        _ => Some(Geometry::MultiLineString(MultiLineString(lines))),
    }
}

/// A local frame in metres around a geometry (equirectangular), or the identity for planar
/// coordinates.
struct LocalFrame {
    c: Coord<f64>,
    kx: f64,
    ky: f64,
}

impl LocalFrame {
    fn around(g: &Geometry<f64>, geographic: bool) -> Option<Self> {
        if !geographic {
            return Some(Self {
                c: Coord { x: 0.0, y: 0.0 },
                kx: 1.0,
                ky: 1.0,
            });
        }
        let c = g.bounding_rect()?.center();
        let ky = EARTH_RADIUS * std::f64::consts::PI / 180.0;
        Some(Self {
            c,
            kx: ky * c.y.to_radians().cos(),
            ky,
        })
    }

    fn fwd(&self, p: Coord<f64>) -> Coord<f64> {
        Coord {
            x: (p.x - self.c.x) * self.kx,
            y: (p.y - self.c.y) * self.ky,
        }
    }

    fn inv(&self, p: Coord<f64>) -> Coord<f64> {
        Coord {
            x: p.x / self.kx + self.c.x,
            y: p.y / self.ky + self.c.y,
        }
    }
}

/// The line between intrinsic coordinates `f1` and `f2`, moved `offset` to its left (right
/// when negative), in metres on geographic coordinates.
fn offset_between(
    line: &Measured,
    crs: &Crs,
    f1: f64,
    f2: f64,
    offset: f64,
) -> Option<Geometry<f64>> {
    let (f1, f2) = if f1 <= f2 { (f1, f2) } else { (f2, f1) };
    let (d1, d2) = (f1 * line.total, f2 * line.total);
    if d1 == d2 || line.total == 0.0 {
        // A position: the point beside the line.
        let (p, dir) = line.point_at(d1);
        if offset == 0.0 {
            return Some(Geometry::Point(Point(p)));
        }
        let frame = LocalFrame::around(&Geometry::Point(Point(p)), crs.is_geographic())?;
        let (q, e) = (frame.fwd(p), frame.fwd(p + dir));
        let n = e - q;
        let len = n.x.hypot(n.y);
        (len > 0.0).then_some(())?;
        let moved = q + Coord {
            x: -n.y / len * offset,
            y: n.x / len * offset,
        };
        return Some(Geometry::Point(Point(frame.inv(moved))));
    }
    let pieces = line.substring(d1, d2);
    if offset == 0.0 {
        return lines_result(pieces);
    }
    let mut out = Vec::new();
    for p in pieces {
        if p.len() < 2 {
            continue;
        }
        let l = LineString::new(p);
        let moved = if crs.is_geographic() {
            let frame = LocalFrame::around(&Geometry::LineString(l.clone()), true)?;
            let local = l.map_coords(|c| frame.fwd(c));
            offset_curve(&local, offset)?.map_coords(|c| frame.inv(c))
        } else {
            offset_curve(&l, offset)?
        };
        out.push(moved.0);
    }
    lines_result(out)
}

// --- Alpha shape ---------------------------------------------------------------------------

/// pgRouting's `pgr_alphaShape`: the union of the Delaunay triangles of the points of `g`
/// whose circumradius is at most `sqrt(alpha)`, in the units of the CRS.
pub(crate) fn alpha_shape(g: &Geometry<f64>, alpha: f64) -> Option<Geometry<f64>> {
    (alpha > 0.0).then_some(())?;
    let mut pts: Vec<Coord<f64>> = g.coords_iter().collect();
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup();
    (pts.len() >= 3).then_some(())?;
    let tris = MultiPoint(pts.into_iter().map(Point).collect())
        .unconstrained_triangulation()
        .ok()?;
    let kept: Vec<Polygon<f64>> = tris
        .into_iter()
        .filter(|t| {
            let (a, b, c) = (t.v1(), t.v2(), t.v3());
            let (ab, bc, ca) = (
                Euclidean.distance(a, b),
                Euclidean.distance(b, c),
                Euclidean.distance(c, a),
            );
            let cross = ((b - a).x * (c - a).y - (b - a).y * (c - a).x).abs();
            cross > 0.0 && {
                let r = ab * bc * ca / (2.0 * cross);
                r * r <= alpha
            }
        })
        .map(|t| t.to_polygon())
        .collect();
    (!kept.is_empty()).then_some(())?;
    let union: geo::MultiPolygon<f64> = geo::algorithm::unary_union(&kept);
    (!union.0.is_empty()).then(|| polygons(union.0))
}

// --- Conflation ----------------------------------------------------------------------------

/// How [`conflate`] measures the distance between two matched lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflateMethod {
    /// Discrete Fréchet distance (respecting direction), on the lines densified to half the
    /// tolerance.
    Frechet,
    /// Hausdorff distance, on the lines densified to half the tolerance.
    Hausdorff,
}

/// A source line matched to a target line.
#[derive(Debug, Clone, PartialEq)]
pub struct ConflateMatch {
    pub source: usize,
    pub target: usize,
    /// Distance between the lines, in metres on geographic coordinates.
    pub distance: f64,
    /// Fraction of the source line's length within `tolerance` of the target.
    pub overlap: f64,
}

fn one_line(g: &Geometry<f64>) -> Option<LineString<f64>> {
    let mut coords: Vec<Coord<f64>> = Vec::new();
    for l in lines_of(g) {
        for c in l.0 {
            if coords.last() != Some(&c) {
                coords.push(c);
            }
        }
    }
    (coords.len() >= 2).then(|| LineString::new(coords))
}

/// `l` with points added so no segment is longer than `step` (or than a thousandth of the line).
fn densify(l: &LineString<f64>, step: f64) -> LineString<f64> {
    let step = step.max(Euclidean.length(l) / 1000.0);
    let mut out = vec![l.0[0]];
    for s in l.lines() {
        let n = (Euclidean.length(&s) / step).ceil().max(1.0) as usize;
        for k in 1..=n {
            out.push(s.start + (s.end - s.start) * (k as f64 / n as f64));
        }
    }
    LineString::new(out)
}

/// Fraction of the length of `a` within `tol` of `b`, sampled at intervals of `tol / 2`.
fn overlap(a: &LineString<f64>, b: &MultiLineString<f64>, tol: f64) -> f64 {
    let len = Euclidean.length(a);
    if len == 0.0 {
        return 0.0;
    }
    let n = ((len / (tol / 2.0)).ceil() as usize).clamp(1, 2000);
    let mut walked = 0.0;
    let mut segs = a.lines().map(|s| (s, Euclidean.length(&s))).peekable();
    let mut within = 0;
    for k in 0..n {
        let d = (k as f64 + 0.5) / n as f64 * len;
        while let Some((_, l)) = segs.peek() {
            if walked + l >= d {
                break;
            }
            walked += l;
            segs.next();
        }
        let Some(&(s, l)) = segs.peek() else { break };
        let t = if l > 0.0 { (d - walked) / l } else { 0.0 };
        let p = Point(s.start + (s.end - s.start) * t);
        if Euclidean.distance(&p, b) <= tol {
            within += 1;
        }
    }
    within as f64 / n as f64
}

/// Matches every source line to the target lines within `tolerance` of it (metres on
/// geographic coordinates): candidates come from an R-tree of the targets' boxes grown by the
/// tolerance; a pair matches when part of the source lies within the tolerance of the target.
pub fn conflate(
    sources: &[Term],
    targets: &[Term],
    tolerance: f64,
    method: ConflateMethod,
) -> Vec<ConflateMatch> {
    use rstar::{AABB, RTree, RTreeObject};
    struct Boxed(usize, AABB<[f64; 2]>);
    impl RTreeObject for Boxed {
        type Envelope = AABB<[f64; 2]>;
        fn envelope(&self) -> Self::Envelope {
            self.1
        }
    }
    if tolerance.is_nan() || tolerance <= 0.0 {
        return Vec::new();
    }
    let parse = |t: &Term| -> Option<(Arc<Geom>, LineString<f64>)> {
        let g = geom(t)?;
        let l = one_line(&g.geometry)?;
        Some((g, l))
    };
    let targets: Vec<Option<(Arc<Geom>, LineString<f64>)>> = targets.iter().map(parse).collect();
    let tree = RTree::bulk_load(
        targets
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let r = t.as_ref()?.1.bounding_rect()?;
                Some(Boxed(
                    i,
                    AABB::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y]),
                ))
            })
            .collect(),
    );
    let mut out = Vec::new();
    for (si, s) in sources.iter().enumerate() {
        let Some((sg, sl)) = parse(s) else { continue };
        let Some(r) = sl.bounding_rect() else {
            continue;
        };
        // The tolerance in the units of the coordinates.
        let (tx, ty) = if sg.crs.is_geographic() {
            let ky = EARTH_RADIUS * std::f64::consts::PI / 180.0;
            let lat = r.max().y.abs().max(r.min().y.abs()).min(89.0);
            (tolerance / (ky * lat.to_radians().cos()), tolerance / ky)
        } else {
            (tolerance, tolerance)
        };
        let query = AABB::from_corners(
            [r.min().x - tx, r.min().y - ty],
            [r.max().x + tx, r.max().y + ty],
        );
        let mut candidates: Vec<usize> = tree
            .locate_in_envelope_intersecting(&query)
            .map(|b| b.0)
            .collect();
        candidates.sort_unstable();
        for ti in candidates {
            let Some((tg, tl)) = &targets[ti] else {
                continue;
            };
            if !same_space(&sg, tg) {
                continue;
            }
            let (a, b) = if sg.crs.is_geographic() {
                let both = Geometry::MultiLineString(MultiLineString(vec![sl.clone(), tl.clone()]));
                let Some(frame) = LocalFrame::around(&both, true) else {
                    continue;
                };
                (
                    sl.map_coords(|c| frame.fwd(c)),
                    tl.map_coords(|c| frame.fwd(c)),
                )
            } else {
                (sl.clone(), tl.clone())
            };
            let frac = overlap(&a, &MultiLineString(vec![b.clone()]), tolerance);
            let (a, b) = (densify(&a, tolerance / 2.0), densify(&b, tolerance / 2.0));
            if frac <= 0.0 {
                continue;
            }
            let distance = match method {
                ConflateMethod::Frechet => Euclidean.frechet_distance(&a, &b),
                ConflateMethod::Hausdorff => a.hausdorff_distance(&b),
            };
            out.push(ConflateMatch {
                source: si,
                target: ti,
                distance,
                overlap: frac,
            });
        }
    }
    out
}

// --- Functions -----------------------------------------------------------------------------

/// The line and, for a second geometry, that geometry in the line's CRS.
fn line_arg(t: &Term) -> Option<(Arc<Geom>, Measured)> {
    let g = geom(t)?;
    let m = Measured::new(&g)?;
    Some((g, m))
}

fn fraction(t: &Term) -> Option<f64> {
    let f = num(t)?;
    (0.0..=1.0).contains(&f).then_some(f)
}

fn out(g: Geometry<f64>, crs: &Crs) -> Option<Term> {
    Some(wkt_literal(&g, crs))
}

pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    add(
        RAIL,
        "locate",
        func(|args| {
            let [l, f] = args else { return None };
            let (g, m) = line_arg(l)?;
            let (p, _) = m.point_at(fraction(f)? * m.total);
            out(Geometry::Point(Point(p)), &g.crs)
        }),
    );
    add(
        RAIL,
        "intrinsicAt",
        func(|args| {
            let [l, p] = args else { return None };
            let (g, m) = line_arg(l)?;
            let p = geom(p)?;
            let p = if same_space(&g, &p) {
                p
            } else {
                Arc::new(crate::crs::transform_geom(&p, &g.crs)?)
            };
            let Geometry::Point(p) = p.geometry else {
                return None;
            };
            (m.total > 0.0).then_some(())?;
            double((m.locate(p.0) / m.total).clamp(0.0, 1.0))
        }),
    );
    add(
        RAIL,
        "kmAt",
        func(|args| {
            let (l, f, table) = match args {
                [l, f] => (l, f, l),
                [l, f, t] => (l, f, t),
                _ => return None,
            };
            let (_, m) = line_arg(l)?;
            let table = KmTable::parse(table, &m)?;
            double(table.km_at(fraction(f)?))
        }),
    );
    add(
        RAIL,
        "intrinsicAtKm",
        func(|args| {
            let (l, km, table) = match args {
                [l, km] => (l, km, l),
                [l, km, t] => (l, km, t),
                _ => return None,
            };
            let (_, m) = line_arg(l)?;
            let table = KmTable::parse(table, &m)?;
            double(table.intrinsic_at(num(km)?)?)
        }),
    );
    add(
        RAIL,
        "geomBetweenKm",
        func(|args| {
            let (l, k1, k2, table) = match args {
                [l, a, b] => (l, a, b, l),
                [l, a, b, t] => (l, a, b, t),
                _ => return None,
            };
            let (g, m) = line_arg(l)?;
            let table = KmTable::parse(table, &m)?;
            let f1 = table.intrinsic_at(num(k1)?)?;
            let f2 = table.intrinsic_at(num(k2)?)?;
            let (f1, f2) = if f1 <= f2 { (f1, f2) } else { (f2, f1) };
            let pieces = m.substring(f1 * m.total, f2 * m.total);
            out(lines_result(pieces)?, &g.crs)
        }),
    );
    add(
        RAIL,
        "offsetGeom",
        func(|args| {
            let [l, f1, f2, d] = args else { return None };
            let (g, m) = line_arg(l)?;
            out(
                offset_between(&m, &g.crs, fraction(f1)?, fraction(f2)?, num(d)?)?,
                &g.crs,
            )
        }),
    );
    add(
        RAIL,
        "alphaShape",
        func(|args| {
            let [g, a] = args else { return None };
            let g = geom(g)?;
            Some(geom_literal(&Geom::new(
                alpha_shape(&g.geometry, num(a)?)?,
                g.crs.clone(),
            )))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::Literal;

    fn wkt(s: &str) -> Term {
        Literal::new_typed_literal(s, WKT_LITERAL).into()
    }

    fn line(s: &str) -> Measured {
        Measured::new(&geom(&wkt(s)).unwrap()).unwrap()
    }

    fn table(s: &str, l: &Measured) -> KmTable {
        KmTable::parse(&Literal::new_simple_literal(s).into(), l).unwrap()
    }

    #[test]
    fn km_table_interpolates_and_extrapolates() {
        let l = line("<http://www.opengis.net/def/crs/EPSG/0/25832> LINESTRING(0 0,1000 0)");
        let t = table("0:12.300 0.5:12.950 1:13.600", &l);
        assert!((t.km_at(0.25) - 12.625).abs() < 1e-9);
        assert!((t.intrinsic_at(13.275).unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(t.intrinsic_at(20.0), None);
        let t = table("0.2:5", &l);
        assert!((t.km_at(0.4) - 5.2).abs() < 1e-9);
        assert!((t.intrinsic_at(4.9).unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn km_table_discontinuity_and_decreasing() {
        let l = line("<http://www.opengis.net/def/crs/EPSG/0/25832> LINESTRING(0 0,1000 0)");
        // A jump from km 10.5 to km 11.0 at the middle.
        let t = table("0:10 0.5:10.5 0.5:11.0 1:11.5", &l);
        assert!((t.km_at(0.5) - 11.0).abs() < 1e-9);
        assert!((t.km_at(0.4) - 10.4).abs() < 1e-9);
        assert_eq!(t.intrinsic_at(10.7), None);
        assert!((t.intrinsic_at(11.25).unwrap() - 0.75).abs() < 1e-9);
        // Decreasing kilometres.
        let t = table("0:5,1:4", &l);
        assert!((t.intrinsic_at(4.25).unwrap() - 0.75).abs() < 1e-9);
        assert!(KmTable::parse(&Literal::new_simple_literal("1:4 0:5").into(), &l).is_none());
    }

    #[test]
    fn alpha_shape_drops_long_triangles() {
        // Two unit squares of points one apart from each other's corner.
        let g = geom(&wkt(
            "MULTIPOINT((0 0),(1 0),(1 1),(0 1),(10 0),(11 0),(11 1),(10 1))",
        ))
        .unwrap();
        let s = alpha_shape(&g.geometry, 1.0).unwrap();
        assert!(
            matches!(s, Geometry::MultiPolygon(ref m) if m.0.len() == 2),
            "{s:?}"
        );
    }
}
