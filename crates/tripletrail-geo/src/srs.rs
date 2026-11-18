//! PostGIS's `postgis_srs`, `postgis_srs_all` and `postgis_srs_search` over the coordinate
//! reference systems [`crate::transform`] supports (there is no PROJ database here): CRS84,
//! EPSG:4326, EPSG:4258, EPSG:3857, the UTM zones on WGS 84 (EPSG:326zz, 327zz) and ETRS89
//! (EPSG:25828-25838), EPSG:3006 and EPSG:3035, and the national CRSs of
//! [`crate::national`] (Belgian Lambert 72 and 2008, Lambert-93, RD New…).
//!
//! Each has its name, an OGC WKT 1 definition (as PostGIS's `spatial_ref_sys.srtext`), a PROJ
//! string and its area of use in longitude and latitude. Areas of use are those of the EPSG
//! dataset, rounded; for the ETRS89 UTM zones the zone's longitudes within the ETRS89 extent.

use crate::functions::{Func, ST, func};
use geo::BoundingRect;
use oxigraph::model::{Literal, Term};

/// One coordinate reference system, as a row of PostGIS's `postgis_srs`.
#[derive(Debug, Clone, PartialEq)]
pub struct Srs {
    pub auth_name: &'static str,
    /// The code (a string in PostGIS too: `CRS84` for OGC).
    pub auth_srid: String,
    pub srname: String,
    /// OGC WKT 1.
    pub srtext: String,
    pub proj4text: String,
    /// Area of use: west, south, east, north, in degrees.
    pub area: [f64; 4],
    /// Projected (as opposed to geographic).
    pub projected: bool,
}

impl Srs {
    /// The area of use's south-west and north-east corners as CRS84 points.
    pub fn point_sw(&self) -> Term {
        point(self.area[0], self.area[1])
    }

    pub fn point_ne(&self) -> Term {
        point(self.area[2], self.area[3])
    }

    /// The CRS IRI (`http://www.opengis.net/def/crs/EPSG/0/{code}`).
    pub fn iri(&self) -> String {
        if self.auth_name == "OGC" {
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84".to_owned()
        } else {
            format!("http://www.opengis.net/def/crs/EPSG/0/{}", self.auth_srid)
        }
    }

    fn degrees_squared(&self) -> f64 {
        (self.area[2] - self.area[0]) * (self.area[3] - self.area[1])
    }
}

fn point(x: f64, y: f64) -> Term {
    Literal::new_typed_literal(format!("POINT({x} {y})"), crate::literal::WKT_LITERAL).into()
}

const WGS84_DATUM: &str = r#"DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]],AUTHORITY["EPSG","6326"]]"#;
const ETRS89_DATUM: &str = r#"DATUM["European_Terrestrial_Reference_System_1989",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6258"]]"#;
const SWEREF_DATUM: &str = r#"DATUM["SWEREF99",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6619"]]"#;
const PRIMEM_DEGREE: &str = r#"PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]"#;
const METRE: &str = r#"UNIT["metre",1,AUTHORITY["EPSG","9001"]]"#;
const EN: &str = r#"AXIS["Easting",EAST],AXIS["Northing",NORTH]"#;
const NE: &str = r#"AXIS["Northing",NORTH],AXIS["Easting",EAST]"#;
const GRS80_TOWGS84: &str = "+ellps=GRS80 +towgs84=0,0,0,0,0,0,0";
/// The ETRS89 extent (EPSG area 1298).
const EUROPE: [f64; 4] = [-16.1, 32.88, 40.18, 84.73];

