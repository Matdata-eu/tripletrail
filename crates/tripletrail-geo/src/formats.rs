//! Feature formats written by hand: FlatGeobuf (`ST_AsFlatGeobuf`, `ST_FromFlatGeobuf`), Geobuf
//! (`ST_AsGeobuf`) and MARC21/XML field 034 (`ST_AsMARC21`, `ST_GeomFromMARC21`).
//!
//! FlatGeobuf follows the 3.x specification (magic bytes, a size-prefixed `Header` flatbuffer,
//! an optional packed Hilbert R-tree, then size-prefixed `Feature` flatbuffers). Geobuf follows
//! the reference encoder (`geobuf.proto` 1.x): a `Data` message holding a feature collection,
//! with the coordinate precision found from the data, up to six decimals. Binary results are
//! `xsd:hexBinary`, like `ST_AsBinary`.

use crate::functions::{Func, ST, func, geom};
use crate::literal::{Crs, Geom, Zm, geom_literal};
use crate::postgis::{crs_of_srid, srid, string};
use crate::postgis_processing::{HEX_BINARY, from_hex, to_hex};
use geo::{
    BoundingRect, Coord, CoordsIter, Geometry, GeometryCollection, LineString, MultiLineString,
    MultiPoint, MultiPolygon, Point, Polygon, Rect,
};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNodeRef, Term};
use oxigraph::sparql::AggregateFunctionAccumulator;
use std::collections::HashMap;
use std::sync::Arc;

/// One feature to write: a geometry (absent for a feature without one) and its properties, in
/// column order. Properties without a value are left out.
#[derive(Debug, Clone)]
pub struct Feature {
    pub geom: Option<Arc<Geom>>,
    pub properties: Vec<(String, Term)>,
}

/// Binary data as an `xsd:hexBinary` literal.
pub fn hex_literal(bytes: &[u8]) -> Term {
    Literal::new_typed_literal(to_hex(bytes), NamedNodeRef::new_unchecked(HEX_BINARY)).into()
}

/// The bytes of an `xsd:hexBinary` (or plain hex string) literal.
pub fn hex_bytes(t: &Term) -> Option<Vec<u8>> {
    match t {
        Term::Literal(l) => from_hex(l.value()),
        _ => None,
    }
}

const INTEGER_TYPES: [NamedNodeRef<'static>; 13] = [
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
];

/// The kind of value a property term holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Long,
    Double,
    String,
    DateTime,
    Binary,
}

fn kind_of(t: &Term) -> Kind {
    let Term::Literal(l) = t else {
        return Kind::String;
    };
    let dt = l.datatype();
    if dt == xsd::BOOLEAN && matches!(l.value(), "true" | "false" | "1" | "0") {
        Kind::Bool
    } else if INTEGER_TYPES.contains(&dt) && l.value().parse::<i64>().is_ok() {
        Kind::Long
    } else if (dt == xsd::DOUBLE || dt == xsd::FLOAT || dt == xsd::DECIMAL)
        && l.value().parse::<f64>().is_ok()
    {
        Kind::Double
    } else if dt == xsd::DATE_TIME {
        Kind::DateTime
    } else if dt.as_str() == HEX_BINARY && from_hex(l.value()).is_some() {
        Kind::Binary
    } else {
        Kind::String
    }
}

fn text_of(t: &Term) -> String {
    match t {
        Term::Literal(l) => l.value().to_owned(),
        Term::NamedNode(n) => n.as_str().to_owned(),
        other => other.to_string(),
    }
}

// --- Flatbuffers -------------------------------------------------------------------------------

/// A flatbuffer object to serialise.
enum Fb {
    /// A table: (field id, value).
    Table(Vec<(u16, Val)>),
    Str(String),
    /// A vector of scalars: little-endian bytes, element size, element count.
    Scalars(Vec<u8>, usize, usize),
    Tables(Vec<Fb>),
}

enum Val {
    U8(u8),
    U16(u16),
    I32(i32),
    U64(u64),
    Obj(Fb),
}

impl Val {
    fn size(&self) -> usize {
        match self {
            Val::U8(_) => 1,
            Val::U16(_) => 2,
            Val::I32(_) | Val::Obj(_) => 4,
            Val::U64(_) => 8,
        }
    }
}

fn doubles(v: &[f64]) -> Fb {
    Fb::Scalars(v.iter().flat_map(|x| x.to_le_bytes()).collect(), 8, v.len())
}

fn u32s(v: &[u32]) -> Fb {
    Fb::Scalars(v.iter().flat_map(|x| x.to_le_bytes()).collect(), 4, v.len())
}

/// Writes flatbuffers front to back: a table, then the objects it points to. Offsets always
/// point forward, as the format requires; alignment is relative to the start of the buffer,
/// which is the size prefix.
struct FbWriter {
    buf: Vec<u8>,
}

impl FbWriter {
    fn pad(&mut self, align: usize) {
        while !self.buf.len().is_multiple_of(align) {
            self.buf.push(0);
        }
    }

    fn patch(&mut self, at: usize, target: usize) {
        let off = (target - at) as u32;
        self.buf[at..at + 4].copy_from_slice(&off.to_le_bytes());
    }

    fn obj(&mut self, o: &Fb) -> usize {
        match o {
            Fb::Str(s) => {
                self.pad(4);
                let pos = self.buf.len();
                self.buf.extend((s.len() as u32).to_le_bytes());
                self.buf.extend(s.as_bytes());
                self.buf.push(0);
                pos
            }
            Fb::Scalars(bytes, size, n) => {
                let align = (*size).max(4);
                while !(self.buf.len() + 4).is_multiple_of(align) {
                    self.buf.push(0);
                }
                let pos = self.buf.len();
                self.buf.extend((*n as u32).to_le_bytes());
                self.buf.extend(bytes);
                pos
            }
            Fb::Tables(items) => {
                self.pad(4);
                let pos = self.buf.len();
                self.buf.extend((items.len() as u32).to_le_bytes());
                let slots = self.buf.len();
                self.buf.resize(slots + 4 * items.len(), 0);
                for (i, item) in items.iter().enumerate() {
                    let target = self.obj(item);
                    self.patch(slots + 4 * i, target);
                }
                pos
            }
            Fb::Table(fields) => {
                let nfields = fields
                    .iter()
                    .map(|(id, _)| *id as usize + 1)
                    .max()
                    .unwrap_or(0);
                // Inline layout, largest fields first, after the vtable offset.
                let mut order: Vec<usize> = (0..fields.len()).collect();
                order.sort_by_key(|&i| std::cmp::Reverse(fields[i].1.size()));
                let mut rel = vec![0usize; fields.len()];
                let mut at = 4usize;
                for &i in &order {
                    let s = fields[i].1.size();
                    at = at.next_multiple_of(s);
                    rel[i] = at;
                    at += s;
                }
                let table_size = at.next_multiple_of(4);
                // The vtable, then the table aligned to 8.
                self.pad(2);
                let vt = self.buf.len();
                let vt_size = 4 + 2 * nfields;
                self.buf.extend((vt_size as u16).to_le_bytes());
                self.buf.extend((table_size as u16).to_le_bytes());
                let mut slots = vec![0u16; nfields];
                for (i, (id, _)) in fields.iter().enumerate() {
                    slots[*id as usize] = rel[i] as u16;
                }
                for s in slots {
                    self.buf.extend(s.to_le_bytes());
                }
                self.pad(8);
                let table = self.buf.len();
                self.buf.resize(table + table_size, 0);
                self.buf[table..table + 4].copy_from_slice(&((table - vt) as i32).to_le_bytes());
                let mut children = Vec::new();
                for (i, (_, v)) in fields.iter().enumerate() {
                    let p = table + rel[i];
                    match v {
                        Val::U8(x) => self.buf[p] = *x,
                        Val::U16(x) => self.buf[p..p + 2].copy_from_slice(&x.to_le_bytes()),
                        Val::I32(x) => self.buf[p..p + 4].copy_from_slice(&x.to_le_bytes()),
                        Val::U64(x) => self.buf[p..p + 8].copy_from_slice(&x.to_le_bytes()),
                        Val::Obj(o) => children.push((p, o)),
                    }
                }
                for (p, o) in children {
                    let target = self.obj(o);
                    self.patch(p, target);
                }
                table
            }
        }
    }

