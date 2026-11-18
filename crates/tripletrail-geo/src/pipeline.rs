//! `ST_TransformPipeline` and `ST_InverseTransformPipeline`: coordinate operations given as
//! PROJ strings, run with the projections of [`crate::crs`] (there is no PROJ here).
//!
//! Supported:
//! - `+proj=pipeline` with `+step`s (and global parameters before the first step), or a single
//!   operation such as `+proj=utm +zone=31`, with `+inv`, `+omit_fwd` and `+omit_inv`;
//! - the steps `unitconvert` (`xy_in`/`xy_out` in `deg`, `rad`, `grad`, `m`, `km`, `dm`, `cm`,
//!   `mm`, `ft`, `us-ft`), `axisswap` (`order=2,1`, with signs), `noop`, `longlat`/`latlong`,
//!   `utm` (`zone`, `south`), `tmerc`/`etmerc` (`lon_0`, `k`/`k_0`, `x_0`, `y_0`; `lat_0=0`),
//!   `webmerc`, spherical `merc` (`+a=6378137 +b=6378137` or `+R=6378137`: Web Mercator) and
//!   `laea` (`lat_0`, `lon_0`, `x_0`, `y_0`), on the WGS 84 or GRS 80 ellipsoid;
//! - EPSG conversions by URN, `urn:ogc:def:coordinateOperation:EPSG::code`: UTM zones
//!   (16001–16060 north, 16101–16160 south), Popular Visualisation Pseudo-Mercator (3856),
//!   Europe Equal Area 2001 (19986, the LAEA of EPSG:3035) and SWEREF99 TM (17333), taking
//!   longitude and latitude in degrees, as PostGIS does.
//!
//! Not supported, for want of PROJ's database and grids: datum transformations (`helmert`,
//! `cart`, `molodensky`, grid shifts, EPSG transformations such as 1618), concatenated
//! operation URNs, other ellipsoids and the projections not listed.

use crate::crs::{Projection, forward, inverse};
use crate::literal::{Crs, Geom, Zm};
use geo::{Coord, CoordsIter, MapCoords};
use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    /// Multiply x and y by the factor.
    Scale(f64),
    /// Output axis i is input axis `order[i]` (0 or 1), negated when the flag is set.
    Swap([(usize, bool); 2]),
    Noop,
    /// A projection: radians in, metres out.
    Project(Projection),
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Step {
    op: Op,
    inv: bool,
    omit_fwd: bool,
    omit_inv: bool,
}

fn apply(op: Op, inv: bool, c: Coord<f64>) -> Coord<f64> {
    match (op, inv) {
        (Op::Scale(f), false) => Coord {
            x: c.x * f,
            y: c.y * f,
        },
        (Op::Scale(f), true) => Coord {
            x: c.x / f,
            y: c.y / f,
        },
        (Op::Swap(order), false) => {
            let v = [c.x, c.y];
            let pick = |(i, neg): (usize, bool)| if neg { -v[i] } else { v[i] };
            Coord {
                x: pick(order[0]),
                y: pick(order[1]),
            }
        }
        (Op::Swap(order), true) => {
            let mut v = [0.0; 2];
            for (out, (i, neg)) in [c.x, c.y].into_iter().zip(order) {
                v[i] = if neg { -out } else { out };
            }
            Coord { x: v[0], y: v[1] }
        }
        (Op::Noop, _) => c,
        (Op::Project(p), false) => forward(
            p,
            Coord {
                x: c.x.to_degrees(),
                y: c.y.to_degrees(),
            },
        ),
        (Op::Project(p), true) => {
            let d = inverse(p, c);
            Coord {
                x: d.x.to_radians(),
                y: d.y.to_radians(),
            }
        }
    }
}

fn angular(unit: &str) -> Option<f64> {
    Some(match unit {
        "rad" => 1.0,
        "deg" => PI / 180.0,
        "grad" => PI / 200.0,
        _ => return None,
    })
}

fn linear(unit: &str) -> Option<f64> {
    Some(match unit {
        "m" => 1.0,
        "km" => 1000.0,
        "dm" => 0.1,
        "cm" => 0.01,
        "mm" => 0.001,
        "ft" => 0.3048,
        "us-ft" => 1200.0 / 3937.0,
        _ => return None,
    })
}

