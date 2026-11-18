//! PostGIS P2 functions: SFCGAL-style 2-D and planar-3-D operations (alpha shapes, Minkowski
//! sums, constrained triangulations, medial axis, orientation, planarity), curve functions on
//! linear geometries, X3D output and the coverage aggregates.

use crate::functions::{Func, ST, boolean, double, func, geom, integer, num};
use crate::literal::Geom;
use crate::postgis::{int, out, string};
use crate::postgis_processing::{polygons_of, segments};
use geo::{
    BooleanOps, ConvexHull, Coord, CoordsIter, Geometry, GeometryCollection, LineString,
    MultiLineString, MultiPoint, MultiPolygon, Orient, Point, Polygon, Relate, TriangulateDelaunay,
    TriangulateDelaunayUnconstrained, Winding, orient::Direction,
};
use oxigraph::model::Term;

fn zm_at(g: &Geom, c: Coord<f64>) -> (f64, f64) {
    g.zm.as_ref()
        .and_then(|z| z.at(c))
        .unwrap_or((f64::NAN, f64::NAN))
}

fn z_of(g: &Geom, c: Coord<f64>) -> f64 {
    let z = zm_at(g, c).0;
    if z.is_nan() { 0.0 } else { z }
}

fn collection(parts: Vec<Geometry<f64>>) -> Geometry<f64> {
    Geometry::GeometryCollection(GeometryCollection(parts))
}

fn polys(p: Vec<Polygon<f64>>) -> Geometry<f64> {
    if p.len() == 1 {
        Geometry::Polygon(p.into_iter().next().unwrap())
    } else {
        Geometry::MultiPolygon(MultiPolygon(p))
    }
}

/// Circumradius² of a triangle.
fn circumradius2(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> f64 {
    let d = 2.0 * (a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y));
    if d.abs() < f64::EPSILON {
        return f64::INFINITY;
    }
    let (a2, b2, c2) = (
        a.x * a.x + a.y * a.y,
        b.x * b.x + b.y * b.y,
        c.x * c.x + c.y * c.y,
    );
    let ux = (a2 * (b.y - c.y) + b2 * (c.y - a.y) + c2 * (a.y - b.y)) / d;
    let uy = (a2 * (c.x - b.x) + b2 * (a.x - c.x) + c2 * (b.x - a.x)) / d;
    (a.x - ux).powi(2) + (a.y - uy).powi(2)
}

/// The Delaunay triangles of the points of `g`, with their circumradius².
fn delaunay(g: &Geometry<f64>) -> Vec<(Polygon<f64>, f64)> {
    let mut pts: Vec<Coord<f64>> = g.coords_iter().collect();
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup();
    let mp = MultiPoint(pts.into_iter().map(Point).collect());
    mp.unconstrained_triangulation()
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            let r = circumradius2(t.v1(), t.v2(), t.v3());
            (t.to_polygon(), r)
        })
        .collect()
}

/// The alpha shape (CGAL's convention: `alpha` is a squared radius): the union of the Delaunay
/// triangles whose circumcircle has a squared radius of at most `alpha`.
fn alpha_shape(tris: &[(Polygon<f64>, f64)], alpha: f64, holes: bool) -> Vec<Polygon<f64>> {
    let kept: Vec<Polygon<f64>> = tris
        .iter()
        .filter(|(_, r)| *r <= alpha)
        .map(|(p, _)| p.clone())
        .collect();
    let u: MultiPolygon<f64> = geo::algorithm::unary_union(&kept);
    u.0.into_iter()
        .map(|p| {
            if holes {
                p
            } else {
                Polygon::new(p.exterior().clone(), Vec::new())
            }
        })
        .collect()
}

