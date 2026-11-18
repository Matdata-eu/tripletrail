//! More PostGIS functions under their `st:ST_*` names: constructors, accessors, editors,
//! affine transforms, measures and CRS handling. Semantics follow PostGIS for `geometry`
//! (planar, in the units of the CRS) unless a function says otherwise.

use crate::functions::{
    Func, FunctionTable, GEOF, ST, boolean, double, func, geom, integer, num, same_space,
};
use crate::literal::{Crs, crs_from_iri, wkt_literal};
use geo::line_measures::{Densify, Destination, Euclidean, FrechetDistance, Geodesic};
use geo::{
    BooleanOps, BoundingRect, ChaikinSmoothing, Closest, ClosestPoint, Coord, CoordsIter, Geometry,
    GeometryCollection, HausdorffDistance, InteriorPoint, Line, LineString, MapCoords,
    MultiLineString, MultiPoint, MultiPolygon, Orient, Point, Polygon, Rect, SimplifyVw,
    Validation, orient::Direction,
};
use oxigraph::model::NamedNode;
use oxigraph::model::{Literal, Term};
use std::f64::consts::PI;
use std::sync::Arc;

pub(crate) fn string(v: impl Into<String>) -> Option<Term> {
    Some(Literal::new_simple_literal(v).into())
}

pub(crate) fn int(t: &Term) -> Option<i64> {
    let v = num(t)?;
    (v.fract() == 0.0).then_some(v as i64)
}

/// The EPSG code of a CRS, as PostGIS's SRID (CRS84 is 4326, unknown is 0).
pub(crate) fn srid(crs: &Crs) -> i64 {
    match crs {
        Crs::Crs84 | Crs::Epsg4326 => 4326,
        Crs::Other(iri) => iri
            .rsplit_once("/EPSG/0/")
            .and_then(|(_, code)| code.parse().ok())
            .unwrap_or(0),
    }
}

/// The CRS of an SRID. 4326 becomes CRS84: PostGIS keeps 4326 data in longitude, latitude
/// order, which is what CRS84 literals hold.
pub(crate) fn crs_of_srid(srid: i64) -> Option<Crs> {
    if srid == 4326 {
        return Some(Crs::Crs84);
    }
    (srid > 0).then(|| crs_from_iri(&format!("http://www.opengis.net/def/crs/EPSG/0/{srid}")))
}

/// A box as PostGIS writes it: from the lower-left corner, up the left side first.
pub(crate) fn envelope(min: Coord<f64>, max: Coord<f64>) -> Geometry<f64> {
    let ring = vec![
        min,
        Coord { x: min.x, y: max.y },
        max,
        Coord { x: max.x, y: min.y },
        min,
    ];
    Geometry::Polygon(Polygon::new(LineString::new(ring), Vec::new()))
}

pub(crate) fn out(g: Geometry<f64>, crs: &Crs) -> Option<Term> {
    Some(wkt_literal(&g, crs))
}

fn points(g: &Geometry<f64>) -> Vec<Coord<f64>> {
    g.coords_iter().collect()
}

/// A geometry as one line: a LineString, or a single-part MultiLineString.
pub(crate) fn line(g: &Geometry<f64>) -> Option<&LineString<f64>> {
    match g {
        Geometry::LineString(l) => Some(l),
        Geometry::MultiLineString(m) if m.0.len() == 1 => m.0.first(),
        _ => None,
    }
}

pub(crate) fn polygon(g: &Geometry<f64>) -> Option<&Polygon<f64>> {
    match g {
        Geometry::Polygon(p) => Some(p),
        Geometry::MultiPolygon(m) if m.0.len() == 1 => m.0.first(),
        _ => None,
    }
}

pub(crate) fn parts(g: &Geometry<f64>) -> Vec<Geometry<f64>> {
    match g {
        Geometry::MultiPoint(m) => m.0.iter().map(|p| Geometry::Point(*p)).collect(),
        Geometry::MultiLineString(m) => m.0.iter().cloned().map(Geometry::LineString).collect(),
        Geometry::MultiPolygon(m) => m.0.iter().cloned().map(Geometry::Polygon).collect(),
        Geometry::GeometryCollection(c) => c.0.iter().flat_map(parts).collect(),
        other => vec![other.clone()],
    }
}

