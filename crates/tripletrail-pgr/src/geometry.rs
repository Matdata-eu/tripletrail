//! Line geometry for routes and topology: edge geometries oriented in traversal direction,
//! parts of edges, whole-route geometries and hulls. Lengths and fractions are measured in
//! the coordinates' own units (planar), as pgRouting's `fraction` is.

use geo::{Coord, Geometry, LineString, MultiLineString, MultiPoint, Point};
use oxigraph::model::Term;
use tripletrail_geo::{Crs, parse_wkt_literal, wkt_literal};

/// The coordinates of a line literal (a `LineString`, or a `MultiLineString` whose parts are
/// joined in order) and its CRS.
pub(crate) fn line(term: &Term) -> Option<(Vec<Coord>, Crs)> {
    let g = parse_wkt_literal(term)?;
    let coords = match &g.geometry {
        Geometry::LineString(l) => l.0.clone(),
        Geometry::MultiLineString(m) => {
            let mut out: Vec<Coord> = Vec::new();
            for part in &m.0 {
                let skip = usize::from(out.last().is_some() && out.last() == part.0.first());
                out.extend_from_slice(&part.0[skip..]);
            }
            out
        }
        Geometry::Line(l) => vec![l.start, l.end],
        _ => return None,
    };
    (coords.len() >= 2).then(|| (coords, g.crs.clone()))
}

fn dist(a: Coord, b: Coord) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

pub(crate) fn length(coords: &[Coord]) -> f64 {
    coords.windows(2).map(|w| dist(w[0], w[1])).sum()
}

/// The point at `fraction` (0 at the start, 1 at the end) of the line's length.
pub(crate) fn interpolate(coords: &[Coord], fraction: f64) -> Coord {
    at_distance(coords, fraction.clamp(0.0, 1.0) * length(coords)).0
}

/// The point `d` along the line, and the index of the segment it lies on.
fn at_distance(coords: &[Coord], d: f64) -> (Coord, usize) {
    let mut walked = 0.0;
    for (i, w) in coords.windows(2).enumerate() {
        let l = dist(w[0], w[1]);
        if walked + l >= d && l > 0.0 {
            let t = ((d - walked) / l).clamp(0.0, 1.0);
            return (
                Coord {
                    x: w[0].x + (w[1].x - w[0].x) * t,
                    y: w[0].y + (w[1].y - w[0].y) * t,
                },
                i,
            );
        }
        walked += l;
    }
    (*coords.last().unwrap(), coords.len().saturating_sub(2))
}

/// The part of a line between two fractions of its length, running from `from` to `to`
/// (reversed when `from > to`).
pub(crate) fn substring(coords: &[Coord], from: f64, to: f64) -> Vec<Coord> {
    let (lo, hi) = (from.min(to).clamp(0.0, 1.0), from.max(to).clamp(0.0, 1.0));
    let mut out = if lo <= 0.0 && hi >= 1.0 {
        coords.to_vec()
    } else {
        let total = length(coords);
        between(coords, lo * total, hi * total)
    };
    if from > to {
        out.reverse();
    }
    out
}

/// The part of a line between distances `a <= b` along it.
pub(crate) fn between(coords: &[Coord], a: f64, b: f64) -> Vec<Coord> {
    let (start, i) = at_distance(coords, a);
    let (end, j) = at_distance(coords, b);
    let mut out = vec![start];
    for &c in &coords[i + 1..=j] {
        if out.last() != Some(&c) {
            out.push(c);
        }
    }
    if out.last() != Some(&end) || out.len() == 1 {
        out.push(end);
    }
    out
}

/// Joins line parts in order into one `LineString`, or a `MultiLineString` where a part
/// does not start where the previous one ended.
pub(crate) fn merge(parts: impl IntoIterator<Item = Vec<Coord>>) -> Option<Geometry> {
    let mut lines: Vec<Vec<Coord>> = Vec::new();
    for part in parts {
        if part.len() < 2 {
            continue;
        }
        match lines.last_mut() {
            Some(last) if last.last() == part.first() => last.extend_from_slice(&part[1..]),
            _ => lines.push(part),
        }
    }
    match lines.len() {
        0 => None,
        1 => Some(LineString(lines.pop().unwrap()).into()),
        _ => Some(MultiLineString(lines.into_iter().map(LineString).collect()).into()),
    }
}