/// The smallest alpha whose shape has at most `components` parts and covers every point.
fn optimal_alpha(g: &Geometry<f64>, tris: &[(Polygon<f64>, f64)], components: usize) -> f64 {
    let mut radii: Vec<f64> = tris.iter().map(|t| t.1).filter(|r| r.is_finite()).collect();
    radii.sort_by(f64::total_cmp);
    radii.dedup();
    let points: Vec<Point<f64>> = g.coords_iter().map(Point).collect();
    let ok = |alpha: f64| {
        let shape = MultiPolygon(alpha_shape(tris, alpha, true));
        shape.0.len() <= components.max(1)
            && !shape.0.is_empty()
            && points.iter().all(|p| shape.relate(p).is_covers())
    };
    let (mut lo, mut hi) = (0usize, radii.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if ok(radii[mid]) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    radii.get(lo).copied().unwrap_or(f64::INFINITY)
}

/// Convex parts of a geometry, for Minkowski sums: triangles of polygons, segments of lines,
/// points.
fn convex_parts(g: &Geometry<f64>) -> Vec<Vec<Coord<f64>>> {
    let mut out: Vec<Vec<Coord<f64>>> = Vec::new();
    let polys = polygons_of(g);
    if !polys.is_empty() {
        let mp = MultiPolygon(polys);
        for t in mp
            .constrained_triangulation(Default::default())
            .unwrap_or_default()
        {
            out.push(vec![t.v1(), t.v2(), t.v3()]);
        }
    }
    for s in segments(g) {
        out.push(vec![s.start, s.end]);
    }
    for p in crate::postgis_processing::points_of(g) {
        out.push(vec![p.0]);
    }
    out
}

/// The Minkowski sum of `a` (polygonal or linear) and `b`.
fn minkowski(a: &Geometry<f64>, b: &Geometry<f64>) -> Vec<Polygon<f64>> {
    let (pa, pb) = (convex_parts(a), convex_parts(b));
    let mut pieces: Vec<Polygon<f64>> = Vec::new();
    for x in &pa {
        for y in &pb {
            let sums: Vec<Point<f64>> = x
                .iter()
                .flat_map(|p| y.iter().map(move |q| Point::new(p.x + q.x, p.y + q.y)))
                .collect();
            let hull = MultiPoint(sums).convex_hull();
            if hull.exterior().0.len() >= 4 {
                pieces.push(hull);
            }
        }
    }
    let u: MultiPolygon<f64> = geo::algorithm::unary_union(&pieces);
    u.0
}

/// Voronoi edges of the densified boundary that lie inside the polygon: an approximation of
/// the medial axis.
fn medial_axis(p: &MultiPolygon<f64>) -> Option<MultiLineString<f64>> {
    let rect = geo::BoundingRect::bounding_rect(p)?;
    let step = rect.width().max(rect.height()) / 200.0;
    let mut pts: Vec<Coord<f64>> = Vec::new();
    for s in segments(&Geometry::MultiPolygon(p.clone())) {
        let len = (s.dx()).hypot(s.dy());
        let n = ((len / step).ceil() as usize).max(1);
        for i in 0..n {
            let t = i as f64 / n as f64;
            pts.push(Coord {
                x: s.start.x + s.dx() * t,
                y: s.start.y + s.dy() * t,
            });
        }
    }
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup();
    let mp = MultiPoint(pts.into_iter().map(Point).collect());
    // The finite Voronoi edges join the circumcentres of Delaunay triangles that share an
    // edge. Building them from the triangulation skips the unbounded edges, which the medial
    // axis never uses and which the Voronoi routine cannot clip for collinear input.
    let tris = mp.unconstrained_triangulation().ok()?;
    let key = |c: Coord<f64>| (c.x.to_bits(), c.y.to_bits());
    let mut by_edge: std::collections::HashMap<_, Vec<Coord<f64>>> =
        std::collections::HashMap::new();
    for t in &tris {
        let [a, b, c] = t.to_array();
        let Some(centre) = circumcentre(a, b, c) else {
            continue;
        };
        for (u, v) in [(a, b), (b, c), (c, a)] {
            let (ku, kv) = (key(u), key(v));
            let k = if ku <= kv { (ku, kv) } else { (kv, ku) };
            by_edge.entry(k).or_default().push(centre);
        }
    }
    let mut inside: Vec<LineString<f64>> = by_edge
        .into_values()
        .filter(|c| c.len() == 2 && c[0] != c[1])
        .map(|c| LineString::new(vec![c[0], c[1]]))
        .filter(|line| p.relate(line).is_contains())
        .collect();
    inside.sort_by(|x, y| {
        let (a, b) = (x.0[0], y.0[0]);
        a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y))
    });
    Some(MultiLineString(inside))
}

