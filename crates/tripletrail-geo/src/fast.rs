//! Fast paths for the topological relations, ahead of the full DE-9IM `relate()`.
//!
//! `relate()` builds a topology graph of both geometries for every call. Most spatial joins
//! and selections test a point against a polygon, or ask only whether two geometries
//! intersect; both are answered here with a point-in-polygon test and a segment crossing test
//! against a [`Prepared`] polygon, which indexes the polygon's edges once and is kept with its
//! parsed geometry ([`Geom::prepared`]), so a polygon met by many candidates is prepared once.

use crate::literal::Geom;
use geo::kernels::{Kernel, Orientation, RobustKernel};
use geo::relate::IntersectionMatrix;
use geo::{BoundingRect, Coord, Geometry, Intersects, Line, LineString, Point, Relate};
use std::str::FromStr;
use std::sync::LazyLock;

/// Where a point lies relative to a geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Position {
    Interior,
    Boundary,
    Exterior,
}

/// The edges of a polygonal geometry (every ring of every polygon), bucketed by horizontal
/// band, for point-in-polygon tests and segment crossing tests that look at the edges near the
/// tested point or segment only.
#[derive(Debug)]
pub struct Prepared {
    edges: Vec<[Coord<f64>; 2]>,
    min: Coord<f64>,
    max: Coord<f64>,
    band_height: f64,
    /// Edge indexes of band `i` are `members[offsets[i]..offsets[i + 1]]`.
    offsets: Vec<u32>,
    members: Vec<u32>,
}

/// Edges per band, on average; a polygon with fewer edges has one band.
const EDGES_PER_BAND: usize = 8;
const MAX_BANDS: usize = 1 << 16;

impl Prepared {
    /// The edges of `g`, if it is a non-empty polygon, multipolygon, rectangle or triangle.
    pub(crate) fn new(g: &Geometry<f64>) -> Option<Self> {
        let mut edges = Vec::new();
        let mut ring = |l: &LineString<f64>| {
            edges.extend(l.0.windows(2).map(|w| [w[0], w[1]]));
            // An unclosed ring is closed, as a polygon's ring always is.
            if let (Some(a), Some(b)) = (l.0.first(), l.0.last())
                && a != b
            {
                edges.push([*b, *a]);
            }
        };
        match g {
            Geometry::Polygon(p) => {
                ring(p.exterior());
                p.interiors().iter().for_each(&mut ring);
            }
            Geometry::MultiPolygon(m) => {
                for p in &m.0 {
                    ring(p.exterior());
                    p.interiors().iter().for_each(&mut ring);
                }
            }
            Geometry::Rect(r) => ring(r.to_polygon().exterior()),
            Geometry::Triangle(t) => ring(t.to_polygon().exterior()),
            _ => return None,
        }
        let rect = g.bounding_rect()?;
        if edges.is_empty() {
            return None;
        }
        let bands = (edges.len() / EDGES_PER_BAND).clamp(1, MAX_BANDS);
        let height = (rect.max().y - rect.min().y) / bands as f64;
        let mut prepared = Self {
            edges,
            min: rect.min(),
            max: rect.max(),
            band_height: if height > 0.0 { height } else { 1.0 },
            offsets: vec![0; bands + 1],
            members: Vec::new(),
        };
        let mut counts = vec![0u32; bands];
        for e in &prepared.edges {
            let (lo, hi) = prepared.bands_of(e[0].y.min(e[1].y), e[0].y.max(e[1].y));
            for c in &mut counts[lo..=hi] {
                *c += 1;
            }
        }
        let mut total = 0u32;
        for (i, c) in counts.iter().enumerate() {
            prepared.offsets[i] = total;
            total += c;
        }
        prepared.offsets[bands] = total;
        prepared.members = vec![0; total as usize];
        let mut fill = prepared.offsets.clone();
        for (k, e) in prepared.edges.iter().enumerate() {
            let (lo, hi) = prepared.bands_of(e[0].y.min(e[1].y), e[0].y.max(e[1].y));
            for slot in &mut fill[lo..=hi] {
                prepared.members[*slot as usize] = k as u32;
                *slot += 1;
            }
        }
        Some(prepared)
    }

    fn band_count(&self) -> usize {
        self.offsets.len() - 1
    }

