//! The rest of GeoSPARQL 1.1: Egenhofer and RCC8 relations, geometry accessors and
//! constructors, CRS transformation, serialisation to GeoJSON/GML/KML, and the aggregates.

use crate::crs;
use crate::functions::{Func, GEOF};
use crate::literal::{Crs, GEOJSON_LITERAL, Geom, parse_wkt_literal, wkt_literal};
use geo::{
    BoundingRect, Centroid, ConcaveHull, ConvexHull, Coord, CoordsIter, Geometry, HasDimensions,
    Line, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon, Relate,
    Validation,
};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, NamedNodeRef, Term};
use oxigraph::sparql::AggregateFunctionAccumulator;
use std::fmt::Write;
use std::sync::Arc;

const SF: &str = "http://www.opengis.net/ont/sf#";
const GML_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#gmlLiteral");
const KML_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#kmlLiteral");

fn func(f: impl Fn(&[Term]) -> Option<Term> + Send + Sync + 'static) -> Func {
    Arc::new(f)
}

fn geom(t: &Term) -> Option<Arc<Geom>> {
    parse_wkt_literal(t)
}

fn boolean(v: bool) -> Option<Term> {
    Some(Literal::from(v).into())
}

fn integer(v: i64) -> Option<Term> {
    Some(Literal::from(v).into())
}

fn num(t: &Term) -> Option<f64> {
    let Term::Literal(l) = t else { return None };
    l.datatype()
        .as_str()
        .starts_with("http://www.w3.org/2001/XMLSchema#")
        .then(|| l.value().parse().ok())
        .flatten()
}

/// Egenhofer and RCC8 relations as DE-9IM patterns (GeoSPARQL 1.1, tables 7 and 8). A
/// relation listing several patterns holds when any of them matches.
pub(crate) const RELATIONS: [(&str, &[&str]); 16] = [
    ("ehEquals", &["TFFFTFFFT"]),
    ("ehDisjoint", &["FF*FF****"]),
    ("ehMeet", &["FT*******", "F**T*****", "F***T****"]),
    ("ehOverlap", &["T*T***T**"]),
    ("ehCovers", &["T*TFT*FF*"]),
    ("ehCoveredBy", &["TFF*TFT**"]),
    ("ehInside", &["TFF*FFT**"]),
    ("ehContains", &["T*TFF*FF*"]),
    ("rcc8eq", &["TFFFTFFFT"]),
    ("rcc8dc", &["FFTFFTTTT"]),
    ("rcc8ec", &["FFTFTTTTT"]),
    ("rcc8po", &["TTTTTTTTT"]),
    ("rcc8tppi", &["TTTFTTFFT"]),
    ("rcc8tpp", &["TFFTTFTTT"]),
    ("rcc8ntpp", &["TFFTFFTTT"]),
    ("rcc8ntppi", &["TTTFFTFFT"]),
];

/// Whether a relation can only hold between geometries whose bounding boxes meet (every
/// pattern requires an interior or boundary intersection).
pub(crate) fn needs_contact(patterns: &[&str]) -> bool {
    patterns.iter().all(|p| {
        let b = p.as_bytes();
        [0, 1, 3, 4].iter().any(|&i| b[i] == b'T')
    })
}

/// Coordinate dimensions of a literal's raw text (WKT Z/M, or a third GeoJSON ordinate).
struct Ordinates {
    z: bool,
    m: bool,
    z_range: Option<(f64, f64)>,
}

