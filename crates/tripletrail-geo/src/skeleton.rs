//! The straight skeleton of a polygon (with holes), by simulating its shrinking wavefront.
//!
//! Every edge moves inward at unit speed; each wavefront vertex moves along the bisector of
//! its two edges. The next event is found by scanning all of them: an edge event (an edge
//! shrinks to nothing and its two vertices merge) or a split event (a reflex vertex reaches a
//! non-adjacent edge, which splits the wavefront, or joins a hole to the outer front). The
//! traces of the vertices are the skeleton's arcs. A front that has collapsed to zero area
//! (the last stage of every part) is closed off along its remaining segments. This is the
//! event model of Felkel and Obdržálek, scanned in O(n²) per event rather than kept in a
//! priority queue; tolerances are relative (`1e-9` of the polygon's size).

use geo::{Coord, LineString, MultiLineString, Orient, Polygon, orient::Direction};

#[derive(Clone, Copy)]
struct V {
    pos: Coord<f64>,
    vel: Coord<f64>,
    origin: Coord<f64>,
    prev: usize,
    next: usize,
    /// Inward unit normals of the edges before and after the vertex.
    n_in: Coord<f64>,
    n_out: Coord<f64>,
    alive: bool,
}

fn dot(a: Coord<f64>, b: Coord<f64>) -> f64 {
    a.x * b.x + a.y * b.y
}

fn cross(a: Coord<f64>, b: Coord<f64>) -> f64 {
    a.x * b.y - a.y * b.x
}

/// The direction of an edge with inward normal `n` (the interior is on its left).
fn dir(n: Coord<f64>) -> Coord<f64> {
    Coord { x: n.y, y: -n.x }
}

/// The velocity that moves a vertex with both its edges at unit speed.
fn velocity(n1: Coord<f64>, n2: Coord<f64>) -> Coord<f64> {
    let det = cross(n1, n2);
    if det.abs() < 1e-12 {
        // Collinear edges: move with them. Opposite edges: the front has collapsed here.
        return if dot(n1, n2) > 0.0 {
            n1
        } else {
            Coord { x: 0.0, y: 0.0 }
        };
    }
    Coord {
        x: (n2.y - n1.y) / det,
        y: (n1.x - n2.x) / det,
    }
}

enum Event {
    Edge(usize),
    /// A reflex vertex reaches the edge starting at the second vertex.
    Split(usize, usize),
}

struct Front {
    v: Vec<V>,
    arcs: Vec<(Coord<f64>, Coord<f64>)>,
    tol: f64,
}