fn circumcentre(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> Option<Coord<f64>> {
    let d = 2.0 * (a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y));
    if d.abs() < f64::EPSILON {
        return None;
    }
    let (a2, b2, c2) = (
        a.x * a.x + a.y * a.y,
        b.x * b.x + b.y * b.y,
        c.x * c.x + c.y * c.y,
    );
    Some(Coord {
        x: (a2 * (b.y - c.y) + b2 * (c.y - a.y) + c2 * (a.y - b.y)) / d,
        y: (a2 * (c.x - b.x) + b2 * (a.x - c.x) + c2 * (b.x - a.x)) / d,
    })
}

/// The unit normal (Newell) of a ring with Z.
fn ring_normal(g: &Geom, ring: &LineString<f64>) -> [f64; 3] {
    let mut n = [0.0; 3];
    for w in ring.0.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (za, zb) = (z_of(g, a), z_of(g, b));
        n[0] += (a.y - b.y) * (za + zb);
        n[1] += (za - zb) * (a.x + b.x);
        n[2] += (a.x - b.x) * (a.y + b.y);
    }
    n
}

fn is_planar(g: &Geom) -> bool {
    polygons_of(&g.geometry).iter().all(|p| {
        let n = ring_normal(g, p.exterior());
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len == 0.0 {
            return true;
        }
        let o = p.exterior().0[0];
        let o3 = [o.x, o.y, z_of(g, o)];
        let scale = p
            .exterior()
            .0
            .iter()
            .map(|c| (c.x - o.x).abs().max((c.y - o.y).abs()))
            .fold(1.0, f64::max);
        std::iter::once(p.exterior())
            .chain(p.interiors())
            .flat_map(|r| r.0.iter())
            .all(|&c| {
                let d = ((c.x - o3[0]) * n[0] + (c.y - o3[1]) * n[1] + (z_of(g, c) - o3[2]) * n[2])
                    / len;
                d.abs() <= 1e-9 * scale
            })
    })
}