    fn band(&self, y: f64) -> usize {
        let b = ((y - self.min.y) / self.band_height).floor();
        if b.is_nan() || b < 0.0 {
            0
        } else {
            (b as usize).min(self.band_count() - 1)
        }
    }

    /// The bands an edge or segment spanning `lo..=hi` in y falls in.
    fn bands_of(&self, lo: f64, hi: f64) -> (usize, usize) {
        (self.band(lo), self.band(hi))
    }

    fn edges_in(&self, band: usize) -> impl Iterator<Item = &[Coord<f64>; 2]> {
        let (s, e) = (self.offsets[band] as usize, self.offsets[band + 1] as usize);
        self.members[s..e].iter().map(|&k| &self.edges[k as usize])
    }

    /// Where `p` lies: inside, on an edge, or outside (even-odd rule over all rings, which is
    /// the polygon's interior for valid polygons and multipolygons).
    pub(crate) fn position(&self, p: Coord<f64>) -> Position {
        if p.x < self.min.x || p.x > self.max.x || p.y < self.min.y || p.y > self.max.y {
            return Position::Exterior;
        }
        let mut inside = false;
        for [a, b] in self.edges_in(self.band(p.y)) {
            let (a, b) = (*a, *b);
            // On the edge: collinear and within its bounding box.
            if p.x >= a.x.min(b.x)
                && p.x <= a.x.max(b.x)
                && p.y >= a.y.min(b.y)
                && p.y <= a.y.max(b.y)
                && RobustKernel::orient2d(a, b, p) == Orientation::Collinear
            {
                return Position::Boundary;
            }
            // Ray to +x, half-open in y so a vertex on the ray counts once.
            if (a.y > p.y) != (b.y > p.y) {
                let o = RobustKernel::orient2d(a, b, p);
                let right = if b.y > a.y {
                    o == Orientation::CounterClockwise
                } else {
                    o == Orientation::Clockwise
                };
                if right {
                    inside = !inside;
                }
            }
        }
        if inside {
            Position::Interior
        } else {
            Position::Exterior
        }
    }

    /// Whether segment `s` meets an edge of the polygon (touching counts).
    pub(crate) fn segment_meets_edge(&self, s: Line<f64>) -> bool {
        let (lo, hi) = (s.start.y.min(s.end.y), s.start.y.max(s.end.y));
        if hi < self.min.y || lo > self.max.y {
            return false;
        }
        if s.start.x.max(s.end.x) < self.min.x || s.start.x.min(s.end.x) > self.max.x {
            return false;
        }
        let (b0, b1) = self.bands_of(lo, hi);
        // An edge spanning several bands is met once per band: fine for a yes/no answer.
        (b0..=b1).any(|b| {
            self.edges_in(b)
                .any(|[a, c]| Line::new(*a, *c).intersects(&s))
        })
    }

    /// Whether some edge of the polygon meets some edge of `other`.
    fn edges_meet(&self, other: &Prepared) -> bool {
        let (small, large) = if self.edges.len() <= other.edges.len() {
            (self, other)
        } else {
            (other, self)
        };
        small
            .edges
            .iter()
            .any(|[a, b]| large.segment_meets_edge(Line::new(*a, *b)))
    }

    /// One vertex of each ring: if no edges cross, each ring lies wholly on one side.
    fn ring_vertices(&self) -> impl Iterator<Item = Coord<f64>> + '_ {
        // Every edge's start is a vertex; checking all of them is cheap and covers every ring.
        self.edges.iter().map(|e| e[0])
    }
}

/// Position of `p` relative to a line string: its boundary is its two end points unless it is
/// closed (then it has none).
pub(crate) fn position_on_line(l: &LineString<f64>, p: Coord<f64>) -> Position {
    let n = l.0.len();
    if n == 0 {
        return Position::Exterior;
    }
    let closed = n > 1 && l.0[0] == l.0[n - 1];
    if !closed && (p == l.0[0] || p == l.0[n - 1]) {
        return Position::Boundary;
    }
    if n == 1 {
        return if p == l.0[0] {
            Position::Interior
        } else {
            Position::Exterior
        };
    }
    let on = l.0.windows(2).any(|w| {
        let (a, b) = (w[0], w[1]);
        p.x >= a.x.min(b.x)
            && p.x <= a.x.max(b.x)
            && p.y >= a.y.min(b.y)
            && p.y <= a.y.max(b.y)
            && RobustKernel::orient2d(a, b, p) == Orientation::Collinear
    });
    if on {
        Position::Interior
    } else {
        Position::Exterior
    }
}