fn ordinates(t: &Term) -> Option<Ordinates> {
    let Term::Literal(l) = t else { return None };
    let mut out = Ordinates {
        z: false,
        m: false,
        z_range: None,
    };
    let see_z = |z: f64, out: &mut Ordinates| {
        out.z = true;
        out.z_range = Some(match out.z_range {
            None => (z, z),
            Some((lo, hi)) => (lo.min(z), hi.max(z)),
        });
    };
    if l.datatype() == GEOJSON_LITERAL {
        fn walk(v: &serde_json::Value, f: &mut dyn FnMut(f64)) {
            if let Some(a) = v.as_array() {
                if a.first().is_some_and(serde_json::Value::is_number) {
                    if let Some(z) = a.get(2).and_then(serde_json::Value::as_f64) {
                        f(z);
                    }
                } else {
                    a.iter().for_each(|x| walk(x, f));
                }
            } else if let Some(o) = v.as_object() {
                o.values().for_each(|x| walk(x, f));
            }
        }
        let json: serde_json::Value = serde_json::from_str(l.value()).ok()?;
        walk(&json, &mut |z| see_z(z, &mut out));
        return Some(out);
    }
    if let Some(s) = &geom(t)?.surface {
        // A valid 3-D geometry literal.
        if s.has_z {
            s.patches
                .iter()
                .flatten()
                .flatten()
                .for_each(|p| see_z(p[2], &mut out));
            out.z = true;
        }
        return Some(out);
    }
    let text = l.value().trim();
    let text = match text.strip_prefix('<') {
        Some(rest) => rest[rest.find('>')? + 1..].trim_start(),
        None => text,
    };
    let parsed: wkt::Wkt<f64> = text.parse().ok()?;
    use geo_traits::{CoordTrait, GeometryTrait};
    match parsed.dim() {
        geo_traits::Dimensions::Xyz => out.z = true,
        geo_traits::Dimensions::Xym => out.m = true,
        geo_traits::Dimensions::Xyzm => {
            out.z = true;
            out.m = true;
        }
        _ => {}
    }
    if out.z {
        fn coords(w: &wkt::Wkt<f64>, f: &mut dyn FnMut(&wkt::types::Coord<f64>)) {
            use wkt::Wkt::*;
            match w {
                Point(p) => p.coord().into_iter().for_each(&mut *f),
                LineString(l) => l.coords().iter().for_each(&mut *f),
                Polygon(p) => p.rings().iter().flat_map(|r| r.coords()).for_each(&mut *f),
                MultiPoint(m) => m
                    .points()
                    .iter()
                    .filter_map(|p| p.coord())
                    .for_each(&mut *f),
                MultiLineString(m) => m
                    .line_strings()
                    .iter()
                    .flat_map(|l| l.coords())
                    .for_each(&mut *f),
                MultiPolygon(m) => m
                    .polygons()
                    .iter()
                    .flat_map(|p| p.rings())
                    .flat_map(|r| r.coords())
                    .for_each(&mut *f),
                GeometryCollection(g) => g.geometries().iter().for_each(|x| coords(x, f)),
            }
        }
        let mut zs = Vec::new();
        coords(&parsed, &mut |c| {
            if let Some(z) = c.nth(2) {
                zs.push(z);
            }
        });
        out.z_range = None;
        for z in zs {
            see_z(z, &mut out);
        }
    }
    Some(out)
}

fn type_name(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) => "Point",
        Geometry::Line(_) | Geometry::LineString(_) => "LineString",
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => "Polygon",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
    }
}

fn empty() -> Geometry<f64> {
    Geometry::GeometryCollection(geo::GeometryCollection(Vec::new()))
}

/// Topological boundary (OGC Simple Features, mod-2 rule for curves).
pub(crate) fn boundary(g: &Geometry<f64>) -> Geometry<f64> {
    fn curve_ends(lines: &[&LineString<f64>]) -> Geometry<f64> {
        let mut ends: Vec<Coord<f64>> = Vec::new();
        for l in lines {
            if l.0.len() < 2 || l.is_closed() {
                continue;
            }
            for c in [l.0[0], *l.0.last().unwrap()] {
                match ends.iter().position(|e| *e == c) {
                    Some(i) => {
                        ends.swap_remove(i);
                    }
                    None => ends.push(c),
                }
            }
        }
        if ends.is_empty() {
            empty()
        } else {
            Geometry::MultiPoint(MultiPoint(ends.into_iter().map(Point).collect()))
        }
    }
    fn rings(polys: &[&Polygon<f64>]) -> Geometry<f64> {
        let rings: Vec<LineString<f64>> = polys
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .filter(|r| !r.0.is_empty())
            .cloned()
            .collect();
        match rings.len() {
            0 => empty(),
            1 => Geometry::LineString(rings.into_iter().next().unwrap()),
            _ => Geometry::MultiLineString(MultiLineString(rings)),
        }
    }
    match g {
        Geometry::Point(_) | Geometry::MultiPoint(_) => empty(),
        Geometry::Line(l) => curve_ends(&[&LineString(vec![l.start, l.end])]),
        Geometry::LineString(l) => curve_ends(&[l]),
        Geometry::MultiLineString(m) => curve_ends(&m.0.iter().collect::<Vec<_>>()),
        Geometry::Polygon(p) => rings(&[p]),
        Geometry::Rect(r) => rings(&[&r.to_polygon()]),
        Geometry::Triangle(t) => rings(&[&t.to_polygon()]),
        Geometry::MultiPolygon(m) => rings(&m.0.iter().collect::<Vec<_>>()),
        Geometry::GeometryCollection(c) => Geometry::GeometryCollection(geo::GeometryCollection(
            c.iter().map(boundary).filter(|b| !b.is_empty()).collect(),
        )),
    }
}