const KNOWN: [&str; 32] = [
    "proj", "inv", "zone", "south", "lat_0", "lon_0", "k", "k_0", "x_0", "y_0", "ellps", "datum",
    "towgs84", "units", "no_defs", "type", "a", "b", "rf", "R", "lat_ts", "omit_fwd", "omit_inv",
    "approx", "xy_in", "xy_out", "z_in", "z_out", "order", "wktext", "over", "no_uoff",
];

fn step(params: &HashMap<&str, &str>) -> Option<Step> {
    params.keys().all(|k| KNOWN.contains(k)).then_some(())?;
    let num = |k: &str, default: f64| -> Option<f64> {
        match params.get(k) {
            None => Some(default),
            Some(v) => v.parse().ok(),
        }
    };
    // The ellipsoid must be WGS 84 or GRS 80 (they differ by well under a millimetre here).
    let ellipsoid_ok = || -> bool {
        let ellps = params
            .get("ellps")
            .is_none_or(|e| matches!(*e, "WGS84" | "GRS80"));
        let datum = params
            .get("datum")
            .is_none_or(|d| matches!(*d, "WGS84" | "ETRS89" | "NAD83"));
        let towgs = params
            .get("towgs84")
            .is_none_or(|t| t.split(',').all(|v| v.parse::<f64>() == Ok(0.0)));
        let a = num("a", 6_378_137.0) == Some(6_378_137.0);
        let rf = num("rf", 298.257_223_563).is_some_and(|r| (r - 298.257_223_563).abs() < 2e-6);
        let units = params.get("units").is_none_or(|u| *u == "m");
        ellps
            && datum
            && towgs
            && a
            && rf
            && units
            && !params.contains_key("b")
            && !params.contains_key("R")
    };
    let op = match *params.get("proj")? {
        "unitconvert" => {
            if params.get("z_in") != params.get("z_out") {
                return None;
            }
            match (params.get("xy_in"), params.get("xy_out")) {
                (None, None) => Op::Noop,
                (Some(i), Some(o)) => match (angular(i), angular(o), linear(i), linear(o)) {
                    (Some(a), Some(b), _, _) | (_, _, Some(a), Some(b)) => Op::Scale(a / b),
                    _ => return None,
                },
                _ => return None,
            }
        }
        "axisswap" => {
            let order: Vec<i32> = params
                .get("order")?
                .split(',')
                .map(|v| v.trim().parse().ok())
                .collect::<Option<_>>()?;
            // Axes 3 and 4 (Z and time) must stay where they are.
            for (i, &o) in order.iter().enumerate().skip(2) {
                (o == i as i32 + 1).then_some(())?;
            }
            let axis = |o: i32| -> Option<(usize, bool)> {
                matches!(o.abs(), 1 | 2).then_some(((o.abs() - 1) as usize, o < 0))
            };
            let (a, b) = (axis(*order.first()?)?, axis(*order.get(1)?)?);
            (a.0 != b.0).then_some(())?;
            Op::Swap([a, b])
        }
        "noop" | "longlat" | "latlong" | "lonlat" | "latlon" => Op::Noop,
        "utm" => {
            ellipsoid_ok().then_some(())?;
            let zone: u32 = params.get("zone")?.parse().ok()?;
            (1..=60).contains(&zone).then_some(())?;
            Op::Project(Projection::TransverseMercator {
                lon0: f64::from(zone) * 6.0 - 183.0,
                k0: 0.9996,
                false_easting: 500_000.0,
                false_northing: if params.contains_key("south") {
                    10_000_000.0
                } else {
                    0.0
                },
            })
        }
        "tmerc" | "etmerc" => {
            ellipsoid_ok().then_some(())?;
            (num("lat_0", 0.0)? == 0.0).then_some(())?;
            Op::Project(Projection::TransverseMercator {
                lon0: num("lon_0", 0.0)?,
                k0: match params.get("k_0") {
                    Some(_) => num("k_0", 1.0)?,
                    None => num("k", 1.0)?,
                },
                false_easting: num("x_0", 0.0)?,
                false_northing: num("y_0", 0.0)?,
            })
        }
        "webmerc" => {
            ellipsoid_ok().then_some(())?;
            Op::Project(Projection::WebMercator)
        }
        "merc" => {
            let sphere = num("R", 0.0)? == 6_378_137.0
                || num("a", 0.0)? == 6_378_137.0 && num("b", 0.0)? == 6_378_137.0;
            let plain = num("lat_ts", 0.0)? == 0.0
                && num("lon_0", 0.0)? == 0.0
                && num("x_0", 0.0)? == 0.0
                && num("y_0", 0.0)? == 0.0
                && num("k", 1.0)? == 1.0
                && params.get("units").is_none_or(|u| *u == "m");
            (sphere && plain).then_some(())?;
            Op::Project(Projection::WebMercator)
        }
        "laea" => {
            ellipsoid_ok().then_some(())?;
            Op::Project(Projection::LambertAzimuthalEqualArea {
                lat0: num("lat_0", 0.0)?,
                lon0: num("lon_0", 0.0)?,
                false_easting: num("x_0", 0.0)?,
                false_northing: num("y_0", 0.0)?,
            })
        }
        _ => return None,
    };
    Some(Step {
        op,
        inv: params.contains_key("inv"),
        omit_fwd: params.contains_key("omit_fwd"),
        omit_inv: params.contains_key("omit_inv"),
    })
}

