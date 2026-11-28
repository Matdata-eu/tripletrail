use crate::geosparql11::literal_like;
use crate::literal::{Crs, Geom, parse_wkt_literal, wkt_literal};
use geo::algorithm::dimensions::Dimensions;
use geo::line_measures::{Distance, Euclidean, Geodesic, InterpolateLine, Length};
use geo::{
    Area, BoundingRect, Buffer, Centroid, Closest, ClosestPoint, ConvexHull, Coord, GeodesicArea,
    Geometry, HasDimensions, LineLocatePoint, LineString, MapCoords, Point, Relate, Simplify,
};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, Term};
use oxigraph::sparql::SparqlEvaluator;
use std::sync::Arc;

/// GeoSPARQL function namespace.
pub const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
/// Namespace for PostGIS-named functions (`st:ST_LineLocatePoint`, ...).
pub const ST: &str = "http://w3id.org/tripletrail/st#";
const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";

/// A registered function: SPARQL arguments in, value out (`None` is a SPARQL error).
pub type Func = Arc<dyn Fn(&[Term]) -> Option<Term> + Send + Sync>;

pub(crate) fn func(f: impl Fn(&[Term]) -> Option<Term> + Send + Sync + 'static) -> Func {
    Arc::new(f)
}

/// All functions of this crate, by IRI.
pub type FunctionTable = Vec<(NamedNode, Func)>;

static BY_IRI: std::sync::LazyLock<std::collections::HashMap<String, Func>> =
    std::sync::LazyLock::new(|| {
        function_table()
            .into_iter()
            .map(|(n, f)| (n.into_string(), f))
            .collect()
    });

/// Looks a function up by IRI, e.g. to evaluate a predicate outside the SPARQL evaluator.
pub fn function(iri: &str) -> Option<Func> {
    BY_IRI.get(iri).cloned()
}

/// Registers every function of [`function_table`] and every aggregate of
/// [`crate::aggregate_functions`] on the evaluator.
pub fn register(evaluator: SparqlEvaluator) -> SparqlEvaluator {
    register_with_source(evaluator, None)
}

/// [`register`], with the functions reading geometry literals from `source` first (see
/// [`crate::with_geometry_source`]).
pub fn register_with_source(
    evaluator: SparqlEvaluator,
    source: Option<Arc<crate::GeometrySource>>,
) -> SparqlEvaluator {
    register_with(evaluator, source, None)
}

/// [`register_with_source`], with the functions stopping once `cancel` says so: a call
/// then has no value, and functions that loop for long (`ST_GeometricMedian`,
/// `ST_GeneratePoints`, ...) end early. The evaluator, with the same token, then fails the
/// query as cancelled.
pub fn register_with(
    mut evaluator: SparqlEvaluator,
    source: Option<Arc<crate::GeometrySource>>,
    cancel: Option<crate::cancel::Check>,
) -> SparqlEvaluator {
    for (name, f) in function_table() {
        let f: Func = match &cancel {
            Some(check) => {
                let check = Arc::clone(check);
                Arc::new(move |args: &[Term]| {
                    if check() {
                        return None;
                    }
                    crate::cancel::scope(Some(Arc::clone(&check)), || f(args))
                })
            }
            None => f,
        };
        evaluator = match &source {
            Some(source) => {
                let source = Arc::clone(source);
                evaluator.with_custom_function(name, move |args| {
                    crate::with_geometry_source(&*source, || f(args))
                })
            }
            None => evaluator.with_custom_function(name, move |args| f(args)),
        };
    }
    for (name, make) in crate::geosparql11::aggregate_functions() {
        evaluator = evaluator.with_custom_aggregate_function(name, make);
    }
    evaluator
}

const EARTH_RADIUS: f64 = 6_371_008.8;

/// Local equirectangular projection to metres, accurate enough at railway-network scale.
/// Used for metric distance and buffer on geographic coordinates until PROJ is wired in.
struct LocalMetric {
    lon0: f64,
    lat0: f64,
    kx: f64,
    ky: f64,
}

impl LocalMetric {
    fn around(geoms: &[&Geometry<f64>]) -> Option<Self> {
        let mut rect: Option<geo::Rect<f64>> = None;
        for g in geoms {
            let r = g.bounding_rect()?;
            rect = Some(match rect {
                None => r,
                Some(acc) => geo::Rect::new(
                    Coord {
                        x: acc.min().x.min(r.min().x),
                        y: acc.min().y.min(r.min().y),
                    },
                    Coord {
                        x: acc.max().x.max(r.max().x),
                        y: acc.max().y.max(r.max().y),
                    },
                ),
            });
        }
        let c = rect?.center();
        let ky = EARTH_RADIUS * std::f64::consts::PI / 180.0;
        Some(Self {
            lon0: c.x,
            lat0: c.y,
            kx: ky * c.y.to_radians().cos(),
            ky,
        })
    }

    fn forward(&self, g: &Geometry<f64>) -> Geometry<f64> {
        g.map_coords(|c| Coord {
            x: (c.x - self.lon0) * self.kx,
            y: (c.y - self.lat0) * self.ky,
        })
    }

    fn inverse(&self, g: &Geometry<f64>) -> Geometry<f64> {
        g.map_coords(|c| Coord {
            x: c.x / self.kx + self.lon0,
            y: c.y / self.ky + self.lat0,
        })
    }
}

pub(crate) fn geom(t: &Term) -> Option<Arc<Geom>> {
    parse_wkt_literal(t)
}

pub(crate) fn num(t: &Term) -> Option<f64> {
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype();
    if dt == xsd::DOUBLE
        || dt == xsd::DECIMAL
        || dt == xsd::INTEGER
        || dt == xsd::FLOAT
        || dt.as_str().starts_with("http://www.w3.org/2001/XMLSchema#")
            && l.value().parse::<f64>().is_ok()
    {
        l.value().parse().ok()
    } else {
        None
    }
}

pub(crate) fn double(v: f64) -> Option<Term> {
    v.is_finite().then(|| Literal::from(v).into())
}

pub(crate) fn boolean(v: bool) -> Option<Term> {
    Some(Literal::from(v).into())
}

pub(crate) fn integer(v: i64) -> Option<Term> {
    Some(Literal::from(v).into())
}

/// Two geometries comparable in one coordinate space.
pub(crate) fn same_space(a: &Geom, b: &Geom) -> bool {
    a.crs == b.crs || (a.crs.is_geographic() && b.crs.is_geographic())
}

fn unit_iri(t: Option<&Term>) -> Option<&str> {
    match t {
        None => Some("metre"),
        Some(Term::NamedNode(n)) => n.as_str().strip_prefix(UOM),
        _ => None,
    }
}

/// WGS84 semi-major axis and first eccentricity squared.
const WGS84_A: f64 = 6_378_137.0;
const WGS84_E2: f64 = 0.006_694_379_990_141_316;

/// Metres per degree of latitude and of longitude at latitude `lat` (degrees), on the WGS84
/// ellipsoid: the meridional radius and the radius of the parallel.
fn metres_per_degree(lat: f64) -> (f64, f64) {
    let (s, c) = lat.to_radians().sin_cos();
    let w = 1.0 - WGS84_E2 * s * s;
    let n = WGS84_A / w.sqrt();
    let m = WGS84_A * (1.0 - WGS84_E2) / (w * w.sqrt());
    let k = std::f64::consts::PI / 180.0;
    (m * k, n * c * k)
}

/// A plane tangent to the WGS84 ellipsoid at one point, in metres: exact at the point, and
/// within a millimetre over tens of metres around it.
struct Tangent {
    origin: Coord<f64>,
    kx: f64,
    ky: f64,
}

impl Tangent {
    fn at(origin: Coord<f64>) -> Self {
        let (ky, kx) = metres_per_degree(origin.y);
        Self { origin, kx, ky }
    }

    fn forward(&self, g: &Geometry<f64>) -> Geometry<f64> {
        g.map_coords(|c| Coord {
            x: (c.x - self.origin.x) * self.kx,
            y: (c.y - self.origin.y) * self.ky,
        })
    }

