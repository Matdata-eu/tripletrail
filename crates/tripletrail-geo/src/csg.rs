//! Boolean operations on closed polyhedra with BSP trees (the csg.js algorithm of Evan
//! Wallace), and clipping of surfaces, lines and points by a solid.
//!
//! Predicates are epsilon-robust, not exact: a vertex is on a plane when its distance to it is
//! at most [`Csg::eps`], `1e-9` times the largest absolute ordinate of the operands (at least
//! `1e-9`). Vertices created where an edge crosses a plane are rounded to `f64`. The trees are
//! held in arenas and walked with work lists, so deep trees do not overflow the stack.

use crate::solid::{
    P3, Patch, area_vector, cross, dot, is_convex, kept_axes, norm, projection, scale, sub,
    triangulate,
};

#[derive(Debug, Clone, Copy)]
struct Plane {
    n: P3,
    w: f64,
}

impl Plane {
    fn flip(self) -> Self {
        Self {
            n: scale(self.n, -1.0),
            w: -self.w,
        }
    }
}

/// A convex planar polygon, with the plane of the input polygon it comes from.
#[derive(Debug, Clone)]
struct Poly {
    v: Vec<P3>,
    plane: Plane,
}

impl Poly {
    fn flip(&mut self) {
        self.v.reverse();
        self.plane = self.plane.flip();
    }
}

const COPLANAR: u8 = 0;
const FRONT: u8 = 1;
const BACK: u8 = 2;
const SPANNING: u8 = 3;

#[derive(Default)]
struct Node {
    plane: Option<Plane>,
    front: Option<usize>,
    back: Option<usize>,
    polys: Vec<Poly>,
}

struct Bsp {
    nodes: Vec<Node>,
}

/// The tolerance of a CSG operation.
pub(crate) struct Csg {
    pub eps: f64,
}

impl Csg {
    /// The tolerance for operands whose largest absolute ordinate is `scale`.
    pub fn new(scale: f64) -> Self {
        Self {
            eps: 1e-9 * scale.max(1.0),
        }
    }

    fn side(&self, plane: Plane, v: P3) -> u8 {
        let t = dot(plane.n, v) - plane.w;
        if t < -self.eps {
            BACK
        } else if t > self.eps {
            FRONT
        } else {
            COPLANAR
        }
    }

    /// Splits `p` by `plane` into the four lists (coplanar ones by facing).
    fn split(
        &self,
        plane: Plane,
        p: Poly,
        coplanar_front: &mut Vec<Poly>,
        coplanar_back: &mut Vec<Poly>,
        front: &mut Vec<Poly>,
        back: &mut Vec<Poly>,
    ) {
        let types: Vec<u8> = p.v.iter().map(|&v| self.side(plane, v)).collect();
        let kind = types.iter().fold(COPLANAR, |a, &t| a | t);
        match kind {
            COPLANAR => {
                if dot(plane.n, p.plane.n) > 0.0 {
                    coplanar_front.push(p)
                } else {
                    coplanar_back.push(p)
                }
            }
            FRONT => front.push(p),
            BACK => back.push(p),
            _ => {
                let (mut f, mut b) = (Vec::new(), Vec::new());
                let n = p.v.len();
                for i in 0..n {
                    let j = (i + 1) % n;
                    let (ti, tj) = (types[i], types[j]);
                    let (vi, vj) = (p.v[i], p.v[j]);
                    if ti != BACK {
                        f.push(vi);
                    }
                    if ti != FRONT {
                        b.push(vi);
                    }
                    if ti | tj == SPANNING {
                        let t = (plane.w - dot(plane.n, vi)) / dot(plane.n, sub(vj, vi));
                        let v = [
                            vi[0] + (vj[0] - vi[0]) * t,
                            vi[1] + (vj[1] - vi[1]) * t,
                            vi[2] + (vj[2] - vi[2]) * t,
                        ];
                        f.push(v);
                        b.push(v);
                    }
                }
                if f.len() >= 3 {
                    front.push(Poly {
                        v: f,
                        plane: p.plane,
                    });
                }
                if b.len() >= 3 {
                    back.push(Poly {
                        v: b,
                        plane: p.plane,
                    });
                }
            }
        }
    }