fn tokens(s: &str) -> Vec<(&str, &str)> {
    s.split_whitespace()
        .map(|t| {
            let t = t.strip_prefix('+').unwrap_or(t);
            t.split_once('=').unwrap_or((t, ""))
        })
        .collect()
}

/// The operation of an EPSG conversion code, from longitude and latitude in degrees.
fn epsg_conversion(code: u32) -> Option<Vec<Step>> {
    let utm = |zone: u32, south: bool| Projection::TransverseMercator {
        lon0: f64::from(zone) * 6.0 - 183.0,
        k0: 0.9996,
        false_easting: 500_000.0,
        false_northing: if south { 10_000_000.0 } else { 0.0 },
    };
    let p = match code {
        16001..=16060 => utm(code - 16000, false),
        16101..=16160 => utm(code - 16100, true),
        3856 => Projection::WebMercator,
        19986 => Projection::LambertAzimuthalEqualArea {
            lat0: 52.0,
            lon0: 10.0,
            false_easting: 4_321_000.0,
            false_northing: 3_210_000.0,
        },
        17333 => Projection::TransverseMercator {
            lon0: 15.0,
            k0: 0.9996,
            false_easting: 500_000.0,
            false_northing: 0.0,
        },
        _ => return None,
    };
    let s = |op| Step {
        op,
        inv: false,
        omit_fwd: false,
        omit_inv: false,
    };
    Some(vec![s(Op::Scale(PI / 180.0)), s(Op::Project(p))])
}

/// The steps of a pipeline string.
fn parse(pipeline: &str) -> Option<Vec<Step>> {
    let p = pipeline.trim();
    if let Some(rest) = p
        .strip_prefix("urn:ogc:def:coordinateOperation:EPSG::")
        .or_else(|| p.strip_prefix("EPSG:"))
    {
        return epsg_conversion(rest.parse().ok()?);
    }
    let toks = tokens(p);
    let is_pipeline = toks.contains(&("proj", "pipeline"));
    if !is_pipeline {
        let params: HashMap<&str, &str> = toks.into_iter().collect();
        return Some(vec![step(&params)?]);
    }
    // Global parameters (before the first step) apply to every step.
    let mut groups: Vec<Vec<(&str, &str)>> = vec![Vec::new()];
    for t in toks {
        match t.0 {
            "step" => groups.push(Vec::new()),
            "proj" if t.1 == "pipeline" => {}
            _ => groups.last_mut()?.push(t),
        }
    }
    let globals = groups.remove(0);
    let mut inv_all = false;
    let globals: Vec<(&str, &str)> = globals
        .into_iter()
        .filter(|t| {
            if t.0 == "inv" {
                inv_all = true;
                false
            } else {
                true
            }
        })
        .collect();
    let mut steps: Vec<Step> = groups
        .iter()
        .map(|g| {
            let mut params: HashMap<&str, &str> = g.iter().copied().collect();
            for &(k, v) in &globals {
                params.entry(k).or_insert(v);
            }
            step(&params)
        })
        .collect::<Option<_>>()?;
    (!steps.is_empty()).then_some(())?;
    if inv_all {
        steps = invert(&steps);
    }
    Some(steps)
}