fn matrix(s: &str) -> IntersectionMatrix {
    IntersectionMatrix::from_str(s).expect("valid DE-9IM matrix")
}

/// DE-9IM matrices of a point (first) against a polygon, a line and a point.
static POINT_POLYGON: LazyLock<[IntersectionMatrix; 3]> = LazyLock::new(|| {
    [
        matrix("0FFFFF212"),
        matrix("F0FFFF212"),
        matrix("FF0FFF212"),
    ]
});
static POINT_LINE: LazyLock<[IntersectionMatrix; 3]> = LazyLock::new(|| {
    [
        matrix("0FFFFF102"),
        matrix("F0FFFF102"),
        matrix("FF0FFF102"),
    ]
});
/// A closed line string has no boundary.
static POINT_RING: LazyLock<[IntersectionMatrix; 3]> = LazyLock::new(|| {
    [
        matrix("0FFFFF1F2"),
        matrix("0FFFFF1F2"),
        matrix("FF0FFF1F2"),
    ]
});
static POINT_POINT: LazyLock<[IntersectionMatrix; 2]> =
    LazyLock::new(|| [matrix("0FFFFFFF2"), matrix("FF0FFF0F2")]);

fn pick(m: &[IntersectionMatrix; 3], p: Position) -> IntersectionMatrix {
    m[match p {
        Position::Interior => 0,
        Position::Boundary => 1,
        Position::Exterior => 2,
    }]
    .clone()
}

fn transpose(m: IntersectionMatrix) -> IntersectionMatrix {
    use geo::coordinate_position::CoordPos::{Inside, OnBoundary, Outside};
    let positions = [Inside, OnBoundary, Outside];
    let mut s = String::with_capacity(9);
    for a in positions {
        for b in positions {
            s.push(match m.get(b, a) {
                geo::dimensions::Dimensions::Empty => 'F',
                geo::dimensions::Dimensions::ZeroDimensional => '0',
                geo::dimensions::Dimensions::OneDimensional => '1',
                geo::dimensions::Dimensions::TwoDimensional => '2',
            });
        }
    }
    matrix(&s)
}

/// The DE-9IM matrix of a point against `other`, if a fast path covers `other`.
fn point_matrix(p: Point<f64>, other: &Geom) -> Option<IntersectionMatrix> {
    let c = p.0;
    Some(match &other.geometry {
        Geometry::Point(q) => POINT_POINT[usize::from(q.0 != c)].clone(),
        Geometry::LineString(l) if l.0.len() >= 2 => {
            let closed = l.0[0] == l.0[l.0.len() - 1];
            let table = if closed { &POINT_RING } else { &POINT_LINE };
            pick(table, position_on_line(l, c))
        }
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) => {
            pick(&POINT_POLYGON, other.prepared()?.position(c))
        }
        _ => return None,
    })
}

/// `a.relate(b)`, through a fast path when one side is a point.
pub(crate) fn relate(a: &Geom, b: &Geom) -> IntersectionMatrix {
    if let Geometry::Point(p) = &a.geometry
        && let Some(m) = point_matrix(*p, b)
    {
        return m;
    }
    if let Geometry::Point(p) = &b.geometry
        && let Some(m) = point_matrix(*p, a)
    {
        return transpose(m);
    }
    if let Some(m) = apart_rings(a, b) {
        return m;
    }
    use geo::CoordsIter;
    let (na, nb) = (a.geometry.coords_count(), b.geometry.coords_count());
    if na.max(nb) >= PREPARE_COORDS {
        return if na >= nb {
            with_graph(a, |g| g.relate(&b.geometry))
        } else {
            transpose(with_graph(b, |g| g.relate(&a.geometry)))
        };
    }
    a.geometry.relate(&b.geometry)
}

/// Geometries with at least this many coordinates keep their topology graph for `relate`.
const PREPARE_COORDS: usize = 256;
/// Topology graphs kept per thread.
const GRAPHS_KEPT: usize = 8;