/// A geometry literal.
pub(crate) fn literal(g: &Geometry, crs: &Crs) -> Term {
    wkt_literal(g, crs)
}

pub(crate) fn line_literal(coords: Vec<Coord>, crs: &Crs) -> Term {
    literal(&LineString(coords).into(), crs)
}

/// The concave hull of points (geo's port of concaveman; `concavity` 2 is fairly detailed,
/// infinity gives the convex hull). One point gives a point, two a line.
pub(crate) fn hull(points: &[Coord], concavity: f64) -> Option<Geometry> {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup();
    Some(match pts.len() {
        0 => return None,
        1 => Point(pts[0]).into(),
        2 => LineString(pts).into(),
        _ => {
            use geo::ConcaveHull;
            use geo::concave_hull::ConcaveHullOptions;
            let mp = MultiPoint(pts.into_iter().map(Point).collect());
            mp.concave_hull_with_options(ConcaveHullOptions {
                concavity,
                length_threshold: 0.0,
            })
            .into()
        }
    })
}

/// The point of segment `a`-`b` closest to `p`, and its parameter along the segment.
pub(crate) fn project(p: Coord, a: Coord, b: Coord) -> (Coord, f64) {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let l2 = dx * dx + dy * dy;
    let t = if l2 == 0.0 {
        0.0
    } else {
        (((p.x - a.x) * dx + (p.y - a.y) * dy) / l2).clamp(0.0, 1.0)
    };
    (
        Coord {
            x: a.x + dx * t,
            y: a.y + dy * t,
        },
        t,
    )
}

/// The point of a line closest to `p`: the point, its distance along the line, the distance
/// to `p`, and on which side of the line `p` lies (`'l'`, `'r'`, or `'b'` on it).
pub(crate) fn locate(coords: &[Coord], p: Coord) -> (Coord, f64, f64, char) {
    let mut best = (coords[0], 0.0, f64::INFINITY, 'b');
    let mut walked = 0.0;
    for w in coords.windows(2) {
        let (q, t) = project(p, w[0], w[1]);
        let d = dist(p, q);
        let l = dist(w[0], w[1]);
        if d < best.2 {
            let cross = (w[1].x - w[0].x) * (p.y - w[0].y) - (w[1].y - w[0].y) * (p.x - w[0].x);
            let side = if d == 0.0 || cross == 0.0 {
                'b'
            } else if cross > 0.0 {
                'l'
            } else {
                'r'
            };
            best = (q, walked + t * l, d, side);
        }
        walked += l;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: f64, y: f64) -> Coord {
        Coord { x, y }
    }

    #[test]
    fn substrings_and_merges() {
        let l = [c(0.0, 0.0), c(2.0, 0.0), c(2.0, 2.0)];
        assert_eq!(substring(&l, 0.0, 1.0), l);
        assert_eq!(
            substring(&l, 1.0, 0.0),
            [c(2.0, 2.0), c(2.0, 0.0), c(0.0, 0.0)]
        );
        assert_eq!(
            substring(&l, 0.25, 0.75),
            [c(1.0, 0.0), c(2.0, 0.0), c(2.0, 1.0)]
        );
        assert_eq!(substring(&l, 0.75, 0.5), [c(2.0, 1.0), c(2.0, 0.0)]);
        assert_eq!(interpolate(&l, 0.5), c(2.0, 0.0));
        let merged = merge([
            vec![c(0.0, 0.0), c(1.0, 0.0)],
            vec![c(1.0, 0.0), c(2.0, 0.0)],
            vec![c(5.0, 0.0), c(6.0, 0.0)],
        ])
        .unwrap();
        assert!(
            matches!(merged, Geometry::MultiLineString(ref m) if m.0.len() == 2 && m.0[0].0.len() == 3)
        );
        let (q, along, d, side) = locate(&l, c(1.0, 1.0));
        assert_eq!((q, along, d, side), (c(1.0, 0.0), 1.0, 1.0, 'l'));
        assert_eq!(locate(&l, c(3.0, 1.0)).3, 'r');
    }
}