/// The area of polygons in 3-D (their planes' area).
fn area3d(g: &Geom) -> f64 {
    polygons_of(&g.geometry)
        .iter()
        .map(|p| {
            let norm = |r: &LineString<f64>| {
                let n = ring_normal(g, r);
                (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() / 2.0
            };
            norm(p.exterior()) - p.interiors().iter().map(norm).sum::<f64>()
        })
        .sum()
}

fn fmt(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

/// X3D 3.0 for a geometry (PostGIS ST_AsX3D, without the GeoCoordinate option).
fn x3d(g: &Geom, decimals: usize) -> String {
    let p3 = |c: Coord<f64>| {
        format!(
            "{} {} {}",
            fmt(c.x, decimals),
            fmt(c.y, decimals),
            fmt(z_of(g, c), decimals)
        )
    };
    fn walk(geometry: &Geometry<f64>, p3: &dyn Fn(Coord<f64>) -> String, out: &mut String) {
        match geometry {
            Geometry::Point(p) => out.push_str(&p3(p.0)),
            Geometry::MultiPoint(m) => {
                let pts: Vec<String> = m.0.iter().map(|p| p3(p.0)).collect();
                out.push_str(&format!("<Polypoint2D point='{}' />", pts.join(" ")));
            }
            Geometry::LineString(l) => {
                let pts: Vec<String> = l.0.iter().map(|c| p3(*c)).collect();
                out.push_str(&format!(
                    "<LineSet vertexCount='{}'><Coordinate point='{}' /></LineSet>",
                    l.0.len(),
                    pts.join(" ")
                ));
            }
            Geometry::MultiLineString(m) => {
                let counts: Vec<String> = m.0.iter().map(|l| l.0.len().to_string()).collect();
                let pts: Vec<String> =
                    m.0.iter()
                        .flat_map(|l| l.0.iter())
                        .map(|c| p3(*c))
                        .collect();
                out.push_str(&format!(
                    "<LineSet vertexCount='{}'><Coordinate point='{}' /></LineSet>",
                    counts.join(" "),
                    pts.join(" ")
                ));
            }
            Geometry::Polygon(_)
            | Geometry::MultiPolygon(_)
            | Geometry::Rect(_)
            | Geometry::Triangle(_) => {
                let mut index = Vec::new();
                let mut pts = Vec::new();
                for p in polygons_of(geometry) {
                    let ring = &p.exterior().0;
                    let n = ring.len().saturating_sub(1);
                    for (i, c) in ring.iter().take(n).enumerate() {
                        index.push((pts.len() + i).to_string());
                        let _ = c;
                    }
                    pts.extend(ring.iter().take(n).map(|c| p3(*c)));
                    index.push("-1".into());
                }
                out.push_str(&format!(
                    "<IndexedFaceSet convex='false' coordIndex='{}'><Coordinate point='{}' /></IndexedFaceSet>",
                    index.join(" "),
                    pts.join(" ")
                ));
            }
            Geometry::Line(l) => walk(
                &Geometry::LineString(LineString::new(vec![l.start, l.end])),
                p3,
                out,
            ),
            Geometry::GeometryCollection(c) => {
                out.push_str("<Shape>");
                for g in &c.0 {
                    walk(g, p3, out);
                }
                out.push_str("</Shape>");
            }
        }
    }
    let mut s = String::new();
    walk(&g.geometry, &p3, &mut s);
    s
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    add(
        ST,
        "ST_AlphaShape",
        func(|args| {
            let g = geom(args.first()?)?;
            let alpha = args.get(1).map_or(Some(1.0), num)?;
            let holes = matches!(args.get(2), Some(Term::Literal(l)) if l.value() == "true");
            let tris = delaunay(&g.geometry);
            out(polys(alpha_shape(&tris, alpha, holes)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_OptimalAlphaShape",
        func(|args| {
            let g = geom(args.first()?)?;
            let holes = matches!(args.get(1), Some(Term::Literal(l)) if l.value() == "true");
            let components = args.get(2).map_or(Some(1), int)?.max(1) as usize;
            let tris = delaunay(&g.geometry);
            let alpha = optimal_alpha(&g.geometry, &tris, components);
            out(polys(alpha_shape(&tris, alpha, holes)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_MinkowskiSum",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            out(polys(minkowski(&a.geometry, &b.geometry)), &a.crs)
        }),
    );
    add(
        ST,
        "ST_ConstrainedDelaunayTriangles",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let lines: Vec<geo::Line<f64>> = segments(&g.geometry);
            let tris = if lines.is_empty() {
                delaunay(&g.geometry).into_iter().map(|t| t.0).collect()
            } else {
                let mut cdt = lines
                    .constrained_outer_triangulation(Default::default())
                    .ok()?;
                cdt.sort_by(|a, b| {
                    let (p, q) = (a.v1(), b.v1());
                    p.x.total_cmp(&q.x).then(p.y.total_cmp(&q.y))
                });
                cdt.into_iter().map(|t| t.to_polygon()).collect::<Vec<_>>()
            };
            out(
                collection(tris.into_iter().map(Geometry::Polygon).collect()),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_ApproximateMedialAxis",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let mp = MultiPolygon(polygons_of(&g.geometry));
            (!mp.0.is_empty()).then_some(())?;
            out(Geometry::MultiLineString(medial_axis(&mp)?), &g.crs)
        }),
    );
    add(
        ST,
        "ST_IsPlanar",
        func(|args| {
            let [a] = args else { return None };
            boolean(is_planar(&*geom(a)?))
        }),
    );
    add(
        ST,
        "ST_3DArea",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            if let Some(s) = &g.surface {
                return double(crate::postgis_3d::surface_area(s));
            }
            double(area3d(&g))
        }),
    );
    add(
        ST,
        "ST_Orientation",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let Geometry::Polygon(p) = &g.geometry else {
                return None;
            };
            integer(if p.exterior().is_ccw() { -1 } else { 1 })
        }),
    );
    add(
        ST,
        "ST_ForceLHR",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let oriented = match &g.geometry {
                Geometry::Polygon(p) => Geometry::Polygon(p.orient(Direction::Default)),
                Geometry::MultiPolygon(m) => Geometry::MultiPolygon(m.orient(Direction::Default)),
                other => other.clone(),
            };
            Some(crate::literal::geom_literal(&Geom {
                geometry: oriented,
                crs: g.crs.clone(),
                zm: g.zm.clone(),
                surface: None,
                prepared: Default::default(),
            }))
        }),
    );
    // Curves: geometries here are always linear, so the curve functions see a line as a
    // single linear curve.
    for name in [
        "ST_CurveToLine",
        "ST_LineToCurve",
        "ST_ForceCurve",
        "ST_CurveN",
    ] {
        add(
            ST,
            name,
            func(move |args| {
                let g = geom(args.first()?)?;
                if name == "ST_CurveN" {
                    (int(args.get(1)?)? == 1).then_some(())?;
                    matches!(g.geometry, Geometry::LineString(_)).then_some(())?;
                }
                Some(crate::literal::geom_literal(&g))
            }),
        );
    }
    add(
        ST,
        "ST_NumCurves",
        func(|args| {
            let [a] = args else { return None };
            match geom(a)?.geometry {
                Geometry::LineString(_) => integer(1),
                _ => None,
            }
        }),
    );
    add(
        ST,
        "ST_AsX3D",
        func(|args| {
            let g = geom(args.first()?)?;
            let decimals = args.get(1).map_or(Some(15), int)?.clamp(0, 17) as usize;
            string(x3d(&g, decimals))
        }),
    );
}

/// `ST_CoverageUnion` over polygons that form a coverage: their union.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn coverage_union(parts: &[Geometry<f64>]) -> Geometry<f64> {
    let p: Vec<Polygon<f64>> = parts.iter().flat_map(polygons_of).collect();
    let u: MultiPolygon<f64> = geo::algorithm::unary_union(&p);
    polys(u.0)
}