/// Parts of one kind as a multi-geometry, or a collection when they are mixed.
pub(crate) fn collect(parts: Vec<Geometry<f64>>) -> Geometry<f64> {
    if !parts.is_empty() && parts.iter().all(|g| matches!(g, Geometry::Point(_))) {
        return Geometry::MultiPoint(MultiPoint(
            parts
                .into_iter()
                .filter_map(|g| Point::try_from(g).ok())
                .collect(),
        ));
    }
    if !parts.is_empty() && parts.iter().all(|g| matches!(g, Geometry::LineString(_))) {
        return Geometry::MultiLineString(MultiLineString(
            parts
                .into_iter()
                .filter_map(|g| LineString::try_from(g).ok())
                .collect(),
        ));
    }
    if !parts.is_empty() && parts.iter().all(|g| matches!(g, Geometry::Polygon(_))) {
        return Geometry::MultiPolygon(MultiPolygon(
            parts
                .into_iter()
                .filter_map(|g| Polygon::try_from(g).ok())
                .collect(),
        ));
    }
    Geometry::GeometryCollection(GeometryCollection(parts))
}

pub(crate) fn postgis_type(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) => "POINT",
        Geometry::Line(_) | Geometry::LineString(_) => "LINESTRING",
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => "POLYGON",
        Geometry::MultiPoint(_) => "MULTIPOINT",
        Geometry::MultiLineString(_) => "MULTILINESTRING",
        Geometry::MultiPolygon(_) => "MULTIPOLYGON",
        Geometry::GeometryCollection(_) => "GEOMETRYCOLLECTION",
    }
}

/// Merges lines that share end points into longer lines (PostGIS ST_LineMerge, without
/// direction changes across degree-3 nodes: a line continues only where exactly two lines
/// meet).
fn line_merge(lines: Vec<LineString<f64>>) -> Vec<LineString<f64>> {
    use std::collections::HashMap;
    let key = |c: Coord<f64>| (c.x.to_bits(), c.y.to_bits());
    let mut degree: HashMap<(u64, u64), usize> = HashMap::new();
    for l in &lines {
        if let (Some(a), Some(b)) = (l.0.first(), l.0.last()) {
            *degree.entry(key(*a)).or_default() += 1;
            *degree.entry(key(*b)).or_default() += 1;
        }
    }
    let mut used = vec![false; lines.len()];
    let mut merged = Vec::new();
    for start in 0..lines.len() {
        if used[start] || lines[start].0.len() < 2 {
            continue;
        }
        used[start] = true;
        let mut coords = lines[start].0.clone();
        // Extend forward, then backward, through nodes where exactly two lines meet.
        for _ in 0..2 {
            loop {
                let end = *coords.last().unwrap();
                if degree.get(&key(end)) != Some(&2) {
                    break;
                }
                let next = (0..lines.len()).find(|&i| {
                    !used[i]
                        && lines[i].0.len() >= 2
                        && (lines[i].0[0] == end || *lines[i].0.last().unwrap() == end)
                });
                let Some(i) = next else { break };
                used[i] = true;
                let mut part = lines[i].0.clone();
                if part[0] != end {
                    part.reverse();
                }
                coords.extend(part.into_iter().skip(1));
                if coords.first() == coords.last() {
                    break;
                }
            }
            coords.reverse();
        }
        merged.push(LineString::new(coords));
    }
    merged
}

fn max_distance(a: &Geometry<f64>, b: &Geometry<f64>) -> f64 {
    let (pa, pb) = (points(a), points(b));
    let mut best: f64 = 0.0;
    for p in &pa {
        for q in &pb {
            best = best.max((p.x - q.x).hypot(p.y - q.y));
        }
    }
    best
}

fn closest_on(g: &Geometry<f64>, p: Point<f64>) -> Option<Point<f64>> {
    match g.closest_point(&p) {
        Closest::Intersection(q) | Closest::SinglePoint(q) => Some(q),
        Closest::Indeterminate => None,
    }
}