type Graph = geo::PreparedGeometry<'static, Geometry<f64>>;

thread_local! {
    /// The topology graphs of the large geometries this thread related last, by address
    /// (#703): a spatial join relates one large polygon with many candidates, and `relate`
    /// would build its graph, with an index of its segments, every time. A graph is not
    /// `Send`, hence per thread. The address only finds the entry: it is used only if its
    /// geometry is equal, so a geometry dropped and another allocated at its address is
    /// never mistaken for it.
    static GRAPHS: std::cell::RefCell<Vec<(usize, Graph)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn with_graph(g: &Geom, f: impl FnOnce(&Graph) -> IntersectionMatrix) -> IntersectionMatrix {
    let key = std::ptr::from_ref(g) as usize;
    GRAPHS.with_borrow_mut(|graphs| {
        let at = graphs
            .iter()
            .position(|(k, p)| *k == key && *p.geometry() == g.geometry);
        let at = match at {
            Some(i) => i,
            None => {
                if graphs.len() == GRAPHS_KEPT {
                    graphs.remove(0);
                }
                graphs.push((key, geo::PreparedGeometry::from(g.geometry.clone())));
                graphs.len() - 1
            }
        };
        f(&graphs[at].1)
    })
}

fn polygonal(g: &Geom) -> Option<&Prepared> {
    match &g.geometry {
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) => g.prepared(),
        _ => None,
    }
}

/// The DE-9IM matrix of two polygonal geometries whose boundaries do not meet (#703): every
/// ring then lies wholly inside or outside the other geometry, so the positions of the rings'
/// vertices give the matrix. In a spatial join of two coverages most candidate pairs are a
/// polygon inside a much larger one, and `relate` would build a graph of the large one each
/// time. `None` when the boundaries meet, or a geometry is not polygonal.
fn apart_rings(a: &Geom, b: &Geom) -> Option<IntersectionMatrix> {
    let (pa, pb) = (polygonal(a)?, polygonal(b)?);
    if pa.edges_meet(pb) {
        return None;
    }
    // Where each geometry's rings lie relative to the other: in its interior, outside it.
    let sides = |p: &Prepared, other: &Prepared| {
        let (mut inside, mut outside) = (false, false);
        for c in p.ring_vertices() {
            match other.position(c) {
                Position::Interior => inside = true,
                Position::Exterior => outside = true,
                // A vertex on the other's boundary: the boundaries meet after all.
                Position::Boundary => return None,
            }
        }
        Some((inside, outside))
    };
    let (a_in, a_out) = sides(pa, pb)?;
    let (b_in, b_out) = sides(pb, pa)?;
    let d = |yes: bool, dim: char| if yes { dim } else { 'F' };
    let s: String = [
        d(a_in || b_in, '2'),
        d(b_in, '1'),
        d(a_out || b_in, '2'),
        d(a_in, '1'),
        'F',
        d(a_out, '1'),
        d(b_out || a_in, '2'),
        d(b_out, '1'),
        '2',
    ]
    .into_iter()
    .collect();
    Some(matrix(&s))
}

/// Whether `a` and `b` intersect, without the full DE-9IM matrix when one of them is a
/// polygon and the other a point, a line or a polygon.
pub(crate) fn intersects(a: &Geom, b: &Geom) -> bool {
    if let Some(v) = intersects_fast(a, b).or_else(|| intersects_fast(b, a)) {
        return v;
    }
    relate(a, b).is_intersects()
}

/// Whether some vertex of `inner` lies outside the polygonal `outer` (false if `outer` is not
/// a polygon): then `inner` is not within, nor covered by, `outer`.
pub(crate) fn vertex_outside(inner: &Geom, outer: &Geom) -> bool {
    use geo::CoordsIter;
    let Some(p) = (match &outer.geometry {
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) => outer.prepared(),
        _ => None,
    }) else {
        return false;
    };
    inner
        .geometry
        .coords_iter()
        .any(|c| p.position(c) == Position::Exterior)
}