/// The edges of each polygon of a coverage that do not match a neighbour's edges exactly
/// (`ST_CoverageInvalidEdges`): segments that overlap or cross another polygon's boundary
/// without being shared. `None` where the polygon is valid in the coverage.
pub fn coverage_invalid_edges(
    parts: &[Geometry<f64>],
    tolerance: f64,
) -> Vec<Option<Geometry<f64>>> {
    let polys: Vec<MultiPolygon<f64>> =
        parts.iter().map(|g| MultiPolygon(polygons_of(g))).collect();
    let segs: Vec<Vec<geo::Line<f64>>> = parts.iter().map(segments).collect();
    let key = |l: &geo::Line<f64>| {
        let (a, b) = if (l.start.x, l.start.y) <= (l.end.x, l.end.y) {
            (l.start, l.end)
        } else {
            (l.end, l.start)
        };
        (a.x.to_bits(), a.y.to_bits(), b.x.to_bits(), b.y.to_bits())
    };
    let mut count: std::collections::HashMap<(u64, u64, u64, u64), usize> = Default::default();
    for s in segs.iter().flatten() {
        *count.entry(key(s)).or_default() += 1;
    }
    (0..parts.len())
        .map(|i| {
            let bad: Vec<LineString<f64>> = segs[i]
                .iter()
                .filter(|s| {
                    let shared = count.get(&key(s)).copied().unwrap_or(0) > 1;
                    if shared {
                        return false;
                    }
                    let line = LineString::new(vec![s.start, s.end]);
                    let mid = Point::new((s.start.x + s.end.x) / 2.0, (s.start.y + s.end.y) / 2.0);
                    (0..parts.len()).any(|j| {
                        j != i
                            && (polys[j].relate(&mid).is_intersects()
                                && !polys[j].relate(&line).is_touches()
                                || geo::Distance::distance(&geo::Euclidean, &polys[j], &line) > 0.0
                                    && geo::Distance::distance(&geo::Euclidean, &polys[j], &line)
                                        <= tolerance)
                    })
                })
                .map(|s| LineString::new(vec![s.start, s.end]))
                .collect();
            (!bad.is_empty()).then_some(Geometry::MultiLineString(MultiLineString(bad)))
        })
        .collect()
}

