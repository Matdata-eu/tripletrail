//! Intersection, union, difference and symmetric difference of geometries of any dimension.
//!
//! Polygons against polygons go through `geo`'s boolean operations. A line or a point against
//! a polygon, a line or a point follows the OGC / PostGIS semantics: the intersection has the
//! dimension of the lower-dimensional input, a difference keeps the parts of the first input
//! outside the second, and a union or symmetric difference of inputs of different dimensions is
//! a collection of the higher-dimensional input and the parts of the other outside it.

use crate::fast::{Position, Prepared, position_on_line};
use crate::literal::Geom;
use geo::line_intersection::{LineIntersection, line_intersection};
use geo::{BooleanOps, Coord, Geometry, GeometryCollection, LineString};
use geo::{MultiLineString, MultiPoint, MultiPolygon, Point};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetOp {
    Intersection,
    Union,
    Difference,
    SymDifference,
}

/// A geometry split by dimension. Only one of the three is used per input.
enum Parts {
    Points(Vec<Point<f64>>),
    Lines(MultiLineString<f64>),
    Areas(MultiPolygon<f64>),
}

fn parts(g: &Geometry<f64>) -> Option<Parts> {
    Some(match g {
        Geometry::Point(p) => Parts::Points(vec![*p]),
        Geometry::MultiPoint(m) => Parts::Points(m.0.clone()),
        Geometry::Line(l) => Parts::Lines(MultiLineString(vec![LineString(vec![l.start, l.end])])),
        Geometry::LineString(l) => Parts::Lines(MultiLineString(vec![l.clone()])),
        Geometry::MultiLineString(m) => Parts::Lines(m.clone()),
        Geometry::Polygon(p) => Parts::Areas(MultiPolygon(vec![p.clone()])),
        Geometry::MultiPolygon(m) => Parts::Areas(m.clone()),
        Geometry::Rect(r) => Parts::Areas(MultiPolygon(vec![r.to_polygon()])),
        Geometry::Triangle(t) => Parts::Areas(MultiPolygon(vec![t.to_polygon()])),
        Geometry::GeometryCollection(c) if c.0.is_empty() => Parts::Points(Vec::new()),
        _ => return None,
    })
}

/// The pieces of a result, written as the simplest geometry that holds them.
#[derive(Default)]
struct Out {
    points: Vec<Point<f64>>,
    lines: Vec<LineString<f64>>,
    areas: Vec<geo::Polygon<f64>>,
}

impl Out {
    /// `empty` is the dimension of an empty result: 0, 1 or 2.
    fn geometry(mut self, empty: u8) -> Geometry<f64> {
        let mut seen = Vec::new();
        self.points.retain(|p| {
            let fresh = !seen.contains(p);
            seen.push(*p);
            fresh
        });
        let kinds = usize::from(!self.points.is_empty())
            + usize::from(!self.lines.is_empty())
            + usize::from(!self.areas.is_empty());
        if kinds > 1 {
            let mut c = Vec::new();
            if !self.areas.is_empty() {
                c.push(Geometry::MultiPolygon(MultiPolygon(self.areas)));
            }
            if !self.lines.is_empty() {
                c.push(lines_geometry(self.lines));
            }
            if !self.points.is_empty() {
                c.push(points_geometry(self.points));
            }
            return Geometry::GeometryCollection(GeometryCollection(c));
        }
        if !self.areas.is_empty() || (kinds == 0 && empty == 2) {
            return Geometry::MultiPolygon(MultiPolygon(self.areas));
        }
        if !self.lines.is_empty() {
            return lines_geometry(self.lines);
        }
        if !self.points.is_empty() {
            return points_geometry(self.points);
        }
        match empty {
            1 => Geometry::MultiLineString(MultiLineString(Vec::new())),
            _ => Geometry::MultiPoint(MultiPoint(Vec::new())),
        }
    }
}