/// Whether the points or lines `inner` lie in the interior of the polygonal `outer`: every
/// vertex inside, and no segment meeting an edge of `outer` (touching counts), so no part of
/// `inner` reaches the boundary or the outside. Then `outer` contains and covers `inner`.
/// False when that is not shown (polygonal or empty `inner`, a vertex on or outside the
/// boundary, a segment meeting an edge): the caller then computes the relation.
pub(crate) fn in_interior(inner: &Geom, outer: &Geom) -> bool {
    let Some(p) = (match &outer.geometry {
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) => outer.prepared(),
        _ => None,
    }) else {
        return false;
    };
    let line = |l: &LineString<f64>| {
        !l.0.is_empty()
            && l.0.iter().all(|c| p.position(*c) == Position::Interior)
            && !l.lines().any(|s| p.segment_meets_edge(s))
    };
    match &inner.geometry {
        Geometry::Point(q) => p.position(q.0) == Position::Interior,
        Geometry::MultiPoint(m) => {
            !m.0.is_empty() && m.0.iter().all(|q| p.position(q.0) == Position::Interior)
        }
        Geometry::Line(l) => {
            p.position(l.start) == Position::Interior
                && p.position(l.end) == Position::Interior
                && !p.segment_meets_edge(*l)
        }
        Geometry::LineString(l) => line(l),
        Geometry::MultiLineString(m) => !m.0.is_empty() && m.0.iter().all(line),
        _ => false,
    }
}