/// The two closest points of `a` and `b` (vertices of each against the other geometry).
fn shortest_line(a: &Geometry<f64>, b: &Geometry<f64>) -> Option<Line<f64>> {
    let mut best: Option<(f64, Line<f64>)> = None;
    let mut consider = |p: Point<f64>, q: Point<f64>, swap: bool| {
        let d = (p.x() - q.x()).hypot(p.y() - q.y());
        if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
            let l = if swap {
                Line::new(q.0, p.0)
            } else {
                Line::new(p.0, q.0)
            };
            best = Some((d, l));
        }
    };
    for c in points(a) {
        if let Some(q) = closest_on(b, Point(c)) {
            consider(Point(c), q, false);
        }
    }
    for c in points(b) {
        if let Some(q) = closest_on(a, Point(c)) {
            consider(Point(c), q, true);
        }
    }
    best.map(|(_, l)| l)
}

fn simplify_vw(g: &Geometry<f64>, tol: f64) -> Geometry<f64> {
    match g {
        Geometry::LineString(l) => Geometry::LineString(l.simplify_vw(tol)),
        Geometry::MultiLineString(l) => Geometry::MultiLineString(l.simplify_vw(tol)),
        Geometry::Polygon(p) => Geometry::Polygon(p.simplify_vw(tol)),
        Geometry::MultiPolygon(p) => Geometry::MultiPolygon(p.simplify_vw(tol)),
        other => other.clone(),
    }
}

fn densify(g: &Geometry<f64>, max: f64) -> Geometry<f64> {
    match g {
        Geometry::LineString(l) => Geometry::LineString(Euclidean.densify(l, max)),
        Geometry::MultiLineString(l) => Geometry::MultiLineString(Euclidean.densify(l, max)),
        Geometry::Polygon(p) => Geometry::Polygon(Euclidean.densify(p, max)),
        Geometry::MultiPolygon(p) => Geometry::MultiPolygon(Euclidean.densify(p, max)),
        Geometry::GeometryCollection(c) => Geometry::GeometryCollection(GeometryCollection(
            c.0.iter().map(|g| densify(g, max)).collect(),
        )),
        other => other.clone(),
    }
}

fn orient(g: &Geometry<f64>, dir: Direction) -> Geometry<f64> {
    match g {
        Geometry::Polygon(p) => Geometry::Polygon(p.orient(dir)),
        Geometry::MultiPolygon(p) => Geometry::MultiPolygon(p.orient(dir)),
        other => other.clone(),
    }
}

fn affine(g: &Geometry<f64>, f: impl Fn(Coord<f64>) -> Coord<f64> + Copy) -> Geometry<f64> {
    g.map_coords(f)
}

fn edit_line(
    args: &[Term],
    f: impl Fn(&mut Vec<Coord<f64>>, &[Term]) -> Option<()>,
) -> Option<Term> {
    let (first, rest) = args.split_first()?;
    let g = geom(first)?;
    let mut coords = line(&g.geometry)?.0.clone();
    f(&mut coords, rest)?;
    out(Geometry::LineString(LineString::new(coords)), &g.crs)
}

fn point_arg(t: &Term) -> Option<Coord<f64>> {
    match geom(t)?.geometry {
        Geometry::Point(p) => Some(p.0),
        _ => None,
    }
}