/// Smallest enclosing circle (Welzl, iterative), as a 64-gon.
pub(crate) fn bounding_circle(points: &[Coord<f64>]) -> Option<Geometry<f64>> {
    type Circle = (Coord<f64>, f64);
    fn contains(c: &Circle, p: Coord<f64>) -> bool {
        (p.x - c.0.x).hypot(p.y - c.0.y) <= c.1 * (1.0 + 1e-12) + 1e-12
    }
    fn two(a: Coord<f64>, b: Coord<f64>) -> Circle {
        let c = Coord {
            x: (a.x + b.x) / 2.0,
            y: (a.y + b.y) / 2.0,
        };
        (c, (a.x - c.x).hypot(a.y - c.y))
    }
    fn three(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> Circle {
        let d = 2.0 * (a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y));
        if d.abs() < f64::EPSILON {
            // Collinear: the circle on the two farthest points.
            let mut best = two(a, b);
            for cand in [two(a, c), two(b, c)] {
                if cand.1 > best.1 {
                    best = cand;
                }
            }
            return best;
        }
        let (a2, b2, c2) = (
            a.x * a.x + a.y * a.y,
            b.x * b.x + b.y * b.y,
            c.x * c.x + c.y * c.y,
        );
        let center = Coord {
            x: (a2 * (b.y - c.y) + b2 * (c.y - a.y) + c2 * (a.y - b.y)) / d,
            y: (a2 * (c.x - b.x) + b2 * (a.x - c.x) + c2 * (b.x - a.x)) / d,
        };
        (center, (a.x - center.x).hypot(a.y - center.y))
    }
    let first = *points.first()?;
    let mut c: Circle = (first, 0.0);
    for i in 0..points.len() {
        if contains(&c, points[i]) {
            continue;
        }
        c = (points[i], 0.0);
        for j in 0..i {
            if contains(&c, points[j]) {
                continue;
            }
            c = two(points[i], points[j]);
            for k in 0..j {
                if !contains(&c, points[k]) {
                    c = three(points[i], points[j], points[k]);
                }
            }
        }
    }
    if c.1 == 0.0 {
        return Some(Geometry::Point(Point(c.0)));
    }
    let ring: Vec<Coord<f64>> = (0..=64)
        .map(|i| {
            let a = std::f64::consts::TAU * (i % 64) as f64 / 64.0;
            // The polygon circumscribes the circle so it still encloses every point.
            let r = c.1 / (std::f64::consts::PI / 64.0).cos();
            Coord {
                x: c.0.x + r * a.cos(),
                y: c.0.y + r * a.sin(),
            }
        })
        .collect();
    Some(Geometry::Polygon(Polygon::new(
        LineString(ring),
        Vec::new(),
    )))
}

pub(crate) fn coords_of(g: &Geometry<f64>) -> Vec<Coord<f64>> {
    g.coords_iter().collect()
}

/// PostGIS-style target percent (1 = convex hull) to the concavity of `geo`'s concave hull.
fn concave_hull(g: &Geometry<f64>, target: f64) -> Option<Geometry<f64>> {
    if target >= 1.0 {
        return Some(Geometry::Polygon(g.convex_hull()));
    }
    let points = MultiPoint(coords_of(g).into_iter().map(Point).collect());
    if points.0.len() < 3 {
        return Some(Geometry::Polygon(g.convex_hull()));
    }
    let concavity = 1.0 + 4.0 * target.max(0.0);
    Some(Geometry::Polygon(points.concave_hull_with_options(
        geo::algorithm::concave_hull::ConcaveHullOptions {
            concavity,
            length_threshold: 0.0,
        },
    )))
}

