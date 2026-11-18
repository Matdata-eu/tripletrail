//! Mapbox Vector Tiles: `st:ST_AsMVTGeom` (a geometry in tile coordinates) and the encoder of
//! `ST_AsMVT` (a Mapbox Vector Tile 2.1 protobuf, written by hand), used by the `st:ST_AsMVT`
//! aggregate and the `ST_AsMVT` function of the `st` SERVICE.
//!
//! Tiles are returned as `xsd:hexBinary`, like `ST_AsBinary`; the hex of two tiles concatenated
//! is a tile with the layers of both, as with PostGIS's `||` on `bytea`.

use crate::functions::{Func, ST, func, geom, num, same_space};
use crate::literal::{Crs, Geom, geom_literal};
use crate::postgis_processing::{
    HEX_BINARY, lines_of, make_valid, points_of, polygons, polygons_of, remove_repeated, to_hex,
};
use geo::orient::{Direction, Orient};
use geo::{
    Area, BooleanOps, BoundingRect, Coord, Geometry, LineString, MapCoords, MultiLineString,
    MultiPoint, MultiPolygon, Point, Polygon, Rect, Simplify, Validation,
};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, Term};
use oxigraph::sparql::AggregateFunctionAccumulator;

/// The parts of the highest dimension of a geometry, as PostGIS keeps for a tile.
fn basic(g: &Geometry<f64>) -> Option<Geometry<f64>> {
    let polys = polygons_of(g);
    if !polys.is_empty() {
        return Some(Geometry::MultiPolygon(MultiPolygon(polys)));
    }
    let lines = lines_of(g);
    if !lines.is_empty() {
        return Some(Geometry::MultiLineString(MultiLineString(lines)));
    }
    let points = points_of(g);
    (!points.is_empty()).then_some(Geometry::MultiPoint(MultiPoint(points)))
}

fn round(c: Coord<f64>) -> Coord<f64> {
    Coord {
        x: c.x.round() + 0.0,
        y: c.y.round() + 0.0,
    }
}

/// Points of a ring snapped to the grid, or `None` when it collapses.
fn snap_ring(r: &LineString<f64>) -> Option<LineString<f64>> {
    let c: Vec<Coord<f64>> = r.0.iter().map(|&c| round(c)).collect();
    let c = remove_repeated(&c, 0.0, true);
    let ring = LineString::new(c);
    (ring.0.len() >= 4 && Polygon::new(ring.clone(), vec![]).unsigned_area() > 0.0).then_some(ring)
}

fn snap_polygons(polys: &[Polygon<f64>]) -> Vec<Polygon<f64>> {
    polys
        .iter()
        .filter_map(|p| {
            Some(Polygon::new(
                snap_ring(p.exterior())?,
                p.interiors().iter().filter_map(snap_ring).collect(),
            ))
        })
        .collect()
}

/// PostGIS `ST_AsMVTGeom`: `g` in the coordinate space of a tile covering `bounds`, `extent`
/// units wide, y pointing down; clipped to the tile plus `buffer` when `clip` is set, snapped to
/// integers, with parts that collapse dropped. Polygons have their exterior rings clockwise as
/// drawn in tile coordinates (positive area by the surveyor's formula), as the MVT spec wants.
pub fn tile_geometry(
    g: &Geometry<f64>,
    bounds: Rect<f64>,
    extent: f64,
    buffer: f64,
    clip: bool,
) -> Option<Geometry<f64>> {
    tile_geometry_simplified(g, bounds, extent, buffer, clip, 0.0)
}