    fn inverse(&self, c: Coord<f64>) -> Coord<f64> {
        Coord {
            x: c.x / self.kx + self.origin.x,
            y: c.y / self.ky + self.origin.y,
        }
    }
}

/// Up to this distance the tangent plane at the point gives the distance to well under a
/// millimetre; beyond, the closest point found in it is measured on the ellipsoid.
const TANGENT_EXACT: f64 = 50.0;

/// Distance in metres from a point to a geometry on geographic coordinates: the closest point
/// is found in the plane tangent at the point, and its distance measured on the ellipsoid
/// (Karney's geodesic), as PostGIS does for `geography` on the spheroid.
fn point_metric_distance(p: Point<f64>, g: &Geometry<f64>) -> Option<f64> {
    if let Geometry::Point(q) = g {
        return Some(Geodesic.distance(p, *q));
    }
    let plane = Tangent::at(p.0);
    let projected = plane.forward(g);
    match projected.closest_point(&Point::new(0.0, 0.0)) {
        Closest::Intersection(_) => Some(0.0),
        Closest::SinglePoint(c) => {
            let d = c.x().hypot(c.y());
            Some(if d <= TANGENT_EXACT {
                d
            } else {
                Geodesic.distance(p, Point(plane.inverse(c.0)))
            })
        }
        Closest::Indeterminate => Some(Euclidean.distance(&Point::new(0.0, 0.0), &projected)),
    }
}

pub(crate) fn metric_distance(a: &Geom, b: &Geom) -> Option<f64> {
    if !same_space(a, b) {
        return None;
    }
    if !a.crs.is_geographic() {
        return Some(Euclidean.distance(&a.geometry, &b.geometry));
    }
    match (&a.geometry, &b.geometry) {
        (Geometry::Point(p), g) | (g, Geometry::Point(p)) => point_metric_distance(*p, g),
        (a, b)
            if !has_area(a) && !has_area(b) && boundary(a).is_some() && boundary(b).is_some() =>
        {
            line_metric_distance(&boundary(a)?, &boundary(b)?)
        }
        (a, b) => {
            // With an area, or points among other parts: first in the plane tangent at the
            // middle of both boxes, as geo measures it (an area holding any part of the other
            // gives 0 there).
            let (ra, rb) = (a.bounding_rect()?, b.bounding_rect()?);
            let (lo, hi) = (ra.min().y.min(rb.min().y), ra.max().y.max(rb.max().y));
            let plane = Tangent::at(Coord {
                x: (ra.min().x.min(rb.min().x) + ra.max().x.max(rb.max().x)) / 2.0,
                y: (lo + hi) / 2.0,
            });
            let planar = Euclidean.distance(&plane.forward(a), &plane.forward(b));
            // The plane is an affine map: geometries that meet in it meet. In a small box it
            // is within a millimetre (`drift` is twice the scale change over the whole span,
            // a point is at most half the span from the middle: a quarter of it); otherwise,
            // apart, the distance is that between the rings and lines, measured where they
            // come closest.
            if planar == 0.0 || (planar * drift(lo, hi) / 4.0 < 1e-3 && planar <= TANGENT_EXACT) {
                return Some(planar);
            }
            match (boundary(a), boundary(b)) {
                (Some(sa), Some(sb)) => line_metric_distance(&sa, &sb),
                _ => Some(planar),
            }
        }
    }
}

/// How far off a distance in the plane tangent at the middle of latitudes `lo` to `hi` may
/// be, relative to it: the scale of a degree of longitude changes by tan(latitude) per
/// radian of latitude.
fn drift(lo: f64, hi: f64) -> f64 {
    let span = (hi - lo).to_radians();
    let lat = lo.abs().max(hi.abs()).min(89.0).to_radians();
    2.0 * span * lat.tan() + span * span + 1e-9
}

/// Whether a geometry has an interior of two dimensions (a polygon, possibly in a
/// collection).
fn has_area(g: &Geometry<f64>) -> bool {
    match g {
        Geometry::Polygon(_)
        | Geometry::MultiPolygon(_)
        | Geometry::Rect(_)
        | Geometry::Triangle(_) => true,
        Geometry::GeometryCollection(c) => c.iter().any(has_area),
        _ => false,
    }
}

/// A segment, as its two ends.
type Segment = (Coord<f64>, Coord<f64>);

/// The segments of a geometry's lines and of its polygons' rings, or `None` if it has a point
/// (or nothing): the distance between two such geometries that don't meet is that between
/// their segments.
fn boundary(g: &Geometry<f64>) -> Option<Vec<Segment>> {
    fn add(g: &Geometry<f64>, out: &mut Vec<Segment>) -> Option<()> {
        let mut line = |l: &LineString<f64>| -> Option<()> {
            match l.0.as_slice() {
                [] | [_] => None,
                c => {
                    out.extend(c.windows(2).map(|w| (w[0], w[1])));
                    Some(())
                }
            }
        };
        match g {
            Geometry::Line(l) => out.push((l.start, l.end)),
            Geometry::LineString(l) => line(l)?,
            Geometry::MultiLineString(m) => {
                for l in &m.0 {
                    line(l)?;
                }
            }
            Geometry::Polygon(p) => {
                line(p.exterior())?;
                for r in p.interiors() {
                    line(r)?;
                }
            }
            Geometry::MultiPolygon(m) => {
                for p in &m.0 {
                    add(&Geometry::Polygon(p.clone()), out)?;
                }
            }
            Geometry::Rect(r) => add(&Geometry::Polygon(r.to_polygon()), out)?,
            Geometry::Triangle(t) => add(&Geometry::Polygon(t.to_polygon()), out)?,
            Geometry::GeometryCollection(c) => {
                for g in c {
                    add(g, out)?;
                }
            }
            Geometry::Point(_) | Geometry::MultiPoint(_) => return None,
        }
        Some(())
    }
    let mut out = Vec::new();
    add(g, &mut out)?;
    (!out.is_empty()).then_some(out)
}

