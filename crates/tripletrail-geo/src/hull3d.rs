//! The 3-D convex hull of a point set, built incrementally (Clarkson–Shor) from an initial
//! tetrahedron of extreme points, with Shewchuk's exact `orient3d` (the `robust` crate) for
//! every visibility test: a point is added when it is strictly above at least one face, and
//! the faces it strictly sees always form a connected region, so the hull is exact for the
//! `f64` input points. Coplanar points on a face are left out of the hull's vertices.

use crate::solid::{P3, cross, dot, norm, sub};
use robust::{Coord, Coord3D, orient2d, orient3d};
use std::collections::HashSet;

fn c3(p: P3) -> Coord3D<f64> {
    Coord3D {
        x: p[0],
        y: p[1],
        z: p[2],
    }
}

/// Positive when `d` is below the plane of `a`, `b`, `c` (counter-clockwise seen from above),
/// exactly.
fn orient(a: P3, b: P3, c: P3, d: P3) -> f64 {
    orient3d(c3(a), c3(b), c3(c), c3(d))
}

fn collinear(a: P3, b: P3, c: P3) -> bool {
    let o = |i: usize, j: usize| {
        orient2d(
            Coord { x: a[i], y: a[j] },
            Coord { x: b[i], y: b[j] },
            Coord { x: c[i], y: c[j] },
        )
    };
    o(0, 1) == 0.0 && o(0, 2) == 0.0 && o(1, 2) == 0.0
}

/// The hull of a point set, by dimension.
pub(crate) enum Hull {
    Empty,
    Point(P3),
    /// The two extreme points of collinear input.
    Segment(P3, P3),
    /// The convex polygon (closed ring) of coplanar input.
    Polygon(Vec<P3>),
    /// Triangles with outward normals (counter-clockwise seen from outside).
    Polyhedron(Vec<[P3; 3]>),
}

pub(crate) fn convex_hull(points: &[P3]) -> Hull {
    let mut pts: Vec<P3> = points
        .iter()
        .copied()
        .filter(|p| p.iter().all(|v| v.is_finite()))
        .collect();
    pts.sort_by(|a, b| {
        a[0].total_cmp(&b[0])
            .then(a[1].total_cmp(&b[1]))
            .then(a[2].total_cmp(&b[2]))
    });
    pts.dedup();
    let Some(&p0) = pts.first() else {
        return Hull::Empty;
    };
    // The farthest point from the first, then the point farthest from their line, then the
    // point farthest from their plane: a well-shaped first tetrahedron.
    let dist2 = |a: P3, b: P3| dot(sub(a, b), sub(a, b));
    let Some(i1) = (1..pts.len()).max_by(|&i, &j| dist2(pts[i], p0).total_cmp(&dist2(pts[j], p0)))
    else {
        return Hull::Point(p0);
    };
    let p1 = pts[i1];
    let i2 = (0..pts.len())
        .filter(|&i| !collinear(p0, p1, pts[i]))
        .max_by(|&i, &j| {
            let f = |p: P3| norm(cross(sub(p1, p0), sub(p, p0)));
            f(pts[i]).total_cmp(&f(pts[j]))
        });
    let Some(i2) = i2 else {
        // Collinear: the extremes along the line.
        let d = sub(p1, p0);
        let lo = pts
            .iter()
            .min_by(|a, b| dot(**a, d).total_cmp(&dot(**b, d)))
            .copied()
            .unwrap_or(p0);
        let hi = pts
            .iter()
            .max_by(|a, b| dot(**a, d).total_cmp(&dot(**b, d)))
            .copied()
            .unwrap_or(p1);
        return Hull::Segment(lo, hi);
    };
    let p2 = pts[i2];
    let i3 = (0..pts.len())
        .filter(|&i| orient(p0, p1, p2, pts[i]) != 0.0)
        .max_by(|&i, &j| {
            orient(p0, p1, p2, pts[i])
                .abs()
                .total_cmp(&orient(p0, p1, p2, pts[j]).abs())
        });
    let Some(i3) = i3 else {
        return Hull::Polygon(planar_hull(&pts, cross(sub(p1, p0), sub(p2, p0))));
    };
    let p3 = pts[i3];
    let mut faces: Vec<[P3; 3]> = if orient(p0, p1, p2, p3) > 0.0 {
        vec![[p0, p1, p2], [p0, p3, p1], [p1, p3, p2], [p2, p3, p0]]
    } else {
        vec![[p0, p2, p1], [p0, p1, p3], [p1, p2, p3], [p2, p0, p3]]
    };
    let key = |p: P3| [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()];
    for (i, &p) in pts.iter().enumerate() {
        if i == 0 || i == i1 || i == i2 || i == i3 {
            continue;
        }
        let visible: Vec<bool> = faces
            .iter()
            .map(|f| orient(f[0], f[1], f[2], p) < 0.0)
            .collect();
        if !visible.contains(&true) {
            continue;
        }
        let mut edges = HashSet::new();
        for (f, _) in faces.iter().zip(&visible).filter(|(_, v)| **v) {
            for k in 0..3 {
                edges.insert((key(f[k]), key(f[(k + 1) % 3])));
            }
        }
        let mut next = Vec::with_capacity(faces.len() + 4);
        let mut horizon = Vec::new();
        for (f, v) in faces.into_iter().zip(visible) {
            if v {
                for k in 0..3 {
                    let (a, b) = (f[k], f[(k + 1) % 3]);
                    if !edges.contains(&(key(b), key(a))) {
                        horizon.push((a, b));
                    }
                }
            } else {
                next.push(f);
            }
        }
        next.extend(horizon.into_iter().map(|(a, b)| [a, b, p]));
        faces = next;
    }
    Hull::Polyhedron(clean(faces))
}