    /// A size-prefixed buffer with `root` as its root table, padded to 8 bytes.
    fn finish(root: &Fb) -> Vec<u8> {
        let mut w = FbWriter { buf: vec![0; 8] };
        let table = w.obj(root);
        w.patch(4, table);
        w.pad(8);
        let size = (w.buf.len() - 4) as u32;
        w.buf[0..4].copy_from_slice(&size.to_le_bytes());
        w.buf
    }
}

/// Reads flatbuffer tables, with bounds checks everywhere.
#[derive(Clone, Copy)]
struct FbReader<'a> {
    b: &'a [u8],
}

impl<'a> FbReader<'a> {
    fn bytes<const N: usize>(&self, pos: usize) -> Option<[u8; N]> {
        self.b.get(pos..pos.checked_add(N)?)?.try_into().ok()
    }
    fn u16(&self, pos: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(pos)?))
    }
    fn u32(&self, pos: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(pos)?))
    }
    fn i32(&self, pos: usize) -> Option<i32> {
        Some(i32::from_le_bytes(self.bytes(pos)?))
    }
    fn u64(&self, pos: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(pos)?))
    }
    fn deref(&self, pos: usize) -> Option<usize> {
        pos.checked_add(self.u32(pos)? as usize)
    }
    /// The root table of a buffer that starts at its root offset.
    fn root(&self) -> Option<usize> {
        self.deref(0)
    }
    /// The position of a table field, or `None` when absent.
    fn field(&self, table: usize, id: u16) -> Option<usize> {
        let vt = (table as i64 - i64::from(self.i32(table)?)) as usize;
        let vt_size = self.u16(vt)? as usize;
        let slot = 4 + 2 * id as usize;
        if slot + 2 > vt_size {
            return None;
        }
        match self.u16(vt + slot)? {
            0 => None,
            off => Some(table + off as usize),
        }
    }
    fn field_u8(&self, table: usize, id: u16, default: u8) -> u8 {
        self.field(table, id)
            .and_then(|p| self.b.get(p).copied())
            .unwrap_or(default)
    }
    fn table(&self, table: usize, id: u16) -> Option<usize> {
        self.deref(self.field(table, id)?)
    }
    /// A vector field: the position of its first element and its length.
    fn vector(&self, table: usize, id: u16) -> Option<(usize, usize)> {
        let v = self.table(table, id)?;
        let n = self.u32(v)? as usize;
        Some((v + 4, n))
    }
    fn string(&self, table: usize, id: u16) -> Option<&'a str> {
        let (start, n) = self.vector(table, id)?;
        std::str::from_utf8(self.b.get(start..start.checked_add(n)?)?).ok()
    }
    fn doubles(&self, table: usize, id: u16) -> Vec<f64> {
        self.vector(table, id)
            .map(|(s, n)| {
                (0..n)
                    .map_while(|i| Some(f64::from_le_bytes(self.bytes(s + 8 * i)?)))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn u32s(&self, table: usize, id: u16) -> Vec<u32> {
        self.vector(table, id)
            .map(|(s, n)| (0..n).map_while(|i| self.u32(s + 4 * i)).collect())
            .unwrap_or_default()
    }
    fn tables(&self, table: usize, id: u16) -> Vec<usize> {
        self.vector(table, id)
            .map(|(s, n)| (0..n).map_while(|i| self.deref(s + 4 * i)).collect())
            .unwrap_or_default()
    }
}

// --- FlatGeobuf --------------------------------------------------------------------------------

const FGB_MAGIC: [u8; 8] = [0x66, 0x67, 0x62, 0x03, 0x66, 0x67, 0x62, 0x00];

/// FlatGeobuf geometry type codes.
fn fgb_type(g: &Geometry<f64>) -> u8 {
    match g {
        Geometry::Point(_) => 1,
        Geometry::LineString(_) | Geometry::Line(_) => 2,
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => 3,
        Geometry::MultiPoint(_) => 4,
        Geometry::MultiLineString(_) => 5,
        Geometry::MultiPolygon(_) => 6,
        Geometry::GeometryCollection(_) => 7,
    }
}

/// FlatGeobuf column type codes.
fn column_type(k: Kind) -> u8 {
    match k {
        Kind::Bool => 2,
        Kind::Long => 7,
        Kind::Double => 10,
        Kind::String => 11,
        Kind::DateTime => 13,
        Kind::Binary => 14,
    }
}

/// The FlatGeobuf `Geometry` table of a geometry, with Z and M where `dims` asks for them.
fn fgb_geometry(g: &Geometry<f64>, zm: Option<&Zm>, dims: (bool, bool)) -> Fb {
    let mut xy: Vec<f64> = Vec::new();
    let mut z: Vec<f64> = Vec::new();
    let mut m: Vec<f64> = Vec::new();
    let mut ends: Vec<u32> = Vec::new();
    let mut parts: Vec<Fb> = Vec::new();
    let mut push = |c: Coord<f64>, xy: &mut Vec<f64>| {
        xy.push(c.x);
        xy.push(c.y);
        let (cz, cm) = zm.and_then(|v| v.at(c)).unwrap_or((f64::NAN, f64::NAN));
        if dims.0 {
            z.push(cz);
        }
        if dims.1 {
            m.push(cm);
        }
    };
    let rings = |p: &Polygon<f64>| -> Vec<LineString<f64>> {
        std::iter::once(p.exterior().clone())
            .chain(p.interiors().iter().cloned())
            .collect()
    };
    match g {
        Geometry::Point(p) => push(p.0, &mut xy),
        Geometry::MultiPoint(mp) => mp.0.iter().for_each(|p| push(p.0, &mut xy)),
        Geometry::Line(_) | Geometry::LineString(_) => {
            g.coords_iter().for_each(|c| push(c, &mut xy))
        }
        Geometry::MultiLineString(ml) => {
            for l in &ml.0 {
                l.0.iter().for_each(|&c| push(c, &mut xy));
                ends.push((xy.len() / 2) as u32);
            }
        }
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => {
            let p = match g {
                Geometry::Polygon(p) => p.clone(),
                Geometry::Rect(r) => r.to_polygon(),
                Geometry::Triangle(t) => t.to_polygon(),
                _ => unreachable!(),
            };
            for r in rings(&p) {
                r.0.iter().for_each(|&c| push(c, &mut xy));
                ends.push((xy.len() / 2) as u32);
            }
        }
        Geometry::MultiPolygon(mp) => {
            parts =
                mp.0.iter()
                    .map(|p| fgb_geometry(&Geometry::Polygon(p.clone()), zm, dims))
                    .collect();
        }
        Geometry::GeometryCollection(gc) => {
            parts = gc.0.iter().map(|p| fgb_geometry(p, zm, dims)).collect();
        }
    }
    let mut fields = Vec::new();
    // `ends` is left out for a single part, as the specification recommends.
    if ends.len() > 1 {
        fields.push((0, Val::Obj(u32s(&ends))));
    }
    if !xy.is_empty() {
        fields.push((1, Val::Obj(doubles(&xy))));
        if dims.0 {
            fields.push((2, Val::Obj(doubles(&z))));
        }
        if dims.1 {
            fields.push((3, Val::Obj(doubles(&m))));
        }
    }
    fields.push((6, Val::U8(fgb_type(g))));
    if !parts.is_empty() {
        fields.push((7, Val::Obj(Fb::Tables(parts))));
    }
    Fb::Table(fields)
}

fn is_empty(g: &Geometry<f64>) -> bool {
    g.coords_iter().next().is_none()
}

/// The Hilbert value of a point on a 2¹⁶ × 2¹⁶ grid (as FlatGeobuf's reference writers).
fn hilbert(x: u32, y: u32) -> u32 {
    let mut a = x ^ y;
    let mut b = 0xFFFF ^ a;
    let mut c = 0xFFFF ^ (x | y);
    let mut d = x & (y ^ 0xFFFF);
    let mut aa = a | (b >> 1);
    let mut bb = (a >> 1) ^ a;
    let mut cc = ((c >> 1) ^ (b & (d >> 1))) ^ c;
    let mut dd = ((a & (c >> 1)) ^ (d >> 1)) ^ d;
    a = aa;
    b = bb;
    c = cc;
    d = dd;
    aa = (a & (a >> 2)) ^ (b & (b >> 2));
    bb = (a & (b >> 2)) ^ (b & ((a ^ b) >> 2));
    cc ^= (a & (c >> 2)) ^ (b & (d >> 2));
    dd ^= (b & (c >> 2)) ^ ((a ^ b) & (d >> 2));
    a = aa;
    b = bb;
    c = cc;
    d = dd;
    aa = (a & (a >> 4)) ^ (b & (b >> 4));
    bb = (a & (b >> 4)) ^ (b & ((a ^ b) >> 4));
    cc ^= (a & (c >> 4)) ^ (b & (d >> 4));
    dd ^= (b & (c >> 4)) ^ ((a ^ b) & (d >> 4));
    a = aa;
    b = bb;
    c = cc;
    d = dd;
    cc ^= (a & (c >> 8)) ^ (b & (d >> 8));
    dd ^= (b & (c >> 8)) ^ ((a ^ b) & (d >> 8));
    a = cc ^ (cc >> 1);
    b = dd ^ (dd >> 1);
    let mut i0 = x ^ y;
    let mut i1 = b | (0xFFFF ^ (i0 | a));
    let spread = |mut v: u32| {
        v = (v | (v << 8)) & 0x00FF_00FF;
        v = (v | (v << 4)) & 0x0F0F_0F0F;
        v = (v | (v << 2)) & 0x3333_3333;
        (v | (v << 1)) & 0x5555_5555
    };
    i0 = spread(i0);
    i1 = spread(i1);
    (i1 << 1) | i0
}

/// Node counts per level of a packed R-tree, leaves first, as FlatGeobuf computes them.
fn level_bounds(items: usize, node_size: usize) -> Vec<std::ops::Range<usize>> {
    let mut n = items;
    let mut total = n;
    let mut counts = vec![n];
    loop {
        n = n.div_ceil(node_size);
        total += n;
        counts.push(n);
        if n == 1 {
            break;
        }
    }
    let mut bounds = Vec::new();
    let mut end = total;
    for c in counts {
        bounds.push(end - c..end);
        end -= c;
    }
    bounds
}

const NODE_SIZE: usize = 16;

/// A FlatGeobuf file of the features: their properties become columns (typed from the values;
/// a column with values of different kinds is a string column). With `index`, features are
/// ordered along a Hilbert curve and a packed R-tree is written, and features without a
/// geometry are left out (the index needs a box for each).
pub fn encode_flatgeobuf(features: &[Feature], index: bool, name: Option<&str>) -> Vec<u8> {
    let mut features: Vec<&Feature> = features
        .iter()
        .filter(|f| !index || f.geom.as_ref().is_some_and(|g| !is_empty(&g.geometry)))
        .collect();
    // Columns, in order of appearance, with the kind of their values.
    let mut columns: Vec<(String, Kind)> = Vec::new();
    for f in &features {
        for (k, v) in &f.properties {
            let kind = kind_of(v);
            match columns.iter_mut().find(|(n, _)| n == k) {
                Some((_, c)) if *c != kind => {
                    *c = if matches!(
                        (*c, kind),
                        (Kind::Long, Kind::Double) | (Kind::Double, Kind::Long)
                    ) {
                        Kind::Double
                    } else {
                        Kind::String
                    }
                }
                Some(_) => {}
                None => columns.push((k.clone(), kind)),
            }
        }
    }
    let geoms: Vec<&Geom> = features.iter().filter_map(|f| f.geom.as_deref()).collect();
    let has_z = geoms.iter().any(|g| g.zm.as_ref().is_some_and(|z| z.has_z));
    let has_m = geoms.iter().any(|g| g.zm.as_ref().is_some_and(|z| z.has_m));
    let types: Vec<u8> = geoms
        .iter()
        .filter(|g| !is_empty(&g.geometry))
        .map(|g| fgb_type(&g.geometry))
        .collect();
    let header_type = match types.first() {
        Some(&t) if types.iter().all(|&u| u == t) => t,
        _ => 0,
    };
    let crs = geoms.first().map(|g| srid(&g.crs)).unwrap_or(0);
    let mut extent: Option<Rect<f64>> = None;
    let boxes: Vec<Option<Rect<f64>>> = features
        .iter()
        .map(|f| f.geom.as_ref().and_then(|g| g.geometry.bounding_rect()))
        .collect();
    for r in boxes.iter().flatten() {
        extent = Some(match extent {
            None => *r,
            Some(e) => Rect::new(
                Coord {
                    x: e.min().x.min(r.min().x),
                    y: e.min().y.min(r.min().y),
                },
                Coord {
                    x: e.max().x.max(r.max().x),
                    y: e.max().y.max(r.max().y),
                },
            ),
        });
    }
    let mut order: Vec<usize> = (0..features.len()).collect();
    if index && let Some(e) = extent {
        let (w, h) = (e.width(), e.height());
        let scale = |v: f64, lo: f64, span: f64| {
            if span > 0.0 {
                (65535.0 * (v - lo) / span).floor() as u32
            } else {
                0
            }
        };
        let keys: Vec<u32> = boxes
            .iter()
            .map(|b| {
                let b = b.expect("indexed features have a geometry");
                let c = b.center();
                hilbert(scale(c.x, e.min().x, w), scale(c.y, e.min().y, h))
            })
            .collect();
        // Descending, as FlatGeobuf's reference writer.
        order.sort_by_key(|&i| std::cmp::Reverse(keys[i]));
        features = order.iter().map(|&i| features[i]).collect();
    }

    // Header.
    let mut header = Vec::new();
    if let Some(n) = name {
        header.push((0, Val::Obj(Fb::Str(n.to_owned()))));
    }
    if let Some(e) = extent {
        header.push((
            1,
            Val::Obj(doubles(&[e.min().x, e.min().y, e.max().x, e.max().y])),
        ));
    }
    header.push((2, Val::U8(header_type)));
    header.push((3, Val::U8(u8::from(has_z))));
    header.push((4, Val::U8(u8::from(has_m))));
    if !columns.is_empty() {
        header.push((
            7,
            Val::Obj(Fb::Tables(
                columns
                    .iter()
                    .map(|(n, k)| {
                        Fb::Table(vec![
                            (0, Val::Obj(Fb::Str(n.clone()))),
                            (1, Val::U8(column_type(*k))),
                        ])
                    })
                    .collect(),
            )),
        ));
    }
    header.push((8, Val::U64(features.len() as u64)));
    let node_size = if index && !features.is_empty() {
        NODE_SIZE
    } else {
        0
    };
    header.push((9, Val::U16(node_size as u16)));
    if crs > 0 {
        header.push((
            10,
            Val::Obj(Fb::Table(vec![
                (0, Val::Obj(Fb::Str("EPSG".into()))),
                (1, Val::I32(crs as i32)),
            ])),
        ));
    }
    let mut out = FGB_MAGIC.to_vec();
    out.extend(FbWriter::finish(&Fb::Table(header)));

    // Features.
    let mut encoded: Vec<Vec<u8>> = Vec::new();
    for f in &features {
        let mut fields = Vec::new();
        if let Some(g) = &f.geom
            && !is_empty(&g.geometry)
        {
            fields.push((
                0,
                Val::Obj(fgb_geometry(&g.geometry, g.zm.as_deref(), (has_z, has_m))),
            ));
        }
        let mut props = Vec::new();
        for (k, v) in &f.properties {
            let Some(ci) = columns.iter().position(|(n, _)| n == k) else {
                continue;
            };
            props.extend((ci as u16).to_le_bytes());
            let Term::Literal(l) = v else {
                let s = text_of(v);
                props.extend((s.len() as u32).to_le_bytes());
                props.extend(s.as_bytes());
                continue;
            };
            match columns[ci].1 {
                Kind::Bool => props.push(u8::from(matches!(l.value(), "true" | "1"))),
                Kind::Long => props.extend(l.value().parse::<i64>().unwrap_or(0).to_le_bytes()),
                Kind::Double => props.extend(l.value().parse::<f64>().unwrap_or(0.0).to_le_bytes()),
                Kind::Binary => {
                    let b = from_hex(l.value()).unwrap_or_default();
                    props.extend((b.len() as u32).to_le_bytes());
                    props.extend(b);
                }
                Kind::String | Kind::DateTime => {
                    let s = l.value();
                    props.extend((s.len() as u32).to_le_bytes());
                    props.extend(s.as_bytes());
                }
            }
        }
        if !props.is_empty() {
            fields.push((1, Val::Obj(Fb::Scalars(props.clone(), 1, props.len()))));
        }
        encoded.push(FbWriter::finish(&Fb::Table(fields)));
    }

    // Index: leaves are the features in file order, their offset the byte offset of the
    // feature in the data section; a parent's offset is the index of its first child.
    if node_size > 0 {
        let bounds = level_bounds(features.len(), node_size);
        let total = bounds[0].end;
        let mut nodes: Vec<([f64; 4], u64)> = vec![([0.0; 4], 0); total];
        let mut offset = 0u64;
        for (i, f) in features.iter().enumerate() {
            let r = f
                .geom
                .as_ref()
                .and_then(|g| g.geometry.bounding_rect())
                .unwrap();
            nodes[bounds[0].start + i] = ([r.min().x, r.min().y, r.max().x, r.max().y], offset);
            offset += encoded[i].len() as u64;
        }
        for l in 0..bounds.len() - 1 {
            let mut child = bounds[l].start;
            let mut pos = bounds[l + 1].start;
            while child < bounds[l].end {
                let mut b = [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ];
                let first = child;
                for _ in 0..node_size {
                    if child >= bounds[l].end {
                        break;
                    }
                    let c = nodes[child].0;
                    b = [
                        b[0].min(c[0]),
                        b[1].min(c[1]),
                        b[2].max(c[2]),
                        b[3].max(c[3]),
                    ];
                    child += 1;
                }
                nodes[pos] = (b, first as u64);
                pos += 1;
            }
        }
        for (b, off) in &nodes {
            for v in b {
                out.extend(v.to_le_bytes());
            }
            out.extend(off.to_le_bytes());
        }
    }
    for e in encoded {
        out.extend(e);
    }
    out
}

/// A feature read from FlatGeobuf.
#[derive(Debug, Clone)]
pub struct DecodedFeature {
    pub geom: Option<Geom>,
    pub properties: Vec<(String, Term)>,
}

fn decode_geometry(
    r: FbReader<'_>,
    t: usize,
    header_type: u8,
    zm: &mut Vec<(f64, f64)>,
) -> Option<Geometry<f64>> {
    let kind = match r.field_u8(t, 6, 0) {
        0 => header_type,
        k => k,
    };
    let xy = r.doubles(t, 1);
    let z = r.doubles(t, 2);
    let m = r.doubles(t, 3);
    let coords: Vec<Coord<f64>> = xy
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[x, y]| Coord { x, y })
        .collect();
    let start = zm.len();
    for i in 0..coords.len() {
        zm.push((
            z.get(i).copied().unwrap_or(f64::NAN),
            m.get(i).copied().unwrap_or(f64::NAN),
        ));
    }
    let ends: Vec<usize> = match r.u32s(t, 0) {
        e if e.is_empty() => vec![coords.len()],
        e => e.into_iter().map(|v| v as usize).collect(),
    };
    let split = || -> Option<Vec<LineString<f64>>> {
        let mut from = 0;
        let mut out = Vec::new();
        for &e in &ends {
            out.push(LineString::new(coords.get(from..e)?.to_vec()));
            from = e;
        }
        Some(out)
    };
    let parts = |zm: &mut Vec<(f64, f64)>| -> Option<Vec<Geometry<f64>>> {
        r.tables(t, 7)
            .into_iter()
            .map(|p| decode_geometry(r, p, 0, zm))
            .collect()
    };
    let g = match kind {
        1 => Geometry::Point(Point(*coords.first()?)),
        2 => Geometry::LineString(LineString::new(coords.clone())),
        3 => {
            let mut rings = split()?.into_iter();
            Geometry::Polygon(Polygon::new(rings.next()?, rings.collect()))
        }
        4 => Geometry::MultiPoint(MultiPoint(coords.iter().map(|&c| Point(c)).collect())),
        5 => Geometry::MultiLineString(MultiLineString(split()?)),
        6 => {
            zm.truncate(start);
            let polys = parts(zm)?
                .into_iter()
                .map(|p| match p {
                    Geometry::Polygon(p) => Some(p),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Geometry::MultiPolygon(MultiPolygon(polys))
        }
        7 => {
            zm.truncate(start);
            Geometry::GeometryCollection(GeometryCollection(parts(zm)?))
        }
        _ => return None,
    };
    Some(g)
}

fn column_term(r: FbReader<'_>, kind: u8, pos: &mut usize) -> Option<Term> {
    let take = |n: usize, pos: &mut usize| -> Option<&[u8]> {
        let s = r.b.get(*pos..pos.checked_add(n)?)?;
        *pos += n;
        Some(s)
    };
    let int = |v: i128| -> Term { Literal::new_typed_literal(v.to_string(), xsd::INTEGER).into() };
    Some(match kind {
        0 => int(i128::from(take(1, pos)?[0] as i8)),
        1 => int(i128::from(take(1, pos)?[0])),
        2 => Literal::from(take(1, pos)?[0] != 0).into(),
        3 => int(i128::from(i16::from_le_bytes(
            take(2, pos)?.try_into().ok()?,
        ))),
        4 => int(i128::from(u16::from_le_bytes(
            take(2, pos)?.try_into().ok()?,
        ))),
        5 => int(i128::from(i32::from_le_bytes(
            take(4, pos)?.try_into().ok()?,
        ))),
        6 => int(i128::from(u32::from_le_bytes(
            take(4, pos)?.try_into().ok()?,
        ))),
        7 => int(i128::from(i64::from_le_bytes(
            take(8, pos)?.try_into().ok()?,
        ))),
        8 => int(i128::from(u64::from_le_bytes(
            take(8, pos)?.try_into().ok()?,
        ))),
        9 => Literal::from(f64::from(f32::from_le_bytes(
            take(4, pos)?.try_into().ok()?,
        )))
        .into(),
        10 => Literal::from(f64::from_le_bytes(take(8, pos)?.try_into().ok()?)).into(),
        11..=14 => {
            let n = u32::from_le_bytes(take(4, pos)?.try_into().ok()?) as usize;
            let bytes = take(n, pos)?;
            match kind {
                14 => hex_literal(bytes),
                13 => Literal::new_typed_literal(std::str::from_utf8(bytes).ok()?, xsd::DATE_TIME)
                    .into(),
                _ => Literal::new_simple_literal(std::str::from_utf8(bytes).ok()?).into(),
            }
        }
        _ => return None,
    })
}

fn columns(r: FbReader<'_>, table: usize, id: u16) -> Vec<(String, u8)> {
    r.tables(table, id)
        .into_iter()
        .filter_map(|c| Some((r.string(c, 0)?.to_owned(), r.field_u8(c, 1, 0))))
        .collect()
}

/// Reads a FlatGeobuf file: its features, in file order, with their properties. The CRS comes
/// from the header's EPSG code (CRS84 without one).
pub fn decode_flatgeobuf(bytes: &[u8]) -> Option<Vec<DecodedFeature>> {
    // The first three magic bytes, then the major version.
    (bytes.get(0..3)? == &FGB_MAGIC[0..3] && bytes.get(3)? == &3).then_some(())?;
    let mut pos = 8;
    let size = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
    let hr = FbReader {
        b: bytes.get(pos + 4..pos + 4 + size)?,
    };
    pos += 4 + size;
    let h = hr.root()?;
    let header_type = hr.field_u8(h, 2, 0);
    let has_z = hr.field_u8(h, 3, 0) != 0;
    let has_m = hr.field_u8(h, 4, 0) != 0;
    let header_columns = columns(hr, h, 7);
    let count = hr.field(h, 8).and_then(|p| hr.u64(p)).unwrap_or(0) as usize;
    let node_size = hr.field(h, 9).and_then(|p| hr.u16(p)).unwrap_or(16) as usize;
    let crs = match hr.table(h, 10) {
        Some(c) => {
            let org = hr.string(c, 0).unwrap_or("EPSG");
            let code = hr.field(c, 1).and_then(|p| hr.i32(p)).unwrap_or(0);
            if org.eq_ignore_ascii_case("EPSG") && code > 0 {
                crs_of_srid(i64::from(code))?
            } else {
                Crs::Crs84
            }
        }
        None => Crs::Crs84,
    };
    if node_size > 0 && count > 0 {
        let nodes = level_bounds(count, node_size)[0].end;
        pos = pos.checked_add(nodes.checked_mul(40)?)?;
    }
    let mut out = Vec::new();
    while pos < bytes.len() {
        let size = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
        let r = FbReader {
            b: bytes.get(pos + 4..pos.checked_add(4 + size)?)?,
        };
        pos += 4 + size;
        let f = r.root()?;
        let feature_columns = columns(r, f, 2);
        let cols = if feature_columns.is_empty() {
            &header_columns
        } else {
            &feature_columns
        };
        let geom = match r.table(f, 0) {
            Some(g) => {
                let mut zm = Vec::new();
                let geometry = decode_geometry(r, g, header_type, &mut zm)?;
                let zm = (has_z || has_m)
                    .then(|| Arc::new(Zm::from_coords(&geometry, has_z, has_m, zm)));
                Some(Geom {
                    geometry,
                    crs: crs.clone(),
                    zm,
                    surface: None,
                    prepared: Default::default(),
                })
            }
            None => None,
        };
        let mut properties = Vec::new();
        if let Some((start, n)) = r.vector(f, 1) {
            let mut p = start;
            let end = start.checked_add(n)?;
            while p < end {
                let ci = u16::from_le_bytes(r.bytes(p)?) as usize;
                p += 2;
                let (name, kind) = cols.get(ci)?;
                properties.push((name.clone(), column_term(r, *kind, &mut p)?));
            }
        }
        out.push(DecodedFeature { geom, properties });
    }
    Some(out)
}

// --- Geobuf ------------------------------------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn pb_key(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn pb_varint(out: &mut Vec<u8>, field: u32, v: u64) {
    pb_key(out, field, 0);
    varint(out, v);
}

fn pb_bytes(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    pb_key(out, field, 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn pb_packed(out: &mut Vec<u8>, field: u32, values: impl IntoIterator<Item = u64>) {
    let mut data = Vec::new();
    for v in values {
        varint(&mut data, v);
    }
    if !data.is_empty() {
        pb_bytes(out, field, &data);
    }
}

fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

struct GeobufWriter {
    dim: usize,
    e: f64,
}

impl GeobufWriter {
    fn point(&self, c: Coord<f64>, zm: Option<&Zm>) -> Vec<f64> {
        let mut v = vec![c.x, c.y];
        if self.dim == 3 {
            let z = zm.and_then(|zm| zm.at(c)).map_or(f64::NAN, |v| v.0);
            v.push(if z.is_nan() { 0.0 } else { z });
        }
        v
    }

    /// Coordinates of a line, delta-encoded from its first point (the last point of a ring
    /// left out).
    fn line(&self, coords: &[Coord<f64>], closed: bool, zm: Option<&Zm>, out: &mut Vec<i64>) {
        let n = coords.len().saturating_sub(usize::from(closed));
        let mut sum = vec![0i64; self.dim];
        for &c in &coords[..n] {
            for (j, v) in self.point(c, zm).into_iter().enumerate() {
                let q = (v * self.e).round() as i64 - sum[j];
                out.push(q);
                sum[j] += q;
            }
        }
    }

    fn geometry(&self, g: &Geometry<f64>, zm: Option<&Zm>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut coords: Vec<i64> = Vec::new();
        let mut lengths: Vec<u64> = Vec::new();
        let kind = match g {
            Geometry::Point(p) => {
                coords.extend(
                    self.point(p.0, zm)
                        .into_iter()
                        .map(|v| (v * self.e).round() as i64),
                );
                0
            }
            Geometry::MultiPoint(m) => {
                let c: Vec<Coord<f64>> = m.0.iter().map(|p| p.0).collect();
                self.line(&c, false, zm, &mut coords);
                1
            }
            Geometry::Line(_) | Geometry::LineString(_) => {
                let c: Vec<Coord<f64>> = g.coords_iter().collect();
                self.line(&c, false, zm, &mut coords);
                2
            }
            Geometry::MultiLineString(m) => {
                if m.0.len() != 1 {
                    lengths.extend(m.0.iter().map(|l| l.0.len() as u64));
                }
                m.0.iter()
                    .for_each(|l| self.line(&l.0, false, zm, &mut coords));
                3
            }
            Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => {
                let p = match g {
                    Geometry::Polygon(p) => p.clone(),
                    Geometry::Rect(r) => r.to_polygon(),
                    Geometry::Triangle(t) => t.to_polygon(),
                    _ => unreachable!(),
                };
                let rings: Vec<&LineString<f64>> =
                    std::iter::once(p.exterior()).chain(p.interiors()).collect();
                if rings.len() != 1 {
                    lengths.extend(rings.iter().map(|r| r.0.len() as u64 - 1));
                }
                rings
                    .iter()
                    .for_each(|r| self.line(&r.0, true, zm, &mut coords));
                4
            }
            Geometry::MultiPolygon(m) => {
                if m.0.len() != 1 || !m.0[0].interiors().is_empty() {
                    lengths.push(m.0.len() as u64);
                    for p in &m.0 {
                        lengths.push(1 + p.interiors().len() as u64);
                        lengths.push(p.exterior().0.len() as u64 - 1);
                        lengths.extend(p.interiors().iter().map(|r| r.0.len() as u64 - 1));
                    }
                }
                for p in &m.0 {
                    std::iter::once(p.exterior())
                        .chain(p.interiors())
                        .for_each(|r| self.line(&r.0, true, zm, &mut coords));
                }
                5
            }
            Geometry::GeometryCollection(_) => 6,
        };
        pb_varint(&mut out, 1, kind);
        pb_packed(&mut out, 2, lengths);
        pb_packed(&mut out, 3, coords.into_iter().map(zigzag));
        if let Geometry::GeometryCollection(c) = g {
            for part in &c.0 {
                pb_bytes(&mut out, 4, &self.geometry(part, zm));
            }
        }
        out
    }
}

/// A geobuf `Value` message.
fn geobuf_value(t: &Term) -> Vec<u8> {
    let mut out = Vec::new();
    match (kind_of(t), t) {
        (Kind::Bool, Term::Literal(l)) => {
            pb_varint(&mut out, 5, u64::from(matches!(l.value(), "true" | "1")))
        }
        (Kind::Long, Term::Literal(l)) => {
            let v: i64 = l.value().parse().unwrap_or(0);
            if v >= 0 {
                pb_varint(&mut out, 3, v as u64);
            } else {
                pb_varint(&mut out, 4, v.unsigned_abs());
            }
        }
        (Kind::Double, Term::Literal(l)) => {
            let v: f64 = l.value().parse().unwrap_or(0.0);
            // As the reference encoder: whole numbers are integers.
            if v.fract() == 0.0 && v.abs() < 9.0e15 {
                if v >= 0.0 {
                    pb_varint(&mut out, 3, v as u64);
                } else {
                    pb_varint(&mut out, 4, (-v) as u64);
                }
            } else {
                pb_key(&mut out, 2, 1);
                out.extend(v.to_le_bytes());
            }
        }
        (_, Term::Literal(l)) if l.datatype().as_str().ends_with("rdf-syntax-ns#JSON") => {
            pb_bytes(&mut out, 6, l.value().as_bytes())
        }
        _ => pb_bytes(&mut out, 1, text_of(t).as_bytes()),
    }
    out
}

/// A Geobuf `Data` message holding the features as a feature collection, as PostGIS's
/// `ST_AsGeobuf`. Coordinates keep up to six decimals (fewer when the data needs fewer);
/// Z is kept when any geometry has it, M is dropped.
pub fn encode_geobuf(features: &[Feature]) -> Vec<u8> {
    let mut keys: Vec<String> = Vec::new();
    let mut key_index: HashMap<String, usize> = HashMap::new();
    let mut dim = 2;
    let mut e = 1.0f64;
    for f in features {
        for (k, _) in &f.properties {
            key_index.entry(k.clone()).or_insert_with(|| {
                keys.push(k.clone());
                keys.len() - 1
            });
        }
        let Some(g) = &f.geom else { continue };
        let zm = g.zm.as_deref().filter(|z| z.has_z);
        if zm.is_some() {
            dim = 3;
        }
        for c in g.geometry.coords_iter() {
            let mut vals = vec![c.x, c.y];
            if let Some((z, _)) = zm.and_then(|z| z.at(c))
                && !z.is_nan()
            {
                vals.push(z);
            }
            for v in vals {
                while (v * e).round() / e != v && e < 1e6 {
                    e *= 10.0;
                }
            }
        }
    }
    let e = e.min(1e6);
    let precision = e.log10().round() as u64;
    let w = GeobufWriter { dim, e };
    let mut collection = Vec::new();
    for f in features {
        let mut feature = Vec::new();
        if let Some(g) = f.geom.as_ref().filter(|g| !is_empty(&g.geometry)) {
            pb_bytes(&mut feature, 1, &w.geometry(&g.geometry, g.zm.as_deref()));
        }
        let mut indexes = Vec::new();
        for (i, (k, v)) in f.properties.iter().enumerate() {
            pb_bytes(&mut feature, 13, &geobuf_value(v));
            indexes.push(key_index[k] as u64);
            indexes.push(i as u64);
        }
        pb_packed(&mut feature, 14, indexes);
        pb_bytes(&mut collection, 1, &feature);
    }
    let mut data = Vec::new();
    for k in &keys {
        pb_bytes(&mut data, 1, k.as_bytes());
    }
    if dim != 2 {
        pb_varint(&mut data, 2, dim as u64);
    }
    if precision != 6 {
        pb_varint(&mut data, 3, precision);
    }
    pb_bytes(&mut data, 4, &collection);
    data
}

// --- Aggregates --------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum FeatureFormat {
    FlatGeobuf,
    Geobuf,
}

/// `st:ST_AsFlatGeobuf(?geom)` and `st:ST_AsGeobuf(?geom)`: the geometries of a group as
/// features without properties. The `st` SERVICE forms add the other columns as properties.
pub(crate) struct FeatureAccumulator {
    format: FeatureFormat,
    features: Vec<Feature>,
    failed: bool,
}

impl FeatureAccumulator {
    pub(crate) fn flatgeobuf() -> Box<dyn AggregateFunctionAccumulator + Send + Sync> {
        Self::boxed(FeatureFormat::FlatGeobuf)
    }

    pub(crate) fn geobuf() -> Box<dyn AggregateFunctionAccumulator + Send + Sync> {
        Self::boxed(FeatureFormat::Geobuf)
    }

    fn boxed(format: FeatureFormat) -> Box<dyn AggregateFunctionAccumulator + Send + Sync> {
        Box::new(Self {
            format,
            features: Vec::new(),
            failed: false,
        })
    }
}

impl AggregateFunctionAccumulator for FeatureAccumulator {
    fn accumulate(&mut self, element: Term) {
        match geom(&element) {
            Some(g) => self.features.push(Feature {
                geom: Some(g),
                properties: Vec::new(),
            }),
            None => self.failed = true,
        }
    }

    fn finish(&mut self) -> Option<Term> {
        if self.failed {
            return None;
        }
        Some(hex_literal(&match self.format {
            FeatureFormat::FlatGeobuf => encode_flatgeobuf(&self.features, false, None),
            FeatureFormat::Geobuf => encode_geobuf(&self.features),
        }))
    }
}

// --- MARC21 ------------------------------------------------------------------------------------

/// A coordinate format of MARC21 field 034: hemisphere letter or sign, then degrees and
/// optionally minutes and seconds, the last with `decimals` decimals.
struct MarcFormat {
    hemisphere: bool,
    /// 0: degrees, 1: degrees and minutes, 2: degrees, minutes and seconds.
    parts: usize,
    decimals: usize,
    separator: char,
}

fn marc_format(f: &str) -> Option<MarcFormat> {
    let (hemisphere, rest) = match f.strip_prefix('h') {
        Some(r) => (true, r),
        None => (false, f),
    };
    let (int, frac, separator) = match rest.find(['.', ',']) {
        Some(i) => (&rest[..i], &rest[i + 1..], rest.as_bytes()[i] as char),
        None => (rest, "", '.'),
    };
    let parts = match int {
        "ddd" => 0,
        "dddmm" => 1,
        "dddmmss" => 2,
        _ => return None,
    };
    let unit = ['d', 'm', 's'][parts];
    let separated = rest.len() > int.len();
    (frac.chars().all(|c| c == unit) && separated != frac.is_empty()).then_some(())?;
    Some(MarcFormat {
        hemisphere,
        parts,
        decimals: frac.len(),
        separator,
    })
}

fn marc_coordinate(v: f64, lon: bool, f: &MarcFormat) -> String {
    let negative = v < 0.0;
    let a = v.abs();
    let scale = 10f64.powi(f.decimals as i32);
    let decimals = |x: f64, width: usize| {
        let s = format!(
            "{:0w$.p$}",
            x,
            w = width + if f.decimals > 0 { f.decimals + 1 } else { 0 },
            p = f.decimals
        );
        s.replace('.', &f.separator.to_string())
    };
    let body = match f.parts {
        0 => decimals((a * scale).round() / scale, 3),
        1 => {
            let total = (a * 60.0 * scale).round() / scale;
            let deg = (total / 60.0).floor();
            let min = total - deg * 60.0;
            format!("{:03}{}", deg as u64, decimals(min, 2))
        }
        _ => {
            let total = (a * 3600.0 * scale).round() / scale;
            let deg = (total / 3600.0).floor();
            let min = ((total - deg * 3600.0) / 60.0).floor();
            let sec = total - deg * 3600.0 - min * 60.0;
            format!("{:03}{:02}{}", deg as u64, min as u64, decimals(sec, 2))
        }
    };
    let prefix = match (f.hemisphere, lon, negative) {
        (true, true, false) => 'E',
        (true, true, true) => 'W',
        (true, false, false) => 'N',
        (true, false, true) => 'S',
        (false, _, false) => '+',
        (false, _, true) => '-',
    };
    format!("{prefix}{body}")
}

/// PostGIS `ST_AsMARC21`: a MARC21/XML record with one field 034 per part of the geometry,
/// holding its bounding box (`$d` west, `$e` east, `$f` north, `$g` south).
pub(crate) fn as_marc21(g: &Geom, format: &str) -> Option<String> {
    let f = marc_format(format)?;
    let geometry = if g.crs.is_geographic() {
        g.geometry.clone()
    } else {
        crate::crs::transform(&g.geometry, &g.crs, &Crs::Crs84)?
    };
    let parts: Vec<Geometry<f64>> = match geometry {
        Geometry::MultiPoint(m) => m.0.into_iter().map(Geometry::Point).collect(),
        Geometry::MultiLineString(m) => m.0.into_iter().map(Geometry::LineString).collect(),
        Geometry::MultiPolygon(m) => m.0.into_iter().map(Geometry::Polygon).collect(),
        Geometry::GeometryCollection(c) => c.0,
        other => vec![other],
    };
    let mut s = String::from(r#"<record xmlns="http://www.loc.gov/MARC21/slim">"#);
    for p in parts {
        let Some(r) = p.bounding_rect() else { continue };
        s.push_str(r#"<datafield tag="034" ind1="1" ind2=" "><subfield code="a">a</subfield>"#);
        for (code, v, lon) in [
            ('d', r.min().x, true),
            ('e', r.max().x, true),
            ('f', r.max().y, false),
            ('g', r.min().y, false),
        ] {
            s.push_str(&format!(
                r#"<subfield code="{code}">{}</subfield>"#,
                marc_coordinate(v, lon, &f)
            ));
        }
        s.push_str("</datafield>");
    }
    s.push_str("</record>");
    Some(s)
}

/// A coordinate of field 034 in any of its forms: `hdddmmss`, `hdddmm.mmmm`, `hddd.dddddd`,
/// `dddmmss.sss` and the like, with a hemisphere letter, a sign or neither, and `.` or `,` as
/// the decimal sign.
fn parse_marc_coordinate(s: &str) -> Option<f64> {
    let s = s.trim();
    let (sign, rest) = match s.chars().next()? {
        'N' | 'E' | 'n' | 'e' | '+' => (1.0, &s[1..]),
        'S' | 'W' | 's' | 'w' | '-' => (-1.0, &s[1..]),
        _ => (1.0, s),
    };
    let rest = rest.replace(',', ".");
    let (int, frac) = rest.split_once('.').unwrap_or((&rest, ""));
    (int.bytes().all(|b| b.is_ascii_digit()) && frac.bytes().all(|b| b.is_ascii_digit()))
        .then_some(())?;
    let frac_value: f64 = if frac.is_empty() {
        0.0
    } else {
        format!("0.{frac}").parse().ok()?
    };
    let n = |r: std::ops::Range<usize>| int.get(r)?.parse::<f64>().ok();
    let v = match int.len() {
        1..=3 => n(0..int.len())? + frac_value,
        5 => n(0..3)? + (n(3..5)? + frac_value) / 60.0,
        7 => n(0..3)? + n(3..5)? / 60.0 + (n(5..7)? + frac_value) / 3600.0,
        _ => return None,
    };
    Some(sign * v)
}

/// PostGIS `ST_GeomFromMARC21`: the geometry of the 034 fields of a MARC21/XML record. A field
/// whose west and east, and north and south, coordinates are equal is a point; any other is a
/// box. Several fields make a multi-point, a multi-polygon or a collection.
pub(crate) fn from_marc21(xml: &str) -> Option<Geometry<f64>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut fields: Vec<HashMap<u8, String>> = Vec::new();
    let mut in_034 = false;
    let mut code: Option<u8> = None;
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => {
                let name = e.local_name();
                let attr = |key: &[u8]| -> Option<String> {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == key)
                        .and_then(|a| crate::gml::attribute_value(&a))
                };
                match name.as_ref() {
                    b"datafield" => {
                        in_034 = attr(b"tag").as_deref() == Some("034");
                        if in_034 {
                            fields.push(HashMap::new());
                        }
                    }
                    b"subfield" if in_034 => {
                        code = attr(b"code").and_then(|c| c.bytes().next());
                    }
                    _ => {}
                }
            }
            Event::Text(t) => {
                if let (true, Some(c)) = (in_034, code) {
                    let v = t.decode().ok()?;
                    fields.last_mut()?.entry(c).or_default().push_str(&v);
                }
            }
            Event::GeneralRef(r) => {
                if let (true, Some(c)) = (in_034, code) {
                    let v = crate::gml::reference_text(&r)?;
                    fields.last_mut()?.entry(c).or_default().push_str(&v);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"subfield" => code = None,
                b"datafield" => in_034 = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    let mut parts: Vec<Geometry<f64>> = Vec::new();
    for f in &fields {
        let get = |c: u8| f.get(&c).and_then(|v| parse_marc_coordinate(v));
        let (Some(w), Some(e), Some(n), Some(s)) = (get(b'd'), get(b'e'), get(b'f'), get(b'g'))
        else {
            continue;
        };
        if w == e && n == s {
            parts.push(Geometry::Point(Point::new(w, n)));
        } else {
            parts.push(crate::postgis::envelope(
                Coord {
                    x: w.min(e),
                    y: s.min(n),
                },
                Coord {
                    x: w.max(e),
                    y: s.max(n),
                },
            ));
        }
    }
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(crate::postgis::collect(parts)),
    }
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    add(
        ST,
        "ST_AsMARC21",
        func(|args| {
            let g = geom(args.first()?)?;
            let format = match args.get(1) {
                Some(Term::Literal(l)) => l.value().to_owned(),
                Some(_) => return None,
                None => "hdddmmss".to_owned(),
            };
            string(as_marc21(&g, &format)?)
        }),
    );
    add(
        ST,
        "ST_GeomFromMARC21",
        func(|args| {
            let [Term::Literal(l)] = args else {
                return None;
            };
            Some(geom_literal(&Geom::new(
                from_marc21(l.value())?,
                Crs::Crs84,
            )))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkt(s: &str) -> Arc<Geom> {
        crate::literal::parse_wkt_literal(
            &Literal::new_typed_literal(s, crate::literal::WKT_LITERAL).into(),
        )
        .unwrap()
    }

    #[test]
    fn varints() {
        let mut v = Vec::new();
        varint(&mut v, 300);
        assert_eq!(v, [0xAC, 0x02]);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
    }

    #[test]
    fn geobuf_point() {
        let f = Feature {
            geom: Some(wkt("POINT(1 2)")),
            properties: Vec::new(),
        };
        // precision 0, then a collection with a feature with a point (type 0, coords 1 2).
        assert_eq!(to_hex(&encode_geobuf(&[f])), "1800220A0A080A0608001A020204");
    }

    #[test]
    fn flatgeobuf_round_trip() {
        let features = vec![
            Feature {
                geom: Some(wkt("POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,2 1,2 2,1 1))")),
                properties: vec![
                    ("name".into(), Literal::new_simple_literal("a").into()),
                    ("n".into(), Literal::from(3i64).into()),
                ],
            },
            Feature {
                geom: Some(wkt("MULTIPOLYGON(((5 5,6 5,6 6,5 5)),((7 7,8 7,8 8,7 7)))")),
                properties: vec![("n".into(), Literal::from(4i64).into())],
            },
        ];
        for index in [false, true] {
            let bytes = encode_flatgeobuf(&features, index, None);
            let back = decode_flatgeobuf(&bytes).unwrap();
            assert_eq!(back.len(), 2);
            let mut geoms: Vec<Geometry<f64>> = back
                .iter()
                .map(|f| f.geom.clone().unwrap().geometry)
                .collect();
            if index {
                geoms.sort_by(|a, b| {
                    a.bounding_rect()
                        .unwrap()
                        .min()
                        .x
                        .total_cmp(&b.bounding_rect().unwrap().min().x)
                });
            }
            assert_eq!(geoms[0], features[0].geom.as_ref().unwrap().geometry);
            assert_eq!(geoms[1], features[1].geom.as_ref().unwrap().geometry);
        }
    }

    #[test]
    fn level_bounds_match_flatgeobuf() {
        assert_eq!(level_bounds(1, 16), vec![1..2, 0..1]);
        assert_eq!(level_bounds(20, 16), vec![3..23, 1..3, 0..1]);
    }

    #[test]
    fn marc21() {
        let g = wkt("POINT(-4.504289 54.253312)");
        let x = as_marc21(&g, "hdddmmss.sss").unwrap();
        assert!(
            x.contains(r#"<subfield code="d">W0043015.440</subfield>"#),
            "{x}"
        );
        assert!(
            x.contains(r#"<subfield code="f">N0541511.923</subfield>"#),
            "{x}"
        );
        assert_eq!(
            parse_marc_coordinate("W0043015.440").unwrap(),
            -(4.0 + 30.0 / 60.0 + 15.44 / 3600.0)
        );
        assert_eq!(parse_marc_coordinate("+004,5").unwrap(), 4.5);
        assert_eq!(parse_marc_coordinate("S01230").unwrap(), -12.5);
        assert!(marc_format("hdd").is_none());
        assert_eq!(
            marc_coordinate(4.5, true, &marc_format("ddd.dd").unwrap()),
            "+004.50"
        );
        assert_eq!(
            marc_coordinate(-4.5, false, &marc_format("hdddmm,mm").unwrap()),
            "S00430,00"
        );
    }
}