/// OGC simplicity: no self-intersection other than at boundary points.
pub(crate) fn is_simple(g: &Geometry<f64>) -> bool {
    fn line_simple(l: &LineString<f64>) -> bool {
        let segs: Vec<Line<f64>> = l.lines().filter(|s| s.start != s.end).collect();
        let closed = l.is_closed();
        for i in 0..segs.len() {
            for j in i + 1..segs.len() {
                let Some(hit) =
                    geo::algorithm::line_intersection::line_intersection(segs[i], segs[j])
                else {
                    continue;
                };
                use geo::algorithm::line_intersection::LineIntersection::*;
                let adjacent = j == i + 1 || (closed && i == 0 && j == segs.len() - 1);
                match hit {
                    Collinear { .. } => return false,
                    SinglePoint { intersection, .. } => {
                        let shared = if j == i + 1 {
                            segs[i].end
                        } else {
                            segs[i].start
                        };
                        if !(adjacent && intersection == shared) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }
    match g {
        Geometry::Point(_) | Geometry::Line(_) => true,
        Geometry::LineString(l) => line_simple(l),
        Geometry::MultiLineString(m) => {
            m.0.iter().all(line_simple)
                && m.0.iter().enumerate().all(|(i, a)| {
                    m.0[i + 1..].iter().all(|b| {
                        let im = a.relate(b);
                        // Curves may only meet at boundary points.
                        im.matches("F**F*****").unwrap_or(false)
                    })
                })
        }
        Geometry::MultiPoint(m) => {
            m.0.iter()
                .enumerate()
                .all(|(i, p)| m.0[i + 1..].iter().all(|q| p != q))
        }
        Geometry::Polygon(_)
        | Geometry::MultiPolygon(_)
        | Geometry::Rect(_)
        | Geometry::Triangle(_) => g.is_valid(),
        Geometry::GeometryCollection(c) => c.iter().all(is_simple),
    }
}

fn geometry_n(g: &Geometry<f64>, n: usize) -> Option<Geometry<f64>> {
    let n = n.checked_sub(1)?;
    match g {
        Geometry::MultiPoint(m) => m.0.get(n).copied().map(Geometry::Point),
        Geometry::MultiLineString(m) => m.0.get(n).cloned().map(Geometry::LineString),
        Geometry::MultiPolygon(m) => m.0.get(n).cloned().map(Geometry::Polygon),
        Geometry::GeometryCollection(c) => c.0.get(n).cloned(),
        other => (n == 0).then(|| other.clone()),
    }
}

fn fmt_num(out: &mut String, v: f64) {
    if v == v.trunc() && v.abs() < 1e15 {
        let _ = write!(out, "{}", v as i64);
    } else {
        crate::wkt_out::number(out, v);
    }
}

/// RFC 7946 GeoJSON (coordinates must already be CRS84).
pub(crate) fn to_geojson(g: &Geometry<f64>) -> String {
    fn coord(out: &mut String, c: Coord<f64>) {
        out.push('[');
        fmt_num(out, c.x);
        out.push(',');
        fmt_num(out, c.y);
        out.push(']');
    }
    fn coords(out: &mut String, cs: &[Coord<f64>]) {
        out.push('[');
        for (i, c) in cs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            coord(out, *c);
        }
        out.push(']');
    }
    fn poly(out: &mut String, p: &Polygon<f64>) {
        out.push('[');
        coords(out, &p.exterior().0);
        for r in p.interiors() {
            out.push(',');
            coords(out, &r.0);
        }
        out.push(']');
    }
    fn list<T>(out: &mut String, items: &[T], f: fn(&mut String, &T)) {
        out.push('[');
        for (i, x) in items.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            f(out, x);
        }
        out.push(']');
    }
    let mut out = String::new();
    match g {
        Geometry::GeometryCollection(c) => {
            out.push_str(r#"{"type":"GeometryCollection","geometries":["#);
            for (i, x) in c.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&to_geojson(x));
            }
            out.push_str("]}");
            return out;
        }
        Geometry::Line(l) => {
            return to_geojson(&Geometry::LineString(LineString(vec![l.start, l.end])));
        }
        Geometry::Rect(r) => return to_geojson(&Geometry::Polygon(r.to_polygon())),
        Geometry::Triangle(t) => return to_geojson(&Geometry::Polygon(t.to_polygon())),
        _ => {}
    }
    let _ = write!(out, r#"{{"type":"{}","coordinates":"#, type_name(g));
    match g {
        Geometry::Point(p) => coord(&mut out, p.0),
        Geometry::LineString(l) => coords(&mut out, &l.0),
        Geometry::Polygon(p) => poly(&mut out, p),
        Geometry::MultiPoint(m) => list(&mut out, &m.0, |o, p| coord(o, p.0)),
        Geometry::MultiLineString(m) => list(&mut out, &m.0, |o, l| coords(o, &l.0)),
        Geometry::MultiPolygon(m) => list(&mut out, &m.0, poly),
        _ => unreachable!(),
    }
    out.push('}');
    out
}

/// GML 3.2 (`gml:` prefix declared on the root element).
pub(crate) fn to_gml(g: &Geometry<f64>, srs: &str, swap: bool) -> String {
    to_gml_in(g, srs, swap, "http://www.opengis.net/gml/3.2")
}

/// [`to_gml`] with the `gml:` prefix bound to namespace `ns`.
fn to_gml_in(g: &Geometry<f64>, srs: &str, swap: bool, ns: &str) -> String {
    fn pos_list(out: &mut String, cs: &[Coord<f64>], swap: bool) {
        for (i, c) in cs.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let (a, b) = if swap { (c.y, c.x) } else { (c.x, c.y) };
            fmt_num(out, a);
            out.push(' ');
            fmt_num(out, b);
        }
    }
    fn element(out: &mut String, g: &Geometry<f64>, attrs: &str, swap: bool) {
        let ring = |out: &mut String, tag: &str, r: &LineString<f64>| {
            let _ = write!(out, "<gml:{tag}><gml:LinearRing><gml:posList>");
            pos_list(out, &r.0, swap);
            let _ = write!(out, "</gml:posList></gml:LinearRing></gml:{tag}>");
        };
        let poly = |out: &mut String, p: &Polygon<f64>, attrs: &str| {
            let _ = write!(out, "<gml:Polygon{attrs}>");
            ring(out, "exterior", p.exterior());
            for r in p.interiors() {
                ring(out, "interior", r);
            }
            out.push_str("</gml:Polygon>");
        };
        match g {
            Geometry::Point(p) => {
                let _ = write!(out, "<gml:Point{attrs}><gml:pos>");
                pos_list(out, &[p.0], swap);
                out.push_str("</gml:pos></gml:Point>");
            }
            Geometry::Line(l) => element(
                out,
                &Geometry::LineString(LineString(vec![l.start, l.end])),
                attrs,
                swap,
            ),
            Geometry::LineString(l) => {
                let _ = write!(out, "<gml:LineString{attrs}><gml:posList>");
                pos_list(out, &l.0, swap);
                out.push_str("</gml:posList></gml:LineString>");
            }
            Geometry::Polygon(p) => poly(out, p, attrs),
            Geometry::Rect(r) => poly(out, &r.to_polygon(), attrs),
            Geometry::Triangle(t) => poly(out, &t.to_polygon(), attrs),
            Geometry::MultiPoint(m) => {
                let _ = write!(out, "<gml:MultiPoint{attrs}>");
                for p in &m.0 {
                    out.push_str("<gml:pointMember>");
                    element(out, &Geometry::Point(*p), "", swap);
                    out.push_str("</gml:pointMember>");
                }
                out.push_str("</gml:MultiPoint>");
            }
            Geometry::MultiLineString(m) => {
                let _ = write!(out, "<gml:MultiCurve{attrs}>");
                for l in &m.0 {
                    out.push_str("<gml:curveMember>");
                    element(out, &Geometry::LineString(l.clone()), "", swap);
                    out.push_str("</gml:curveMember>");
                }
                out.push_str("</gml:MultiCurve>");
            }
            Geometry::MultiPolygon(m) => {
                let _ = write!(out, "<gml:MultiSurface{attrs}>");
                for p in &m.0 {
                    out.push_str("<gml:surfaceMember>");
                    poly(out, p, "");
                    out.push_str("</gml:surfaceMember>");
                }
                out.push_str("</gml:MultiSurface>");
            }
            Geometry::GeometryCollection(c) => {
                let _ = write!(out, "<gml:MultiGeometry{attrs}>");
                for x in c.iter() {
                    out.push_str("<gml:geometryMember>");
                    element(out, x, "", swap);
                    out.push_str("</gml:geometryMember>");
                }
                out.push_str("</gml:MultiGeometry>");
            }
        }
    }
    let mut out = String::new();
    let attrs = format!(r#" xmlns:gml="{ns}" srsName="{srs}""#);
    element(&mut out, g, &attrs, swap);
    out
}

/// KML 2.2 (coordinates must already be CRS84).
pub(crate) fn to_kml(g: &Geometry<f64>) -> String {
    fn coordinates(out: &mut String, cs: &[Coord<f64>]) {
        out.push_str("<coordinates>");
        for (i, c) in cs.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            fmt_num(out, c.x);
            out.push(',');
            fmt_num(out, c.y);
        }
        out.push_str("</coordinates>");
    }
    fn element(out: &mut String, g: &Geometry<f64>) {
        let poly = |out: &mut String, p: &Polygon<f64>| {
            out.push_str("<Polygon><outerBoundaryIs><LinearRing>");
            coordinates(out, &p.exterior().0);
            out.push_str("</LinearRing></outerBoundaryIs>");
            for r in p.interiors() {
                out.push_str("<innerBoundaryIs><LinearRing>");
                coordinates(out, &r.0);
                out.push_str("</LinearRing></innerBoundaryIs>");
            }
            out.push_str("</Polygon>");
        };
        let multi = |out: &mut String, parts: Vec<Geometry<f64>>| {
            out.push_str("<MultiGeometry>");
            for p in &parts {
                element(out, p);
            }
            out.push_str("</MultiGeometry>");
        };
        match g {
            Geometry::Point(p) => {
                out.push_str("<Point>");
                coordinates(out, &[p.0]);
                out.push_str("</Point>");
            }
            Geometry::Line(l) => {
                element(out, &Geometry::LineString(LineString(vec![l.start, l.end])))
            }
            Geometry::LineString(l) => {
                out.push_str("<LineString>");
                coordinates(out, &l.0);
                out.push_str("</LineString>");
            }
            Geometry::Polygon(p) => poly(out, p),
            Geometry::Rect(r) => poly(out, &r.to_polygon()),
            Geometry::Triangle(t) => poly(out, &t.to_polygon()),
            Geometry::MultiPoint(m) => {
                multi(out, m.0.iter().map(|p| Geometry::Point(*p)).collect())
            }
            Geometry::MultiLineString(m) => {
                multi(out, m.0.iter().cloned().map(Geometry::LineString).collect())
            }
            Geometry::MultiPolygon(m) => {
                multi(out, m.0.iter().cloned().map(Geometry::Polygon).collect())
            }
            Geometry::GeometryCollection(c) => multi(out, c.0.clone()),
        }
    }
    let mut out = String::new();
    element(&mut out, g);
    out
}

fn typed(value: String, datatype: NamedNodeRef<'_>) -> Option<Term> {
    Some(Literal::new_typed_literal(value, datatype).into())
}

fn crs_iri(crs: &Crs) -> String {
    match crs {
        Crs::Crs84 => "http://www.opengis.net/def/crs/OGC/1.3/CRS84".into(),
        Crs::Epsg4326 => "http://www.opengis.net/def/crs/EPSG/0/4326".into(),
        Crs::Other(iri) => iri.clone(),
    }
}

/// The result of a geometry function as a literal in the serialization of its input `like`:
/// GML for a GML literal (in the input's GML namespace), else WKT.
pub(crate) fn literal_like(geometry: &Geometry<f64>, crs: &Crs, like: &Term) -> Term {
    if let Term::Literal(l) = like
        && l.datatype() == GML_LITERAL
    {
        let text = l.value();
        let ns = text
            .split_once("xmlns:gml=")
            .and_then(|(_, rest)| {
                let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
                rest[1..].split_once(quote)
            })
            .map_or("http://www.opengis.net/gml/3.2", |(ns, _)| ns);
        let swap = *crs == Crs::Epsg4326;
        return Literal::new_typed_literal(
            to_gml_in(geometry, &crs_iri(crs), swap, ns),
            GML_LITERAL,
        )
        .into();
    }
    wkt_literal(geometry, crs)
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    for (name, patterns) in RELATIONS {
        add(
            GEOF,
            name,
            func(move |args| {
                let [a, b] = args else { return None };
                let (a, b) = (geom(a)?, geom(b)?);
                boolean(crate::predicate::patterns(patterns, &a, &b)?)
            }),
        );
    }

    let unary = |add: &mut dyn FnMut(&str, &str, Func),
                 name: &str,
                 f: fn(&Geom) -> Option<Geometry<f64>>| {
        add(
            GEOF,
            name,
            func(move |args| {
                let [a] = args else { return None };
                let g = geom(a)?;
                Some(literal_like(&f(&g)?, &g.crs, a))
            }),
        );
    };
    unary(add, "boundary", |g| Some(boundary(&g.geometry)));
    unary(add, "boundingCircle", |g| {
        bounding_circle(&coords_of(&g.geometry))
    });

    add(
        GEOF,
        "concaveHull",
        func(|args| {
            let (a, target) = match args {
                [a] => (a, 0.0),
                [a, t] => (a, num(t)?),
                _ => return None,
            };
            let g = geom(a)?;
            Some(wkt_literal(&concave_hull(&g.geometry, target)?, &g.crs))
        }),
    );
    add(
        GEOF,
        "geometryN",
        func(|args| {
            let [a, n] = args else { return None };
            let g = geom(a)?;
            let n = num(n)?;
            (n.fract() == 0.0 && n >= 1.0).then_some(())?;
            if let Some(s) = &g.surface {
                return crate::postgis_3d::surface_n(s, n as usize, &g.crs);
            }
            Some(wkt_literal(&geometry_n(&g.geometry, n as usize)?, &g.crs))
        }),
    );
    add(
        GEOF,
        "geometryType",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let name = match g.surface.as_deref().map(|s| s.kind) {
                Some(crate::literal::SurfaceKind::PolyhedralSurface) => "PolyhedralSurface",
                Some(crate::literal::SurfaceKind::Tin) => "TIN",
                Some(crate::literal::SurfaceKind::Triangle) => "Triangle",
                _ => type_name(&g.geometry),
            };
            typed(format!("{SF}{name}"), xsd::ANY_URI)
        }),
    );
    add(
        GEOF,
        "isSimple",
        func(|args| {
            let [a] = args else { return None };
            boolean(is_simple(&geom(a)?.geometry))
        }),
    );
    add(
        GEOF,
        "is3D",
        func(|args| {
            let [a] = args else { return None };
            boolean(ordinates(a)?.z)
        }),
    );
    add(
        GEOF,
        "isMeasured",
        func(|args| {
            let [a] = args else { return None };
            boolean(ordinates(a)?.m)
        }),
    );
    add(
        GEOF,
        "coordinateDimension",
        func(|args| {
            let [a] = args else { return None };
            let o = ordinates(a)?;
            integer(2 + i64::from(o.z) + i64::from(o.m))
        }),
    );
    add(
        GEOF,
        "spatialDimension",
        func(|args| {
            let [a] = args else { return None };
            integer(2 + i64::from(ordinates(a)?.z))
        }),
    );
    add(
        GEOF,
        "minZ",
        func(|args| {
            let [a] = args else { return None };
            Some(Literal::from(ordinates(a)?.z_range?.0).into())
        }),
    );
    add(
        GEOF,
        "maxZ",
        func(|args| {
            let [a] = args else { return None };
            Some(Literal::from(ordinates(a)?.z_range?.1).into())
        }),
    );
    add(
        GEOF,
        "transform",
        func(|args| {
            let [a, target] = args else {
                return None;
            };
            let g = geom(a)?;
            let to = crs::crs_arg(target)?;
            Some(crate::literal::geom_literal(&crs::transform_geom(&g, &to)?))
        }),
    );
    add(
        GEOF,
        "asGeoJSON",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let ll = crs::to_lon_lat(&g.geometry, &g.crs)?;
            typed(to_geojson(&ll), GEOJSON_LITERAL)
        }),
    );
    add(
        GEOF,
        "asKML",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let ll = crs::to_lon_lat(&g.geometry, &g.crs)?;
            typed(to_kml(&ll), KML_LITERAL)
        }),
    );
    add(
        GEOF,
        "asGML",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            // GML follows the CRS's own axis order: EPSG:4326 is latitude first.
            let swap = g.crs == Crs::Epsg4326;
            typed(to_gml(&g.geometry, &crs_iri(&g.crs), swap), GML_LITERAL)
        }),
    );
}