impl Front {
    fn new(p: &Polygon<f64>, tol: f64) -> Option<Self> {
        let mut f = Front {
            v: Vec::new(),
            arcs: Vec::new(),
            tol,
        };
        for ring in std::iter::once(p.exterior()).chain(p.interiors()) {
            let mut pts: Vec<Coord<f64>> = ring.0.clone();
            pts.dedup_by(|a, b| (a.x - b.x).abs() <= tol && (a.y - b.y).abs() <= tol);
            if pts.len() > 1 && pts.first() == pts.last() {
                pts.pop();
            }
            // Drop vertices where the ring runs straight on or doubles back.
            let mut changed = true;
            while changed && pts.len() >= 3 {
                changed = false;
                for i in 0..pts.len() {
                    let n = pts.len();
                    let (a, b, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
                    let (d1, d2) = (b - a, c - b);
                    if cross(d1, d2).abs() <= tol * (d1.x.hypot(d1.y) + d2.x.hypot(d2.y))
                        && dot(d1, d2) < 0.0
                    {
                        pts.remove(i);
                        changed = true;
                        break;
                    }
                }
            }
            if pts.len() < 3 {
                if std::ptr::eq(ring, p.exterior()) {
                    return None;
                }
                continue;
            }
            let base = f.v.len();
            let n = pts.len();
            let normal = |a: Coord<f64>, b: Coord<f64>| {
                let d = b - a;
                let l = d.x.hypot(d.y);
                Coord {
                    x: -d.y / l,
                    y: d.x / l,
                }
            };
            for i in 0..n {
                let (a, b, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
                let (n_in, n_out) = (normal(a, b), normal(b, c));
                f.v.push(V {
                    pos: b,
                    vel: velocity(n_in, n_out),
                    origin: b,
                    prev: base + (i + n - 1) % n,
                    next: base + (i + 1) % n,
                    n_in,
                    n_out,
                    alive: true,
                });
            }
        }
        Some(f)
    }

    fn arc(&mut self, a: Coord<f64>, b: Coord<f64>) {
        if (a.x - b.x).hypot(a.y - b.y) > self.tol {
            self.arcs.push((a, b));
        }
    }

    /// The next event and the time until it.
    fn next_event(&self) -> Option<(f64, Event)> {
        let mut best: Option<(f64, Event)> = None;
        let offer = |t: f64, e: Event, best: &mut Option<(f64, Event)>| {
            if best.as_ref().is_none_or(|(bt, _)| t < *bt) {
                *best = Some((t, e));
            }
        };
        for (i, a) in self.v.iter().enumerate().filter(|(_, a)| a.alive) {
            let b = &self.v[a.next];
            let d = dir(a.n_out);
            let len = dot(d, b.pos - a.pos);
            let rate = dot(d, b.vel - a.vel);
            if rate < -1e-12 {
                offer((-len / rate).max(0.0), Event::Edge(i), &mut best);
            } else if len <= self.tol && rate <= 1e-12 && a.next != i {
                offer(0.0, Event::Edge(i), &mut best);
            }
        }
        for (r, rv) in self.v.iter().enumerate().filter(|(_, r)| r.alive) {
            if cross(dir(rv.n_in), dir(rv.n_out)) >= 0.0 {
                continue; // not reflex
            }
            for (x, xv) in self.v.iter().enumerate().filter(|(_, x)| x.alive) {
                if x == r || xv.next == r {
                    continue;
                }
                let yv = &self.v[xv.next];
                let n = xv.n_out;
                let gap = dot(n, rv.pos - xv.pos);
                let approach = 1.0 - dot(n, rv.vel);
                if approach <= 1e-12 || gap < -self.tol {
                    continue;
                }
                let t = (gap / approach).max(0.0);
                let p = rv.pos + rv.vel * t;
                let (xt, yt) = (xv.pos + xv.vel * t, yv.pos + yv.vel * t);
                let d = dir(n);
                let (s, l) = (dot(d, p - xt), dot(d, yt - xt));
                if s >= -self.tol && s <= l + self.tol {
                    offer(t, Event::Split(r, x), &mut best);
                }
            }
        }
        best
    }

    fn advance(&mut self, t: f64) {
        for v in self.v.iter_mut().filter(|v| v.alive) {
            v.pos = v.pos + v.vel * t;
        }
    }

    /// The vertices of the cycle through `start`.
    fn cycle(&self, start: usize) -> Option<Vec<usize>> {
        let mut out = vec![start];
        let mut i = self.v[start].next;
        while i != start {
            (self.v[i].alive && out.len() <= self.v.len()).then_some(())?;
            out.push(i);
            i = self.v[i].next;
        }
        Some(out)
    }

    /// Closes off fronts with fewer than three vertices or no area. Returns whether any was.
    fn collapse(&mut self) -> Option<bool> {
        let mut seen = vec![false; self.v.len()];
        let mut any = false;
        for s in 0..self.v.len() {
            if !self.v[s].alive || seen[s] {
                continue;
            }
            let c = self.cycle(s)?;
            c.iter().for_each(|&i| seen[i] = true);
            let pos: Vec<Coord<f64>> = c.iter().map(|&i| self.v[i].pos).collect();
            let m = pos.len();
            let area2: f64 = (0..m).map(|k| cross(pos[k], pos[(k + 1) % m])).sum();
            let perimeter: f64 = (0..m)
                .map(|k| {
                    (pos[(k + 1) % m] - pos[k])
                        .x
                        .hypot((pos[(k + 1) % m] - pos[k]).y)
                })
                .sum();
            if m >= 3 && area2.abs() > self.tol * perimeter.max(self.tol) {
                continue;
            }
            any = true;
            for &i in &c {
                let v = self.v[i];
                self.arc(v.origin, v.pos);
                self.arc(v.pos, self.v[v.next].pos);
                self.v[i].alive = false;
            }
        }
        Some(any)
    }

    fn push(&mut self, v: V) -> usize {
        self.v.push(v);
        self.v.len() - 1
    }

    fn edge_event(&mut self, a: usize) {
        let b = self.v[a].next;
        let (va, vb) = (self.v[a], self.v[b]);
        let p = (va.pos + vb.pos) / 2.0;
        self.arc(va.origin, p);
        self.arc(vb.origin, p);
        self.v[a].alive = false;
        self.v[b].alive = false;
        let n = self.push(V {
            pos: p,
            vel: velocity(va.n_in, vb.n_out),
            origin: p,
            prev: va.prev,
            next: vb.next,
            n_in: va.n_in,
            n_out: vb.n_out,
            alive: true,
        });
        self.v[va.prev].next = n;
        self.v[vb.next].prev = n;
    }

    fn split_event(&mut self, r: usize, x: usize) {
        let rv = self.v[r];
        let y = self.v[x].next;
        let e = self.v[x].n_out;
        self.arc(rv.origin, rv.pos);
        self.v[r].alive = false;
        let v1 = self.push(V {
            pos: rv.pos,
            vel: velocity(rv.n_in, e),
            origin: rv.pos,
            prev: rv.prev,
            next: y,
            n_in: rv.n_in,
            n_out: e,
            alive: true,
        });
        let v2 = self.push(V {
            pos: rv.pos,
            vel: velocity(e, rv.n_out),
            origin: rv.pos,
            prev: x,
            next: rv.next,
            n_in: e,
            n_out: rv.n_out,
            alive: true,
        });
        self.v[rv.prev].next = v1;
        self.v[y].prev = v1;
        self.v[x].next = v2;
        self.v[rv.next].prev = v2;
    }
}

/// The straight skeleton of a polygon: its arcs, as two-point lines.
pub(crate) fn straight_skeleton(p: &Polygon<f64>) -> Option<Vec<LineString<f64>>> {
    use geo::BoundingRect;
    let p = p.orient(Direction::Default);
    let r = p.bounding_rect()?;
    let size = r.width().max(r.height());
    (size > 0.0).then_some(())?;
    let tol = 1e-9 * size.max(r.min().x.abs().max(r.min().y.abs()) * 1e-3);
    let mut f = Front::new(&p, tol)?;
    let limit = 64 * f.v.len() + 1024;
    for _ in 0..limit {
        if f.collapse()? {
            continue;
        }
        let Some((t, event)) = f.next_event() else {
            break;
        };
        f.advance(t);
        if f.collapse()? {
            continue;
        }
        match event {
            Event::Edge(a) => f.edge_event(a),
            Event::Split(r, x) => f.split_event(r, x),
        }
    }
    f.v.iter()
        .any(|v| v.alive)
        .then_some(())
        .map_or(Some(()), |_| None)?;
    // Each arc once, whichever way round.
    let key = |c: Coord<f64>| ((c.x / tol).round() as i64, (c.y / tol).round() as i64);
    let mut seen = std::collections::HashSet::new();
    Some(
        f.arcs
            .into_iter()
            .filter(|(a, b)| {
                let (ka, kb) = (key(*a), key(*b));
                seen.insert(if ka < kb { (ka, kb) } else { (kb, ka) })
            })
            .map(|(a, b)| LineString::new(vec![a, b]))
            .collect(),
    )
}

/// The straight skeleton of each polygon.
pub(crate) fn skeleton_of(polys: &[Polygon<f64>]) -> Option<MultiLineString<f64>> {
    let mut lines = Vec::new();
    for p in polys {
        lines.extend(straight_skeleton(p)?);
    }
    Some(MultiLineString(lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Euclidean, Length, Relate};

    fn skeleton(coords: &[(f64, f64)]) -> (Polygon<f64>, Vec<LineString<f64>>) {
        let p = Polygon::new(LineString::from(coords.to_vec()), Vec::new());
        let s = straight_skeleton(&p).expect("skeleton");
        (p, s)
    }

    #[test]
    fn square_meets_in_the_centre() {
        let (_, s) = skeleton(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)]);
        assert_eq!(s.len(), 4);
        for l in &s {
            assert!(l.0.contains(&Coord { x: 1.0, y: 1.0 }), "{l:?}");
        }
    }

    #[test]
    fn non_convex_shapes_stay_inside() {
        let shapes: [&[(f64, f64)]; 3] = [
            // A star.
            &[
                (0.0, 3.0),
                (1.0, 1.0),
                (3.0, 0.8),
                (1.4, -0.4),
                (2.0, -2.5),
                (0.0, -1.2),
                (-2.0, -2.5),
                (-1.4, -0.4),
                (-3.0, 0.8),
                (-1.0, 1.0),
            ],
            // A U.
            &[
                (0.0, 0.0),
                (5.0, 0.0),
                (5.0, 4.0),
                (4.0, 4.0),
                (4.0, 1.0),
                (1.0, 1.0),
                (1.0, 4.0),
                (0.0, 4.0),
            ],
            // A notched rectangle (a split event).
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 4.0),
                (6.0, 4.0),
                (5.0, 2.0),
                (4.0, 4.0),
                (0.0, 4.0),
            ],
        ];
        for coords in shapes {
            let (p, s) = skeleton(coords);
            assert!(s.len() >= coords.len(), "{coords:?}: {s:?}");
            for l in &s {
                assert!(p.relate(l).is_covers(), "{coords:?}: {l:?}");
                assert!(Euclidean.length(l) > 0.0);
            }
            // Every vertex of the polygon starts an arc.
            for c in coords {
                let c = Coord { x: c.0, y: c.1 };
                assert!(s.iter().any(|l| l.0.contains(&c)), "{coords:?}: {c:?}");
            }
        }
    }
}