/// The result of a boolean operation on areas, cleaned up. `geo`'s boolean operations work
/// on integers scaled to the inputs' extent, which moves a vertex by up to about
/// extent / 2^31 (`-83.6` came back as `-83.60000000018627`): each ordinate that close to an
/// ordinate of an input is put back on it. Shells are then clockwise and holes
/// counter-clockwise, as GEOS and JTS write them, and a single polygon is a `POLYGON`.
fn tidy_areas(
    mut m: MultiPolygon<f64>,
    a: &MultiPolygon<f64>,
    b: &MultiPolygon<f64>,
) -> Geometry<f64> {
    // The inputs' ordinates and their extent.
    let rings = || {
        a.0.iter()
            .chain(&b.0)
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
    };
    let n: usize = rings().map(|r| r.0.len()).sum();
    let (mut xs, mut ys) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for r in rings() {
        for c in &r.0 {
            xs.push(c.x);
            ys.push(c.y);
            lo = [lo[0].min(c.x), lo[1].min(c.y)];
            hi = [hi[0].max(c.x), hi[1].max(c.y)];
        }
    }
    let tolerance = (hi[0] - lo[0]).max(hi[1] - lo[1]).max(0.0) * 1e-8;
    // Few output vertices against few input ones: a scan per ordinate is cheaper than
    // sorting the inputs for a binary search.
    let out: usize =
        m.0.iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .map(|r| r.0.len())
            .sum();
    let sorted = out * n > 1024;
    if sorted {
        xs.sort_unstable_by(f64::total_cmp);
        ys.sort_unstable_by(f64::total_cmp);
    }
    let snap = |v: &mut f64, inputs: &[f64]| {
        let nearest = if sorted {
            let i = inputs.partition_point(|&x| x < *v);
            [i.wrapping_sub(1), i]
                .into_iter()
                .filter_map(|k| inputs.get(k))
                .fold((f64::INFINITY, *v), |best, &x| {
                    let d = (x - *v).abs();
                    if d < best.0 { (d, x) } else { best }
                })
        } else {
            inputs.iter().fold((f64::INFINITY, *v), |best, &x| {
                let d = (x - *v).abs();
                if d < best.0 { (d, x) } else { best }
            })
        };
        if nearest.0 <= tolerance {
            *v = nearest.1;
        }
    };
    // Snapped, without repeated vertices, wound clockwise (`cw`) or counter-clockwise.
    let tidy = |r: &mut LineString<f64>, cw: bool| {
        for c in &mut r.0 {
            snap(&mut c.x, &xs);
            snap(&mut c.y, &ys);
        }
        r.0.dedup();
        // Twice the signed area (shoelace): positive for counter-clockwise.
        let area: f64 =
            r.0.windows(2)
                .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
                .sum();
        if (area > 0.0) == cw {
            r.0.reverse();
        }
    };
    for p in &mut m.0 {
        p.exterior_mut(|r| tidy(r, true));
        p.interiors_mut(|rings| {
            for r in rings.iter_mut() {
                tidy(r, false);
            }
        });
        if p.interiors().iter().any(|r| r.0.len() < 4) {
            let empty = geo::Polygon::new(LineString(Vec::new()), Vec::new());
            let (exterior, mut interiors) = std::mem::replace(p, empty).into_inner();
            interiors.retain(|r| r.0.len() >= 4);
            *p = geo::Polygon::new(exterior, interiors);
        }
    }
    m.0.retain(|p| p.exterior().0.len() >= 4);
    if m.0.len() == 1 {
        Geometry::Polygon(m.0.pop().expect("one polygon"))
    } else {
        Geometry::MultiPolygon(m)
    }
}

fn lines_geometry(mut lines: Vec<LineString<f64>>) -> Geometry<f64> {
    if lines.len() == 1 {
        Geometry::LineString(lines.pop().unwrap())
    } else {
        Geometry::MultiLineString(MultiLineString(lines))
    }
}

fn points_geometry(mut points: Vec<Point<f64>>) -> Geometry<f64> {
    if points.len() == 1 {
        Geometry::Point(points.pop().unwrap())
    } else {
        Geometry::MultiPoint(MultiPoint(points))
    }
}

fn point_in_areas(prepared: Option<&Prepared>, p: Point<f64>) -> bool {
    prepared.is_some_and(|a| a.position(p.0) != Position::Exterior)
}

fn point_on_lines(lines: &MultiLineString<f64>, p: Point<f64>) -> bool {
    lines
        .0
        .iter()
        .any(|l| position_on_line(l, p.0) != Position::Exterior)
}