#[derive(Clone, Copy)]
enum Aggregate {
    BoundingBox,
    BoundingCircle,
    Centroid,
    ConcaveHull,
    ConvexHull,
    Union,
    /// PostGIS `ST_Collect` as an aggregate: the parts of the group as one multi-geometry or
    /// collection.
    Collect,
    /// PostGIS `ST_MakeLine` as an aggregate: the points and lines of the group, in the
    /// order they come, joined into one line.
    MakeLine,
    /// PostGIS `ST_Polygonize` as an aggregate: the faces formed by the group's linework.
    Polygonize,
    /// PostGIS `ST_ClusterIntersecting`: the group split into collections of geometries
    /// connected by intersection.
    ClusterIntersecting,
    /// PostGIS `ST_3DExtent`: the 3-D box of the group, as the line from its lowest to its
    /// highest corner (`LINESTRING Z`), Z 0 where no input has Z.
    Extent3D,
}

struct Accumulator {
    kind: Aggregate,
    crs: Option<Crs>,
    geometries: Vec<Geometry<f64>>,
    /// Inputs with Z or M, whose ordinates the result keeps.
    zm_sources: Vec<Arc<Geom>>,
    failed: bool,
}

impl AggregateFunctionAccumulator for Accumulator {
    fn accumulate(&mut self, element: Term) {
        let Some(g) = geom(&element) else {
            self.failed = true;
            return;
        };
        match &self.crs {
            None => self.crs = Some(g.crs.clone()),
            Some(c) if *c == g.crs || (c.is_geographic() && g.crs.is_geographic()) => {}
            Some(_) => self.failed = true,
        }
        self.geometries.push(g.geometry.clone());
        if g.zm.is_some() {
            self.zm_sources.push(g);
        }
    }