    fn build(&self, bsp: &mut Bsp, polys: Vec<Poly>) {
        let mut work = vec![(0usize, polys)];
        while let Some((i, polys)) = work.pop() {
            if polys.is_empty() {
                continue;
            }
            let plane = *bsp.nodes[i].plane.get_or_insert(polys[0].plane);
            let (mut cf, mut cb, mut front, mut back) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for p in polys {
                self.split(plane, p, &mut cf, &mut cb, &mut front, &mut back);
            }
            bsp.nodes[i].polys.extend(cf);
            bsp.nodes[i].polys.extend(cb);
            if !front.is_empty() {
                let f = match bsp.nodes[i].front {
                    Some(f) => f,
                    None => {
                        bsp.nodes.push(Node::default());
                        let f = bsp.nodes.len() - 1;
                        bsp.nodes[i].front = Some(f);
                        f
                    }
                };
                work.push((f, front));
            }
            if !back.is_empty() {
                let b = match bsp.nodes[i].back {
                    Some(b) => b,
                    None => {
                        bsp.nodes.push(Node::default());
                        let b = bsp.nodes.len() - 1;
                        bsp.nodes[i].back = Some(b);
                        b
                    }
                };
                work.push((b, back));
            }
        }
    }

    fn tree(&self, polys: Vec<Poly>) -> Bsp {
        let mut bsp = Bsp {
            nodes: vec![Node::default()],
        };
        self.build(&mut bsp, polys);
        bsp
    }

    /// The parts of `polys` outside the solid of `bsp`.
    fn clip_polys(&self, bsp: &Bsp, polys: Vec<Poly>) -> Vec<Poly> {
        let mut out = Vec::new();
        let mut work = vec![(0usize, polys)];
        while let Some((i, polys)) = work.pop() {
            let node = &bsp.nodes[i];
            let Some(plane) = node.plane else {
                out.extend(polys);
                continue;
            };
            let (mut front, mut back) = (Vec::new(), Vec::new());
            for p in polys {
                let (mut cf, mut cb) = (Vec::new(), Vec::new());
                self.split(plane, p, &mut cf, &mut cb, &mut front, &mut back);
                front.extend(cf);
                back.extend(cb);
            }
            match node.front {
                Some(f) => work.push((f, front)),
                None => out.extend(front),
            }
            if let Some(b) = node.back {
                work.push((b, back));
            }
        }
        out
    }

    /// Removes from `a` the parts inside the solid of `b`.
    fn clip_to(&self, a: &mut Bsp, b: &Bsp) {
        for i in 0..a.nodes.len() {
            let polys = std::mem::take(&mut a.nodes[i].polys);
            a.nodes[i].polys = self.clip_polys(b, polys);
        }
    }

    fn invert(bsp: &mut Bsp) {
        for n in &mut bsp.nodes {
            n.polys.iter_mut().for_each(Poly::flip);
            n.plane = n.plane.map(Plane::flip);
            std::mem::swap(&mut n.front, &mut n.back);
        }
    }

    fn all(bsp: &Bsp) -> Vec<Poly> {
        bsp.nodes.iter().flat_map(|n| n.polys.clone()).collect()
    }

    /// Convex polygons of the patches, each with its patch's plane.
    fn polys(&self, patches: &[Patch]) -> Vec<Poly> {
        let mut out = Vec::new();
        for p in patches {
            let a = area_vector(p);
            let len = norm(a);
            if len <= self.eps * self.eps {
                continue;
            }
            let n = scale(a, 1.0 / len);
            let plane = Plane {
                n,
                w: dot(n, p[0][0]),
            };
            if is_convex(p, self.eps) {
                let r = &p[0];
                out.push(Poly {
                    v: r[..r.len() - 1].to_vec(),
                    plane,
                });
            } else {
                out.extend(triangulate(p).into_iter().map(|t| Poly {
                    v: t.to_vec(),
                    plane,
                }));
            }
        }
        out
    }