/// The parts of `a` along `b`'s segments (`inside`), or off them: each segment of `a` is cut
/// where it overlaps a segment of `b` collinearly. Crossing points do not cut it.
fn line_overlaps(
    a: &MultiLineString<f64>,
    b: &MultiLineString<f64>,
    inside: bool,
) -> Vec<LineString<f64>> {
    let mut out: Vec<LineString<f64>> = Vec::new();
    let mut current: Vec<Coord<f64>> = Vec::new();
    let flush = |current: &mut Vec<Coord<f64>>, out: &mut Vec<LineString<f64>>| {
        if current.len() >= 2 {
            out.push(LineString(std::mem::take(current)));
        }
        current.clear();
    };
    for l in &a.0 {
        for s in l.lines() {
            let len2 = (s.end.x - s.start.x).powi(2) + (s.end.y - s.start.y).powi(2);
            if len2 == 0.0 {
                continue;
            }
            let along = |c: Coord<f64>| {
                ((c.x - s.start.x) * (s.end.x - s.start.x)
                    + (c.y - s.start.y) * (s.end.y - s.start.y))
                    / len2
            };
            // Intervals of the segment (as fractions) covered by b.
            let mut covered: Vec<(f64, f64)> =
                b.0.iter()
                    .flat_map(|m| m.lines())
                    .filter_map(|t| match line_intersection(s, t)? {
                        LineIntersection::Collinear { intersection } => {
                            let (u, v) = (along(intersection.start), along(intersection.end));
                            Some((u.min(v).max(0.0), u.max(v).min(1.0)))
                        }
                        LineIntersection::SinglePoint { .. } => None,
                    })
                    .collect();
            covered.sort_by(|x, y| x.0.total_cmp(&y.0));
            let mut merged: Vec<(f64, f64)> = Vec::new();
            for (u, v) in covered {
                match merged.last_mut() {
                    Some(last) if u <= last.1 => last.1 = last.1.max(v),
                    _ => merged.push((u, v)),
                }
            }
            let pieces: Vec<(f64, f64)> = if inside {
                merged
            } else {
                let mut free = Vec::new();
                let mut at = 0.0;
                for (u, v) in merged {
                    if u > at {
                        free.push((at, u));
                    }
                    at = at.max(v);
                }
                if at < 1.0 {
                    free.push((at, 1.0));
                }
                free
            };
            let point = |t: f64| Coord {
                x: s.start.x + (s.end.x - s.start.x) * t,
                y: s.start.y + (s.end.y - s.start.y) * t,
            };
            for (u, v) in pieces {
                if v <= u {
                    continue;
                }
                let (p, q) = (point(u), point(v));
                if current.last() != Some(&p) {
                    flush(&mut current, &mut out);
                    current.push(p);
                }
                current.push(q);
            }
        }
        flush(&mut current, &mut out);
    }
    out
}

/// Crossing points of two sets of lines, without the points on their shared segments.
fn crossings(a: &MultiLineString<f64>, b: &MultiLineString<f64>) -> Vec<Point<f64>> {
    let mut out = Vec::new();
    for s in a.0.iter().flat_map(|l| l.lines()) {
        for t in b.0.iter().flat_map(|l| l.lines()) {
            if let Some(LineIntersection::SinglePoint { intersection, .. }) =
                line_intersection(s, t)
            {
                out.push(Point(intersection));
            }
        }
    }
    out
}

fn on_segments(lines: &[LineString<f64>], p: Point<f64>) -> bool {
    lines
        .iter()
        .any(|l| position_on_line(l, p.0) != Position::Exterior)
}

/// The lines inside the polygon `area`, and those outside it, clipping only the lines that
/// cross its boundary: most lines of a network lie wholly inside one region.
fn clip(
    area: &Geom,
    m: &MultiPolygon<f64>,
    lines: &MultiLineString<f64>,
) -> (Vec<LineString<f64>>, Vec<LineString<f64>>) {
    let Some(p) = area.prepared() else {
        return (m.clip(lines, false).0, m.clip(lines, true).0);
    };
    let (mut inside, mut outside, mut crossing) = (Vec::new(), Vec::new(), Vec::new());
    for l in &lines.0 {
        let crosses = l.lines().any(|s| p.segment_meets_edge(s));
        match l.0.first().map(|c| p.position(*c)) {
            _ if crosses => crossing.push(l.clone()),
            Some(Position::Interior) => inside.push(l.clone()),
            Some(Position::Exterior) => outside.push(l.clone()),
            _ => crossing.push(l.clone()),
        }
    }
    if !crossing.is_empty() {
        let crossing = MultiLineString(crossing);
        inside.extend(m.clip(&crossing, false).0);
        outside.extend(m.clip(&crossing, true).0);
    }
    (inside, outside)
}