fn invert(steps: &[Step]) -> Vec<Step> {
    steps
        .iter()
        .rev()
        .map(|s| Step {
            inv: !s.inv,
            omit_fwd: s.omit_inv,
            omit_inv: s.omit_fwd,
            ..*s
        })
        .collect()
}

/// What the coordinates hold after a step, to name the output CRS when none is given.
#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    LonLatDeg,
    LatLonDeg,
    LonLatRad,
    LatLonRad,
    Projected(Projection),
    Unknown,
}

fn next_state(s: State, step: &Step) -> State {
    match (step.op, step.inv, s) {
        (Op::Noop, _, s) => s,
        (Op::Project(p), false, State::LonLatRad) => State::Projected(p),
        (Op::Project(p), true, State::Projected(q)) if p == q => State::LonLatRad,
        (Op::Project(_), true, State::Unknown) => State::LonLatRad,
        (Op::Project(_), _, _) => State::Unknown,
        (Op::Scale(f), inv, s) => {
            let to_rad = if inv { 1.0 / f } else { f };
            match s {
                State::LonLatDeg if (to_rad - PI / 180.0).abs() < 1e-15 => State::LonLatRad,
                State::LatLonDeg if (to_rad - PI / 180.0).abs() < 1e-15 => State::LatLonRad,
                State::LonLatRad if (to_rad - 180.0 / PI).abs() < 1e-12 => State::LonLatDeg,
                State::LatLonRad if (to_rad - 180.0 / PI).abs() < 1e-12 => State::LatLonDeg,
                State::Unknown if (to_rad - PI / 180.0).abs() < 1e-15 => State::LonLatRad,
                _ => State::Unknown,
            }
        }
        (Op::Swap([(1, false), (0, false)]), _, s) => match s {
            State::LonLatDeg => State::LatLonDeg,
            State::LatLonDeg => State::LonLatDeg,
            State::LonLatRad => State::LatLonRad,
            State::LatLonRad => State::LonLatRad,
            _ => State::Unknown,
        },
        (Op::Swap(_), _, _) => State::Unknown,
    }
}

/// The EPSG CRS of a projection, where there is one.
fn crs_of(p: Projection) -> Option<Crs> {
    let code = match p {
        Projection::WebMercator => 3857,
        Projection::TransverseMercator {
            lon0,
            k0,
            false_easting,
            false_northing,
        } if k0 == 0.9996 && false_easting == 500_000.0 => {
            let zone = (lon0 + 183.0) / 6.0;
            if zone.fract() != 0.0 || !(1.0..=60.0).contains(&zone) {
                return None;
            }
            match false_northing {
                0.0 => 32600 + zone as i64,
                10_000_000.0 => 32700 + zone as i64,
                _ => return None,
            }
        }
        Projection::LambertAzimuthalEqualArea {
            lat0: 52.0,
            lon0: 10.0,
            false_easting: 4_321_000.0,
            false_northing: 3_210_000.0,
        } => 3035,
        _ => return None,
    };
    crate::postgis::crs_of_srid(code)
}

/// An unknown CRS (PostGIS's SRID 0): planar operations only.
const UNKNOWN_CRS: &str = "http://www.opengis.net/def/crs/OGC/0/unknown";