    fn finish(&mut self) -> Option<Term> {
        if self.failed {
            return None;
        }
        let crs = self.crs.clone()?;
        let sources = std::mem::take(&mut self.zm_sources);
        if let Aggregate::MakeLine = self.kind {
            let mut coords: Vec<geo::Coord<f64>> = Vec::new();
            for g in &self.geometries {
                match g {
                    Geometry::Point(_)
                    | Geometry::MultiPoint(_)
                    | Geometry::Line(_)
                    | Geometry::LineString(_)
                    | Geometry::MultiLineString(_) => {}
                    _ => return None,
                }
                for c in geo::CoordsIter::coords_iter(g) {
                    if coords.last() != Some(&c) {
                        coords.push(c);
                    }
                }
            }
            (coords.len() >= 2).then_some(())?;
            let line = Geometry::LineString(geo::LineString::new(coords));
            return Some(crate::literal::with_zm_sources(sources, || {
                wkt_literal(&line, &crs)
            }));
        }
        if let Aggregate::Extent3D = self.kind {
            return extent_3d(&self.geometries, &sources, &crs);
        }
        let all = Geometry::GeometryCollection(geo::GeometryCollection(std::mem::take(
            &mut self.geometries,
        )));
        let out = match self.kind {
            Aggregate::BoundingBox => Geometry::Polygon(all.bounding_rect()?.to_polygon()),
            Aggregate::BoundingCircle => bounding_circle(&coords_of(&all))?,
            Aggregate::Centroid => Geometry::Point(all.centroid()?),
            Aggregate::ConcaveHull => concave_hull(&all, 0.0)?,
            Aggregate::ConvexHull => Geometry::Polygon(all.convex_hull()),
            Aggregate::Union => union_all(&all),
            Aggregate::Collect => {
                let Geometry::GeometryCollection(c) = all else {
                    unreachable!()
                };
                crate::postgis::collect(c.0)
            }
            Aggregate::Polygonize => crate::postgis_shapes::polygonize(&all),
            Aggregate::ClusterIntersecting => {
                let Geometry::GeometryCollection(c) = all else {
                    unreachable!()
                };
                crate::postgis_shapes::cluster_intersecting(c.0)
            }
            Aggregate::MakeLine | Aggregate::Extent3D => unreachable!(),
        };
        Some(wkt_literal(&out, &crs))
    }
}