/// `a` against a polygonal `b`, if a fast path covers them.
fn intersects_fast(a: &Geom, b: &Geom) -> Option<bool> {
    let pb = match &b.geometry {
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) => b.prepared()?,
        _ => return None,
    };
    let lines = |ls: &mut dyn Iterator<Item = &LineString<f64>>| {
        for l in ls {
            if l.0.iter().any(|c| pb.position(*c) != Position::Exterior) {
                return true;
            }
            if l.lines().any(|s| pb.segment_meets_edge(s)) {
                return true;
            }
        }
        false
    };
    Some(match &a.geometry {
        Geometry::Point(p) => pb.position(p.0) != Position::Exterior,
        Geometry::MultiPoint(m) => m.0.iter().any(|p| pb.position(p.0) != Position::Exterior),
        Geometry::Line(l) => {
            pb.position(l.start) != Position::Exterior || pb.segment_meets_edge(*l)
        }
        Geometry::LineString(l) => lines(&mut std::iter::once(l)),
        Geometry::MultiLineString(m) => lines(&mut m.0.iter()),
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) => {
            let pa = a.prepared()?;
            pa.ring_vertices()
                .any(|c| pb.position(c) != Position::Exterior)
                || pb
                    .ring_vertices()
                    .any(|c| pa.position(c) != Position::Exterior)
                || pa.edges_meet(pb)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::literal::{Crs, Geom};

    fn g(wkt: &str) -> Geom {
        use std::str::FromStr;
        let w = wkt::Wkt::<f64>::from_str(wkt).unwrap();
        Geom::new(Geometry::try_from(w).unwrap(), Crs::Crs84)
    }

    /// Every fast path against `relate()` on a grid of points and a few shapes.
    #[test]
    fn fast_paths_agree_with_relate() {
        let shapes = [
            "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0), (3 3, 6 3, 6 6, 3 6, 3 3))",
            "MULTIPOLYGON(((0 0, 4 0, 4 4, 0 4, 0 0)), ((6 6, 9 6, 9 9, 6 9, 6 6)))",
            "POLYGON((0 0, 10 0, 5 8, 0 0))",
            "LINESTRING(0 0, 5 5, 10 0)",
            "LINESTRING(0 0, 10 0, 10 10, 0 0)",
            "POINT(5 5)",
        ];
        let probes: Vec<Geom> = (-2..=12)
            .flat_map(|x| (-2..=12).map(move |y| (x, y)))
            .map(|(x, y)| g(&format!("POINT({} {})", f64::from(x), f64::from(y))))
            .chain(
                [
                    "LINESTRING(-1 -1, 11 11)",
                    "LINESTRING(4 4, 5 5)",
                    "LINESTRING(20 20, 30 30)",
                    "LINESTRING(1 1, 2 2)",
                    "LINESTRING(-5 5, -1 5)",
                    "POLYGON((4 4, 5 4, 5 5, 4 5, 4 4))",
                    "POLYGON((-5 -5, 20 -5, 20 20, -5 20, -5 -5))",
                    "POLYGON((20 20, 30 20, 30 30, 20 20))",
                    "POLYGON((9 9, 12 9, 12 12, 9 12, 9 9))",
                    "MULTIPOINT((1 1), (50 50))",
                    "MULTIPOINT((1 1), (2 2))",
                    "LINESTRING(1 1, 1 9, 2 9)",
                    "LINESTRING(1 1, 8 1, 8 8)",
                    "LINESTRING(2 2, 7 7)",
                    "LINESTRING(1 3, 9 3)",
                    "LINESTRING(0 1, 0 9)",
                    "LINESTRING(1 1, 3 3)",
                    "LINESTRING(2 5, 8 5.5)",
                    "LINESTRING(1 7, 9 7)",
                    "MULTILINESTRING((1 1, 2 1), (8 8, 9 8))",
                    "MULTILINESTRING((1 1, 2 1), (8 8, 11 8))",
                    // Polygons whose boundaries do not meet the shapes' (#703).
                    "POLYGON((1 1, 2 1, 2 2, 1 2, 1 1))",
                    "POLYGON((2 2, 7 2, 7 7, 2 7, 2 2))",
                    "POLYGON((2 2, 7 2, 7 7, 2 7, 2 2), (2.5 2.5, 6.5 2.5, 6.5 6.5, 2.5 6.5, 2.5 2.5))",
                    "POLYGON((1 1, 9 1, 9 9, 1 9, 1 1), (2 2, 8 2, 8 8, 2 8, 2 2))",
                    "POLYGON((-1 -1, 11 -1, 11 11, -1 11, -1 -1), (-0.5 -0.5, 10.5 -0.5, 10.5 10.5, -0.5 10.5, -0.5 -0.5))",
                    "MULTIPOLYGON(((1 1, 2 1, 2 2, 1 2, 1 1)), ((20 20, 21 20, 21 21, 20 21, 20 20)))",
                    "MULTIPOLYGON(((4.5 4.5, 5.5 4.5, 5.5 5.5, 4.5 5.5, 4.5 4.5)), ((1 1, 2 1, 2 2, 1 2, 1 1)))",
                ]
                .iter()
                .map(|w| g(w)),
            )
            .collect();
        for s in shapes {
            let s = g(s);
            for p in &probes {
                let full = p.geometry.relate(&s.geometry);
                assert_eq!(
                    format!("{:?}", relate(p, &s)),
                    format!("{full:?}"),
                    "{:?} {:?}",
                    p.geometry,
                    s.geometry
                );
                assert_eq!(
                    format!("{:?}", relate(&s, p)),
                    format!("{:?}", s.geometry.relate(&p.geometry)),
                );
                assert_eq!(intersects(p, &s), full.is_intersects(), "{:?}", p.geometry);
                assert_eq!(intersects(&s, p), full.is_intersects(), "{:?}", p.geometry);
                // Every relation, shortcuts included, against the full matrix.
                for (x, y) in [(p, &s), (&s, p)] {
                    let m = x.geometry.relate(&y.geometry);
                    for (name, expected) in [
                        ("ST_Within", m.is_within()),
                        ("ST_Contains", m.is_contains()),
                        ("ST_Covers", m.is_covers()),
                        ("ST_CoveredBy", m.is_coveredby()),
                        ("ST_Touches", m.is_touches()),
                        ("ST_Disjoint", m.is_disjoint()),
                    ] {
                        assert_eq!(
                            crate::predicate::simple_features(name, x, y),
                            Some(expected),
                            "{name} {:?} {:?}",
                            x.geometry,
                            y.geometry
                        );
                    }
                    for (name, patterns) in crate::geosparql11::RELATIONS {
                        let expected = patterns.iter().any(|q| m.matches(q).unwrap());
                        assert_eq!(
                            crate::predicate::patterns(patterns, x, y),
                            Some(expected),
                            "{name} {:?} {:?}",
                            x.geometry,
                            y.geometry
                        );
                    }
                }
            }
        }
    }

    /// Lines and points strictly inside a polygon take the fast "true" path.
    #[test]
    fn interior_shortcut() {
        let poly = g("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0), (3 3, 6 3, 6 6, 3 6, 3 3))");
        for (wkt, inside) in [
            ("POINT(1 1)", true),
            ("POINT(0 5)", false),
            ("POINT(4 4)", false),
            ("MULTIPOINT((1 1), (9 9))", true),
            ("LINESTRING(1 1, 1 9, 9 9)", true),
            ("LINESTRING(1 1, 9 9)", false),
            ("LINESTRING(0 1, 0 9)", false),
            ("LINESTRING(1 1, 3 3)", false),
            ("LINESTRING(1 1, 11 1)", false),
            ("MULTILINESTRING((1 1, 2 1), (8 8, 9 8))", true),
            ("MULTILINESTRING((1 1, 2 1), (8 8, 11 8))", false),
            ("POLYGON((1 1, 2 1, 2 2, 1 1))", false),
        ] {
            let x = g(wkt);
            assert_eq!(in_interior(&x, &poly), inside, "{wkt}");
            if inside {
                assert!(x.geometry.relate(&poly.geometry).is_within(), "{wkt}");
            }
        }
        // A concave polygon: both vertices inside, the segment leaves it.
        let notch = g("POLYGON((0 0, 10 0, 10 10, 5 2, 0 10, 0 0))");
        assert!(!in_interior(&g("LINESTRING(1 8, 9 8)"), &notch));
        assert!(in_interior(&g("LINESTRING(1 1, 9 1)"), &notch));
        // Not a polygon: no shortcut.
        assert!(!in_interior(&g("POINT(1 1)"), &g("LINESTRING(0 0, 2 2)")));
    }

    #[test]
    fn many_edges() {
        // A 2000-gon around (0, 0): many bands.
        let n = 2000;
        let ring: Vec<String> = (0..=n)
            .map(|i| {
                let a = 2.0 * std::f64::consts::PI * f64::from(i % n) / f64::from(n);
                format!("{} {}", 10.0 * a.cos(), 10.0 * a.sin())
            })
            .collect();
        let poly = g(&format!("POLYGON(({}))", ring.join(",")));
        for (x, y) in [
            (0.0, 0.0),
            (9.9, 0.0),
            (10.1, 0.0),
            (7.0, 7.0),
            (7.2, 7.2),
            (0.0, -9.99),
        ] {
            let p = g(&format!("POINT({x} {y})"));
            assert_eq!(
                intersects(&p, &poly),
                p.geometry.relate(&poly.geometry).is_intersects(),
                "{x} {y}"
            );
        }
    }

    /// A large polygon keeps its topology graph between calls (#703): the matrices are those
    /// of `relate()`, in both orders, from the first call and from the kept graph, and a
    /// different polygon at the same address is not taken for it.
    #[test]
    fn kept_graphs_agree_with_relate() {
        let gon = |n: u32, r: f64, dx: f64| {
            let ring: Vec<String> = (0..=n)
                .map(|i| {
                    let a = 2.0 * std::f64::consts::PI * f64::from(i % n) / f64::from(n);
                    format!("{} {}", dx + r * a.cos(), r * a.sin())
                })
                .collect();
            g(&format!("POLYGON(({}))", ring.join(",")))
        };
        let probes = [
            "POLYGON((-1 -1, 1 -1, 1 1, -1 1, -1 -1))",
            "POLYGON((8 -1, 12 -1, 12 1, 8 1, 8 -1))",
            "POLYGON((20 20, 21 20, 21 21, 20 20))",
            "LINESTRING(-20 0, 20 0)",
            "LINESTRING(0 0, 5 5)",
            "POINT(10 0)",
            "MULTIPOLYGON(((-1 -1, 1 -1, 1 1, -1 1, -1 -1)), ((30 0, 31 0, 31 1, 30 0)))",
        ];
        for (n, dx) in [(700, 0.0), (900, 3.0)] {
            let big = gon(n, 10.0, dx);
            let other = gon(300, 4.0, 2.0);
            for p in probes.iter().map(|w| g(w)).chain([other]) {
                for _ in 0..2 {
                    assert_eq!(
                        format!("{:?}", relate(&big, &p)),
                        format!("{:?}", big.geometry.relate(&p.geometry)),
                        "{:?}",
                        p.geometry
                    );
                    assert_eq!(
                        format!("{:?}", relate(&p, &big)),
                        format!("{:?}", p.geometry.relate(&big.geometry)),
                        "{:?}",
                        p.geometry
                    );
                }
            }
        }
    }
}