/// `ST_CoverageSimplify`: Douglas-Peucker per polygon with the shared edges simplified the
/// same way in each polygon (edges are cut at vertices where three or more polygons meet or
/// where neighbours change, simplified once, and reassembled).
pub fn coverage_simplify(parts: &[Geometry<f64>], tolerance: f64) -> Vec<Geometry<f64>> {
    use geo::Simplify;
    use std::collections::HashMap;
    // A vertex is a node when it is used by rings of different polygon sets.
    let mut users: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    let ring_lists: Vec<Vec<LineString<f64>>> = parts
        .iter()
        .map(|g| {
            polygons_of(g)
                .iter()
                .flat_map(|p| {
                    std::iter::once(p.exterior().clone()).chain(p.interiors().iter().cloned())
                })
                .collect()
        })
        .collect();
    let k = |c: &Coord<f64>| (c.x.to_bits(), c.y.to_bits());
    for (i, rings) in ring_lists.iter().enumerate() {
        for r in rings {
            for c in &r.0 {
                let u = users.entry(k(c)).or_default();
                if u.last() != Some(&i) {
                    u.push(i);
                }
            }
        }
    }
    let mut cache: HashMap<Vec<(u64, u64)>, Vec<Coord<f64>>> = HashMap::new();
    let mut simplify_run = |run: &[Coord<f64>]| -> Vec<Coord<f64>> {
        let fwd: Vec<(u64, u64)> = run.iter().map(k).collect();
        let mut rev = fwd.clone();
        rev.reverse();
        if let Some(s) = cache.get(&fwd) {
            return s.clone();
        }
        if let Some(s) = cache.get(&rev) {
            let mut s = s.clone();
            s.reverse();
            return s;
        }
        let s = LineString::new(run.to_vec()).simplify(tolerance).0;
        cache.insert(fwd, s.clone());
        s
    };
    parts
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let mut out_polys = Vec::new();
            for p in polygons_of(g) {
                let mut rings = Vec::new();
                for r in std::iter::once(p.exterior()).chain(p.interiors()) {
                    let pts = &r.0;
                    let n = pts.len().saturating_sub(1);
                    let neighbours = |c: &Coord<f64>| users.get(&k(c)).cloned().unwrap_or_default();
                    let is_node = |j: usize| {
                        let here = neighbours(&pts[j]);
                        here.len() > 2
                            || here != neighbours(&pts[(j + n - 1) % n])
                            || here != neighbours(&pts[(j + 1) % n])
                    };
                    let nodes: Vec<usize> = (0..n).filter(|&j| is_node(j)).collect();
                    let simplified: Vec<Coord<f64>> = if nodes.is_empty() {
                        let s = LineString::new(pts.clone()).simplify(tolerance).0;
                        if s.len() >= 4 { s } else { pts.clone() }
                    } else {
                        let mut acc: Vec<Coord<f64>> = Vec::new();
                        for w in 0..nodes.len() {
                            let (a, b) = (nodes[w], nodes[(w + 1) % nodes.len()]);
                            let mut run = Vec::new();
                            let mut j = a;
                            loop {
                                run.push(pts[j]);
                                if j == b && run.len() > 1 {
                                    break;
                                }
                                j = (j + 1) % n;
                                if run.len() > n + 1 {
                                    break;
                                }
                            }
                            let s = simplify_run(&run);
                            if acc.is_empty() {
                                acc.extend(s);
                            } else {
                                acc.extend(s.into_iter().skip(1));
                            }
                        }
                        if acc.first() != acc.last() {
                            let f = acc[0];
                            acc.push(f);
                        }
                        if acc.len() >= 4 { acc } else { pts.clone() }
                    };
                    rings.push(LineString::new(simplified));
                }
                let mut it = rings.into_iter();
                let shell = it.next().unwrap();
                out_polys.push(Polygon::new(shell, it.collect()));
            }
            let _ = i;
            polys(out_polys)
        })
        .collect()
}