/// The 3-D box of geometries as the line between its corners, in WKT.
pub(crate) fn extent_3d(geometries: &[Geometry<f64>], zm: &[Arc<Geom>], crs: &Crs) -> Option<Term> {
    let rect = Geometry::GeometryCollection(geo::GeometryCollection(geometries.to_vec()))
        .bounding_rect()?;
    let mut z: Option<(f64, f64)> = None;
    for g in zm {
        let Some(values) = g.zm.as_deref().filter(|v| v.has_z) else {
            continue;
        };
        for c in geo::CoordsIter::coords_iter(&g.geometry) {
            if let Some((v, _)) = values.at(c).filter(|(v, _)| v.is_finite()) {
                z = Some(z.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))));
            }
        }
    }
    let (z0, z1) = z.unwrap_or((0.0, 0.0));
    let (min, max) = (rect.min(), rect.max());
    let corner = |x: f64, y: f64, z: f64| {
        if *crs == Crs::Epsg4326 {
            format!("{y} {x} {z}")
        } else {
            format!("{x} {y} {z}")
        }
    };
    let body = format!(
        "LINESTRING Z({},{})",
        corner(min.x, min.y, z0),
        corner(max.x, max.y, z1)
    );
    let text = match crs {
        Crs::Crs84 => body,
        other => format!("<{}> {body}", crs_iri(other)),
    };
    typed(text, crate::literal::WKT_LITERAL)
}