/// `a op b`, or `None` for inputs this does not cover (a collection of mixed dimensions,
/// curves, or a polygon operation `geo` cannot do).
pub(crate) fn set_operation(op: SetOp, ga: &Geom, gb: &Geom) -> Option<Geometry<f64>> {
    let (a, b) = (&ga.geometry, &gb.geometry);
    let (pa, pb) = (parts(a)?, parts(b)?);
    let prepared = |g: &Geometry<f64>| Prepared::new(g);
    let mut out = Out::default();
    use Parts::{Areas, Lines, Points};
    use SetOp::*;
    // Dimension of an empty result.
    let dim = |p: &Parts| match p {
        Points(_) => 0u8,
        Lines(_) => 1,
        Areas(_) => 2,
    };
    let empty = match op {
        Intersection => dim(&pa).min(dim(&pb)),
        Difference => dim(&pa),
        _ => dim(&pa).max(dim(&pb)),
    };
    match (op, &pa, &pb) {
        (_, Areas(x), Areas(y)) => {
            let m = match op {
                Intersection => x.intersection(y),
                Union => x.union(y),
                Difference => x.difference(y),
                SymDifference => x.xor(y),
            };
            return Some(tidy_areas(m, x, y));
        }
        // Line against area.
        (Difference, Areas(m), Lines(_) | Points(_)) => out.areas = m.0.clone(),
        (_, Lines(l), Areas(m)) | (_, Areas(m), Lines(l)) => {
            let area = if matches!(pa, Areas(_)) { ga } else { gb };
            let (inside, outside) = clip(area, m, l);
            match op {
                Intersection => out.lines = inside,
                Difference => out.lines = outside,
                _ => {
                    out.areas = m.0.clone();
                    out.lines = outside;
                }
            }
        }
        // Point against area.
        (_, Points(p), Areas(_)) | (_, Areas(_), Points(p)) => {
            let area = if matches!(pa, Areas(_)) { a } else { b };
            let prepared = prepared(area);
            let (inside, outside): (Vec<Point<f64>>, Vec<Point<f64>>) = p
                .iter()
                .partition(|q| point_in_areas(prepared.as_ref(), **q));
            match op {
                Intersection => out.points = inside,
                Difference => out.points = outside, // points minus area (area minus points is above)
                _ => {
                    if let Areas(m) = if matches!(pa, Areas(_)) { &pa } else { &pb } {
                        out.areas = m.0.clone();
                    }
                    out.points = outside;
                }
            }
        }
        // Lines against lines.
        (Intersection, Lines(x), Lines(y)) => {
            out.lines = line_overlaps(x, y, true);
            out.points = crossings(x, y)
                .into_iter()
                .filter(|p| !on_segments(&out.lines, *p))
                .collect();
        }
        (Difference, Lines(x), Lines(y)) => out.lines = line_overlaps(x, y, false),
        (Union, Lines(x), Lines(y)) => {
            out.lines = x.0.clone();
            out.lines.extend(line_overlaps(y, x, false));
        }
        (SymDifference, Lines(x), Lines(y)) => {
            out.lines = line_overlaps(x, y, false);
            out.lines.extend(line_overlaps(y, x, false));
        }
        // Points against lines.
        (_, Points(p), Lines(l)) | (_, Lines(l), Points(p)) => {
            let (on, off): (Vec<Point<f64>>, Vec<Point<f64>>) =
                p.iter().partition(|q| point_on_lines(l, **q));
            match op {
                Intersection => out.points = on,
                Difference if matches!(pa, Points(_)) => out.points = off,
                Difference => out.lines = l.0.clone(),
                _ => {
                    out.lines = l.0.clone();
                    out.points = off;
                }
            }
        }
        // Points against points.
        (_, Points(x), Points(y)) => {
            let (in_y, not_y): (Vec<Point<f64>>, Vec<Point<f64>>) =
                x.iter().partition(|p| y.contains(p));
            out.points = match op {
                Intersection => in_y,
                Difference => not_y,
                Union => x.iter().chain(y).copied().collect(),
                SymDifference => not_y
                    .into_iter()
                    .chain(y.iter().filter(|p| !x.contains(p)).copied())
                    .collect(),
            };
        }
    }
    Some(out.geometry(empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn g(w: &str) -> Geom {
        Geom::new(
            Geometry::try_from(wkt::Wkt::<f64>::from_str(w).unwrap()).unwrap(),
            crate::literal::Crs::Crs84,
        )
    }

    fn text(g: &Geometry<f64>) -> String {
        wkt::ToWkt::wkt_string(g)
    }

    #[test]
    fn line_and_polygon() {
        let square = g("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))");
        let line = g("LINESTRING(-5 5, 15 5)");
        let i = set_operation(SetOp::Intersection, &line, &square).unwrap();
        assert_eq!(text(&i), "LINESTRING(0 5,10 5)");
        let i = set_operation(SetOp::Intersection, &square, &line).unwrap();
        assert_eq!(text(&i), "LINESTRING(0 5,10 5)");
        let d = set_operation(SetOp::Difference, &line, &square).unwrap();
        assert_eq!(text(&d), "MULTILINESTRING((-5 5,0 5),(10 5,15 5))");
        let d = set_operation(SetOp::Difference, &square, &line).unwrap();
        assert!(matches!(d, Geometry::MultiPolygon(_)));
        let inside = g("LINESTRING(1 1, 2 2, 3 1)");
        let i = set_operation(SetOp::Intersection, &inside, &square).unwrap();
        assert_eq!(text(&i), "LINESTRING(1 1,2 2,3 1)");
        let d = set_operation(SetOp::Difference, &inside, &square).unwrap();
        assert_eq!(text(&d), "MULTILINESTRING EMPTY");
        let u = set_operation(SetOp::Union, &line, &square).unwrap();
        assert!(
            matches!(u, Geometry::GeometryCollection(ref c) if c.0.len() == 2),
            "{}",
            text(&u)
        );
        let away = g("LINESTRING(20 20, 30 30)");
        let i = set_operation(SetOp::Intersection, &away, &square).unwrap();
        assert_eq!(text(&i), "MULTILINESTRING EMPTY");
    }

    #[test]
    fn point_cases() {
        let square = g("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))");
        let i = set_operation(SetOp::Intersection, &g("POINT(5 5)"), &square).unwrap();
        assert_eq!(text(&i), "POINT(5 5)");
        let i = set_operation(SetOp::Intersection, &g("POINT(50 5)"), &square).unwrap();
        assert_eq!(text(&i), "MULTIPOINT EMPTY");
        let d = set_operation(SetOp::Difference, &g("MULTIPOINT((5 5),(50 5))"), &square).unwrap();
        assert_eq!(text(&d), "POINT(50 5)");
        let line = g("LINESTRING(0 0, 10 0)");
        let i = set_operation(SetOp::Intersection, &line, &g("POINT(5 0)")).unwrap();
        assert_eq!(text(&i), "POINT(5 0)");
        let u = set_operation(SetOp::Union, &g("POINT(1 1)"), &g("POINT(2 2)")).unwrap();
        assert_eq!(text(&u), "MULTIPOINT((1 1),(2 2))");
    }

    #[test]
    fn lines_and_lines() {
        let a = g("LINESTRING(0 0, 10 0)");
        let b = g("LINESTRING(5 0, 15 0)");
        let i = set_operation(SetOp::Intersection, &a, &b).unwrap();
        assert_eq!(text(&i), "LINESTRING(5 0,10 0)");
        let d = set_operation(SetOp::Difference, &a, &b).unwrap();
        assert_eq!(text(&d), "LINESTRING(0 0,5 0)");
        let cross = g("LINESTRING(3 -1, 3 1)");
        let i = set_operation(SetOp::Intersection, &a, &cross).unwrap();
        assert_eq!(text(&i), "POINT(3 0)");
        let d = set_operation(SetOp::Difference, &a, &cross).unwrap();
        assert_eq!(text(&d), "LINESTRING(0 0,10 0)");
        let s = set_operation(SetOp::SymDifference, &a, &b).unwrap();
        assert_eq!(text(&s), "MULTILINESTRING((0 0,5 0),(10 0,15 0))");
    }
}
