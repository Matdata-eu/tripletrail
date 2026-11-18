//! Coordinate transformations between the CRSs common in European railway data, without PROJ.
//!
//! Supported: CRS84 / EPSG:4326 / EPSG:4258 (geographic), EPSG:3857 (Web Mercator), UTM on
//! WGS 84 (EPSG:326zz, 327zz) and ETRS89 (EPSG:258zz), SWEREF99 TM (EPSG:3006) and
//! ETRS89-LAEA Europe (EPSG:3035), computed here; and the national CRSs of
//! [`crate::national`] (Belgian Lambert 72 and 2008, Lambert-93, RD New, Gauss-Krüger,
//! LV95, British National Grid…), through `proj4rs` with their datum shifts; and any other
//! projected CRS of the EPSG registry (`crs-definitions`), with the datum shift its proj4
//! definition gives (none for some: then off by up to a few hundred metres). ETRS89 and
//! WGS 84 are treated as identical (they differ by well under a metre in Europe, below what
//! these datasets resolve). Everything converts through longitude/latitude in degrees.

use crate::literal::{Crs, Geom, Zm};
use geo::{Coord, Geometry, MapCoords};
use oxigraph::model::Term;
use std::sync::Arc;

const A: f64 = 6_378_137.0;
const F: f64 = 1.0 / 298.257_223_563;
const EPSG: &str = "http://www.opengis.net/def/crs/EPSG/0/";

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Projection {
    Geographic,
    WebMercator,
    TransverseMercator {
        lon0: f64,
        k0: f64,
        false_easting: f64,
        false_northing: f64,
    },
    LambertAzimuthalEqualArea {
        lat0: f64,
        lon0: f64,
        false_easting: f64,
        false_northing: f64,
    },
    /// The CRS at this position in [`crate::national::NATIONAL`].
    National(usize),
    /// Another projected CRS of the EPSG registry (`crs-definitions`), parsed once.
    Registry(Registry),
}

/// A projected CRS of the EPSG registry: its code and parsed definition.
#[derive(Clone, Copy)]
pub(crate) struct Registry {
    code: u16,
    proj: &'static proj4rs::Proj,
}

impl PartialEq for Registry {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EPSG:{}", self.code)
    }
}

/// NAD27 as a 3-parameter shift to WGS 84 (EPSG:1173, the mean for the contiguous US, good to
/// about 10 m there): `proj4rs` has no NAD27 datum, which PROJ shifts with grid files.
const NAD27: &str = "+ellps=clrk66 +towgs84=-8,160,176,0,0,0,0";

/// Registry codes left out because `proj4rs` disagrees with PROJ 9.8 by a kilometre or more
/// at the centre of their area of use, or does not convert them back (oblique Mercator with
/// `+gamma`, some 7-parameter shifts with large scale terms); found by comparing every
/// projected code with pyproj (#755).
const REGISTRY_SKIP: &[u16] = &[3167, 3303];

/// The definition of a projected CRS of the EPSG registry (`crs-definitions`) as `proj4rs`
/// reads it: code, PROJ.4 text and OGC WKT 1. Left out: longitude/latitude CRSs (whose axis
/// order the definition doesn't give), geocentric ones, those needing grid files, those
/// whose axes point south or west (Krovak's south-west form, say: PROJ.4 text gives east and
/// north) and [`REGISTRY_SKIP`]. NAD27 gets the shift of [`NAD27`].
pub(crate) fn registry_def(
    code: u32,
) -> Option<(u16, std::borrow::Cow<'static, str>, &'static str)> {
    let code = u16::try_from(code).ok()?;
    if REGISTRY_SKIP.contains(&code) {
        return None;
    }
    let def = crs_definitions::from_code(code)?;
    let p = def.proj4;
    if [
        "+proj=longlat",
        "+proj=latlong",
        "+proj=geocent",
        "+nadgrids",
        "+geoidgrids",
        "+axis=",
    ]
    .iter()
    .any(|x| p.contains(x))
    {
        return None;
    }
    // The axes of the WKT: east and north (or none named, or north first, which the PROJ.4
    // text still gives as east, north).
    let mut axes = def.wkt.split("AXIS[").skip(1).map(|a| {
        a.split(']')
            .next()
            .unwrap_or_default()
            .rsplit(',')
            .next()
            .unwrap_or_default()
    });
    if axes.any(|a| !matches!(a, "EAST" | "NORTH" | "UP")) {
        return None;
    }
    if def.wkt.matches("AXIS[").count() == 1 {
        return None;
    }
    let proj4 = if p.contains("+datum=NAD27") {
        p.replace("+datum=NAD27", NAD27).into()
    } else {
        p.into()
    };
    Some((code, proj4, def.wkt))
}