/// The closest points of segments `a` and `b` in a plane, and their distance (0 where they
/// meet).
fn segment_closest(
    a: (Coord<f64>, Coord<f64>),
    b: (Coord<f64>, Coord<f64>),
) -> (f64, Coord<f64>, Coord<f64>) {
    let cross = |o: Coord<f64>, p: Coord<f64>, q: Coord<f64>| {
        (p.x - o.x) * (q.y - o.y) - (p.y - o.y) * (q.x - o.x)
    };
    let (d1, d2) = (cross(b.0, b.1, a.0), cross(b.0, b.1, a.1));
    let (d3, d4) = (cross(a.0, a.1, b.0), cross(a.0, a.1, b.1));
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        let t = d1 / (d1 - d2);
        let c = Coord {
            x: a.0.x + (a.1.x - a.0.x) * t,
            y: a.0.y + (a.1.y - a.0.y) * t,
        };
        return (0.0, c, c);
    }
    // Apart (or touching at an end): the closest pair has an end of one of them.
    let onto = |p: Coord<f64>, s: (Coord<f64>, Coord<f64>)| {
        let (dx, dy) = (s.1.x - s.0.x, s.1.y - s.0.y);
        let len = dx * dx + dy * dy;
        let t = if len > 0.0 {
            (((p.x - s.0.x) * dx + (p.y - s.0.y) * dy) / len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let q = Coord {
            x: s.0.x + dx * t,
            y: s.0.y + dy * t,
        };
        ((p.x - q.x).hypot(p.y - q.y), q)
    };
    let mut best = (f64::INFINITY, a.0, b.0);
    for (p, s, p_on_a) in [
        (a.0, b, true),
        (a.1, b, true),
        (b.0, a, false),
        (b.1, a, false),
    ] {
        let (d, q) = onto(p, s);
        if d < best.0 {
            best = if p_on_a { (d, p, q) } else { (d, q, p) };
        }
    }
    best
}

/// Distance in metres between lines on geographic coordinates. The segments are compared in
/// the plane tangent at the middle of both, whose scale drifts away from its centre (by
/// about 1 % a degree of latitude at 50°N), so it only picks the segment pairs that may be
/// closest; each of those is measured in the plane tangent at its own middle, and on the
/// ellipsoid beyond [`TANGENT_EXACT`], as a point is (#350).
fn line_metric_distance(a: &[Segment], b: &[Segment]) -> Option<f64> {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut west, mut east) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(p, q) in a.iter().chain(b) {
        lo = lo.min(p.y).min(q.y);
        hi = hi.max(p.y).max(q.y);
        west = west.min(p.x).min(q.x);
        east = east.max(p.x).max(q.x);
    }
    if !(lo.is_finite() && hi.is_finite() && west.is_finite() && east.is_finite()) {
        return None;
    }
    let centre = Coord {
        x: (west + east) / 2.0,
        y: (lo + hi) / 2.0,
    };
    let plane = Tangent::at(centre);
    let forward = |c: Coord<f64>| Coord {
        x: (c.x - centre.x) * plane.kx,
        y: (c.y - centre.y) * plane.ky,
    };
    let drift = drift(lo, hi);
    let project = |s: &[Segment]| -> Vec<(Segment, [f64; 4])> {
        s.iter()
            .map(|&(p, q)| {
                let (p, q) = (forward(p), forward(q));
                (
                    (p, q),
                    [p.x.min(q.x), p.y.min(q.y), p.x.max(q.x), p.y.max(q.y)],
                )
            })
            .collect()
    };
    let (pa, pb) = (project(a), project(b));
    // Planar distances within `bound(d)` of the smallest planar one `d` may be the smallest
    // on the ellipsoid.
    let bound = |d: f64| d * (1.0 + drift) / (1.0 - drift).max(0.5) + 1e-6;
    // First pass: the smallest planar distance and the pairs that may come near it. Pairs
    // are found through a tree of `b`'s segments when there are many.
    let mut planar = f64::INFINITY;
    // (segment of `a`, segment of `b`, planar distance, closest points)
    let mut pairs: Vec<(usize, usize, f64, Segment)> = Vec::new();
    let mut visit = |i: usize, j: usize, planar: &mut f64| -> bool {
        let ((sa, ba), (sb, bb)) = (&pa[i], &pb[j]);
        let dx = (ba[0] - bb[2]).max(bb[0] - ba[2]).max(0.0);
        let dy = (ba[1] - bb[3]).max(bb[1] - ba[3]).max(0.0);
        if dx.hypot(dy) > bound(*planar) {
            return false;
        }
        let (d, p, q) = segment_closest(*sa, *sb);
        if d <= bound(*planar) {
            *planar = planar.min(d);
            pairs.push((i, j, d, (p, q)));
        }
        // The plane is an affine map: segments that meet in it meet.
        d == 0.0
    };
    // A first bound: from the end of `a` nearest the middle of `b`'s box to the nearest end
    // of `b`. Only segments within reach of the other's box can then be closest.
    let boxed = |s: &[(Segment, [f64; 4])]| {
        s.iter().fold(
            [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
            |r, (_, b)| {
                [
                    r[0].min(b[0]),
                    r[1].min(b[1]),
                    r[2].max(b[2]),
                    r[3].max(b[3]),
                ]
            },
        )
    };
    let (box_a, box_b) = (boxed(&pa), boxed(&pb));
    let gap = |b: &[f64; 4], r: &[f64; 4]| {
        let dx = (b[0] - r[2]).max(r[0] - b[2]).max(0.0);
        let dy = (b[1] - r[3]).max(r[1] - b[3]).max(0.0);
        dx.hypot(dy)
    };
    let nearest = |s: &[(Segment, [f64; 4])], c: Coord<f64>| {
        s.iter().map(|((p, _), _)| *p).min_by(|p, q| {
            (p.x - c.x)
                .hypot(p.y - c.y)
                .total_cmp(&(q.x - c.x).hypot(q.y - c.y))
        })
    };
    let from = nearest(
        &pa,
        Coord {
            x: (box_b[0] + box_b[2]) / 2.0,
            y: (box_b[1] + box_b[3]) / 2.0,
        },
    )?;
    let to = nearest(&pb, from)?;
    let seed = bound((from.x - to.x).hypot(from.y - to.y));
    let ia: Vec<usize> = (0..pa.len())
        .filter(|&i| gap(&pa[i].1, &box_b) <= seed)
        .collect();
    let ib: Vec<usize> = (0..pb.len())
        .filter(|&j| gap(&pb[j].1, &box_a) <= seed)
        .collect();
    if ia.len() * ib.len() <= 1024 {
        for &i in &ia {
            for &j in &ib {
                if visit(i, j, &mut planar) {
                    return Some(0.0);
                }
            }
        }
    } else {
        use rstar::primitives::{GeomWithData, Rectangle};
        use rstar::{AABB, RTree};
        let tree = RTree::bulk_load(
            ib.iter()
                .map(|&j| {
                    let r = pb[j].1;
                    GeomWithData::new(Rectangle::from_corners([r[0], r[1]], [r[2], r[3]]), j)
                })
                .collect(),
        );
        for &i in &ia {
            let (r, e) = (pa[i].1, bound(planar).min(seed));
            let envelope = AABB::from_corners([r[0] - e, r[1] - e], [r[2] + e, r[3] + e]);
            for n in tree.locate_in_envelope_intersecting(envelope) {
                if visit(i, n.data, &mut planar) {
                    return Some(0.0);
                }
            }
        }
    }
    if !planar.is_finite() {
        return None;
    }
    // In a small box the plane is exact enough.
    if planar * drift < 1e-4 && planar <= TANGENT_EXACT {
        return Some(planar);
    }
    // Second pass: each pair near the smallest measured again in the plane tangent between
    // its two closest points, twice: the second time between the closest points found the
    // first time.
    let mut best = f64::INFINITY;
    for &(i, j, d, (p, q)) in &pairs {
        if d > bound(planar) {
            continue;
        }
        let (s, t) = (a[i], b[j]);
        let mut mid = plane.inverse(Coord {
            x: (p.x + q.x) / 2.0,
            y: (p.y + q.y) / 2.0,
        });
        let mut d = f64::INFINITY;
        for _ in 0..2 {
            let local = Tangent::at(mid);
            let to = |c: Coord<f64>| Coord {
                x: (c.x - mid.x) * local.kx,
                y: (c.y - mid.y) * local.ky,
            };
            let (e, p, q) = segment_closest((to(s.0), to(s.1)), (to(t.0), to(t.1)));
            let (p, q) = (local.inverse(p), local.inverse(q));
            d = if e <= TANGENT_EXACT {
                e
            } else {
                Geodesic.distance(Point(p), Point(q))
            };
            mid = Coord {
                x: (p.x + q.x) / 2.0,
                y: (p.y + q.y) / 2.0,
            };
        }
        best = best.min(d);
    }
    best.is_finite().then_some(best)
}

/// Length of a segment on the WGS84 ellipsoid. Short segments (railway and road geometry)
/// use the mid-latitude ellipsoidal formula, which agrees with the exact geodesic to about
/// 1e-7 of their length below 2 km and costs a fraction of it; longer ones use Karney's.
fn segment_length(a: Coord<f64>, b: Coord<f64>) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let lat = (a.y + b.y) / 2.0;
    if dx.abs() < 0.02 && dy.abs() < 0.02 && lat.abs() < 85.0 {
        let (ky, kx) = metres_per_degree(lat);
        (dx * kx).hypot(dy * ky)
    } else {
        Geodesic.distance(Point(a), Point(b))
    }
}

fn ring_length(l: &LineString<f64>) -> f64 {
    l.0.windows(2).map(|w| segment_length(w[0], w[1])).sum()
}

fn metric_length(g: &Geom) -> f64 {
    fn planar(g: &Geometry<f64>) -> f64 {
        match g {
            Geometry::Line(l) => Euclidean.length(l),
            Geometry::LineString(l) => Euclidean.length(l),
            Geometry::MultiLineString(l) => Euclidean.length(l),
            Geometry::Polygon(p) => p
                .exterior()
                .0
                .windows(2)
                .map(|w| Euclidean.distance(Point(w[0]), Point(w[1])))
                .sum(),
            Geometry::GeometryCollection(c) => c.iter().map(planar).sum(),
            _ => 0.0,
        }
    }
    fn geodesic(g: &Geometry<f64>) -> f64 {
        match g {
            Geometry::Line(l) => segment_length(l.start, l.end),
            Geometry::LineString(l) => ring_length(l),
            Geometry::MultiLineString(l) => l.0.iter().map(ring_length).sum(),
            Geometry::Polygon(p) => ring_length(p.exterior()),
            Geometry::GeometryCollection(c) => c.iter().map(geodesic).sum(),
            _ => 0.0,
        }
    }
    if g.crs.is_geographic() {
        geodesic(&g.geometry)
    } else {
        planar(&g.geometry)
    }
}

