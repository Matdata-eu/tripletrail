//! Apache Jena's spatial functions (`spatialf:`, `http://jena.apache.org/function/spatial#`),
//! so queries written for Jena/Fuseki run unchanged. Each delegates to the GeoSPARQL function
//! it corresponds to; a unit or CRS may be given as an IRI or as a string, as Jena accepts.

use crate::crs::{self, crs_arg};
use crate::functions::{Func, FunctionTable, GEOF, func, geom, num};
use crate::literal::{
    Crs, GEOJSON_LITERAL, GML_LITERAL, KML_LITERAL, WKT_LITERAL, geom_literal, wkt_literal,
};
use geo::{
    BoundingRect, Closest, ClosestPoint, Coord, CoordsIter, Distance, Euclidean, Geometry,
    Intersects, LineString, Point, Polygon, Translate,
};
use oxigraph::model::{Literal, NamedNode, Term};
use std::sync::Arc;

/// Jena's spatial function namespace.
pub const SPATIALF: &str = "http://jena.apache.org/function/spatial#";
const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";
const EARTH_RADIUS: f64 = 6_371_008.8;

/// Metres in a linear unit, by IRI or local name (`None` for degree and radian).
fn metres_per(unit: &str) -> Option<f64> {
    let name = unit
        .rsplit(['/', '#'])
        .next()
        .unwrap_or(unit)
        .to_ascii_lowercase();
    Some(match name.as_str() {
        "metre" | "meter" | "m" => 1.0,
        "kilometre" | "kilometer" | "km" => 1000.0,
        "centimetre" | "centimeter" => 0.01,
        "millimetre" | "millimeter" => 0.001,
        "mile" | "statutemile" | "mi" => 1609.344,
        "nauticalmile" => 1852.0,
        "foot" | "ft" | "surveyfootus" => 0.3048,
        "yard" | "yd" => 0.9144,
        _ => return None,
    })
}

/// Metres in one `unit` (IRI or local name) of a distance, as Jena's spatial functions take
/// them: a linear unit, or a degree or radian of arc on the earth's mean radius.
pub fn metres_in_unit(unit: &str) -> Option<f64> {
    if let Some(m) = metres_per(unit) {
        return Some(m);
    }
    Some(EARTH_RADIUS / in_unit(EARTH_RADIUS, unit)?)
}

/// The text of an IRI-valued argument given as an IRI or a literal.
fn iri_text(t: &Term) -> Option<&str> {
    match t {
        Term::NamedNode(n) => Some(n.as_str()),
        Term::Literal(l) => {
            let v = l.value().trim();
            Some(
                v.strip_prefix('<')
                    .and_then(|v| v.strip_suffix('>'))
                    .unwrap_or(v),
            )
        }
        _ => None,
    }
}

/// A distance in metres converted to `unit` (a linear unit, or degree/radian of arc).
fn in_unit(metres: f64, unit: &str) -> Option<f64> {
    if let Some(m) = metres_per(unit) {
        return Some(metres / m);
    }
    let radians = metres / EARTH_RADIUS;
    match unit
        .rsplit(['/', '#'])
        .next()?
        .to_ascii_lowercase()
        .as_str()
    {
        "radian" | "radians" => Some(radians),
        "degree" | "degrees" => Some(radians.to_degrees()),
        _ => None,
    }
}

/// Great-circle distance in metres (haversine), coordinates in degrees.
fn great_circle(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    haversine(lat1, lon1, lat2, lon2, EARTH_RADIUS)
}

fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64, radius: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * radius * a.sqrt().asin()
}

/// The earth radius of Jena's great-circle distances.
pub const JENA_EARTH_RADIUS: f64 = 6_371_008.771_4;

/// The distance in metres between two geometries as Jena's `GeometryWrapper.distance` gives
/// it when the first is geographic: the nearest points of the two in longitude and latitude
/// (as JTS's `DistanceOp`, the second shifted by 360° when they lie across the antimeridian),
/// then the haversine distance between those points. `None` when the first is not
/// geographic: then Jena's distance is planar, as `geof:distance`'s.
fn jena_great_circle(a: &Term, b: &Term) -> Option<f64> {
    let (a, b) = (geom(a)?, geom(b)?);
    if !a.crs.is_geographic() {
        return None;
    }
    let ga = crs::to_lon_lat(&a.geometry, &a.crs)?;
    let mut gb = crs::to_lon_lat(&b.geometry, &b.crs)?;
    let (ra, rb) = (ga.bounding_rect()?, gb.bounding_rect()?);
    let diff = rb.max().x - ra.min().x;
    if diff > 180.0 {
        gb = gb.translate(-360.0, 0.0);
    } else if diff < -180.0 {
        gb = gb.translate(360.0, 0.0);
    }
    let (p, q) = if ga.intersects(&gb) {
        return Some(0.0);
    } else {
        nearest_points(&ga, &gb)?
    };
    Some(haversine(p.y(), p.x(), q.y(), q.x(), JENA_EARTH_RADIUS))
}