/// [`tile_geometry`], with lines and polygon rings first simplified (Douglas–Peucker) to
/// `tolerance` tile units, so a low zoom tile does not carry detail it cannot show. A polygon
/// the simplification makes invalid is repaired as any other.
pub fn tile_geometry_simplified(
    g: &Geometry<f64>,
    bounds: Rect<f64>,
    extent: f64,
    buffer: f64,
    clip: bool,
    tolerance: f64,
) -> Option<Geometry<f64>> {
    let (w, h) = (bounds.width(), bounds.height());
    if !(w > 0.0 && h > 0.0 && extent > 0.0 && buffer >= 0.0) {
        return None;
    }
    let (sx, sy) = (extent / w, extent / h);
    let (x0, y1) = (bounds.min().x, bounds.max().y);
    let mut t = basic(g)?.map_coords(|c| Coord {
        x: (c.x - x0) * sx,
        y: (y1 - c.y) * sy,
    });
    if tolerance > 0.0 {
        t = match t {
            Geometry::MultiLineString(m) => Geometry::MultiLineString(m.simplify(tolerance)),
            Geometry::MultiPolygon(m) => Geometry::MultiPolygon(m.simplify(tolerance)),
            other => other,
        };
    }
    let (lo, hi) = (-buffer, extent + buffer);
    let window = Rect::new(Coord { x: lo, y: lo }, Coord { x: hi, y: hi });
    match t {
        Geometry::MultiPoint(m) => {
            let mut pts: Vec<Point<f64>> = Vec::new();
            for p in m.0 {
                let inside = (lo..=hi).contains(&p.x()) && (lo..=hi).contains(&p.y());
                let p = Point(round(p.0));
                if (inside || !clip) && !pts.contains(&p) {
                    pts.push(p);
                }
            }
            match pts.len() {
                0 => None,
                1 => Some(Geometry::Point(pts[0])),
                _ => Some(Geometry::MultiPoint(MultiPoint(pts))),
            }
        }
        Geometry::MultiLineString(m) => {
            let m = if clip {
                window.to_polygon().clip(&m, false)
            } else {
                m
            };
            let lines: Vec<LineString<f64>> =
                m.0.iter()
                    .map(|l| {
                        let c: Vec<Coord<f64>> = l.0.iter().map(|&c| round(c)).collect();
                        LineString::new(remove_repeated(&c, 0.0, false))
                    })
                    .filter(|l| l.0.len() >= 2)
                    .collect();
            match lines.len() {
                0 => None,
                1 => Some(Geometry::LineString(lines.into_iter().next()?)),
                _ => Some(Geometry::MultiLineString(MultiLineString(lines))),
            }
        }
        Geometry::MultiPolygon(m) => {
            let m = if clip {
                m.intersection(&window.to_polygon())
            } else {
                m
            };
            let mut polys = snap_polygons(&m.0);
            let multi = MultiPolygon(polys.clone());
            if !multi.is_valid() {
                polys = snap_polygons(&crate::postgis_processing::polygons_of(&make_valid(
                    &Geometry::MultiPolygon(multi),
                )?));
            }
            if polys.is_empty() {
                return None;
            }
            // Positive area for exterior rings, negative for holes.
            let polys: Vec<Polygon<f64>> = polys
                .into_iter()
                .map(|p| p.orient(Direction::Default))
                .collect();
            Some(polygons(polys))
        }
        _ => None,
    }
}

/// [`tile_geometry`] for the tile whose bounds are the box of `bounds`; `g` is transformed to
/// the CRS of `bounds` first if they differ.
pub fn as_mvt_geom(
    g: &Geom,
    bounds: &Geom,
    extent: f64,
    buffer: f64,
    clip: bool,
) -> Option<Geometry<f64>> {
    as_mvt_geom_simplified(g, bounds, extent, buffer, clip, 0.0)
}

/// [`as_mvt_geom`] with [`tile_geometry_simplified`]'s `tolerance`, in tile units.
pub fn as_mvt_geom_simplified(
    g: &Geom,
    bounds: &Geom,
    extent: f64,
    buffer: f64,
    clip: bool,
    tolerance: f64,
) -> Option<Geometry<f64>> {
    let moved;
    let g = if same_space(g, bounds) {
        g
    } else {
        moved = crate::crs::transform_geom(g, &bounds.crs)?;
        &moved
    };
    tile_geometry_simplified(
        &g.geometry,
        bounds.geometry.bounding_rect()?,
        extent,
        buffer,
        clip,
        tolerance,
    )
}