pub(crate) fn as_linestring(g: &Geometry<f64>) -> Option<&LineString<f64>> {
    match g {
        Geometry::LineString(l) => Some(l),
        _ => None,
    }
}

fn line_substring(l: &LineString<f64>, from: f64, to: f64) -> Option<Geometry<f64>> {
    if !(0.0..=1.0).contains(&from) || !(0.0..=1.0).contains(&to) || from > to {
        return None;
    }
    let total = Euclidean.length(l);
    if total == 0.0 {
        return Some(Geometry::Point(Point(*l.0.first()?)));
    }
    let (a, b) = (from * total, to * total);
    let mut out = Vec::new();
    let mut walked = 0.0;
    for w in l.0.windows(2) {
        let seg = Euclidean.distance(Point(w[0]), Point(w[1]));
        let (s, e) = (walked, walked + seg);
        let at = |d: f64| {
            let t = if seg == 0.0 { 0.0 } else { (d - s) / seg };
            Coord {
                x: w[0].x + (w[1].x - w[0].x) * t,
                y: w[0].y + (w[1].y - w[0].y) * t,
            }
        };
        if e >= a && s <= b {
            if out.is_empty() {
                out.push(at(a.max(s)));
            }
            out.push(at(b.min(e)));
        }
        walked = e;
        if walked > b {
            break;
        }
    }
    out.dedup();
    Some(if out.len() == 1 {
        Geometry::Point(Point(out[0]))
    } else {
        Geometry::LineString(LineString(out))
    })
}