/// Runs a pipeline (inverted with `inverse`) on a geometry, in its stored (x, y) order, and
/// labels the result with `to_srid`, or with the CRS the pipeline ends in when it is
/// recognisable (a UTM zone, Web Mercator, EPSG:3035, longitude and latitude in degrees).
pub(crate) fn transform_pipeline(
    g: &Geom,
    pipeline: &str,
    inverse: bool,
    to_srid: Option<i64>,
) -> Option<Geom> {
    let mut steps = parse(pipeline)?;
    if inverse {
        steps = invert(&steps);
    }
    let start = if pipeline.trim_start().starts_with("urn:") || pipeline.starts_with("EPSG:") {
        if inverse {
            State::Unknown
        } else {
            State::LonLatDeg
        }
    } else {
        State::Unknown
    };
    let ok = std::cell::Cell::new(true);
    let mut geometry = g.geometry.map_coords(|mut c| {
        for s in &steps {
            let skip = if s.inv { s.omit_inv } else { s.omit_fwd };
            if !skip {
                c = apply(s.op, s.inv, c);
            }
        }
        ok.set(ok.get() && c.x.is_finite() && c.y.is_finite());
        c
    });
    ok.get().then_some(())?;
    let crs = match to_srid {
        Some(s) => crate::postgis::crs_of_srid(s)?,
        None => {
            let end = steps.iter().fold(start, next_state);
            match end {
                State::LonLatDeg => Crs::Crs84,
                State::LatLonDeg => {
                    // Stored as (longitude, latitude), written in EPSG:4326 order.
                    geometry = geometry.map_coords(|c| Coord { x: c.y, y: c.x });
                    Crs::Epsg4326
                }
                State::Projected(p) => {
                    crs_of(p).unwrap_or_else(|| Crs::Other(UNKNOWN_CRS.to_owned()))
                }
                _ => Crs::Other(UNKNOWN_CRS.to_owned()),
            }
        }
    };
    let zm = g.zm.as_ref().map(|zm| {
        let values = g
            .geometry
            .coords_iter()
            .map(|c| zm.at(c).unwrap_or((f64::NAN, f64::NAN)));
        Arc::new(Zm::from_coords(&geometry, zm.has_z, zm.has_m, values))
    });
    Some(Geom {
        geometry,
        crs,
        zm,
        surface: None,
        prepared: Default::default(),
    })
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, crate::functions::Func)) {
    for (name, inverse) in [
        ("ST_TransformPipeline", false),
        ("ST_InverseTransformPipeline", true),
    ] {
        add(
            crate::functions::ST,
            name,
            crate::functions::func(move |args| {
                let g = crate::functions::geom(args.first()?)?;
                let oxigraph::model::Term::Literal(p) = args.get(1)? else {
                    return None;
                };
                let to = match args.get(2) {
                    None => None,
                    Some(t) => match crate::postgis::int(t)? {
                        0 => None,
                        s => Some(s),
                    },
                };
                (args.len() <= 3).then_some(())?;
                Some(crate::literal::geom_literal(&transform_pipeline(
                    &g,
                    p.value(),
                    inverse,
                    to,
                )?))
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Geometry, Point};

    fn run(p: &str, inverse: bool, x: f64, y: f64) -> (f64, f64, Crs) {
        let g = Geom::new(Geometry::Point(Point::new(x, y)), Crs::Crs84);
        let out = transform_pipeline(&g, p, inverse, None).unwrap();
        let Geometry::Point(q) = out.geometry else {
            panic!()
        };
        (q.x(), q.y(), out.crs)
    }

    #[test]
    fn postgis_examples() {
        // PostGIS documentation: POINT(2 49) to UTM 31N.
        let (x, y, crs) = run(
            "urn:ogc:def:coordinateOperation:EPSG::16031",
            false,
            2.0,
            49.0,
        );
        assert!((x - 426_857.987_717_275).abs() < 1e-3 && (y - 5_427_937.523_466_16).abs() < 1e-3);
        assert_eq!(crs, crate::postgis::crs_of_srid(32631).unwrap());
        let p = "+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad +step +proj=utm +zone=31 +ellps=WGS84";
        let (x2, y2, crs2) = run(p, false, 2.0, 49.0);
        assert!((x2 - x).abs() < 1e-9 && (y2 - y).abs() < 1e-9);
        assert_eq!(crs2, crs);
        let g = Geom::new(Geometry::Point(Point::new(x, y)), crs);
        let back = transform_pipeline(&g, p, true, None).unwrap();
        assert_eq!(back.crs, Crs::Crs84);
        let Geometry::Point(q) = back.geometry else {
            panic!()
        };
        assert!(
            (q.x() - 2.0).abs() < 1e-7 && (q.y() - 49.0).abs() < 1e-7,
            "{q:?}"
        );
    }

    #[test]
    fn axis_swap_and_unsupported() {
        let p = "+proj=pipeline +step +proj=axisswap +order=2,1";
        let (x, y, _) = run(p, false, 1.0, 2.0);
        assert_eq!((x, y), (2.0, 1.0));
        assert!(parse("+proj=pipeline +step +proj=helmert +x=1").is_none());
        assert!(parse("+proj=utm +zone=31 +ellps=intl").is_none());
        assert!(parse("urn:ogc:def:coordinateOperation:EPSG::1618").is_none());
    }
}