fn boolean_arg(t: &Term) -> Option<bool> {
    let Term::Literal(l) = t else { return None };
    match l.value() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    add(
        ST,
        "ST_AsMVTGeom",
        func(|args| {
            let (g, b, rest) = match args {
                [g, b, rest @ ..] if rest.len() <= 3 => (g, b, rest),
                _ => return None,
            };
            let extent = rest.first().map(num).unwrap_or(Some(4096.0))?;
            let buffer = rest.get(1).map(num).unwrap_or(Some(256.0))?;
            let clip = rest.get(2).map(boolean_arg).unwrap_or(Some(true))?;
            let t = as_mvt_geom(&*geom(g)?, &*geom(b)?, extent, buffer, clip)?;
            // Tile coordinates have no CRS; the literal is written without one.
            Some(geom_literal(&Geom::new(t, Crs::Crs84)))
        }),
    );
}

// --- Protobuf encoding ---------------------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn key(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn bytes_field(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    key(out, field, 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn varint_field(out: &mut Vec<u8>, field: u32, v: u64) {
    key(out, field, 0);
    varint(out, v);
}

fn packed(out: &mut Vec<u8>, field: u32, values: &[u32]) {
    let mut data = Vec::with_capacity(values.len() * 2);
    for &v in values {
        varint(&mut data, u64::from(v));
    }
    bytes_field(out, field, &data);
}

fn zigzag(n: i64) -> u32 {
    let n = n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    ((n << 1) ^ (n >> 31)) as u32
}

fn command(id: u32, count: usize) -> u32 {
    (id & 0x7) | ((count as u32) << 3)
}

const MOVE_TO: u32 = 1;
const LINE_TO: u32 = 2;
const CLOSE_PATH: u32 = 7;

/// The MVT geometry type and command stream of a geometry in tile coordinates.
fn encode_geometry(g: &Geometry<f64>) -> Option<(u64, Vec<u32>)> {
    let g = basic(g)?;
    let mut cmds = Vec::new();
    let mut cursor = (0i64, 0i64);
    let mut step = |cmds: &mut Vec<u32>, c: (i64, i64)| {
        cmds.push(zigzag(c.0 - cursor.0));
        cmds.push(zigzag(c.1 - cursor.1));
        cursor = c;
    };
    let ints = |cs: &[Coord<f64>]| -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = Vec::with_capacity(cs.len());
        for c in cs {
            let p = (c.x.round() as i64, c.y.round() as i64);
            if out.last() != Some(&p) {
                out.push(p);
            }
        }
        out
    };
    let kind = match &g {
        Geometry::MultiPoint(m) => {
            cmds.push(command(MOVE_TO, m.0.len()));
            for p in &m.0 {
                step(&mut cmds, (p.x().round() as i64, p.y().round() as i64));
            }
            1
        }
        Geometry::MultiLineString(m) => {
            for l in &m.0 {
                let pts = ints(&l.0);
                if pts.len() < 2 {
                    continue;
                }
                cmds.push(command(MOVE_TO, 1));
                step(&mut cmds, pts[0]);
                cmds.push(command(LINE_TO, pts.len() - 1));
                for &p in &pts[1..] {
                    step(&mut cmds, p);
                }
            }
            2
        }
        Geometry::MultiPolygon(m) => {
            for p in &m.0 {
                // Exterior with positive area in tile coordinates, holes negative.
                let p = p.orient(Direction::Default);
                let mut rings = vec![p.exterior()];
                rings.extend(p.interiors());
                for (i, r) in rings.into_iter().enumerate() {
                    let mut pts = ints(&r.0);
                    if pts.len() > 1 && pts.first() == pts.last() {
                        pts.pop();
                    }
                    if pts.len() < 3 {
                        if i == 0 {
                            break;
                        }
                        continue;
                    }
                    cmds.push(command(MOVE_TO, 1));
                    step(&mut cmds, pts[0]);
                    cmds.push(command(LINE_TO, pts.len() - 1));
                    for &q in &pts[1..] {
                        step(&mut cmds, q);
                    }
                    cmds.push(command(CLOSE_PATH, 1));
                }
            }
            3
        }
        _ => return None,
    };
    (!cmds.is_empty()).then_some((kind, cmds))
}