pub fn function_table() -> FunctionTable {
    let mut t: FunctionTable = Vec::new();
    let mut add = |ns: &str, name: &str, f: Func| {
        t.push((NamedNode::new_unchecked(format!("{ns}{name}")), f));
    };

    // Simple Features topological relations (GeoSPARQL 1.1 §9 and PostGIS ST_<Relation>),
    // with fast paths for points and polygons (predicate.rs, fast.rs).
    let relations: [(&str, &str); 10] = [
        ("sfEquals", "ST_Equals"),
        ("sfDisjoint", "ST_Disjoint"),
        ("sfIntersects", "ST_Intersects"),
        ("sfTouches", "ST_Touches"),
        ("sfCrosses", "ST_Crosses"),
        ("sfWithin", "ST_Within"),
        ("sfContains", "ST_Contains"),
        ("sfOverlaps", "ST_Overlaps"),
        // geof:ehCovers/ehCoveredBy use GeoSPARQL's stricter patterns (geosparql11.rs).
        ("", "ST_Covers"),
        ("", "ST_CoveredBy"),
    ];
    for (geof_name, st_name) in relations {
        let f: Func = func(move |args: &[Term]| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            boolean(crate::predicate::simple_features(st_name, &a, &b)?)
        });
        if !geof_name.is_empty() {
            add(GEOF, geof_name, Arc::clone(&f));
        }
        add(ST, st_name, f);
    }

    // Whether the bounding boxes of two geometries meet, as PostGIS's `&&` operator: the
    // test of Jena's spatial index (`spatial:withinBox` searching for features).
    add(
        ST,
        "ST_EnvelopesIntersect",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            boolean(crate::predicate::envelopes_intersect(&a, &b)?)
        }),
    );

    add(
        GEOF,
        "relate",
        func(|args| {
            let [a, b, Term::Literal(p)] = args else {
                return None;
            };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            boolean(a.geometry.relate(&b.geometry).matches(p.value()).ok()?)
        }),
    );

    // Measurement.
    add(
        GEOF,
        "distance",
        func(|args| {
            let (a, b, unit) = match args {
                [a, b] => (a, b, None),
                [a, b, u] => (a, b, Some(u)),
                _ => return None,
            };
            let (a, b) = (geom(a)?, geom(b)?);
            match unit_iri(unit)? {
                "metre" => double(metric_distance(&a, &b)?),
                "kilometre" => double(metric_distance(&a, &b)? / 1000.0),
                "degree" if a.crs.is_geographic() && same_space(&a, &b) => {
                    double(Euclidean.distance(&a.geometry, &b.geometry))
                }
                _ => None,
            }
        }),
    );
    add(
        ST,
        "ST_DistanceSphere",
        func(|args| {
            let [a, b] = args else { return None };
            double(metric_distance(&*geom(a)?, &*geom(b)?)?)
        }),
    );
    add(
        ST,
        "ST_Distance",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            double(Euclidean.distance(&a.geometry, &b.geometry))
        }),
    );
    add(
        ST,
        "ST_DWithin",
        func(|args| {
            let [a, b, d] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            boolean(Euclidean.distance(&a.geometry, &b.geometry) <= num(d)?)
        }),
    );
    add(
        ST,
        "ST_DWithinSphere",
        func(|args| {
            let [a, b, d] = args else { return None };
            boolean(metric_distance(&*geom(a)?, &*geom(b)?)? <= num(d)?)
        }),
    );
    let metric_len: Func = func(|args| {
        let [a] = args else { return None };
        double(metric_length(&*geom(a)?))
    });
    add(GEOF, "metricLength", Arc::clone(&metric_len));
    add(GEOF, "length", Arc::clone(&metric_len));
    add(ST, "ST_LengthSpheroid", metric_len);
    add(
        ST,
        "ST_Length",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            double(metric_length(&Geom::new(
                g.geometry.clone(),
                Crs::Other(String::new()),
            )))
        }),
    );
    let metric_area: Func = func(|args| {
        let [a] = args else { return None };
        double(geom(a)?.metric_area())
    });
    add(GEOF, "metricArea", Arc::clone(&metric_area));
    add(GEOF, "area", metric_area);
    // Perimeter: the length of every ring (GeoSPARQL 1.1 geof:perimeter, metricPerimeter).
    let perimeter: Func = func(|args| {
        let [a] = args else { return None };
        let g = geom(a)?;
        let mut rings = Vec::new();
        let mut push = |p: &geo::Polygon<f64>| {
            rings.push(p.exterior().clone());
            rings.extend(p.interiors().iter().cloned());
        };
        fn walk(g: &Geometry<f64>, push: &mut dyn FnMut(&geo::Polygon<f64>)) {
            match g {
                Geometry::Polygon(p) => push(p),
                Geometry::MultiPolygon(m) => m.0.iter().for_each(&mut *push),
                Geometry::Rect(r) => push(&r.to_polygon()),
                Geometry::Triangle(t) => push(&t.to_polygon()),
                Geometry::GeometryCollection(c) => c.0.iter().for_each(|g| walk(g, push)),
                _ => {}
            }
        }
        walk(&g.geometry, &mut push);
        let rings = Geometry::MultiLineString(geo::MultiLineString(rings));
        double(metric_length(&Geom::new(rings, g.crs.clone())))
    });
    add(GEOF, "metricPerimeter", Arc::clone(&perimeter));
    add(GEOF, "perimeter", perimeter);
    // Size: length of curves, area of surfaces, 0 for points (GeoSPARQL 1.1 geof:size).
    let size: Func = func(|args| {
        let [a] = args else { return None };
        let g = geom(a)?;
        double(match g.geometry.dimensions() {
            Dimensions::TwoDimensional => g.metric_area(),
            Dimensions::OneDimensional => metric_length(&g),
            _ => 0.0,
        })
    });
    add(GEOF, "metricSize", Arc::clone(&size));
    add(GEOF, "size", size);
    add(
        ST,
        "ST_Area",
        func(|args| {
            let [a] = args else { return None };
            double(geom(a)?.geometry.unsigned_area())
        }),
    );

    // Accessors.
    add(
        GEOF,
        "dimension",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            if let Some(s) = &g.surface {
                use crate::literal::SurfaceKind as K;
                return integer(match s.kind {
                    _ if s.solid => 3,
                    K::Point | K::MultiPoint => 0,
                    K::LineString | K::MultiLineString => 1,
                    _ => 2,
                });
            }
            integer(match g.geometry.dimensions() {
                Dimensions::Empty => -1,
                Dimensions::ZeroDimensional => 0,
                Dimensions::OneDimensional => 1,
                Dimensions::TwoDimensional => 2,
            })
        }),
    );
    add(
        GEOF,
        "isEmpty",
        func(|args| {
            let [a] = args else { return None };
            boolean(geom(a)?.geometry.is_empty())
        }),
    );
    add(
        GEOF,
        "getSRID",
        func(|args| {
            let [a] = args else { return None };
            let iri = match &geom(a)?.crs {
                Crs::Crs84 => "http://www.opengis.net/def/crs/OGC/1.3/CRS84".to_owned(),
                Crs::Epsg4326 => "http://www.opengis.net/def/crs/EPSG/0/4326".to_owned(),
                Crs::Other(iri) => iri.clone(),
            };
            Some(Literal::new_typed_literal(iri, xsd::ANY_URI).into())
        }),
    );
    add(
        GEOF,
        "numGeometries",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            if let Some(s) = &g.surface {
                use crate::literal::SurfaceKind as K;
                return integer(match s.kind {
                    K::Point | K::LineString | K::Polygon | K::Triangle => 1,
                    _ => s.patches.len(),
                } as i64);
            }
            integer(match &g.geometry {
                Geometry::MultiPoint(m) => m.0.len(),
                Geometry::MultiLineString(m) => m.0.len(),
                Geometry::MultiPolygon(m) => m.0.len(),
                Geometry::GeometryCollection(c) => c.0.len(),
                _ => 1,
            } as i64)
        }),
    );
    for (name, pick) in [("minX", 0usize), ("minY", 1), ("maxX", 2), ("maxY", 3)] {
        let f: Func = func(move |args| {
            let [a] = args else { return None };
            let r = geom(a)?.geometry.bounding_rect()?;
            double([r.min().x, r.min().y, r.max().x, r.max().y][pick])
        });
        add(GEOF, name, Arc::clone(&f));
        add(ST, ["ST_XMin", "ST_YMin", "ST_XMax", "ST_YMax"][pick], f);
    }
    for (name, pick) in [("ST_X", 0usize), ("ST_Y", 1)] {
        add(
            ST,
            name,
            func(move |args| {
                let [a] = args else { return None };
                let Geometry::Point(p) = geom(a)?.geometry else {
                    return None;
                };
                double(if pick == 0 { p.x() } else { p.y() })
            }),
        );
    }
    add(
        ST,
        "ST_NPoints",
        func(|args| {
            let [a] = args else { return None };
            integer(geo::CoordsIter::coords_count(&geom(a)?.geometry) as i64)
        }),
    );
    for (name, last) in [("ST_StartPoint", false), ("ST_EndPoint", true)] {
        add(
            ST,
            name,
            func(move |args| {
                let [a] = args else { return None };
                let g = geom(a)?;
                let l = as_linestring(&g.geometry)?;
                let c = if last { l.0.last()? } else { l.0.first()? };
                Some(wkt_literal(&Geometry::Point(Point(*c)), &g.crs))
            }),
        );
    }

    // Constructors and outputs.
    add(
        ST,
        "ST_MakePoint",
        func(|args| match args {
            [x, y] => Some(wkt_literal(
                &Geometry::Point(Point::new(num(x)?, num(y)?)),
                &Crs::Crs84,
            )),
            // PostGIS ST_MakePoint(x, y, z [, m]).
            [x, y, z, m @ ..] if m.len() <= 1 => crate::postgis_zm::point_zm(
                num(x)?,
                num(y)?,
                Some(num(z)?),
                match m.first() {
                    Some(t) => Some(num(t)?),
                    None => None,
                },
                Crs::Crs84,
            ),
            _ => None,
        }),
    );
    add(
        GEOF,
        "asWKT",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            if g.surface.is_some() {
                return Some(crate::literal::geom_literal(&g));
            }
            Some(wkt_literal(&g.geometry, &g.crs))
        }),
    );
    add(
        ST,
        "ST_AsText",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            if let Some(s) = &g.surface {
                return Some(Literal::from(crate::literal::surface_wkt(s, &g.crs, true)).into());
            }
            Some(Literal::from(crate::wkt_out::string(&g.geometry)).into())
        }),
    );
    add(
        ST,
        "ST_GeomFromText",
        func(|args| {
            let Term::Literal(l) = args.first()? else {
                return None;
            };
            let t: Term = Literal::new_typed_literal(l.value(), crate::literal::WKT_LITERAL).into();
            geom(&t).map(|g| wkt_literal(&g.geometry, &g.crs))
        }),
    );

    // Processing.
    let envelope: Func = func(|args| {
        let [a] = args else { return None };
        let g = geom(a)?;
        Some(literal_like(
            &Geometry::Polygon(g.geometry.bounding_rect()?.to_polygon()),
            &g.crs,
            a,
        ))
    });
    add(GEOF, "envelope", Arc::clone(&envelope));
    add(ST, "ST_Envelope", envelope);
    let centroid: Func = func(|args| {
        let [a] = args else { return None };
        let g = geom(a)?;
        Some(wkt_literal(
            &Geometry::Point(g.geometry.centroid()?),
            &g.crs,
        ))
    });
    add(GEOF, "centroid", Arc::clone(&centroid));
    add(ST, "ST_Centroid", centroid);
    let hull: Func = func(|args| {
        let [a] = args else { return None };
        let g = geom(a)?;
        Some(literal_like(
            &Geometry::Polygon(hull_ring_order(g.geometry.convex_hull())),
            &g.crs,
            a,
        ))
    });
    add(GEOF, "convexHull", Arc::clone(&hull));
    add(ST, "ST_ConvexHull", hull);
    add(
        GEOF,
        "buffer",
        func(|args| {
            let (a, r, unit) = match args {
                [a, r] => (a, r, None),
                [a, r, u] => (a, r, Some(u)),
                _ => return None,
            };
            let (g, r) = (geom(a)?, num(r)?);
            if !r.is_finite() {
                return None;
            }
            if g.geometry.is_empty() {
                return Some(wkt_literal(
                    &Geometry::MultiPolygon(geo::MultiPolygon(vec![])),
                    &g.crs,
                ));
            }
            let out = match (unit_iri(unit)?, g.crs.is_geographic()) {
                ("metre", true) => {
                    let proj = LocalMetric::around(&[&g.geometry])?;
                    proj.inverse(&Geometry::MultiPolygon(proj.forward(&g.geometry).buffer(r)))
                }
                ("metre", false) | ("degree", true) => Geometry::MultiPolygon(g.geometry.buffer(r)),
                _ => return None,
            };
            Some(literal_like(&out, &g.crs, a))
        }),
    );
    add(
        ST,
        "ST_Buffer",
        func(|args| {
            let [a, r] = args else { return None };
            let (g, r) = (geom(a)?, num(r)?);
            if !r.is_finite() {
                return None;
            }
            let buffered = if g.geometry.is_empty() {
                geo::MultiPolygon(vec![])
            } else {
                g.geometry.buffer(r)
            };
            Some(wkt_literal(&Geometry::MultiPolygon(buffered), &g.crs))
        }),
    );
    // Set operations on any mix of points, lines and polygons (setops.rs).
    use crate::setops::SetOp;
    let ops: [(&str, &str, SetOp); 4] = [
        ("intersection", "ST_Intersection", SetOp::Intersection),
        ("union", "ST_Union", SetOp::Union),
        ("difference", "ST_Difference", SetOp::Difference),
        ("symDifference", "ST_SymDifference", SetOp::SymDifference),
    ];
    for (geof_name, st_name, op) in ops {
        let f: Func = func(move |args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            let r = crate::setops::set_operation(op, &a, &b)?;
            Some(literal_like(&r, &a.crs, &args[0]))
        });
        add(GEOF, geof_name, Arc::clone(&f));
        add(ST, st_name, f);
    }
    add(
        ST,
        "ST_Simplify",
        func(|args| {
            let [a, tol] = args else { return None };
            let g = geom(a)?;
            let tol = num(tol)?;
            let out = match &g.geometry {
                Geometry::LineString(l) => Geometry::LineString(l.simplify(tol)),
                Geometry::MultiLineString(l) => Geometry::MultiLineString(l.simplify(tol)),
                Geometry::Polygon(p) => Geometry::Polygon(p.simplify(tol)),
                Geometry::MultiPolygon(p) => Geometry::MultiPolygon(p.simplify(tol)),
                other => other.clone(),
            };
            Some(wkt_literal(&out, &g.crs))
        }),
    );
    add(
        ST,
        "ST_Reverse",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let mut l = as_linestring(&g.geometry)?.clone();
            l.0.reverse();
            Some(wkt_literal(&Geometry::LineString(l), &g.crs))
        }),
    );
    add(
        ST,
        "ST_ClosestPoint",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            let Geometry::Point(p) = b.geometry else {
                return None;
            };
            match a.geometry.closest_point(&p) {
                Closest::Intersection(c) | Closest::SinglePoint(c) => {
                    Some(wkt_literal(&Geometry::Point(c), &a.crs))
                }
                Closest::Indeterminate => None,
            }
        }),
    );

    // Linear referencing (PostGIS semantics: fractions of the planar 2-D length).
    add(
        ST,
        "ST_LineLocatePoint",
        func(|args| {
            let [l, p] = args else { return None };
            let (l, p) = (geom(l)?, geom(p)?);
            let Geometry::Point(p) = p.geometry else {
                return None;
            };
            double(as_linestring(&l.geometry)?.line_locate_point(&p)?)
        }),
    );
    add(
        ST,
        "ST_LineInterpolatePoint",
        func(|args| {
            let [l, f] = args else { return None };
            let g = geom(l)?;
            let f = num(f)?;
            if !(0.0..=1.0).contains(&f) {
                return None;
            }
            let p = Euclidean.point_at_ratio_from_start(as_linestring(&g.geometry)?, f)?;
            Some(wkt_literal(&Geometry::Point(p), &g.crs))
        }),
    );
    add(
        ST,
        "ST_LineSubstring",
        func(|args| {
            let [l, a, b] = args else { return None };
            let g = geom(l)?;
            Some(wkt_literal(
                &line_substring(as_linestring(&g.geometry)?, num(a)?, num(b)?)?,
                &g.crs,
            ))
        }),
    );
    crate::geosparql11::add_functions(&mut add);
    crate::postgis::add_functions(&mut add);
    crate::postgis_processing::add_functions(&mut add);
    crate::postgis_zm::add_functions(&mut add);
    crate::postgis_shapes::add_functions(&mut add);
    crate::postgis_p2::add_functions(&mut add);
    crate::postgis_3d::add_functions(&mut add);
    crate::dggs::add_functions(&mut add);
    crate::mvt::add_functions(&mut add);
    crate::rail::add_functions(&mut add);
    crate::formats::add_functions(&mut add);
    crate::letters::add_functions(&mut add);
    crate::pipeline::add_functions(&mut add);
    crate::srs::add_functions(&mut add);
    crate::postgis::add_aliases(&mut t);
    crate::jena::add_functions(&mut t);
    t.into_iter()
        .map(|(name, f)| {
            let keeps_2d = KEEP_2D.iter().any(|k| name.as_str().ends_with(k));
            let f = if keeps_2d { f } else { zm_aware(f) };
            (name, contain_panics(f))
        })
        .collect()
}