    /// Union, intersection or difference of two closed solids.
    pub fn boolean(&self, a: &[Patch], b: &[Patch], op: Op) -> Vec<Patch> {
        let mut a = self.tree(self.polys(a));
        let mut b = self.tree(self.polys(b));
        match op {
            Op::Union => {
                self.clip_to(&mut a, &b);
                self.clip_to(&mut b, &a);
                Self::invert(&mut b);
                self.clip_to(&mut b, &a);
                Self::invert(&mut b);
                self.build(&mut a, Self::all(&b));
            }
            Op::Difference => {
                Self::invert(&mut a);
                self.clip_to(&mut a, &b);
                self.clip_to(&mut b, &a);
                Self::invert(&mut b);
                self.clip_to(&mut b, &a);
                Self::invert(&mut b);
                self.build(&mut a, Self::all(&b));
                Self::invert(&mut a);
            }
            Op::Intersection => {
                Self::invert(&mut a);
                self.clip_to(&mut b, &a);
                Self::invert(&mut b);
                self.clip_to(&mut a, &b);
                self.clip_to(&mut b, &a);
                self.build(&mut a, Self::all(&b));
                Self::invert(&mut a);
            }
        }
        self.merge(Self::all(&a))
    }

    /// The parts of a surface inside (`inside`) or outside the closed solid `solid`.
    pub fn clip_surface(&self, surface: &[Patch], solid: &[Patch], inside: bool) -> Vec<Patch> {
        let mut s = self.tree(self.polys(solid));
        if inside {
            Self::invert(&mut s);
        }
        let mut out = Vec::new();
        // Clip each patch on its own, so that the fragments of different patches are not
        // merged into one.
        for p in surface {
            let kept = self.clip_polys(&s, self.polys(std::slice::from_ref(p)));
            out.extend(self.merge(kept));
        }
        out
    }