/// Union of a collection: areas are dissolved, lower-dimensional parts not covered by them
/// are kept.
fn union_all(all: &Geometry<f64>) -> Geometry<f64> {
    let Geometry::GeometryCollection(c) = all else {
        return all.clone();
    };
    let mut polygons: Vec<Polygon<f64>> = Vec::new();
    let mut others: Vec<Geometry<f64>> = Vec::new();
    fn split(g: &Geometry<f64>, polygons: &mut Vec<Polygon<f64>>, others: &mut Vec<Geometry<f64>>) {
        match g {
            Geometry::Polygon(p) => polygons.push(p.clone()),
            Geometry::MultiPolygon(m) => polygons.extend(m.0.iter().cloned()),
            Geometry::Rect(r) => polygons.push(r.to_polygon()),
            Geometry::Triangle(t) => polygons.push(t.to_polygon()),
            Geometry::GeometryCollection(c) => c.iter().for_each(|x| split(x, polygons, others)),
            other => others.push(other.clone()),
        }
    }
    c.iter().for_each(|g| split(g, &mut polygons, &mut others));
    let area: MultiPolygon<f64> = geo::algorithm::unary_union(&polygons);
    let others: Vec<Geometry<f64>> = others
        .into_iter()
        .filter(|g| area.0.is_empty() || !area.relate(g).is_covers())
        .collect();
    match (area.0.is_empty(), others.is_empty()) {
        (false, true) if area.0.len() == 1 => Geometry::Polygon(area.0[0].clone()),
        (false, true) => Geometry::MultiPolygon(area),
        (true, false) if others.len() == 1 => others.into_iter().next().unwrap(),
        _ => {
            let mut parts: Vec<Geometry<f64>> = area.0.into_iter().map(Geometry::Polygon).collect();
            parts.extend(others);
            Geometry::GeometryCollection(geo::GeometryCollection(parts))
        }
    }
}

/// Creates a fresh accumulator for one aggregate group.
pub type AccumulatorFactory = fn() -> Box<dyn AggregateFunctionAccumulator + Send + Sync>;

/// The GeoSPARQL aggregate functions and the PostGIS ones (`st:ST_MakeLineAgg`,
/// `st:ST_Extent`, `st:ST_UnionAgg`, `st:ST_CollectAgg`), by IRI.
pub fn aggregate_functions() -> Vec<(NamedNode, AccumulatorFactory)> {
    fn make(kind: Aggregate) -> Box<dyn AggregateFunctionAccumulator + Send + Sync> {
        Box::new(Accumulator {
            kind,
            crs: None,
            geometries: Vec::new(),
            zm_sources: Vec::new(),
            failed: false,
        })
    }
    let entry = |name: &str, f: fn() -> Box<dyn AggregateFunctionAccumulator + Send + Sync>| {
        (NamedNode::new_unchecked(format!("{GEOF}{name}")), f)
    };
    vec![
        entry("aggBoundingBox", || make(Aggregate::BoundingBox)),
        entry("aggBoundingCircle", || make(Aggregate::BoundingCircle)),
        entry("aggCentroid", || make(Aggregate::Centroid)),
        entry("aggConcaveHull", || make(Aggregate::ConcaveHull)),
        entry("aggConvexHull", || make(Aggregate::ConvexHull)),
        entry("aggUnion", || make(Aggregate::Union)),
        (
            NamedNode::new_unchecked(format!("{}ST_MakeLineAgg", crate::functions::ST)),
            || make(Aggregate::MakeLine),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_Extent", crate::functions::ST)),
            || make(Aggregate::BoundingBox),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_EstimatedExtent", crate::functions::ST)),
            || make(Aggregate::BoundingBox),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_AsFlatGeobuf", crate::functions::ST)),
            crate::formats::FeatureAccumulator::flatgeobuf,
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_AsGeobuf", crate::functions::ST)),
            crate::formats::FeatureAccumulator::geobuf,
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_CoverageUnion", crate::functions::ST)),
            || make(Aggregate::Union),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_UnionAgg", crate::functions::ST)),
            || make(Aggregate::Union),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_CollectAgg", crate::functions::ST)),
            || make(Aggregate::Collect),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_MemUnion", crate::functions::ST)),
            || make(Aggregate::Union),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_PolygonizeAgg", crate::functions::ST)),
            || make(Aggregate::Polygonize),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_ClusterIntersecting", crate::functions::ST)),
            || make(Aggregate::ClusterIntersecting),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_3DExtent", crate::functions::ST)),
            || make(Aggregate::Extent3D),
        ),
        (
            NamedNode::new_unchecked(format!("{}ST_AsMVT", crate::functions::ST)),
            crate::mvt::MvtAccumulator::boxed,
        ),
    ]
}