/// Turns a panic inside a function (a bug here or in a geometry library, typically on NaN,
/// infinite or empty input) into a SPARQL error for that call, as PostGIS raises an error,
/// instead of unwinding through the query evaluator.
fn contain_panics(f: Func) -> Func {
    func(move |args| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(args)))
            .ok()
            .flatten()
    })
}

/// Functions whose result PostGIS writes in 2-D even for 3-D input (by local name).
const KEEP_2D: [&str; 9] = [
    "ST_Force2D",
    "#Box2D",
    "/centroid",
    "ST_Centroid",
    "/envelope",
    "ST_Envelope",
    "ST_MakeEnvelope",
    "ST_AsText",
    "/boundingCircle",
];

/// Runs `f` with its Z/M inputs as the sources of the Z and M of its geometry result.
fn zm_aware(f: Func) -> Func {
    func(move |args| {
        if !args.iter().any(crate::literal::may_have_zm) {
            return f(args);
        }
        let sources: Vec<Arc<Geom>> = args
            .iter()
            .filter(|a| crate::literal::may_have_zm(a))
            .filter_map(geom)
            .filter(|g| g.zm.is_some())
            .collect();
        if sources.is_empty() {
            return f(args);
        }
        crate::literal::with_zm_sources(sources, || f(args))
    })
}

/// The geodesic area of the polygons of `g`, in square metres, whichever way their rings
/// wind. `geo`'s geodesic area takes a clockwise shell for the outside of the polygon (the
/// Earth's surface minus it), so each polygon is oriented first: shell counter-clockwise,
/// holes clockwise.
pub(crate) fn geodesic_area(g: &Geometry<f64>) -> f64 {
    use geo::orient::{Direction, Orient};
    let polygon = |p: &geo::Polygon<f64>| p.orient(Direction::Default).geodesic_area_unsigned();
    match g {
        Geometry::Polygon(p) => polygon(p),
        Geometry::MultiPolygon(m) => m.0.iter().map(polygon).sum(),
        Geometry::Rect(r) => polygon(&r.to_polygon()),
        Geometry::Triangle(t) => polygon(&t.to_polygon()),
        Geometry::GeometryCollection(c) => c.0.iter().map(geodesic_area).sum(),
        _ => 0.0,
    }
}