/// Whether the transforms convert EPSG `code` through the registry.
pub(crate) fn registry_supported(code: u32) -> bool {
    registry(code).is_some()
}

/// The parsed definition of EPSG `code` from the registry, parsed once per code.
fn registry(code: u32) -> Option<Registry> {
    static PARSED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u16, Option<&'static proj4rs::Proj>>>,
    > = std::sync::OnceLock::new();
    let (code, proj4, _) = registry_def(code)?;
    let mut parsed = PARSED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let proj = *parsed.entry(code).or_insert_with(|| {
        // Leaked: at most one per code of the registry, kept for the process.
        proj4rs::Proj::from_proj_string(&proj4)
            .ok()
            .map(|p| &*Box::leak(Box::new(p)))
    });
    Some(Registry { code, proj: proj? })
}

/// The parsed definitions of [`crate::national::NATIONAL`] (`None` for one `proj4rs` cannot
/// read), and WGS 84 longitude/latitude.
fn national() -> &'static (Vec<Option<proj4rs::Proj>>, proj4rs::Proj) {
    static PROJS: std::sync::OnceLock<(Vec<Option<proj4rs::Proj>>, proj4rs::Proj)> =
        std::sync::OnceLock::new();
    PROJS.get_or_init(|| {
        (
            crate::national::NATIONAL
                .iter()
                .map(|n| proj4rs::Proj::from_proj_string(n.proj4).ok())
                .collect(),
            proj4rs::Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").expect("WGS 84"),
        )
    })
}

/// `c` converted by `proj4rs` from `src` to `dst` (radians for longitude/latitude); NaN if
/// it fails.
fn proj4(src: &proj4rs::Proj, dst: &proj4rs::Proj, c: (f64, f64)) -> (f64, f64) {
    let mut p = (c.0, c.1, 0.0);
    match proj4rs::transform::transform(src, dst, &mut p) {
        Ok(()) => (p.0, p.1),
        Err(_) => (f64::NAN, f64::NAN),
    }
}

pub(crate) fn projection(crs: &Crs) -> Option<Projection> {
    let iri = match crs {
        Crs::Crs84 | Crs::Epsg4326 => return Some(Projection::Geographic),
        Crs::Other(iri) => iri,
    };
    let code: u32 = iri.strip_prefix(EPSG)?.parse().ok()?;
    let utm = |zone: u32, south: bool| Projection::TransverseMercator {
        lon0: zone as f64 * 6.0 - 183.0,
        k0: 0.9996,
        false_easting: 500_000.0,
        false_northing: if south { 10_000_000.0 } else { 0.0 },
    };
    Some(match code {
        3857 | 900_913 => Projection::WebMercator,
        32601..=32660 => utm(code - 32600, false),
        32701..=32760 => utm(code - 32700, true),
        25828..=25838 => utm(code - 25800, false),
        3006 => Projection::TransverseMercator {
            lon0: 15.0,
            k0: 0.9996,
            false_easting: 500_000.0,
            false_northing: 0.0,
        },
        3035 => Projection::LambertAzimuthalEqualArea {
            lat0: 52.0,
            lon0: 10.0,
            false_easting: 4_321_000.0,
            false_northing: 3_210_000.0,
        },
        _ => match crate::national::NATIONAL
            .iter()
            .position(|n| n.code == code)
        {
            Some(at) => {
                national().0[at].as_ref()?;
                Projection::National(at)
            }
            None => Projection::Registry(registry(code)?),
        },
    })
}

/// Whether [`transform`] can convert from and to this CRS.
pub fn is_supported(crs: &Crs) -> bool {
    projection(crs).is_some()
}

/// The CRS a CRS IRI denotes.
pub fn crs_from_iri(iri: &str) -> Crs {
    crate::literal::crs_from_iri(iri)
}

/// A CRS argument: an IRI, or (as Jena accepts) an `xsd:anyURI` or string literal holding
/// one, possibly in angle brackets.
pub fn crs_arg(t: &Term) -> Option<Crs> {
    let iri = match t {
        Term::NamedNode(n) => n.as_str(),
        Term::Literal(l) => {
            let v = l.value().trim();
            v.strip_prefix('<')
                .and_then(|v| v.strip_suffix('>'))
                .unwrap_or(v)
        }
        _ => return None,
    };
    oxiri_like(iri).then(|| crs_from_iri(iri))
}