/// The nearest points of two geometries that do not meet, in the plane: one of them is a
/// vertex of one geometry.
fn nearest_points(a: &Geometry<f64>, b: &Geometry<f64>) -> Option<(Point<f64>, Point<f64>)> {
    let mut best: Option<(f64, Point<f64>, Point<f64>)> = None;
    let mut consider = |from: &Geometry<f64>, to: &Geometry<f64>, flip: bool| {
        for c in from.coords_iter() {
            let v = Point::from(c);
            let q = match to.closest_point(&v) {
                Closest::Intersection(q) | Closest::SinglePoint(q) => q,
                Closest::Indeterminate => continue,
            };
            let d = Euclidean.distance(v, q);
            if best.is_none_or(|(b, _, _)| d < b) {
                best = Some(if flip { (d, q, v) } else { (d, v, q) });
            }
        }
    };
    consider(a, b, false);
    consider(b, a, true);
    best.map(|(_, p, q)| (p, q))
}

/// Initial bearing from the first point to the second, in radians clockwise from north.
fn azimuth(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).rem_euclid(std::f64::consts::TAU)
}

fn double(v: f64) -> Option<Term> {
    v.is_finite().then(|| Literal::from(v).into())
}

/// A point geometry as (lat, lon) in degrees.
fn lat_lon(t: &Term) -> Option<(f64, f64)> {
    let g = geom(t)?;
    let Geometry::Point(p) = crs::to_lon_lat(&g.geometry, &g.crs)? else {
        return None;
    };
    Some((p.y(), p.x()))
}