/// Registers `st:` names for GeoSPARQL functions that PostGIS has under another name.
pub(crate) fn add_aliases(table: &mut FunctionTable) {
    for (st, existing) in [
        ("ST_Boundary", format!("{GEOF}boundary")),
        ("ST_MinimumBoundingCircle", format!("{GEOF}boundingCircle")),
        ("ST_ConcaveHull", format!("{GEOF}concaveHull")),
        ("ST_IsSimple", format!("{GEOF}isSimple")),
        ("ST_IsEmpty", format!("{GEOF}isEmpty")),
        ("ST_NumGeometries", format!("{GEOF}numGeometries")),
        ("ST_GeometryN", format!("{GEOF}geometryN")),
        ("ST_Dimension", format!("{GEOF}dimension")),
        ("ST_CoordDim", format!("{GEOF}coordinateDimension")),
        ("ST_NDims", format!("{GEOF}coordinateDimension")),
        ("ST_ZMin", format!("{GEOF}minZ")),
        ("ST_ZMax", format!("{GEOF}maxZ")),
        ("ST_AsGeoJSON", format!("{GEOF}asGeoJSON")),
        ("ST_AsGML", format!("{GEOF}asGML")),
        ("ST_AsKML", format!("{GEOF}asKML")),
        ("ST_Point", format!("{ST}ST_MakePoint")),
        ("ST_NumPoints", format!("{ST}ST_NPoints")),
        ("ST_DistanceSpheroid", format!("{GEOF}distance")),
        ("ST_Length2D", format!("{ST}ST_Length")),
        ("ST_Perimeter2D", format!("{ST}ST_Perimeter")),
        ("ST_ForceRHR", format!("{ST}ST_ForcePolygonCW")),
        ("ST_SwapOrdinates", format!("{ST}ST_FlipCoordinates")),
        ("ST_GMLToSQL", format!("{ST}ST_GeomFromGML")),
    ] {
        let found = table.iter().find(|(n, _)| n.as_str() == existing);
        let f = Arc::clone(&found.unwrap_or_else(|| panic!("no function {existing}")).1);
        table.push((NamedNode::new_unchecked(format!("{ST}{st}")), f));
    }
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    // --- Accessors ---------------------------------------------------------------------
    add(
        ST,
        "ST_GeometryType",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            match g.surface.as_deref().map(|s| s.kind) {
                Some(crate::literal::SurfaceKind::PolyhedralSurface) => {
                    return string("ST_PolyhedralSurface");
                }
                Some(crate::literal::SurfaceKind::Tin) => return string("ST_Tin"),
                Some(crate::literal::SurfaceKind::Triangle) => {
                    return string("ST_Triangle");
                }
                _ => {}
            }
            let t = postgis_type(&g.geometry);
            let mut name = String::from("ST_");
            let mut upper = true;
            for c in t.chars() {
                name.push(if upper { c } else { c.to_ascii_lowercase() });
                upper = false;
            }
            // ST_MultiLineString, ST_GeometryCollection, ...
            let name = name
                .replace("Multipoint", "MultiPoint")
                .replace("Multilinestring", "MultiLineString")
                .replace("Multipolygon", "MultiPolygon")
                .replace("Linestring", "LineString")
                .replace("Geometrycollection", "GeometryCollection");
            string(name)
        }),
    );
    add(
        ST,
        "GeometryType",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            string(match g.surface.as_deref().map(|s| s.kind) {
                Some(crate::literal::SurfaceKind::PolyhedralSurface) => "POLYHEDRALSURFACE",
                Some(crate::literal::SurfaceKind::Tin) => "TIN",
                Some(crate::literal::SurfaceKind::Triangle) => "TRIANGLE",
                _ => postgis_type(&g.geometry),
            })
        }),
    );
    add(
        ST,
        "ST_SRID",
        func(|args| {
            let [a] = args else { return None };
            integer(srid(&geom(a)?.crs))
        }),
    );
    add(
        ST,
        "ST_PointN",
        func(|args| {
            let [a, n] = args else { return None };
            let g = geom(a)?;
            let l = line(&g.geometry)?;
            let n = int(n)?;
            let i = if n > 0 { n - 1 } else { l.0.len() as i64 + n };
            let c = *l.0.get(usize::try_from(i).ok()?)?;
            out(Geometry::Point(Point(c)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_ExteriorRing",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(
                Geometry::LineString(polygon(&g.geometry)?.exterior().clone()),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_NumInteriorRings",
        func(|args| {
            let [a] = args else { return None };
            integer(polygon(&geom(a)?.geometry)?.interiors().len() as i64)
        }),
    );
    add(
        ST,
        "ST_InteriorRingN",
        func(|args| {
            let [a, n] = args else { return None };
            let g = geom(a)?;
            let i = usize::try_from(int(n)? - 1).ok()?;
            out(
                Geometry::LineString(polygon(&g.geometry)?.interiors().get(i)?.clone()),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_IsClosed",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            if let Some(s) = &g.surface {
                return boolean(crate::postgis_3d::surface_closed(s));
            }
            boolean(match &g.geometry {
                Geometry::LineString(l) => l.is_closed(),
                Geometry::MultiLineString(m) => m.0.iter().all(LineString::is_closed),
                _ => true,
            })
        }),
    );
    add(
        ST,
        "ST_IsRing",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let l = line(&g.geometry)?;
            boolean(l.is_closed() && crate::geosparql11::is_simple(&g.geometry))
        }),
    );
    add(
        ST,
        "ST_IsValid",
        func(|args| {
            let [a] = args else { return None };
            boolean(geom(a)?.geometry.is_valid())
        }),
    );
    add(
        ST,
        "ST_IsValidReason",
        func(|args| {
            let [a] = args else { return None };
            let errors = geom(a)?.geometry.validation_errors();
            string(match errors.first() {
                None => "Valid Geometry".to_owned(),
                Some(e) => e.to_string(),
            })
        }),
    );
    add(
        ST,
        "ST_IsValidDetail",
        func(|args| {
            // The reason of PostGIS's (valid, reason, location) row; unbound when valid. The
            // whole row is in the `st` SERVICE. PostGIS's flags argument changes nothing here.
            let a = args.first()?;
            (args.len() <= 2).then_some(())?;
            string(crate::sets::is_valid_detail(a)?.1?)
        }),
    );
    add(
        ST,
        "ST_IsCollection",
        func(|args| {
            let [a] = args else { return None };
            boolean(matches!(
                geom(a)?.geometry,
                Geometry::MultiPoint(_)
                    | Geometry::MultiLineString(_)
                    | Geometry::MultiPolygon(_)
                    | Geometry::GeometryCollection(_)
            ))
        }),
    );
    add(
        ST,
        "ST_OrderingEquals",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            boolean(same_space(&a, &b) && a.geometry == b.geometry)
        }),
    );

    // --- Constructors ------------------------------------------------------------------
    for (name, datatype) in [
        ("ST_GeomFromGeoJSON", crate::literal::GEOJSON_LITERAL),
        ("ST_GeomFromGML", crate::literal::GML_LITERAL),
        ("ST_GeomFromKML", crate::literal::KML_LITERAL),
    ] {
        add(
            ST,
            name,
            func(move |args| {
                let [Term::Literal(l)] = args else {
                    return None;
                };
                let t: Term = Literal::new_typed_literal(l.value(), datatype).into();
                let g = geom(&t)?;
                out(g.geometry.clone(), &g.crs)
            }),
        );
    }
    add(
        ST,
        "ST_MakeLine",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            let mut coords = points(&a.geometry);
            for c in points(&b.geometry) {
                if coords.last() != Some(&c) {
                    coords.push(c);
                }
            }
            (coords.len() >= 2).then_some(())?;
            out(Geometry::LineString(LineString::new(coords)), &a.crs)
        }),
    );
    add(
        ST,
        "ST_LineFromMultiPoint",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let Geometry::MultiPoint(m) = &g.geometry else {
                return None;
            };
            out(
                Geometry::LineString(LineString::new(m.0.iter().map(|p| p.0).collect())),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_MakePolygon",
        func(|args| {
            let (shell, holes) = args.split_first()?;
            let g = geom(shell)?;
            let exterior = line(&g.geometry)?.clone();
            exterior.is_closed().then_some(())?;
            let interiors = holes
                .iter()
                .map(|h| {
                    let h = geom(h)?;
                    let l = line(&h.geometry)?.clone();
                    l.is_closed().then_some(l)
                })
                .collect::<Option<Vec<_>>>()?;
            out(Geometry::Polygon(Polygon::new(exterior, interiors)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_MakeEnvelope",
        func(|args| {
            let (xmin, ymin, xmax, ymax, crs) = match args {
                [a, b, c, d] => (num(a)?, num(b)?, num(c)?, num(d)?, Crs::Crs84),
                [a, b, c, d, s] => (num(a)?, num(b)?, num(c)?, num(d)?, crs_of_srid(int(s)?)?),
                _ => return None,
            };
            let r = Rect::new(Coord { x: xmin, y: ymin }, Coord { x: xmax, y: ymax });
            out(envelope(r.min(), r.max()), &crs)
        }),
    );
    add(
        ST,
        "ST_Collect",
        func(|args| {
            let geoms = args.iter().map(geom).collect::<Option<Vec<_>>>()?;
            let first = geoms.first()?;
            geoms.iter().all(|g| same_space(first, g)).then_some(())?;
            let parts: Vec<Geometry<f64>> = geoms.iter().map(|g| g.geometry.clone()).collect();
            out(collect(parts), &first.crs)
        }),
    );
    add(
        ST,
        "ST_Multi",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(collect(parts(&g.geometry)), &g.crs)
        }),
    );
    add(
        ST,
        "ST_CollectionExtract",
        func(|args| {
            let [a, kind] = args else { return None };
            let g = geom(a)?;
            let kind = int(kind)?;
            let picked: Vec<Geometry<f64>> = parts(&g.geometry)
                .into_iter()
                .filter(|p| match kind {
                    1 => matches!(p, Geometry::Point(_)),
                    2 => matches!(p, Geometry::LineString(_) | Geometry::Line(_)),
                    3 => matches!(p, Geometry::Polygon(_) | Geometry::Rect(_)),
                    _ => false,
                })
                .collect();
            let empty = match kind {
                1 => Geometry::MultiPoint(MultiPoint(Vec::new())),
                2 => Geometry::MultiLineString(MultiLineString(Vec::new())),
                3 => Geometry::MultiPolygon(MultiPolygon(Vec::new())),
                _ => return None,
            };
            out(
                if picked.is_empty() {
                    empty
                } else {
                    collect(picked)
                },
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_Points",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(
                Geometry::MultiPoint(MultiPoint(
                    points(&g.geometry).into_iter().map(Point).collect(),
                )),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_BoundingDiagonal",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let r = g.geometry.bounding_rect()?;
            out(
                Geometry::LineString(LineString::new(vec![r.min(), r.max()])),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_PointOnSurface",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(Geometry::Point(g.geometry.interior_point()?), &g.crs)
        }),
    );
    add(
        ST,
        "ST_LineMerge",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let lines: Vec<LineString<f64>> = parts(&g.geometry)
                .into_iter()
                .filter_map(|p| LineString::try_from(p).ok())
                .collect();
            let mut merged = line_merge(lines);
            let result = if merged.len() == 1 {
                Geometry::LineString(merged.pop()?)
            } else {
                Geometry::MultiLineString(MultiLineString(merged))
            };
            out(result, &g.crs)
        }),
    );
    add(
        ST,
        "ST_ShortestLine",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            let l = shortest_line(&a.geometry, &b.geometry)?;
            out(
                Geometry::LineString(LineString::new(vec![l.start, l.end])),
                &a.crs,
            )
        }),
    );
    add(
        ST,
        "ST_ClipByBox2D",
        func(|args| {
            let [a, b] = args else { return None };
            let (g, b) = (geom(a)?, geom(b)?);
            same_space(&g, &b).then_some(())?;
            if geo::HasDimensions::is_empty(&g.geometry) {
                return out(g.geometry.clone(), &g.crs);
            }
            let clip = b.geometry.bounding_rect()?.to_polygon();
            let result = match &g.geometry {
                Geometry::Polygon(p) => Geometry::MultiPolygon(p.intersection(&clip)),
                Geometry::MultiPolygon(p) => Geometry::MultiPolygon(p.intersection(&clip)),
                Geometry::LineString(l) => {
                    Geometry::MultiLineString(clip.clip(&MultiLineString(vec![l.clone()]), false))
                }
                Geometry::MultiLineString(l) => Geometry::MultiLineString(clip.clip(l, false)),
                _ => return None,
            };
            out(result, &g.crs)
        }),
    );

    // --- Editors -----------------------------------------------------------------------
    add(
        ST,
        "ST_AddPoint",
        func(|args| {
            edit_line(args, |coords, rest| {
                let c = point_arg(rest.first()?)?;
                match rest.get(1) {
                    None => coords.push(c),
                    Some(pos) => {
                        let pos = int(pos)?;
                        let i = if pos < 0 {
                            coords.len() as i64 + 1 + pos
                        } else {
                            pos
                        };
                        let i = usize::try_from(i).ok().filter(|&i| i <= coords.len())?;
                        coords.insert(i, c);
                    }
                }
                Some(())
            })
        }),
    );
    add(
        ST,
        "ST_RemovePoint",
        func(|args| {
            edit_line(args, |coords, rest| {
                let i = usize::try_from(int(rest.first()?)?).ok()?;
                (i < coords.len() && coords.len() > 2).then_some(())?;
                coords.remove(i);
                Some(())
            })
        }),
    );
    add(
        ST,
        "ST_SetPoint",
        func(|args| {
            edit_line(args, |coords, rest| {
                let [pos, p] = rest else { return None };
                let pos = int(pos)?;
                let i = if pos < 0 {
                    coords.len() as i64 + pos
                } else {
                    pos
                };
                let i = usize::try_from(i).ok().filter(|&i| i < coords.len())?;
                coords[i] = point_arg(p)?;
                Some(())
            })
        }),
    );
    add(
        ST,
        "ST_SetSRID",
        func(|args| {
            let [a, s] = args else { return None };
            let g = geom(a)?;
            out(g.geometry.clone(), &crs_of_srid(int(s)?)?)
        }),
    );
    add(
        ST,
        "ST_Transform",
        func(|args| {
            let [a, s] = args else { return None };
            let g = geom(a)?;
            let to = crs_of_srid(int(s)?)?;
            Some(crate::literal::geom_literal(&crate::crs::transform_geom(
                &g, &to,
            )?))
        }),
    );
    add(
        ST,
        "ST_FlipCoordinates",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(affine(&g.geometry, |c| Coord { x: c.y, y: c.x }), &g.crs)
        }),
    );
    add(
        ST,
        "ST_Force2D",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(g.geometry.clone(), &g.crs)
        }),
    );
    add(
        ST,
        "ST_ForcePolygonCCW",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(orient(&g.geometry, Direction::Default), &g.crs)
        }),
    );
    add(
        ST,
        "ST_ForcePolygonCW",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            out(orient(&g.geometry, Direction::Reversed), &g.crs)
        }),
    );
    add(
        ST,
        "ST_Segmentize",
        func(|args| {
            let [a, d] = args else { return None };
            let g = geom(a)?;
            let d = num(d)?;
            (d > 0.0).then_some(())?;
            out(densify(&g.geometry, d), &g.crs)
        }),
    );
    add(
        ST,
        "ST_SimplifyVW",
        func(|args| {
            let [a, tol] = args else { return None };
            let g = geom(a)?;
            out(simplify_vw(&g.geometry, num(tol)?), &g.crs)
        }),
    );
    add(
        ST,
        "ST_ChaikinSmoothing",
        func(|args| {
            let (a, n) = match args {
                [a] => (a, 1),
                [a, n] => (a, int(n)?),
                _ => return None,
            };
            let g = geom(a)?;
            (1..=5).contains(&n).then_some(())?;
            out(g.geometry.chaikin_smoothing(n as usize), &g.crs)
        }),
    );

    // --- Affine transforms (about the origin, as in PostGIS) ---------------------------
    add(
        ST,
        "ST_Translate",
        func(|args| {
            let [a, dx, dy] = args else { return None };
            let g = geom(a)?;
            let (dx, dy) = (num(dx)?, num(dy)?);
            out(
                affine(&g.geometry, move |c| Coord {
                    x: c.x + dx,
                    y: c.y + dy,
                }),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_Scale",
        func(|args| {
            let [a, sx, sy] = args else { return None };
            let g = geom(a)?;
            let (sx, sy) = (num(sx)?, num(sy)?);
            out(
                affine(&g.geometry, move |c| Coord {
                    x: c.x * sx,
                    y: c.y * sy,
                }),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_Rotate",
        func(|args| {
            let (a, angle, origin) = match args {
                [a, r] => (a, num(r)?, Coord { x: 0.0, y: 0.0 }),
                [a, r, o] => (a, num(r)?, point_arg(o)?),
                [a, r, x, y] => (
                    a,
                    num(r)?,
                    Coord {
                        x: num(x)?,
                        y: num(y)?,
                    },
                ),
                _ => return None,
            };
            let g = geom(a)?;
            let (sin, cos) = angle.sin_cos();
            out(
                affine(&g.geometry, move |c| {
                    let (x, y) = (c.x - origin.x, c.y - origin.y);
                    Coord {
                        x: origin.x + x * cos - y * sin,
                        y: origin.y + x * sin + y * cos,
                    }
                }),
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_Expand",
        func(|args| {
            let [a, d] = args else { return None };
            let g = geom(a)?;
            let d = num(d)?;
            let r = g.geometry.bounding_rect()?;
            let min = Coord {
                x: r.min().x - d,
                y: r.min().y - d,
            };
            let max = Coord {
                x: r.max().x + d,
                y: r.max().y + d,
            };
            out(envelope(min, max), &g.crs)
        }),
    );

    // --- Measures ----------------------------------------------------------------------
    add(
        ST,
        "ST_Perimeter",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let rings: Vec<&LineString<f64>> = match &g.geometry {
                Geometry::Polygon(p) => {
                    std::iter::once(p.exterior()).chain(p.interiors()).collect()
                }
                Geometry::MultiPolygon(m) => {
                    m.0.iter()
                        .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
                        .collect()
                }
                _ => Vec::new(),
            };
            use geo::line_measures::Length;
            double(rings.iter().map(|r| Euclidean.length(*r)).sum())
        }),
    );
    add(
        ST,
        "ST_Azimuth",
        func(|args| {
            let [a, b] = args else { return None };
            let (pa, pb) = (point_arg(a)?, point_arg(b)?);
            (pa != pb).then_some(())?;
            // Radians clockwise from north (the positive y axis).
            let az = (pb.x - pa.x).atan2(pb.y - pa.y);
            double(if az < 0.0 { az + 2.0 * PI } else { az })
        }),
    );
    add(
        ST,
        "ST_Project",
        func(|args| {
            let [a, d, az] = args else { return None };
            let g = geom(a)?;
            let Geometry::Point(p) = g.geometry else {
                return None;
            };
            let (d, az) = (num(d)?, num(az)?);
            let q = if g.crs.is_geographic() {
                // Metres along the geodesic, as PostGIS does for geography.
                Geodesic.destination(p, az.to_degrees(), d)
            } else {
                Point::new(p.x() + d * az.sin(), p.y() + d * az.cos())
            };
            out(Geometry::Point(q), &g.crs)
        }),
    );
    add(
        ST,
        "ST_HausdorffDistance",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            double(a.geometry.hausdorff_distance(&b.geometry))
        }),
    );
    add(
        ST,
        "ST_FrechetDistance",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            double(Euclidean.frechet_distance(line(&a.geometry)?, line(&b.geometry)?))
        }),
    );
    add(
        ST,
        "ST_MaxDistance",
        func(|args| {
            let [a, b] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            double(max_distance(&a.geometry, &b.geometry))
        }),
    );
    add(
        ST,
        "ST_DFullyWithin",
        func(|args| {
            let [a, b, d] = args else { return None };
            let (a, b) = (geom(a)?, geom(b)?);
            same_space(&a, &b).then_some(())?;
            boolean(max_distance(&a.geometry, &b.geometry) <= num(d)?)
        }),
    );
    add(
        ST,
        "ST_Relate",
        func(|args| {
            let (a, b) = match args {
                [a, b] => (geom(a)?, geom(b)?),
                [a, b, Term::Literal(p)] => {
                    let (a, b) = (geom(a)?, geom(b)?);
                    same_space(&a, &b).then_some(())?;
                    use geo::Relate;
                    return boolean(a.geometry.relate(&b.geometry).matches(p.value()).ok()?);
                }
                _ => return None,
            };
            same_space(&a, &b).then_some(())?;
            use geo::Relate;
            let m = a.geometry.relate(&b.geometry);
            // The DE-9IM string, row by row (Interior, Boundary, Exterior of a).
            let mut s = String::with_capacity(9);
            use geo::coordinate_position::CoordPos::{Inside, OnBoundary, Outside};
            use geo::dimensions::Dimensions;
            for ra in [Inside, OnBoundary, Outside] {
                for rb in [Inside, OnBoundary, Outside] {
                    s.push(match m.get(ra, rb) {
                        Dimensions::Empty => 'F',
                        Dimensions::ZeroDimensional => '0',
                        Dimensions::OneDimensional => '1',
                        Dimensions::TwoDimensional => '2',
                    });
                }
            }
            string(s)
        }),
    );
}