/// The hull's faces as triangles over its extreme points only: coplanar facets (found with
/// exact `orient3d`) are merged into one convex face, whose points on edges or inside
/// (exact `orient2d`) are dropped, and the face is fanned out again.
fn clean(facets: Vec<[P3; 3]>) -> Vec<[P3; 3]> {
    let mut groups: Vec<Vec<[P3; 3]>> = Vec::new();
    for f in facets {
        let n = cross(sub(f[1], f[0]), sub(f[2], f[0]));
        match groups.iter_mut().find(|g| {
            let h = g[0];
            dot(cross(sub(h[1], h[0]), sub(h[2], h[0])), n) > 0.0
                && f.iter().all(|&p| orient(h[0], h[1], h[2], p) == 0.0)
        }) {
            Some(g) => g.push(f),
            None => groups.push(vec![f]),
        }
    }
    let mut out = Vec::new();
    for g in groups {
        if g.len() == 1 {
            out.push(g[0]);
            continue;
        }
        let n = cross(sub(g[0][1], g[0][0]), sub(g[0][2], g[0][0]));
        let pts: Vec<P3> = g.iter().flatten().copied().collect();
        let ring = planar_hull(&pts, n);
        let corners = &ring[..ring.len().saturating_sub(1)];
        for k in 1..corners.len().saturating_sub(1) {
            out.push([corners[0], corners[k], corners[k + 1]]);
        }
    }
    out
}

/// The convex polygon of coplanar points with plane normal `n` (closed, counter-clockwise
/// seen along `n`).
fn planar_hull(pts: &[P3], n: P3) -> Vec<P3> {
    use geo::{ConvexHull, MultiPoint, Point};
    use std::collections::HashMap;
    let (k, _) = crate::solid::projection(n);
    let (i, j) = crate::solid::kept_axes(k);
    let mut lift = HashMap::new();
    let mp = MultiPoint(
        pts.iter()
            .map(|p| {
                lift.insert((p[i].to_bits(), p[j].to_bits()), *p);
                Point::new(p[i], p[j])
            })
            .collect(),
    );
    let hull = mp.convex_hull();
    let mut ring: Vec<P3> = hull
        .exterior()
        .0
        .iter()
        .filter_map(|c| lift.get(&(c.x.to_bits(), c.y.to_bits())).copied())
        .collect();
    // Drop points on the hull's edges.
    ring.pop();
    let turn = |a: P3, b: P3, c: P3| {
        orient2d(
            Coord { x: a[i], y: a[j] },
            Coord { x: b[i], y: b[j] },
            Coord { x: c[i], y: c[j] },
        )
    };
    let mut changed = true;
    while changed && ring.len() > 3 {
        changed = false;
        for k in 0..ring.len() {
            let m = ring.len();
            if turn(ring[(k + m - 1) % m], ring[k], ring[(k + 1) % m]) == 0.0 {
                ring.remove(k);
                changed = true;
                break;
            }
        }
    }
    if let Some(&first) = ring.first() {
        ring.push(first);
    }
    if dot(crate::solid::ring_normal(&ring), n) < 0.0 {
        ring.reverse();
    }
    ring
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_hull() {
        let mut pts = Vec::new();
        for x in [0.0, 1.0] {
            for y in [0.0, 1.0] {
                for z in [0.0, 1.0] {
                    pts.push([x, y, z]);
                }
            }
        }
        pts.push([0.5, 0.5, 0.5]);
        pts.push([0.5, 0.5, 0.0]);
        println!(
            "{}",
            orient([0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0])
        );
        let Hull::Polyhedron(t) = convex_hull(&pts) else {
            panic!()
        };
        for f in &t {
            println!("{f:?}");
        }
        assert_eq!(t.len(), 12);
    }
}