pub(crate) fn add_functions(table: &mut FunctionTable) {
    let get = |table: &FunctionTable, iri: &str| -> Func {
        Arc::clone(
            &table
                .iter()
                .find(|(n, _)| n.as_str() == iri)
                .unwrap_or_else(|| panic!("no function {iri}"))
                .1,
        )
    };
    let distance = get(table, &format!("{GEOF}distance"));
    let as_geojson = get(table, &format!("{GEOF}asGeoJSON"));
    let as_gml = get(table, &format!("{GEOF}asGML"));
    let as_kml = get(table, &format!("{GEOF}asKML"));
    let mut add = |name: &str, f: Func| {
        table.push((NamedNode::new_unchecked(format!("{SPATIALF}{name}")), f));
    };

    // A geometry serialised as a geometry literal datatype.
    let serialise = move |g: Term, datatype: &str| -> Option<Term> {
        match datatype {
            d if d == WKT_LITERAL.as_str() => Some(g),
            d if d == GEOJSON_LITERAL.as_str() => as_geojson(&[g]),
            d if d == GML_LITERAL.as_str() => as_gml(&[g]),
            d if d == KML_LITERAL.as_str() => as_kml(&[g]),
            _ => None,
        }
    };
    let serialise = Arc::new(serialise);

    let transform_srs = func(|args| {
        let [a, srs] = args else { return None };
        let g = geom(a)?;
        // Jena writes the transformed X and Y with 6 decimals (Fuseki 5.6: "POINT Z(60.430484
        // 9.476223 154.748)"); the rail KG mints coordinate IRIs from them.
        Some(geom_literal(&crs::transform_geom_rounded(
            &g,
            &crs_arg(srs)?,
            Some(6),
        )?))
    });
    add("transformSRS", Arc::clone(&transform_srs));
    {
        let serialise = Arc::clone(&serialise);
        add(
            "transform",
            func(move |args| {
                let [a, datatype, srs] = args else {
                    return None;
                };
                let g = transform_srs(&[a.clone(), srs.clone()])?;
                serialise(g, iri_text(datatype)?)
            }),
        );
    }
    add(
        "transformDatatype",
        func(move |args| {
            let [a, datatype] = args else { return None };
            let g = geom(a)?;
            serialise(geom_literal(&g), iri_text(datatype)?)
        }),
    );
    add(
        "convertLatLon",
        func(|args| {
            let [lat, lon] = args else { return None };
            let (lat, lon) = (num(lat)?, num(lon)?);
            ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some(())?;
            Some(wkt_literal(
                &Geometry::Point(Point::new(lon, lat)),
                &Crs::Epsg4326,
            ))
        }),
    );
    add(
        "convertLatLonBox",
        func(|args| {
            let [lat0, lon0, lat1, lon1] = args else {
                return None;
            };
            let (lat0, lon0, lat1, lon1) = (num(lat0)?, num(lon0)?, num(lat1)?, num(lon1)?);
            let ring = vec![
                Coord { x: lon0, y: lat0 },
                Coord { x: lon1, y: lat0 },
                Coord { x: lon1, y: lat1 },
                Coord { x: lon0, y: lat1 },
                Coord { x: lon0, y: lat0 },
            ];
            Some(wkt_literal(
                &Geometry::Polygon(Polygon::new(LineString::new(ring), Vec::new())),
                &Crs::Epsg4326,
            ))
        }),
    );
    add(
        "distance",
        func(move |args| {
            let [a, b, unit] = args else { return None };
            let unit = iri_text(unit)?;
            // Jena's own distance on geographic geometries; planar ones as geof:distance.
            let metres = match jena_great_circle(a, b) {
                Some(d) => d,
                None => {
                    let metre: Term = NamedNode::new_unchecked(format!("{UOM}metre")).into();
                    let Term::Literal(d) = distance(&[a.clone(), b.clone(), metre])? else {
                        return None;
                    };
                    d.value().parse().ok()?
                }
            };
            double(in_unit(metres, unit)?)
        }),
    );
    add(
        "greatCircle",
        func(|args| {
            let [lat1, lon1, lat2, lon2, unit] = args else {
                return None;
            };
            let d = great_circle(num(lat1)?, num(lon1)?, num(lat2)?, num(lon2)?);
            double(in_unit(d, iri_text(unit)?)?)
        }),
    );
    add(
        "greatCircleGeom",
        func(|args| {
            let [a, b, unit] = args else { return None };
            let ((lat1, lon1), (lat2, lon2)) = (lat_lon(a)?, lat_lon(b)?);
            double(in_unit(
                great_circle(lat1, lon1, lat2, lon2),
                iri_text(unit)?,
            )?)
        }),
    );
    add(
        "azimuth",
        func(|args| {
            let [lat1, lon1, lat2, lon2] = args else {
                return None;
            };
            double(azimuth(num(lat1)?, num(lon1)?, num(lat2)?, num(lon2)?))
        }),
    );
    add(
        "azimuthDeg",
        func(|args| {
            let [lat1, lon1, lat2, lon2] = args else {
                return None;
            };
            double(azimuth(num(lat1)?, num(lon1)?, num(lat2)?, num(lon2)?).to_degrees())
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::function;

    fn call(name: &str, args: &[Term]) -> Option<Term> {
        function(&format!("{SPATIALF}{name}")).unwrap()(args)
    }

    fn value(t: Option<Term>) -> String {
        match t {
            Some(Term::Literal(l)) => l.value().to_owned(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn great_circle_oslo_bergen() {
        let d: f64 = value(call(
            "greatCircle",
            &[
                Literal::from(59.91).into(),
                Literal::from(10.75).into(),
                Literal::from(60.39).into(),
                Literal::from(5.32).into(),
                Literal::from("http://www.opengis.net/def/uom/OGC/1.0/kilometre").into(),
            ],
        ))
        .parse()
        .unwrap();
        assert!((d - 305.0).abs() < 5.0, "{d}");
    }

    #[test]
    fn azimuth_due_east() {
        let a: f64 = value(call(
            "azimuthDeg",
            &[
                Literal::from(0.0).into(),
                Literal::from(0.0).into(),
                Literal::from(0.0).into(),
                Literal::from(1.0).into(),
            ],
        ))
        .parse()
        .unwrap();
        assert!((a - 90.0).abs() < 1e-9, "{a}");
    }

    /// Jena 5.2 gives 20841.567706894733 m: the haversine distance between the nearest
    /// points in longitude and latitude, not the geodesic distance to the line.
    #[test]
    fn distance_is_jenas() {
        let wkt = |w: &str| -> Term { Literal::new_typed_literal(w, WKT_LITERAL).into() };
        let metre: Term = NamedNode::new_unchecked(format!("{UOM}metre")).into();
        let d: f64 = value(call(
            "distance",
            &[
                wkt("POINT(8.5425 60.8794)"),
                wkt("LINESTRING(6 58, 10 62)"),
                metre,
            ],
        ))
        .parse()
        .unwrap();
        assert!((d - 20841.567706894733).abs() < 0.01, "{d}");
    }

    #[test]
    fn convert_lat_lon_is_epsg4326() {
        assert_eq!(
            value(call(
                "convertLatLon",
                &[Literal::from(59.5).into(), Literal::from(10.5).into()]
            )),
            "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(59.5 10.5)"
        );
    }
}