/// The encoded `Value` message for an attribute, or `None` for a term that has none.
fn value_message(t: &Term) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match t {
        Term::Literal(l) => {
            let dt = l.datatype();
            let integer = [
                xsd::INTEGER,
                xsd::INT,
                xsd::LONG,
                xsd::SHORT,
                xsd::BYTE,
                xsd::NON_NEGATIVE_INTEGER,
                xsd::NON_POSITIVE_INTEGER,
                xsd::POSITIVE_INTEGER,
                xsd::NEGATIVE_INTEGER,
                xsd::UNSIGNED_INT,
                xsd::UNSIGNED_LONG,
                xsd::UNSIGNED_SHORT,
                xsd::UNSIGNED_BYTE,
            ]
            .contains(&dt);
            if dt == xsd::BOOLEAN {
                varint_field(&mut out, 7, u64::from(matches!(l.value(), "true" | "1")));
            } else if integer && let Ok(v) = l.value().parse::<i64>() {
                if v >= 0 {
                    varint_field(&mut out, 5, v as u64);
                } else {
                    varint_field(&mut out, 6, ((v << 1) ^ (v >> 63)) as u64);
                }
            } else if (dt == xsd::DOUBLE || dt == xsd::FLOAT || dt == xsd::DECIMAL)
                && let Ok(v) = l.value().parse::<f64>()
            {
                key(&mut out, 3, 1);
                out.extend_from_slice(&v.to_le_bytes());
            } else {
                bytes_field(&mut out, 1, l.value().as_bytes());
            }
        }
        Term::NamedNode(n) => bytes_field(&mut out, 1, n.as_str().as_bytes()),
        _ => return None,
    }
    Some(out)
}

/// One feature of a layer: a geometry in tile coordinates, an optional id and attributes.
pub struct Feature {
    pub geometry: Geometry<f64>,
    pub id: Option<u64>,
    pub properties: Vec<(String, Term)>,
}

/// A tile holding one layer with the given features. Features whose geometry is empty are
/// left out; a layer with no feature gives an empty tile, as in PostGIS.
pub fn encode_tile(name: &str, extent: u32, features: &[Feature]) -> Vec<u8> {
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<Vec<u8>> = Vec::new();
    let mut key_index = std::collections::HashMap::<String, u32>::new();
    let mut value_index = std::collections::HashMap::<Vec<u8>, u32>::new();
    let mut encoded = Vec::new();
    for f in features {
        let Some((kind, geometry)) = encode_geometry(&f.geometry) else {
            continue;
        };
        let mut tags = Vec::new();
        for (k, v) in &f.properties {
            let Some(v) = value_message(v) else { continue };
            let ki = *key_index.entry(k.clone()).or_insert_with(|| {
                keys.push(k.clone());
                (keys.len() - 1) as u32
            });
            let vi = *value_index.entry(v.clone()).or_insert_with(|| {
                values.push(v);
                (values.len() - 1) as u32
            });
            tags.push(ki);
            tags.push(vi);
        }
        let mut feature = Vec::new();
        if let Some(id) = f.id {
            varint_field(&mut feature, 1, id);
        }
        if !tags.is_empty() {
            packed(&mut feature, 2, &tags);
        }
        varint_field(&mut feature, 3, kind);
        packed(&mut feature, 4, &geometry);
        encoded.push(feature);
    }
    if encoded.is_empty() {
        return Vec::new();
    }
    let mut layer = Vec::new();
    varint_field(&mut layer, 15, 2);
    bytes_field(&mut layer, 1, name.as_bytes());
    for f in &encoded {
        bytes_field(&mut layer, 2, f);
    }
    for k in &keys {
        bytes_field(&mut layer, 3, k.as_bytes());
    }
    for v in &values {
        bytes_field(&mut layer, 4, v);
    }
    varint_field(&mut layer, 5, u64::from(extent));
    let mut tile = Vec::new();
    bytes_field(&mut tile, 3, &layer);
    tile
}

/// A tile as an `xsd:hexBinary` literal.
pub fn tile_literal(bytes: &[u8]) -> Term {
    Literal::new_typed_literal(
        to_hex(bytes),
        oxigraph::model::NamedNodeRef::new_unchecked(HEX_BINARY),
    )
    .into()
}