    /// Merges coplanar, equally facing polygons into polygons (with holes) per plane.
    fn merge(&self, polys: Vec<Poly>) -> Vec<Patch> {
        use geo::{Area, Coord, LineString, MultiPolygon, Orient, Polygon};
        let mut groups: Vec<(Plane, Vec<Poly>)> = Vec::new();
        for p in polys {
            match groups.iter_mut().find(|(pl, _)| {
                dot(pl.n, p.plane.n) > 1.0 - 1e-12 && (pl.w - p.plane.w).abs() <= self.eps
            }) {
                Some((_, g)) => g.push(p),
                None => groups.push((p.plane, vec![p])),
            }
        }
        let mut out = Vec::new();
        for (plane, group) in groups {
            let (k, flipped) = projection(plane.n);
            let (i, j) = kept_axes(k);
            let mut parts: Vec<Polygon<f64>> = Vec::new();
            for p in &group {
                let mut ring: Vec<Coord<f64>> =
                    p.v.iter().map(|q| Coord { x: q[i], y: q[j] }).collect();
                if flipped {
                    ring.reverse();
                }
                ring.push(ring[0]);
                let poly = Polygon::new(LineString::new(ring), Vec::new())
                    .orient(geo::orient::Direction::Default);
                if poly.unsigned_area() > self.eps * self.eps {
                    parts.push(poly);
                }
            }
            let merged: MultiPolygon<f64> = if parts.len() == 1 {
                MultiPolygon(parts)
            } else {
                geo::algorithm::unary_union(&parts)
            };
            let lift = |c: &Coord<f64>| -> P3 {
                let mut q = [0.0; 3];
                q[i] = c.x;
                q[j] = c.y;
                q[k] = (plane.w - plane.n[i] * c.x - plane.n[j] * c.y) / plane.n[k];
                q
            };
            for poly in merged.0 {
                if poly.unsigned_area() <= self.eps * self.eps {
                    continue;
                }
                let mut patch: Patch = std::iter::once(poly.exterior())
                    .chain(poly.interiors())
                    .map(|r| r.0.iter().map(lift).collect())
                    .collect();
                // The exterior faces along the plane's normal; holes the other way.
                for (n, r) in patch.iter_mut().enumerate() {
                    let facing = dot(crate::solid::ring_normal(r), plane.n) > 0.0;
                    if facing != (n == 0) {
                        r.reverse();
                    }
                }
                out.push(patch);
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Union,
    Intersection,
    Difference,
}

/// Triangles of the boundary of a solid, for point classification.
pub(crate) fn triangles(solid: &[Patch]) -> Vec<[P3; 3]> {
    solid.iter().flat_map(triangulate).collect()
}

/// Whether `p` is on the boundary of, or inside, the closed surface of `tris`: on a triangle
/// within `eps`, or with a generalized winding number of at least one half.
pub(crate) fn contains(tris: &[[P3; 3]], p: P3, eps: f64) -> bool {
    let mut solid_angle = 0.0;
    for t in tris {
        if distance_to_triangle(p, t) <= eps {
            return true;
        }
        let (a, b, c) = (sub(t[0], p), sub(t[1], p), sub(t[2], p));
        let (la, lb, lc) = (norm(a), norm(b), norm(c));
        let num = dot(a, cross(b, c));
        let den = la * lb * lc + dot(a, b) * lc + dot(a, c) * lb + dot(b, c) * la;
        solid_angle += 2.0 * num.atan2(den);
    }
    (solid_angle / (4.0 * std::f64::consts::PI)).abs() >= 0.5
}

fn distance_to_triangle(p: P3, t: &[P3; 3]) -> f64 {
    let n = cross(sub(t[1], t[0]), sub(t[2], t[0]));
    let len = norm(n);
    if len > 0.0 {
        let un = scale(n, 1.0 / len);
        let d = dot(sub(p, t[0]), un);
        let q = sub(p, scale(un, d));
        // Inside the triangle if on the inner side of its three edges.
        let inside = (0..3).all(|i| {
            let (a, b) = (t[i], t[(i + 1) % 3]);
            dot(cross(sub(b, a), sub(q, a)), n) >= 0.0
        });
        if inside {
            return d.abs();
        }
    }
    (0..3)
        .map(|i| distance_to_segment(p, t[i], t[(i + 1) % 3]))
        .fold(f64::INFINITY, f64::min)
}

fn distance_to_segment(p: P3, a: P3, b: P3) -> f64 {
    let d = sub(b, a);
    let len2 = dot(d, d);
    let t = if len2 == 0.0 {
        0.0
    } else {
        (dot(sub(p, a), d) / len2).clamp(0.0, 1.0)
    };
    norm(sub(p, [a[0] + d[0] * t, a[1] + d[1] * t, a[2] + d[2] * t]))
}

/// The pieces of a line inside (`inside`) or outside a closed surface: the line is cut where
/// it crosses a triangle, and each piece is classified by its middle.
pub(crate) fn clip_line(line: &[P3], tris: &[[P3; 3]], eps: f64, inside: bool) -> Vec<Vec<P3>> {
    let mut out: Vec<Vec<P3>> = Vec::new();
    let mut current: Vec<P3> = Vec::new();
    for w in line.windows(2) {
        let (a, b) = (w[0], w[1]);
        let d = sub(b, a);
        let mut cuts = vec![0.0, 1.0];
        for t in tris {
            let n = cross(sub(t[1], t[0]), sub(t[2], t[0]));
            let den = dot(n, d);
            if den.abs() <= f64::EPSILON * norm(n) * norm(d) {
                continue;
            }
            let s = dot(n, sub(t[0], a)) / den;
            if s > 0.0 && s < 1.0 {
                let q = [a[0] + d[0] * s, a[1] + d[1] * s, a[2] + d[2] * s];
                if distance_to_triangle(q, t) <= eps {
                    cuts.push(s);
                }
            }
        }
        cuts.sort_by(f64::total_cmp);
        cuts.dedup_by(|x, y| (*x - *y).abs() * norm(d) <= eps);
        let at = |s: f64| [a[0] + d[0] * s, a[1] + d[1] * s, a[2] + d[2] * s];
        for c in cuts.windows(2) {
            let (s0, s1) = (c[0], c[1]);
            let keep = contains(tris, at((s0 + s1) / 2.0), eps) == inside;
            if keep {
                if current.is_empty() {
                    current.push(at(s0));
                }
                current.push(if s1 == 1.0 { b } else { at(s1) });
            } else if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        }
    }
    if current.len() >= 2 {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solid::signed_volume;

    /// An axis-aligned box with outward faces.
    fn cuboid(lo: P3, hi: P3) -> Vec<Patch> {
        let c = |x: usize, y: usize, z: usize| {
            [
                if x == 0 { lo[0] } else { hi[0] },
                if y == 0 { lo[1] } else { hi[1] },
                if z == 0 { lo[2] } else { hi[2] },
            ]
        };
        let q = |a: P3, b: P3, cc: P3, d: P3| vec![vec![a, b, cc, d, a]];
        vec![
            q(c(0, 0, 0), c(0, 1, 0), c(1, 1, 0), c(1, 0, 0)),
            q(c(0, 0, 1), c(1, 0, 1), c(1, 1, 1), c(0, 1, 1)),
            q(c(0, 0, 0), c(1, 0, 0), c(1, 0, 1), c(0, 0, 1)),
            q(c(0, 1, 0), c(0, 1, 1), c(1, 1, 1), c(1, 1, 0)),
            q(c(0, 0, 0), c(0, 0, 1), c(0, 1, 1), c(0, 1, 0)),
            q(c(1, 0, 0), c(1, 1, 0), c(1, 1, 1), c(1, 0, 1)),
        ]
    }

    fn volume(a: &[Patch], b: &[Patch], op: Op) -> f64 {
        signed_volume(&Csg::new(10.0).boolean(a, b, op))
    }

    #[test]
    fn coincident_and_touching_solids() {
        let a = cuboid([0.0; 3], [1.0; 3]);
        assert!((signed_volume(&a) - 1.0).abs() < 1e-12);
        // The same solid twice: every face coincides.
        assert!((volume(&a, &a, Op::Union) - 1.0).abs() < 1e-9);
        assert!((volume(&a, &a, Op::Intersection) - 1.0).abs() < 1e-9);
        assert!(volume(&a, &a, Op::Difference).abs() < 1e-9);
        // Sharing a face.
        let b = cuboid([1.0, 0.0, 0.0], [2.0, 1.0, 1.0]);
        assert!((volume(&a, &b, Op::Union) - 2.0).abs() < 1e-9);
        assert!(volume(&a, &b, Op::Intersection).abs() < 1e-9);
        assert!((volume(&a, &b, Op::Difference) - 1.0).abs() < 1e-9);
        // The union's coplanar faces merge: a 2 x 1 x 1 box has six faces.
        assert_eq!(Csg::new(2.0).boolean(&a, &b, Op::Union).len(), 6);
    }

    #[test]
    fn offset_and_nested_boxes() {
        let a = cuboid([0.0; 3], [1.0; 3]);
        let b = cuboid([0.3, 0.2, 0.1], [1.3, 1.2, 1.1]);
        let i = 0.7 * 0.8 * 0.9;
        assert!((volume(&a, &b, Op::Intersection) - i).abs() < 1e-9);
        assert!((volume(&a, &b, Op::Union) - (2.0 - i)).abs() < 1e-9);
        assert!((volume(&a, &b, Op::Difference) - (1.0 - i)).abs() < 1e-9);
        // A box inside another: a cavity.
        let inner = cuboid([0.25; 3], [0.75; 3]);
        assert!((volume(&a, &inner, Op::Difference) - 0.875).abs() < 1e-9);
        assert!((volume(&inner, &a, Op::Intersection) - 0.125).abs() < 1e-9);
        assert!((volume(&inner, &a, Op::Union) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn point_classification() {
        let a = cuboid([0.0; 3], [1.0; 3]);
        let t = triangles(&a);
        assert!(contains(&t, [0.5, 0.5, 0.5], 1e-9));
        assert!(contains(&t, [1.0, 0.5, 0.5], 1e-9));
        assert!(!contains(&t, [1.5, 0.5, 0.5], 1e-9));
        assert!(!contains(&t, [-1e-6, 0.5, 0.5], 1e-9));
    }
}