fn geogcs(name: &str, datum: &str, code: u32, axes: &str) -> String {
    format!(r#"GEOGCS["{name}",{datum},{PRIMEM_DEGREE},{axes}AUTHORITY["EPSG","{code}"]]"#)
}

fn wgs84() -> String {
    geogcs("WGS 84", WGS84_DATUM, 4326, "")
}

fn etrs89() -> String {
    geogcs("ETRS89", ETRS89_DATUM, 4258, "")
}

fn projcs(
    name: &str,
    base: &str,
    projection: &str,
    params: &[(&str, f64)],
    axes: &str,
    code: u32,
) -> String {
    let params: String = params
        .iter()
        .map(|(n, v)| format!(r#"PARAMETER["{n}",{v}],"#))
        .collect();
    format!(
        r#"PROJCS["{name}",{base},PROJECTION["{projection}"],{params}{METRE},{axes},AUTHORITY["EPSG","{code}"]]"#
    )
}

fn tm(lon0: f64, k0: f64, false_northing: f64) -> [(&'static str, f64); 5] {
    [
        ("latitude_of_origin", 0.0),
        ("central_meridian", lon0),
        ("scale_factor", k0),
        ("false_easting", 500_000.0),
        ("false_northing", false_northing),
    ]
}

/// The CRS with this EPSG code (or `CRS84`), when it is one of those supported.
fn by_code(auth: &str, code: &str) -> Option<Srs> {
    let auth = auth.to_ascii_uppercase();
    if auth == "OGC" || code.eq_ignore_ascii_case("CRS84") {
        if !code.eq_ignore_ascii_case("CRS84") {
            return None;
        }
        return Some(Srs {
            auth_name: "OGC",
            auth_srid: "CRS84".into(),
            srname: "WGS 84 (CRS84)".into(),
            srtext: geogcs(
                "WGS 84 (CRS84)",
                WGS84_DATUM,
                4326,
                r#"AXIS["Longitude",EAST],AXIS["Latitude",NORTH],"#,
            )
            .replace(
                r#"AUTHORITY["EPSG","4326"]]"#,
                r#"AUTHORITY["OGC","CRS84"]]"#,
            ),
            proj4text: "+proj=longlat +datum=WGS84 +no_defs".into(),
            area: [-180.0, -90.0, 180.0, 90.0],
            projected: false,
        });
    }
    if auth != "EPSG" {
        return None;
    }
    let code: u32 = code.trim().parse().ok()?;
    let srs = |srname: String, srtext: String, proj4text: String, area: [f64; 4], projected| Srs {
        auth_name: "EPSG",
        auth_srid: code.to_string(),
        srname,
        srtext,
        proj4text,
        area,
        projected,
    };
    let utm_area = |zone: u32, south: bool| {
        let west = zone as f64 * 6.0 - 186.0;
        if south {
            [west, -80.0, west + 6.0, 0.0]
        } else {
            [west, 0.0, west + 6.0, 84.0]
        }
    };
    Some(match code {
        4326 => srs(
            "WGS 84".into(),
            wgs84(),
            "+proj=longlat +datum=WGS84 +no_defs".into(),
            [-180.0, -90.0, 180.0, 90.0],
            false,
        ),
        4258 => srs(
            "ETRS89".into(),
            etrs89(),
            format!("+proj=longlat {GRS80_TOWGS84} +no_defs"),
            EUROPE,
            false,
        ),
        3857 => srs(
            "WGS 84 / Pseudo-Mercator".into(),
            format!(
                r#"PROJCS["WGS 84 / Pseudo-Mercator",{},PROJECTION["Mercator_1SP"],PARAMETER["central_meridian",0],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],{METRE},{EN},EXTENSION["PROJ4","+proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0=0 +x_0=0 +y_0=0 +k=1 +units=m +nadgrids=@null +wktext +no_defs"],AUTHORITY["EPSG","3857"]]"#,
                wgs84()
            ),
            "+proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0=0 +x_0=0 +y_0=0 +k=1 +units=m +nadgrids=@null +wktext +no_defs".into(),
            [-180.0, -85.06, 180.0, 85.06],
            true,
        ),
        32601..=32660 | 32701..=32760 => {
            let south = code > 32700;
            let zone = code - if south { 32700 } else { 32600 };
            let name = format!("WGS 84 / UTM zone {zone}{}", if south { 'S' } else { 'N' });
            let lon0 = zone as f64 * 6.0 - 183.0;
            srs(
                name.clone(),
                projcs(
                    &name,
                    &wgs84(),
                    "Transverse_Mercator",
                    &tm(lon0, 0.9996, if south { 10_000_000.0 } else { 0.0 }),
                    EN,
                    code,
                ),
                format!(
                    "+proj=utm +zone={zone}{} +datum=WGS84 +units=m +no_defs",
                    if south { " +south" } else { "" }
                ),
                utm_area(zone, south),
                true,
            )
        }
        25828..=25838 => {
            let zone = code - 25800;
            let name = format!("ETRS89 / UTM zone {zone}N");
            let lon0 = zone as f64 * 6.0 - 183.0;
            let a = utm_area(zone, false);
            srs(
                name.clone(),
                projcs(
                    &name,
                    &etrs89(),
                    "Transverse_Mercator",
                    &tm(lon0, 0.9996, 0.0),
                    EN,
                    code,
                ),
                format!("+proj=utm +zone={zone} {GRS80_TOWGS84} +units=m +no_defs"),
                [a[0].max(EUROPE[0]), EUROPE[1], a[2].min(EUROPE[2]), EUROPE[3]],
                true,
            )
        }
        3006 => srs(
            "SWEREF99 TM".into(),
            projcs(
                "SWEREF99 TM",
                &geogcs("SWEREF99", SWEREF_DATUM, 4619, ""),
                "Transverse_Mercator",
                &tm(15.0, 0.9996, 0.0),
                NE,
                3006,
            ),
            format!("+proj=utm +zone=33 {GRS80_TOWGS84} +units=m +no_defs"),
            [10.03, 54.96, 24.17, 69.07],
            true,
        ),
        3035 => srs(
            "ETRS89 / LAEA Europe".into(),
            projcs(
                "ETRS89 / LAEA Europe",
                &etrs89(),
                "Lambert_Azimuthal_Equal_Area",
                &[
                    ("latitude_of_center", 52.0),
                    ("longitude_of_center", 10.0),
                    ("false_easting", 4_321_000.0),
                    ("false_northing", 3_210_000.0),
                ],
                NE,
                3035,
            ),
            format!(
                "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 {GRS80_TOWGS84} +units=m +no_defs"
            ),
            [-35.58, 24.6, 44.83, 84.73],
            true,
        ),
        _ => {
            let Some(n) = crate::national::NATIONAL.iter().find(|n| n.code == code) else {
                // Another projected CRS of the EPSG registry, if the transforms read it.
                if !crate::crs::registry_supported(code) {
                    return None;
                }
                let (_, proj4, wkt) = crate::crs::registry_def(code)?;
                let name = wkt.split('"').nth(1).unwrap_or_default();
                return Some(Srs {
                    auth_name: "EPSG",
                    auth_srid: code.to_string(),
                    srname: name.into(),
                    srtext: wkt.into(),
                    proj4text: proj4.into_owned(),
                    // The registry gives no area of use.
                    area: [-180.0, -90.0, 180.0, 90.0],
                    projected: true,
                });
            };
            return Some(Srs {
                auth_name: "EPSG",
                auth_srid: code.to_string(),
                srname: n.name.into(),
                srtext: n.wkt.into(),
                proj4text: format!("{} +no_defs", n.proj4),
                area: n.area,
                projected: true,
            });
        }
    })
}

/// PostGIS's `postgis_srs(auth_name, auth_srid)`.
pub fn srs(auth_name: &str, auth_srid: &str) -> Option<Srs> {
    by_code(auth_name, auth_srid)
}

/// The CRS a code names: `EPSG:25832`, `25832`, `CRS84`, `OGC:CRS84`, a CRS IRI
/// (`http://www.opengis.net/def/crs/EPSG/0/25832`) or URN (`urn:ogc:def:crs:EPSG::25832`).
pub fn lookup(code: &str) -> Option<Srs> {
    let code = code.trim().trim_start_matches('<').trim_end_matches('>');
    if let Some(rest) = code
        .strip_prefix("http://www.opengis.net/def/crs/")
        .or_else(|| code.strip_prefix("https://www.opengis.net/def/crs/"))
    {
        // {authority}/{version}/{code}
        let mut parts = rest.split('/');
        let (auth, _, c) = (parts.next()?, parts.next()?, parts.next()?);
        return by_code(auth, c);
    }
    if let Some(rest) = code.strip_prefix("urn:ogc:def:crs:") {
        // {authority}:{version}:{code}, the version possibly empty.
        let mut parts = rest.split(':');
        let auth = parts.next()?;
        let c = parts.next_back()?;
        return by_code(auth, c);
    }
    match code.split_once(':') {
        Some((auth, c)) => by_code(auth, c),
        None => by_code("EPSG", code),
    }
}

/// PostGIS's `postgis_srs_all()`: every supported CRS, by authority and code.
pub fn all() -> Vec<Srs> {
    let mut codes: Vec<u32> = vec![3006, 3035, 3857, 4258, 4326];
    codes.extend(25828..=25838);
    codes.extend(32601..=32660);
    codes.extend(32701..=32760);
    codes.extend(crate::national::NATIONAL.iter().map(|n| n.code));
    let mut out: Vec<Srs> = codes
        .into_iter()
        .filter_map(|c| by_code("EPSG", &c.to_string()))
        .collect();
    out.extend(by_code("OGC", "CRS84"));
    out
}

/// PostGIS's `postgis_srs_search(bounds)`: the projected CRSs whose area of use contains
/// `bounds` (a geometry in a CRS [`crate::transform`] supports), smallest area of use first.
/// `None` when the bounds cannot be read or brought to longitude and latitude.
pub fn search(bounds: &Term, auth_name: Option<&str>) -> Option<Vec<Srs>> {
    let g = crate::parse_wkt_literal(bounds)?;
    let ll = crate::crs::to_lon_lat(&g.geometry, &g.crs)?;
    let r = ll.bounding_rect()?;
    let (min, max) = (r.min(), r.max());
    let mut found: Vec<Srs> = all()
        .into_iter()
        .filter(|s| s.projected)
        .filter(|s| auth_name.is_none_or(|a| a.eq_ignore_ascii_case(s.auth_name)))
        .filter(|s| {
            s.area[0] <= min.x && s.area[1] <= min.y && max.x <= s.area[2] && max.y <= s.area[3]
        })
        .collect();
    found.sort_by(|a, b| {
        a.degrees_squared()
            .total_cmp(&b.degrees_squared())
            .then_with(|| a.auth_srid.cmp(&b.auth_srid))
    });
    Some(found)
}

/// A code argument: an integer, a string or a CRS IRI.
fn code_arg(t: &Term) -> Option<Srs> {
    match t {
        Term::NamedNode(n) => lookup(n.as_str()),
        Term::Literal(l) => lookup(l.value()),
        _ => None,
    }
}

/// `st:srs(code [, field])`: the CRS's OGC WKT (`srtext`), or the field named
/// (`srname`, `srtext`, `proj4text`, `auth_name`, `auth_srid`, `point_sw`, `point_ne`).
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    add(
        ST,
        "srs",
        func(|args| {
            let (code, field) = match args {
                [c] => (c, "srtext"),
                [c, Term::Literal(f)] => (c, f.value()),
                _ => return None,
            };
            let s = code_arg(code)?;
            let text = |v: &str| Some(Literal::new_simple_literal(v).into());
            match field {
                "srtext" | "wkt" => text(&s.srtext),
                "proj4text" | "proj4" | "proj" => text(&s.proj4text),
                "srname" | "name" => text(&s.srname),
                "auth_name" => text(s.auth_name),
                "auth_srid" => text(&s.auth_srid),
                "point_sw" => Some(s.point_sw()),
                "point_ne" => Some(s.point_ne()),
                _ => None,
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        let s = lookup("EPSG:25832").unwrap();
        assert_eq!(s.srname, "ETRS89 / UTM zone 32N");
        assert_eq!(s.area, [6.0, 32.88, 12.0, 84.73]);
        assert!(
            s.srtext
                .starts_with(r#"PROJCS["ETRS89 / UTM zone 32N",GEOGCS["ETRS89""#)
        );
        assert!(s.srtext.contains(r#"PARAMETER["central_meridian",9]"#));
        assert!(s.srtext.ends_with(r#"AUTHORITY["EPSG","25832"]]"#));
        for code in [
            "25832",
            "http://www.opengis.net/def/crs/EPSG/0/25832",
            "urn:ogc:def:crs:EPSG::25832",
            "epsg:25832",
        ] {
            assert_eq!(lookup(code).as_ref(), Some(&s), "{code}");
        }
        assert_eq!(
            lookup("EPSG:32733").unwrap().proj4text,
            "+proj=utm +zone=33 +south +datum=WGS84 +units=m +no_defs"
        );
        assert_eq!(
            lookup("http://www.opengis.net/def/crs/OGC/1.3/CRS84")
                .unwrap()
                .auth_name,
            "OGC"
        );
        assert!(lookup("EPSG:4269").is_none());
        // A projected CRS of the EPSG registry, outside the national table.
        let nztm = lookup("EPSG:2193").unwrap();
        assert_eq!(
            nztm.srname,
            "NZGD2000 / New Zealand Transverse Mercator 2000"
        );
        assert!(crate::crs::is_supported(&crate::crs::crs_from_iri(
            "http://www.opengis.net/def/crs/EPSG/0/2193"
        )));
        let l72 = lookup("EPSG:31370").unwrap();
        assert_eq!(l72.srname, "BD72 / Belgian Lambert 72");
        assert!(l72.proj4text.contains("+towgs84=-106.8686"));
        // Everything listed is supported by transform.
        for s in all() {
            assert!(
                crate::crs::is_supported(&crate::crs::crs_from_iri(&s.iri())),
                "{}",
                s.srname
            );
        }
    }

    #[test]
    fn search_by_bounds() {
        let b: Term = Literal::new_typed_literal(
            "POLYGON((7 50,8 50,8 51,7 51,7 50))",
            crate::literal::WKT_LITERAL,
        )
        .into();
        let codes: Vec<String> = search(&b, None)
            .unwrap()
            .into_iter()
            .map(|s| s.auth_srid)
            .collect();
        // Areas of use are boxes, as in PostGIS: France's reaches 50-51°N at 7-8°E.
        assert_eq!(
            codes,
            [
                "3950", "4647", "27572", "2154", "25832", "32632", "3034", "3035", "3857"
            ]
        );
        // Across two UTM zones: only the continental ones.
        let wide: Term =
            Literal::new_typed_literal("LINESTRING(5 50,13 51)", crate::literal::WKT_LITERAL)
                .into();
        let codes: Vec<String> = search(&wide, None)
            .unwrap()
            .into_iter()
            .map(|s| s.auth_srid)
            .collect();
        assert_eq!(codes, ["3034", "3035", "3857"]);
    }
}