/// `st:ST_AsMVT(?geom)`: the geometries of a group (from `ST_AsMVTGeom`) as a one-layer tile
/// named `default`, extent 4096, without attributes.
pub(crate) struct MvtAccumulator {
    features: Vec<Feature>,
    failed: bool,
}

impl MvtAccumulator {
    pub(crate) fn boxed() -> Box<dyn AggregateFunctionAccumulator + Send + Sync> {
        Box::new(Self {
            features: Vec::new(),
            failed: false,
        })
    }
}

impl AggregateFunctionAccumulator for MvtAccumulator {
    fn accumulate(&mut self, element: Term) {
        match geom(&element) {
            Some(g) => self.features.push(Feature {
                geometry: g.geometry.clone(),
                id: None,
                properties: Vec::new(),
            }),
            None => self.failed = true,
        }
    }

    fn finish(&mut self) -> Option<Term> {
        if self.failed {
            return None;
        }
        Some(tile_literal(&encode_tile("default", 4096, &self.features)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_zigzag() {
        let mut v = Vec::new();
        varint(&mut v, 300);
        assert_eq!(v, [0xAC, 0x02]);
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(-2), 3);
    }

    #[test]
    fn spec_examples() {
        // MVT 2.1 spec, 4.3.5.1 point and 4.3.5.3 line.
        let p = Geometry::Point(Point::new(25.0, 17.0));
        assert_eq!(encode_geometry(&p).unwrap(), (1, vec![9, 50, 34]));
        let l = Geometry::LineString(LineString::from(vec![
            (2.0, 2.0),
            (2.0, 10.0),
            (10.0, 10.0),
        ]));
        assert_eq!(
            encode_geometry(&l).unwrap(),
            (2, vec![9, 4, 4, 18, 0, 16, 16, 0])
        );
        // 4.3.5.5: polygon, exterior with positive area in tile coordinates.
        let poly = Geometry::Polygon(Polygon::new(
            LineString::from(vec![(3.0, 6.0), (8.0, 12.0), (20.0, 34.0), (3.0, 6.0)]),
            vec![],
        ));
        assert_eq!(
            encode_geometry(&poly).unwrap(),
            (3, vec![9, 6, 12, 18, 10, 12, 24, 44, 15])
        );
    }

    #[test]
    fn tile_space() {
        let b = Rect::new(
            Coord { x: 0.0, y: 0.0 },
            Coord {
                x: 4096.0,
                y: 4096.0,
            },
        );
        let g = Geometry::Point(Point::new(10.2, 4000.0));
        assert_eq!(
            tile_geometry(&g, b, 4096.0, 0.0, true),
            Some(Geometry::Point(Point::new(10.0, 96.0)))
        );
        let far = Geometry::Point(Point::new(-10.0, 10.0));
        assert_eq!(tile_geometry(&far, b, 4096.0, 0.0, true), None);
        assert!(tile_geometry(&far, b, 4096.0, 0.0, false).is_some());
    }

    #[test]
    fn simplified_to_a_tolerance() {
        let b = Rect::new(
            Coord { x: 0.0, y: 0.0 },
            Coord {
                x: 4096.0,
                y: 4096.0,
            },
        );
        // A line with a wiggle of a third of a unit every 10 units.
        let wiggly: Vec<Coord<f64>> = (0..=100)
            .map(|i| Coord {
                x: f64::from(i) * 10.0,
                y: 2000.0 + if i % 2 == 0 { 0.0 } else { 0.3 },
            })
            .collect();
        let g = Geometry::LineString(LineString::new(wiggly));
        let points = |g: Option<Geometry<f64>>| match g {
            Some(Geometry::LineString(l)) => l.0.len(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            points(tile_geometry_simplified(&g, b, 4096.0, 0.0, true, 1.0)),
            2
        );
        // Without a tolerance only snapping applies: the wiggle rounds away, the points stay.
        assert_eq!(points(tile_geometry(&g, b, 4096.0, 0.0, true)), 101);
    }
}