/// A cheap check that a string is an absolute IRI (a scheme, then a colon).
fn oxiri_like(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && !s.contains(char::is_whitespace)
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Converts a geometry between two supported CRSs. Geographic coordinates are (lon, lat).
pub fn transform(g: &Geometry<f64>, from: &Crs, to: &Crs) -> Option<Geometry<f64>> {
    let (p, q) = (projection(from)?, projection(to)?);
    if p == q {
        return Some(g.clone());
    }
    let ok = std::cell::Cell::new(true);
    let out = g.map_coords(|c| {
        let r = forward(q, inverse(p, c));
        ok.set(ok.get() && r.x.is_finite() && r.y.is_finite());
        r
    });
    ok.get().then_some(out)
}

/// [`transform`] for a parsed geometry, keeping its Z and M ordinates.
pub fn transform_geom(g: &Geom, to: &Crs) -> Option<Geom> {
    transform_geom_rounded(g, to, None)
}

/// [`transform_geom`], with the transformed X and Y rounded to `decimals` places (Z and M are
/// kept), as Jena's `spatialf:transformSRS` writes them (6).
pub fn transform_geom_rounded(g: &Geom, to: &Crs, decimals: Option<i32>) -> Option<Geom> {
    use geo::{CoordsIter, MapCoords};
    let round = |v: f64| match decimals {
        Some(d) => {
            let f = 10f64.powi(d);
            let r = (v * f).round() / f;
            // No "-0".
            if r == 0.0 { 0.0 } else { r }
        }
        None => v,
    };
    if let Some(s) = &g.surface {
        // Transform the (x, y) of every vertex; Z is kept as it is.
        let points: Vec<geo::Point<f64>> = s
            .patches
            .iter()
            .flatten()
            .flatten()
            .map(|p| geo::Point::new(p[0], p[1]))
            .collect();
        let Geometry::MultiPoint(moved) =
            transform(&Geometry::MultiPoint(geo::MultiPoint(points)), &g.crs, to)?
        else {
            return None;
        };
        let mut moved = moved.0.into_iter();
        let s = s.map(|p| match moved.next() {
            Some(q) => [round(q.x()), round(q.y()), p[2]],
            None => p,
        });
        return Some(Geom::from_surface(s, to.clone()));
    }
    let mut geometry = transform(&g.geometry, &g.crs, to)?;
    if decimals.is_some() {
        geometry = geometry.map_coords(|c| Coord {
            x: round(c.x),
            y: round(c.y),
        });
    }
    let zm = g.zm.as_ref().map(|zm| {
        let values = g
            .geometry
            .coords_iter()
            .map(|c| zm.at(c).unwrap_or((f64::NAN, f64::NAN)));
        Arc::new(Zm::from_coords(&geometry, zm.has_z, zm.has_m, values))
    });
    Some(Geom {
        geometry,
        crs: to.clone(),
        zm,
        surface: None,
        prepared: Default::default(),
    })
}

/// To geographic CRS84, if the CRS is supported.
pub fn to_lon_lat(g: &Geometry<f64>, from: &Crs) -> Option<Geometry<f64>> {
    transform(g, from, &Crs::Crs84)
}

pub(crate) fn forward(p: Projection, c: Coord<f64>) -> Coord<f64> {
    match p {
        Projection::Geographic => c,
        Projection::WebMercator => Coord {
            x: A * c.x.to_radians(),
            y: A * (std::f64::consts::FRAC_PI_4 + c.y.to_radians() / 2.0)
                .tan()
                .ln(),
        },
        Projection::TransverseMercator {
            lon0,
            k0,
            false_easting,
            false_northing,
        } => {
            let (x, y) = tm::forward(c.x - lon0, c.y);
            Coord {
                x: false_easting + k0 * x,
                y: false_northing + k0 * y,
            }
        }
        Projection::LambertAzimuthalEqualArea {
            lat0,
            lon0,
            false_easting,
            false_northing,
        } => {
            let (x, y) = laea::forward(lat0, c.x - lon0, c.y);
            Coord {
                x: false_easting + x,
                y: false_northing + y,
            }
        }
        Projection::National(at) => {
            let (projs, wgs84) = national();
            let Some(p) = &projs[at] else {
                return Coord {
                    x: f64::NAN,
                    y: f64::NAN,
                };
            };
            let (x, y) = proj4(wgs84, p, (c.x.to_radians(), c.y.to_radians()));
            Coord { x, y }
        }
        Projection::Registry(r) => {
            let (x, y) = proj4(&national().1, r.proj, (c.x.to_radians(), c.y.to_radians()));
            Coord { x, y }
        }
    }
}

pub(crate) fn inverse(p: Projection, c: Coord<f64>) -> Coord<f64> {
    match p {
        Projection::Geographic => c,
        Projection::WebMercator => Coord {
            x: (c.x / A).to_degrees(),
            y: (2.0 * (c.y / A).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees(),
        },
        Projection::TransverseMercator {
            lon0,
            k0,
            false_easting,
            false_northing,
        } => {
            let (dlon, lat) = tm::inverse((c.x - false_easting) / k0, (c.y - false_northing) / k0);
            Coord {
                x: dlon + lon0,
                y: lat,
            }
        }
        Projection::LambertAzimuthalEqualArea {
            lat0,
            lon0,
            false_easting,
            false_northing,
        } => {
            let (dlon, lat) = laea::inverse(lat0, c.x - false_easting, c.y - false_northing);
            Coord {
                x: dlon + lon0,
                y: lat,
            }
        }
        Projection::National(at) => {
            let (projs, wgs84) = national();
            let Some(p) = &projs[at] else {
                return Coord {
                    x: f64::NAN,
                    y: f64::NAN,
                };
            };
            let (lon, lat) = proj4(p, wgs84, (c.x, c.y));
            Coord {
                x: normal_longitude(lon.to_degrees()),
                y: lat.to_degrees(),
            }
        }
        Projection::Registry(r) => {
            let (lon, lat) = proj4(r.proj, &national().1, (c.x, c.y));
            Coord {
                x: normal_longitude(lon.to_degrees()),
                y: lat.to_degrees(),
            }
        }
    }
}

/// A longitude from -180 (included) to 180 degrees: `proj4rs` gives one up to 360 degrees off
/// for a CRS centred on the antimeridian.
fn normal_longitude(lon: f64) -> f64 {
    if (-180.0..180.0).contains(&lon) || !lon.is_finite() {
        lon
    } else {
        (lon + 180.0).rem_euclid(360.0) - 180.0
    }
}

/// Transverse Mercator on WGS 84, Krüger series to third order in n (sub-millimetre within
/// a UTM zone).
mod tm {
    use super::{A, F};

    struct Series {
        a: f64,
        alpha: [f64; 3],
        beta: [f64; 3],
        delta: [f64; 3],
        sqrt_n_term: f64,
    }

    fn series() -> &'static Series {
        static S: std::sync::LazyLock<Series> = std::sync::LazyLock::new(|| {
            let n = F / (2.0 - F);
            let (n2, n3) = (n * n, n * n * n);
            Series {
                a: A / (1.0 + n) * (1.0 + n2 / 4.0 + n2 * n2 / 64.0),
                alpha: [
                    n / 2.0 - 2.0 * n2 / 3.0 + 5.0 * n3 / 16.0,
                    13.0 * n2 / 48.0 - 3.0 * n3 / 5.0,
                    61.0 * n3 / 240.0,
                ],
                beta: [
                    n / 2.0 - 2.0 * n2 / 3.0 + 37.0 * n3 / 96.0,
                    n2 / 48.0 + n3 / 15.0,
                    17.0 * n3 / 480.0,
                ],
                delta: [
                    2.0 * n - 2.0 * n2 / 3.0 - 2.0 * n3,
                    7.0 * n2 / 3.0 - 8.0 * n3 / 5.0,
                    56.0 * n3 / 15.0,
                ],
                sqrt_n_term: 2.0 * n.sqrt() / (1.0 + n),
            }
        });
        &S
    }

    /// (longitude from the central meridian, latitude) in degrees -> (x, y) in metres, k0 = 1.
    pub(super) fn forward(dlon: f64, lat: f64) -> (f64, f64) {
        let s = series();
        let (phi, lambda) = (lat.to_radians(), dlon.to_radians());
        let sin_phi = phi.sin();
        let t = (sin_phi.atanh() - s.sqrt_n_term * (s.sqrt_n_term * sin_phi).atanh()).sinh();
        let xi = (t / lambda.cos()).atan();
        let eta = (lambda.sin() / (1.0 + t * t).sqrt()).atanh();
        let (mut x, mut y) = (eta, xi);
        for (j, a) in s.alpha.iter().enumerate() {
            let k = 2.0 * (j + 1) as f64;
            x += a * (k * xi).cos() * (k * eta).sinh();
            y += a * (k * xi).sin() * (k * eta).cosh();
        }
        (s.a * x, s.a * y)
    }

    /// (x, y) in metres, k0 = 1 -> (longitude from the central meridian, latitude) in degrees.
    pub(super) fn inverse(x: f64, y: f64) -> (f64, f64) {
        let s = series();
        let (xi, eta) = (y / s.a, x / s.a);
        let (mut xi1, mut eta1) = (xi, eta);
        for (j, b) in s.beta.iter().enumerate() {
            let k = 2.0 * (j + 1) as f64;
            xi1 -= b * (k * xi).sin() * (k * eta).cosh();
            eta1 -= b * (k * xi).cos() * (k * eta).sinh();
        }
        let chi = (xi1.sin() / eta1.cosh()).asin();
        let mut phi = chi;
        for (j, d) in s.delta.iter().enumerate() {
            phi += d * (2.0 * (j + 1) as f64 * chi).sin();
        }
        let lambda = (eta1.sinh() / xi1.cos()).atan();
        (lambda.to_degrees(), phi.to_degrees())
    }
}

/// Lambert azimuthal equal-area on the GRS 80 ellipsoid (Snyder, oblique aspect).
mod laea {
    use super::{A, F};

    fn e2() -> f64 {
        F * (2.0 - F)
    }

    fn q(sin_phi: f64) -> f64 {
        let e2 = e2();
        let e = e2.sqrt();
        (1.0 - e2)
            * (sin_phi / (1.0 - e2 * sin_phi * sin_phi)
                - (1.0 / (2.0 * e)) * ((1.0 - e * sin_phi) / (1.0 + e * sin_phi)).ln())
    }

    struct Constants {
        rq: f64,
        d: f64,
        sin_b0: f64,
        cos_b0: f64,
        qp: f64,
    }

    fn constants(lat0: f64) -> Constants {
        let e2 = e2();
        let qp = q(1.0);
        let phi0 = lat0.to_radians();
        let b0 = (q(phi0.sin()) / qp).asin();
        let rq = A * (qp / 2.0).sqrt();
        let d = A * (phi0.cos() / (1.0 - e2 * phi0.sin().powi(2)).sqrt()) / (rq * b0.cos());
        Constants {
            rq,
            d,
            sin_b0: b0.sin(),
            cos_b0: b0.cos(),
            qp,
        }
    }

    pub(super) fn forward(lat0: f64, dlon: f64, lat: f64) -> (f64, f64) {
        let c = constants(lat0);
        let beta = (q(lat.to_radians().sin()) / c.qp).asin();
        let (sb, cb) = beta.sin_cos();
        let (sl, cl) = dlon.to_radians().sin_cos();
        let b = c.rq * (2.0 / (1.0 + c.sin_b0 * sb + c.cos_b0 * cb * cl)).sqrt();
        (
            b * c.d * cb * sl,
            (b / c.d) * (c.cos_b0 * sb - c.sin_b0 * cb * cl),
        )
    }

    pub(super) fn inverse(lat0: f64, x: f64, y: f64) -> (f64, f64) {
        let c = constants(lat0);
        let rho = ((x / c.d).powi(2) + (c.d * y).powi(2)).sqrt();
        if rho == 0.0 {
            return (0.0, lat0);
        }
        let ce = 2.0 * (rho / (2.0 * c.rq)).asin();
        let (sc, cc) = ce.sin_cos();
        let beta = (cc * c.sin_b0 + c.d * y * sc * c.cos_b0 / rho).asin();
        let lambda = (x * sc).atan2(c.d * rho * c.cos_b0 * cc - c.d * c.d * y * c.sin_b0 * sc);
        let e2 = e2();
        let (e4, e6) = (e2 * e2, e2 * e2 * e2);
        let phi = beta
            + (e2 / 3.0 + 31.0 * e4 / 180.0 + 517.0 * e6 / 5040.0) * (2.0 * beta).sin()
            + (23.0 * e4 / 360.0 + 251.0 * e6 / 3780.0) * (4.0 * beta).sin()
            + (761.0 * e6 / 45360.0) * (6.0 * beta).sin();
        (lambda.to_degrees(), phi.to_degrees())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::point;

    fn epsg(code: u32) -> Crs {
        Crs::Other(format!("{EPSG}{code}"))
    }

    fn close(a: Coord<f64>, b: Coord<f64>, tol: f64) -> bool {
        (a.x - b.x).abs() <= tol && (a.y - b.y).abs() <= tol
    }

    fn pt(g: Geometry<f64>) -> Coord<f64> {
        let Geometry::Point(p) = g else { panic!() };
        p.0
    }

    #[test]
    fn laea_matches_epsg_guidance_example() {
        // IOGP Guidance Note 7-2, EPSG:3035 example: 50°N 5°E.
        let g = Geometry::Point(point!(x: 5.0, y: 50.0));
        let p = pt(transform(&g, &Crs::Crs84, &epsg(3035)).unwrap());
        assert!(
            close(
                p,
                Coord {
                    x: 3_962_799.45,
                    y: 2_999_718.85
                },
                0.05
            ),
            "{p:?}"
        );
    }

    #[test]
    fn utm_on_the_central_meridian_and_equator() {
        let g = Geometry::Point(point!(x: 15.0, y: 0.0));
        let p = pt(transform(&g, &Crs::Crs84, &epsg(32633)).unwrap());
        assert!(
            close(
                p,
                Coord {
                    x: 500_000.0,
                    y: 0.0
                },
                1e-6
            ),
            "{p:?}"
        );
    }

    #[test]
    fn matches_proj() {
        // Reference values from PROJ 9 (pyproj, EPSG:4326 -> target, always_xy).
        let cases = [
            (25832, (10.752245, 59.910840), (597991.4372, 6642778.3565)),
            (32633, (18.95, 69.65), (653210.0895, 7731796.8304)),
            (3006, (18.06, 59.33), (674079.2566, 6580798.3872)),
            (3857, (10.75, 59.91), (1196684.5260, 8379727.5819)),
            (3035, (5.0, 50.0), (3962799.4510, 2999718.8532)),
            (32735, (27.75, -29.955), (572369.4217, 6685964.4803)),
        ];
        for (code, (lon, lat), (x, y)) in cases {
            let g = Geometry::Point(point!(x: lon, y: lat));
            let p = pt(transform(&g, &Crs::Crs84, &epsg(code)).unwrap());
            assert!(close(p, Coord { x, y }, 0.01), "{code}: {p:?}");
        }
    }

    #[test]
    fn round_trips() {
        for code in [3857, 32632, 32633, 32735, 25833, 3006, 3035] {
            for (lon, lat) in [(10.75, 59.91), (5.32, 60.39), (18.95, 69.65), (2.35, 48.85)] {
                let crs = epsg(code);
                let (lon, lat) = if code == 32735 {
                    (lon + 17.0, -lat / 2.0)
                } else {
                    (lon, lat)
                };
                let g = Geometry::Point(point!(x: lon, y: lat));
                let there = transform(&g, &Crs::Crs84, &crs).unwrap();
                let back = pt(transform(&there, &crs, &Crs::Epsg4326).unwrap());
                assert!(
                    close(back, Coord { x: lon, y: lat }, 1e-8),
                    "{code}: {back:?}"
                );
            }
        }
    }

    /// `(EPSG code, lon, lat, x, y)` computed by PROJ 9.8 (pyproj) from the same definitions.
    const REFERENCE: &[(u32, f64, f64, f64, f64)] = &[
        (
            31370,
            4.45,
            50.504999999999995,
            155763.8827316225,
            132693.3826531889,
        ),
        (
            3812,
            4.45,
            50.504999999999995,
            655767.9695613892,
            632694.7821491045,
        ),
        (
            2154,
            0.2600000000000007,
            46.355000000000004,
            489353.58775158133,
            6587552.204045072,
        ),
        (
            27572,
            1.6800000000000002,
            46.735,
            549826.6306522982,
            2192990.7121902495,
        ),
        (3942, 4.285, 42.19, 1806133.464641465, 1221898.9166579738),
        (3943, 2.93, 43.17, 1694308.3555044443, 2218886.832639388),
        (3944, 2.96, 43.96, 1696789.8710242882, 3195556.6834046254),
        (3945, 3.125, 45.0, 1709855.0089266587, 4200007.601723443),
        (3946, 2.475, 46.0, 1659335.5353398635, 5200134.0201074025),
        (
            3947,
            1.4300000000000002,
            47.0,
            1580610.273100607,
            6201196.378340442,
        ),
        (
            3948,
            1.6800000000000002,
            48.0,
            1601507.7594183618,
            7200843.179309284,
        ),
        (
            3949,
            1.6800000000000002,
            49.0,
            1603426.3502429728,
            8200839.6210153755,
        ),
        (
            3950,
            3.0250000000000004,
            50.07,
            1701789.6383209554,
            9207785.717349734,
        ),
        (28992, 5.21, 52.225, 142892.19257513565, 470783.86722838413),
        (
            2169,
            6.130000000000001,
            49.815,
            77255.99396064326,
            97842.85114717588,
        ),
        (
            31466,
            6.6850000000000005,
            51.46,
            2547649.711944682,
            5703045.701209401,
        ),
        (
            31467,
            9.004999999999999,
            51.18000000000001,
            3500423.0305487677,
            5671670.909857799,
        ),
        (
            31468,
            12.004999999999999,
            50.99,
            4500456.149219688,
            5650531.594951074,
        ),
        (31469, 13.67, 48.745, 5402325.267489697, 5401677.636285636),
        (
            4647,
            9.004999999999999,
            51.370000000000005,
            32500348.05019652,
            5690971.606736347,
        ),
        (
            5650,
            13.305,
            51.245000000000005,
            33381693.719118673,
            5678435.226906829,
        ),
        (2056, 8.225, 46.81, 2660019.1118579195, 1184616.1678398626),
        (21781, 8.225, 46.81, 660019.1118579194, 184616.16783986264),
        (
            31287,
            13.350000000000001,
            47.71,
            401306.8256351997,
            423398.18346883514,
        ),
        (
            31254,
            10.684999999999999,
            47.19,
            26674.954644998856,
            227959.91257501766,
        ),
        (31255, 13.335, 47.595, 181.4285105365985, 272918.6186108487),
        (
            31256,
            16.0,
            47.790000000000006,
            -24888.57232670043,
            294643.41142402776,
        ),
        (
            31257,
            10.684999999999999,
            47.19,
            176674.95464499886,
            227959.91257501766,
        ),
        (31258, 13.335, 47.595, 450181.4285105366, 272918.6186108487),
        (
            31259,
            16.0,
            47.790000000000006,
            725111.4276732996,
            294643.41142402776,
        ),
        (
            3416,
            13.350000000000001,
            47.71,
            401250.2908077099,
            423340.5570879366,
        ),
        (
            27700,
            -3.5,
            55.379999999999995,
            305055.40550976736,
            610582.4481918588,
        ),
        (2157, -7.95, 53.41, 603324.3765559805, 739986.4340746693),
        (29903, -7.95, 53.41, 203374.16321622807, 239960.74007143913),
        (3007, 12.02, 58.435, 151168.28067387955, 6479733.786659077),
        (
            3008,
            13.454999999999998,
            58.78,
            147397.13250546544,
            6518163.451093328,
        ),
        (
            3009,
            14.844999999999999,
            58.785,
            141035.86069809148,
            6518729.903527869,
        ),
        (3010, 16.52, 59.205, 151142.66582960103, 6565505.620848638),
        (3011, 18.345, 59.68, 169436.55093583668, 6618472.273037763),
        (3012, 13.74, 62.97, 124130.54365610614, 6985143.2851961935),
        (3013, 15.335, 62.785, 128816.37905029746, 6964488.905308066),
        (3014, 16.675, 64.655, 122525.00974195829, 7172999.661263187),
        (3015, 18.7, 61.515, 147338.53632143553, 6822882.759007674),
        (
            3016,
            19.68,
            66.25999999999999,
            124383.34672026377,
            7351948.247889037,
        ),
        (3017, 21.27, 65.72, 127967.2137075571, 7291701.798851329),
        (3018, 23.01, 66.815, 139451.7546358816, 7413742.860812734),
        (3067, 25.335, 64.465, 419924.39978573413, 7149882.0006198855),
        (2180, 19.145, 52.465, 509847.90053546167, 511024.242654006),
        (2176, 15.32, 52.805, 5521578.889627086, 5852517.238459874),
        (2177, 18.0, 52.66, 6500000.0, 5836334.701093706),
        (2178, 21.0, 51.82, 7500000.0, 5742873.498941285),
        (2179, 23.325, 51.705, 8453342.081782231, 5730294.963535688),
        (
            5514,
            17.325,
            49.394999999999996,
            -544006.6689109321,
            -1144002.81178548,
        ),
        (3763, -7.875, 39.555, 22183.2869197783, -12542.933773017621),
        (3003, 8.965, 41.785, 1497117.608433237, 4625918.3801927455),
        (3004, 15.495, 40.93, 2561682.5927382475, 4531111.834827556),
        (23700, 19.505, 47.16, 684692.7847029, 201865.84220322352),
        (
            3794,
            14.995000000000001,
            46.150000000000006,
            499613.7700829212,
            112247.58014518023,
        ),
        (3765, 16.43, 44.44, 494427.7816159777, 4922223.758357072),
        (
            3844,
            25.835,
            45.855000000000004,
            564954.2243085932,
            484257.9440907544,
        ),
        (
            3301,
            24.285,
            58.760000000000005,
            516493.16075571533,
            6513417.968162374,
        ),
        (3059, 23.65, 56.88, 478669.909704032, 304082.77423819713),
        (3346, 22.92, 55.17, 431194.03974992706, 6115464.679789118),
        (
            2100,
            23.935000000000002,
            38.315,
            494168.08116862935,
            4240480.130834421,
        ),
        (
            3034,
            4.625,
            54.665000000000006,
            3664622.8977630353,
            3098904.2726719077,
        ),
    ];

    /// Every national CRS converts as PROJ does (to a millimetre), and back.
    #[test]
    fn national_crss_match_proj() {
        assert_eq!(REFERENCE.len(), crate::national::NATIONAL.len());
        for &(code, lon, lat, x, y) in REFERENCE {
            let crs = epsg(code);
            assert!(is_supported(&crs), "{code} not readable");
            let g = Geometry::Point(point!(x: lon, y: lat));
            let there = transform(&g, &Crs::Crs84, &crs).unwrap();
            let p = pt(there.clone());
            assert!(close(p, Coord { x, y }, 0.001), "{code}: {p:?} vs {x} {y}");
            let back = pt(transform(&there, &crs, &Crs::Crs84).unwrap());
            // Iterated inverses (Swiss oblique Mercator) are good to a centimetre.
            assert!(
                close(back, Coord { x: lon, y: lat }, 1e-7),
                "{code}: back {back:?}"
            );
        }
    }

    /// Belgian Lambert 72 and 2008 of the same place, and between each other: the BD72
    /// shift (EPSG:15929) moves it by about 100 m.
    #[test]
    fn belgian_lambert() {
        // Brussels, Grand-Place.
        let g = Geometry::Point(point!(x: 4.352367, y: 50.846717));
        let l72 = transform(&g, &Crs::Crs84, &epsg(31370)).unwrap();
        let l08 = transform(&g, &Crs::Crs84, &epsg(3812)).unwrap();
        // PROJ 9.8 with the EPSG registry's best transformations.
        let p = pt(l72.clone());
        let expected = Coord {
            x: 148_846.056,
            y: 170_701.569,
        };
        assert!(close(p, expected, 0.01), "{p:?}");
        let q = pt(l08.clone());
        let expected = Coord {
            x: 648_845.669,
            y: 670_701.837,
        };
        assert!(close(q, expected, 0.01), "{q:?}");
        let via = pt(transform(&l72, &epsg(31370), &epsg(3812)).unwrap());
        assert!(close(via, q, 0.001), "{via:?}");
    }

    /// Projected CRSs of the EPSG registry outside the national table, against PROJ 9.8
    /// (pyproj, `always_xy`): New Zealand TM, New York Long Island in US feet, and polar
    /// stereographic; each back to longitude/latitude.
    #[test]
    fn registry_crss_match_proj() {
        for (code, lon, lat, x, y) in [
            (2193, 174.7762, -41.2865, 1_748_735.553, 5_427_916.479),
            (2263, -73.9857, 40.7484, 988_212.237, 211_939.279),
            (3413, -45.0, 75.0, 0.0, -1_633_879.497),
        ] {
            let g = Geometry::Point(point!(x: lon, y: lat));
            let there = transform(&g, &Crs::Crs84, &epsg(code)).unwrap();
            let p = pt(there.clone());
            assert!(close(p, Coord { x, y }, 0.01), "{code}: {p:?}");
            let back = pt(transform(&there, &epsg(code), &Crs::Crs84).unwrap());
            assert!(
                close(back, Coord { x: lon, y: lat }, 1e-7),
                "{code}: {back:?}"
            );
        }
    }

    /// NAD27 through its mean 3-parameter shift (good to about 10 m in the contiguous US);
    /// a CRS on the antimeridian back to a longitude from -180 to 180; codes whose axes point
    /// south and west, or that `proj4rs` gets wrong, left out (#755).
    #[test]
    fn registry_edge_cases() {
        let g = Geometry::Point(point!(x: -75.0, y: 40.0));
        let p = pt(transform(&g, &Crs::Crs84, &epsg(26718)).unwrap());
        assert!(
            close(
                p,
                Coord {
                    x: 499_966.330,
                    y: 4_427_545.846
                },
                15.0
            ),
            "{p:?}"
        );
        for lon in [179.5, -179.5] {
            let g = Geometry::Point(point!(x: lon, y: 60.0));
            let there = transform(&g, &Crs::Crs84, &epsg(2636)).unwrap();
            let back = pt(transform(&there, &epsg(2636), &Crs::Crs84).unwrap());
            assert!(close(back, Coord { x: lon, y: 60.0 }, 1e-7), "{back:?}");
        }
        let g = Geometry::Point(point!(x: 179.5, y: 60.0));
        let p = pt(transform(&g, &Crs::Crs84, &epsg(2636)).unwrap());
        assert!(
            close(
                p,
                Coord {
                    x: 471_946.853,
                    y: 6_654_300.955
                },
                1.0
            ),
            "{p:?}"
        );
        for code in [5513, 2065, 8044, 8045, 3167, 3303] {
            assert!(!is_supported(&epsg(code)), "{code}");
        }
        // EPSG:5514, Krovak East North, stays.
        assert!(is_supported(&epsg(5514)));
    }

    /// The SRS catalogue knows a registry code exactly when the transforms convert it.
    #[test]
    fn catalogue_and_transforms_agree() {
        let wrong: Vec<u32> = (1024..=32767u32)
            .filter(|&code| {
                let crs = crs_from_iri(&format!("{EPSG}{code}"));
                crate::srs::lookup(&format!("EPSG:{code}")).is_some() != is_supported(&crs)
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn unsupported_crs_is_none() {
        let g = Geometry::Point(point!(x: 1.0, y: 2.0));
        assert!(transform(&g, &Crs::Crs84, &epsg(9_999_999)).is_none());
        // Longitude/latitude of another datum: its axis order is not in the registry's
        // definition.
        assert!(transform(&g, &Crs::Crs84, &epsg(4269)).is_none());
    }
}