/// A convex hull's ring as JTS and GEOS write it: clockwise, from its lowest point (the
/// leftmost of the lowest), so results compare with theirs (GeoSPARQL benchmark R19).
fn hull_ring_order(p: geo::Polygon<f64>) -> geo::Polygon<f64> {
    use geo::orient::{Direction, Orient};
    let p = p.orient(Direction::Reversed);
    let (exterior, interiors) = p.into_inner();
    let mut cs = exterior.0;
    if cs.len() < 4 {
        return geo::Polygon::new(LineString(cs), interiors);
    }
    cs.pop();
    let start = (0..cs.len())
        .min_by(|&i, &j| {
            (cs[i].y, cs[i].x)
                .partial_cmp(&(cs[j].y, cs[j].x))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    cs.rotate_left(start);
    cs.push(cs[0]);
    geo::Polygon::new(LineString(cs), interiors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::literal::WKT_LITERAL;

    fn call(name: &str, args: &[Term]) -> Option<Term> {
        let table = function_table();
        let f = table
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .unwrap_or_else(|| panic!("{name}"));
        (f.1)(args)
    }

    fn wkt(s: &str) -> Term {
        Literal::new_typed_literal(s, WKT_LITERAL).into()
    }

    #[test]
    fn long_functions_stop_when_cancelled() {
        let pts = (0..2000)
            .map(|i| format!("{} {}", i % 37, i / 37))
            .collect::<Vec<_>>()
            .join(",");
        let median = format!("{ST}ST_GeometricMedian");
        let args = [
            wkt(&format!("MULTIPOINT({pts})")),
            Literal::from(0.0).into(),
            Literal::from(1_000_000_000_i64).into(),
        ];
        let started = std::time::Instant::now();
        let stop: crate::cancel::Check = Arc::new(|| true);
        assert_eq!(
            crate::cancel::scope(Some(stop), || call(&median, &args)),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // A tiny repeat fraction would be 10^12 points: no value instead.
        let points = format!("{ST}ST_LineInterpolatePoints");
        let line = wkt("LINESTRING(0 0, 1 1)");
        assert_eq!(
            call(&points, &[line.clone(), Literal::from(1e-12).into()]),
            None
        );
        assert!(call(&points, &[line, Literal::from(0.25).into()]).is_some());
    }

    fn val(t: Option<Term>) -> String {
        match t {
            Some(Term::Literal(l)) => l.value().to_owned(),
            other => panic!("{other:?}"),
        }
    }

    fn f(t: Option<Term>) -> f64 {
        val(t).parse().unwrap()
    }

    fn double(v: &str) -> Term {
        Literal::new_typed_literal(v, xsd::DOUBLE).into()
    }

    /// Inputs that used to panic (in this crate, `geo`'s boolean operations or the `wkt`
    /// writer) or to run for minutes. Each gives an answer or an error, quickly.
    #[test]
    fn edge_case_inputs() {
        let st = |n: &str| format!("{ST}{n}");
        let geof = |n: &str| format!("{GEOF}{n}");
        assert_eq!(
            val(call(&st("ST_Multi"), &[wkt("POLYGON EMPTY")])),
            "MULTIPOLYGON EMPTY"
        );
        assert!(call(&st("ST_Collect"), &[wkt("POLYGON EMPTY")]).is_some());
        assert!(
            call(
                &st("ST_SimplifyPolygonHull"),
                &[wkt("POLYGON EMPTY"), double("0")]
            )
            .is_some()
        );
        assert_eq!(
            val(call(
                &st("ST_Buffer"),
                &[wkt("POLYGON EMPTY"), double("-1")]
            )),
            "MULTIPOLYGON EMPTY"
        );
        assert!(call(&st("ST_Buffer"), &[wkt("POINT(0 0)"), double("NaN")]).is_none());
        assert!(
            call(
                &geof("buffer"),
                &[
                    wkt("POLYGON((0 0, 1 1, 1 0, 0 1, 0 0))"),
                    double("NaN"),
                    NamedNode::new_unchecked(format!("{UOM}metre")).into(),
                ]
            )
            .is_none()
        );
        assert!(
            call(
                &st("ST_ClipByBox2D"),
                &[wkt("POLYGON EMPTY"), wkt("LINESTRING(0 0, 1e-300 1e-300)")]
            )
            .is_some()
        );
        let started = std::time::Instant::now();
        assert!(
            call(
                &st("ST_GeometricMedian"),
                &[
                    wkt("POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))"),
                    double("NaN"),
                    Literal::from(1_000_000_000).into(),
                ]
            )
            .is_none()
        );
        assert!(started.elapsed().as_secs() < 5);
        // Millions of H3 cells are an error, not minutes of work; a small area still works.
        let started = std::time::Instant::now();
        assert!(
            call(
                &st("ST_H3Cells"),
                &[wkt("POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))")]
            )
            .is_none()
        );
        assert!(
            call(
                &st("ST_H3Cells"),
                &[wkt(
                    "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1))"
                )]
            )
            .is_none()
        );
        assert!(started.elapsed().as_secs() < 5);
        assert!(
            call(
                &st("ST_H3Cells"),
                &[wkt(
                    "POLYGON((10 59, 10.01 59, 10.01 59.01, 10 59.01, 10 59))"
                )]
            )
            .is_some()
        );
        // A latitude past the pole has no area to estimate (found by the geo_functions fuzz
        // target, which timed out covering most of the globe).
        let started = std::time::Instant::now();
        assert!(
            call(
                &st("ST_H3Cells"),
                &[wkt("POLYGON((49 0, 0 97, 0 2, 49 0))")]
            )
            .is_none()
        );
        assert!(started.elapsed().as_secs() < 5);
        // A box one 1e-300th as high as it is wide.
        let started = std::time::Instant::now();
        let thin = format!("MULTIPOINT(1 0.{}3, 0 0, 0 0)", "0".repeat(299));
        assert!(call(&st("ST_LargestEmptyCircle"), &[wkt(&thin)]).is_some());
        assert!(started.elapsed().as_secs() < 5);
        // Coordinates TWKB has no integers for.
        let huge = format!("MULTIPOINT((-1{} 0), (0 0))", "0".repeat(30));
        assert!(call(&st("ST_AsText"), &[wkt(&huge)]).is_some());
        assert!(call(&st("ST_AsTWKB"), &[wkt(&huge)]).is_none());
        assert!(call(&st("ST_AsTWKB"), &[wkt("POINT(1 2)")]).is_some());
        // A precision too large for an integer.
        assert!(
            call(
                &st("ST_QuantizeCoordinates"),
                &[wkt("POINT Z(1 2 3)"), double("1e300")]
            )
            .is_some()
        );
        // Too few arguments.
        assert!(call(&st("ST_Affine"), &[]).is_none());
        // Panics inside the geometry libraries become errors (with no Z/M left behind).
        for (name, args) in [
            (
                "ST_WrapX",
                vec![
                    wkt("LINESTRING ZM(0 0 0 0, 1 1 1 1)"),
                    double("NaN"),
                    double("1e9"),
                ],
            ),
            (
                "ST_AsMVTGeom",
                vec![
                    wkt("POLYGON((0 0, 0 0, 0 0, 0 0))"),
                    wkt("LINESTRING ZM(0 0 0 0, 1 1 1 1)"),
                    double("INF"),
                ],
            ),
        ] {
            let _ = call(&st(name), &args);
        }
        // The thread still answers normally afterwards.
        assert_eq!(
            val(call(&st("ST_Envelope"), &[wkt("LINESTRING(0 0, 1 1)")])),
            "POLYGON((1 0,1 1,0 1,0 0,1 0))"
        );
    }

    #[test]
    fn relations() {
        let poly = wkt("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))");
        let inside = wkt("POINT(5 5)");
        let outside = wkt("POINT(50 50)");
        assert_eq!(
            val(call(
                &format!("{GEOF}sfContains"),
                &[poly.clone(), inside.clone()]
            )),
            "true"
        );
        assert_eq!(
            val(call(
                &format!("{GEOF}sfWithin"),
                &[inside.clone(), poly.clone()]
            )),
            "true"
        );
        assert_eq!(
            val(call(
                &format!("{GEOF}sfIntersects"),
                &[poly.clone(), outside.clone()]
            )),
            "false"
        );
        assert_eq!(
            val(call(&format!("{GEOF}sfDisjoint"), &[poly.clone(), outside])),
            "true"
        );
        assert_eq!(
            val(call(
                &format!("{GEOF}relate"),
                &[poly, inside, Literal::from("T*****FF*").into()]
            )),
            "true"
        );
    }

    #[test]
    fn geodesic_distance_and_length() {
        // One degree of latitude is about 111 km.
        let d = f(call(
            &format!("{GEOF}distance"),
            &[
                wkt("POINT(10 59)"),
                wkt("POINT(10 60)"),
                NamedNode::new_unchecked(format!("{UOM}metre")).into(),
            ],
        ));
        assert!((d - 111_000.0).abs() < 1_000.0, "{d}");
        let l = f(call(
            &format!("{GEOF}metricLength"),
            &[wkt("LINESTRING(10 59, 10 60)")],
        ));
        assert!((l - d).abs() < 1.0);
        // Point to line, metric through the local projection.
        let pl = f(call(
            &format!("{GEOF}distance"),
            &[wkt("POINT(10.01 59.5)"), wkt("LINESTRING(10 59, 10 60)")],
        ));
        assert!((pl - 566.0).abs() < 10.0, "{pl}");
    }

    #[test]
    fn point_to_line_distance_on_the_ellipsoid() {
        // Reference: the line densified to 1 mm steps, nearest vertex by Karney's geodesic.
        let line = [(10.0, 60.0), (10.003, 60.0004)];
        let reference = |p: (f64, f64)| {
            let n = 400_000;
            (0..=n)
                .map(|i| {
                    let t = f64::from(i) / f64::from(n);
                    let q = Point::new(
                        line[0].0 + (line[1].0 - line[0].0) * t,
                        line[0].1 + (line[1].1 - line[0].1) * t,
                    );
                    Geodesic.distance(Point::new(p.0, p.1), q)
                })
                .fold(f64::INFINITY, f64::min)
        };
        let l = wkt("LINESTRING(10 60, 10.003 60.0004)");
        for p in [
            (10.0015, 60.00038),
            (10.001, 60.0001),
            (10.0012, 60.0009),
            (10.0035, 60.0),
        ] {
            let d = f(call(
                &format!("{GEOF}distance"),
                &[wkt(&format!("POINT({} {})", p.0, p.1)), l.clone()],
            ));
            let r = reference(p);
            assert!((d - r).abs() < 0.002, "{p:?}: {d} vs {r}");
        }
    }

    #[test]
    fn line_to_line_distance_far_from_the_box_centre() {
        // A track along the meridian of 10°E from 59.5° to 60.5°N, and lines that come
        // within a few metres of it near 60.45°N, half a degree from the middle of the
        // boxes, where the tangent plane there is off by about 1.4 % (#350). Reference: the
        // vertex near the track to the meridian, by Karney's geodesic at 1e-8° steps.
        let track = wkt("LINESTRING(10 59.5, 10 60, 10 60.5)");
        let reference = |p: (f64, f64)| {
            (-20_000..=20_000)
                .map(|i| {
                    let q = Point::new(10.0, p.1 + f64::from(i) * 1e-8);
                    Geodesic.distance(Point::new(p.0, p.1), q)
                })
                .fold(f64::INFINITY, f64::min)
        };
        for (east, metres) in [(1.0, 5.0), (-1.0, 4.9), (1.0, 120.0)] {
            // Degrees of longitude for `metres` at 60.45°N.
            let dx = east * metres / metres_per_degree(60.45).1;
            let p = (10.0 + dx, 60.45);
            let line = wkt(&format!(
                "LINESTRING({} 59.5, {} {}, {} 60.46)",
                10.0 + east * 0.3,
                p.0,
                p.1,
                p.0 + east * 0.001
            ));
            let d = f(call(
                &format!("{GEOF}distance"),
                &[track.clone(), line.clone()],
            ));
            let r = reference(p);
            assert!((d - r).abs() < 0.002, "{p:?}: {d} vs {r}");
            let swapped = f(call(&format!("{GEOF}distance"), &[line, track.clone()]));
            assert!((swapped - r).abs() < 0.002, "{p:?}: {swapped} vs {r}");
        }
        // Crossing lines are at 0.
        let cross = wkt("LINESTRING(9.9 60.2, 10.1 60.21)");
        assert_eq!(f(call(&format!("{GEOF}distance"), &[track, cross])), 0.0);
    }

    #[test]
    fn polygon_distances_far_from_the_box_centre() {
        // As above, with the track as the east edge of a polygon and the other side a
        // triangle with a vertex near it.
        let area = wkt("POLYGON((9.9 59.5, 10 59.5, 10 60.5, 9.9 60.5, 9.9 59.5))");
        let reference = |p: (f64, f64)| {
            (-20_000..=20_000)
                .map(|i| {
                    let q = Point::new(10.0, p.1 + f64::from(i) * 1e-8);
                    Geodesic.distance(Point::new(p.0, p.1), q)
                })
                .fold(f64::INFINITY, f64::min)
        };
        let dx = 5.0 / metres_per_degree(60.45).1;
        let p = (10.0 + dx, 60.45);
        let triangle = wkt(&format!(
            "POLYGON(({x} {y}, {e} 59.5, {e} 60.46, {x} {y}))",
            x = p.0,
            y = p.1,
            e = p.0 + 0.3
        ));
        let r = reference(p);
        for (a, b) in [(&area, &triangle), (&triangle, &area)] {
            let d = f(call(&format!("{GEOF}distance"), &[a.clone(), b.clone()]));
            assert!((d - r).abs() < 0.002, "{d} vs {r}");
        }
        // A line inside a polygon, or crossing its edge, is at 0; a polygon in a hole of
        // another is at the distance to the hole's ring (about 5.5 km: 0.05° at the equator).
        let dist = |a: &str, b: &str| f(call(&format!("{GEOF}distance"), &[wkt(a), wkt(b)]));
        assert_eq!(
            dist(
                "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))",
                "LINESTRING(0.2 0.2, 0.3 0.3)"
            ),
            0.0
        );
        assert_eq!(
            dist(
                "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))",
                "LINESTRING(0.5 0.5, 2 2)"
            ),
            0.0
        );
        let holed =
            "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0), (0.4 0.4, 0.6 0.4, 0.6 0.6, 0.4 0.6, 0.4 0.4))";
        let d = dist(
            holed,
            "POLYGON((0.45 0.45, 0.55 0.45, 0.55 0.55, 0.45 0.55, 0.45 0.45))",
        );
        assert!((d - 5_528.0).abs() < 30.0, "{d}");
        assert_eq!(
            dist(holed, "POLYGON((0.1 0.1, 0.2 0.1, 0.2 0.2, 0.1 0.1))"),
            0.0
        );
        // A multi-part geometry with one part far outside an area and one inside it, or in a
        // hole of it.
        let area =
            "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0), (0.6 0.6, 0.9 0.6, 0.9 0.9, 0.6 0.9, 0.6 0.6))";
        for other in [
            "MULTIPOLYGON(((5 5, 6 5, 6 6, 5 5)), ((0.2 0.2, 0.3 0.2, 0.3 0.3, 0.2 0.2)))",
            "MULTILINESTRING((5 5, 6 6), (0.2 0.2, 0.3 0.3))",
            "GEOMETRYCOLLECTION(LINESTRING(5 5, 6 6), POLYGON((0.2 0.2, 0.3 0.2, 0.3 0.3, 0.2 0.2)))",
        ] {
            assert_eq!(dist(area, other), 0.0, "{other}");
            assert_eq!(dist(other, area), 0.0, "{other}");
            let in_hole = other.replace("0.2", "0.7").replace("0.3", "0.8");
            let d = dist(area, &in_hole);
            assert!((d - 11_119.0).abs() < 100.0, "{in_hole}: {d}");
            assert_eq!(d, dist(&in_hole, area), "{in_hole}");
        }
        // Collections with points are measured in the plane, as before.
        let d = dist(
            "GEOMETRYCOLLECTION(POINT(0 0), LINESTRING(0 0, 0 1))",
            "LINESTRING(0.001 0, 0.001 1)",
        );
        assert!((d - 111.3).abs() < 1.0, "{d}");
    }

    #[test]
    fn short_segment_length_matches_karney() {
        for (a, b) in [
            ((10.0, 60.0), (10.01, 60.005)),
            ((-3.0, 40.0), (-2.99, 40.01)),
            ((25.0, 70.0), (25.0, 70.015)),
        ] {
            let fast = segment_length(Coord { x: a.0, y: a.1 }, Coord { x: b.0, y: b.1 });
            let exact = Geodesic.distance(Point::new(a.0, a.1), Point::new(b.0, b.1));
            assert!((fast - exact).abs() / exact < 1e-6, "{fast} vs {exact}");
        }
    }

    #[test]
    fn linear_referencing() {
        let line = wkt("LINESTRING(0 0, 10 0, 10 10)");
        assert_eq!(
            f(call(
                &format!("{ST}ST_LineLocatePoint"),
                &[line.clone(), wkt("POINT(10 0)")]
            )),
            0.5
        );
        assert_eq!(
            f(call(
                &format!("{ST}ST_LineLocatePoint"),
                &[line.clone(), wkt("POINT(5 3)")]
            )),
            0.25
        );
        assert_eq!(
            val(call(
                &format!("{ST}ST_LineInterpolatePoint"),
                &[line.clone(), Literal::from(0.75).into()]
            )),
            "POINT(10 5)"
        );
        assert_eq!(
            val(call(
                &format!("{ST}ST_LineSubstring"),
                &[line, Literal::from(0.25).into(), Literal::from(0.75).into()]
            )),
            "LINESTRING(5 0,10 0,10 5)"
        );
    }

    #[test]
    fn metre_buffer_on_geographic() {
        let b = call(
            &format!("{GEOF}buffer"),
            &[wkt("POINT(10 60)"), Literal::from(1000.0).into()],
        )
        .unwrap();
        let area = f(call(&format!("{GEOF}metricArea"), &[b]));
        let expected = std::f64::consts::PI * 1000.0 * 1000.0;
        assert!((area - expected).abs() / expected < 0.02, "{area}");
    }

    #[test]
    fn crs_mismatch_is_an_error() {
        let a = wkt("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(1 1)");
        assert!(call(&format!("{GEOF}sfIntersects"), &[a, wkt("POINT(1 1)")]).is_none());
    }
}