/// `ST_CoverageClean`: overlaps go to the polygon that comes first, and gaps narrower than
/// `gap_width` (by area/perimeter) are merged into the neighbour sharing the longest border.
pub fn coverage_clean(parts: &[Geometry<f64>], gap_width: f64) -> Vec<Geometry<f64>> {
    use geo::{Area, BoundingRect};
    let mut cleaned: Vec<MultiPolygon<f64>> = Vec::new();
    let mut taken = MultiPolygon::<f64>(Vec::new());
    for g in parts {
        let mp = MultiPolygon(polygons_of(g));
        let own = mp.difference(&taken);
        taken = taken.union(&mp);
        cleaned.push(own);
    }
    if gap_width > 0.0 {
        let all: MultiPolygon<f64> = taken.clone();
        if let Some(r) = all.bounding_rect() {
            let frame = MultiPolygon(vec![r.to_polygon()]);
            let holes: Vec<Polygon<f64>> = all
                .0
                .iter()
                .flat_map(|p| {
                    p.interiors()
                        .iter()
                        .map(|h| Polygon::new(h.clone(), Vec::new()))
                })
                .collect();
            let _ = frame;
            for gap in holes {
                let perimeter = geo::Length::length(&geo::Euclidean, gap.exterior());
                let width = 2.0 * gap.unsigned_area() / perimeter.max(f64::MIN_POSITIVE);
                if width > gap_width {
                    continue;
                }
                let border = |m: &MultiPolygon<f64>| -> f64 {
                    let ring = MultiLineString(vec![gap.exterior().clone()]);
                    m.0.iter()
                        .flat_map(|p| {
                            std::iter::once(p.exterior().clone())
                                .chain(p.interiors().iter().cloned())
                        })
                        .map(|r| {
                            let poly = Polygon::new(r, Vec::new());
                            let buffered = poly.clip(&ring, false);
                            buffered
                                .0
                                .iter()
                                .map(|l| geo::Length::length(&geo::Euclidean, l))
                                .sum::<f64>()
                        })
                        .sum::<f64>()
                };
                if let Some((best, _)) = cleaned
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (i, border(m)))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                {
                    cleaned[best] = cleaned[best].union(&MultiPolygon(vec![gap]));
                }
            }
        }
    }
    cleaned.into_iter().map(|m| polys(m.0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    fn square(x: f64, y: f64) -> Geometry<f64> {
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (x, y),
                (x + 1.0, y),
                (x + 1.0, y + 1.0),
                (x, y + 1.0),
                (x, y),
            ]),
            Vec::new(),
        ))
    }

    #[test]
    fn minkowski_of_square_and_segment() {
        let seg = Geometry::LineString(LineString::from(vec![(0.0, 0.0), (2.0, 0.0)]));
        let s = minkowski(&square(0.0, 0.0), &seg);
        assert_eq!(MultiPolygon(s).unsigned_area(), 3.0);
    }

    #[test]
    fn coverage_union_dissolves_shared_edges() {
        let u = coverage_union(&[square(0.0, 0.0), square(1.0, 0.0)]);
        let Geometry::Polygon(p) = u else {
            panic!("{u:?}")
        };
        assert_eq!(p.unsigned_area(), 2.0);
    }

    #[test]
    fn coverage_invalid_edges_flags_overlaps() {
        let overlapping = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (0.5, 0.0),
                (1.5, 0.0),
                (1.5, 1.0),
                (0.5, 1.0),
                (0.5, 0.0),
            ]),
            Vec::new(),
        ));
        let e = coverage_invalid_edges(&[square(0.0, 0.0), square(1.0, 0.0)], 0.0);
        assert!(e.iter().all(Option::is_none));
        let e = coverage_invalid_edges(&[square(0.0, 0.0), overlapping], 0.0);
        assert!(e.iter().all(Option::is_some));
    }

    #[test]
    fn coverage_clean_removes_overlaps() {
        let overlapping = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (0.5, 0.0),
                (1.5, 0.0),
                (1.5, 1.0),
                (0.5, 1.0),
                (0.5, 0.0),
            ]),
            Vec::new(),
        ));
        let c = coverage_clean(&[square(0.0, 0.0), overlapping], 0.0);
        let area = |g: &Geometry<f64>| MultiPolygon(polygons_of(g)).unsigned_area();
        assert_eq!(area(&c[0]), 1.0);
        assert!((area(&c[1]) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn coverage_simplify_keeps_shared_edges_shared() {
        let a = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (1.0, 0.0),
                (1.01, 0.5),
                (1.0, 1.0),
                (0.0, 1.0),
                (0.0, 0.0),
            ]),
            Vec::new(),
        ));
        let b = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (1.0, 0.0),
                (2.0, 0.0),
                (2.0, 1.0),
                (1.0, 1.0),
                (1.01, 0.5),
                (1.0, 0.0),
            ]),
            Vec::new(),
        ));
        let s = coverage_simplify(&[a, b], 0.1);
        let area = |g: &Geometry<f64>| MultiPolygon(polygons_of(g)).unsigned_area();
        assert!((area(&s[0]) + area(&s[1]) - 2.0).abs() < 1e-12);
        assert!((area(&s[0]) - 1.0).abs() < 1e-12, "{:?}", s[0]);
    }

    #[test]
    fn alpha_shape_of_a_square_grid() {
        let pts: Vec<Point<f64>> = (0..5)
            .flat_map(|x| (0..5).map(move |y| Point::new(x as f64, y as f64)))
            .collect();
        let g = Geometry::MultiPoint(MultiPoint(pts));
        let tris = delaunay(&g);
        let s = alpha_shape(&tris, 1.0, false);
        assert_eq!(MultiPolygon(s).unsigned_area(), 16.0);
        assert!(optimal_alpha(&g, &tris, 1) <= 0.5 + 1e-9);
    }
}
