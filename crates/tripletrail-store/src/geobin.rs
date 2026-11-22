//! A binary form of parsed geometries, saved with a checkpoint so a restarted store does not
//! parse geometry literals again.
//!
//! The text of a literal stays in the dictionary as the source of truth (results show it as it
//! was loaded); this is a cache of its parsed form. A store that keeps WKT literals in binary
//! form (#370) keeps the record in the literal's dictionary key instead, when [`crate::wkt`]
//! writes its text back exactly from it, packed ([`PACKED`], #152) when its coordinates are
//! fixed-point numbers. Each geometry is one record of
//! little-endian values: the CRS, then the geometry as a type tag followed by counts and flat
//! `f64` coordinate pairs (`x y x y ...`), parts and rings nested in order. Decoding reads the
//! coordinates straight into `geo` structures, with none of the number parsing WKT needs.
//!
//! Only plain 2-D geometries are saved; literals with Z or M ordinates, or 3-D surfaces, are
//! parsed from their text as before.
//!
//! The file of a checkpoint (`geoms.bin`):
//!
//! ```text
//! magic "OXGSGEO1", u64 count, u64 bytes
//! count x u32 ordinal (ascending), count + 1 x u64 record offsets, bytes of records
//! ```
//!
//! Files of format 3 also hold the bounding box of each geometry whose record is longer than
//! [`BOX_MIN`] bytes (see `persist.rs`), compact dictionary keys' included: a spatial join
//! reads the boxes of every geometry of its sides, and the records of the polygons of a
//! continent are tens of gigabytes where their boxes are one (#754).

use geo::{
    Coord, Geometry, GeometryCollection, Line, LineString, MultiLineString, MultiPoint,
    MultiPolygon, Point, Polygon, Rect, Triangle,
};
use tripletrail_geo::{Crs, Geom};

pub(crate) const POINT: u8 = 1;
pub(crate) const LINE: u8 = 2;
pub(crate) const LINE_STRING: u8 = 3;
pub(crate) const POLYGON: u8 = 4;
pub(crate) const MULTI_POINT: u8 = 5;
pub(crate) const MULTI_LINE_STRING: u8 = 6;
pub(crate) const MULTI_POLYGON: u8 = 7;
pub(crate) const COLLECTION: u8 = 8;
pub(crate) const RECT: u8 = 9;
pub(crate) const TRIANGLE: u8 = 10;

pub(crate) const CRS84: u8 = 0;
pub(crate) const EPSG4326: u8 = 1;
pub(crate) const OTHER: u8 = 2;

/// Appends the record of `geom` to `out`; `false` (and nothing appended) for a geometry this
/// form does not hold.
pub(crate) fn encode(geom: &Geom, out: &mut Vec<u8>) -> bool {
    if geom.zm.is_some() || geom.surface.is_some() {
        return false;
    }
    match &geom.crs {
        Crs::Crs84 => out.push(CRS84),
        Crs::Epsg4326 => out.push(EPSG4326),
        Crs::Other(iri) => {
            out.push(OTHER);
            len(out, iri.len());
            out.extend_from_slice(iri.as_bytes());
        }
    }
    geometry(&geom.geometry, out);
    true
}

fn len(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}

fn coord(out: &mut Vec<u8>, c: Coord) {
    out.extend_from_slice(&c.x.to_le_bytes());
    out.extend_from_slice(&c.y.to_le_bytes());
}

fn coords(out: &mut Vec<u8>, l: &LineString) {
    len(out, l.0.len());
    for &c in &l.0 {
        coord(out, c);
    }
}

fn polygon(out: &mut Vec<u8>, p: &Polygon) {
    len(out, 1 + p.interiors().len());
    coords(out, p.exterior());
    for r in p.interiors() {
        coords(out, r);
    }
}

fn geometry(g: &Geometry, out: &mut Vec<u8>) {
    match g {
        Geometry::Point(p) => {
            out.push(POINT);
            coord(out, p.0);
        }
        Geometry::Line(l) => {
            out.push(LINE);
            coord(out, l.start);
            coord(out, l.end);
        }
        Geometry::LineString(l) => {
            out.push(LINE_STRING);
            coords(out, l);
        }
        Geometry::Polygon(p) => {
            out.push(POLYGON);
            polygon(out, p);
        }
        Geometry::MultiPoint(m) => {
            out.push(MULTI_POINT);
            len(out, m.0.len());
            for p in &m.0 {
                coord(out, p.0);
            }
        }
        Geometry::MultiLineString(m) => {
            out.push(MULTI_LINE_STRING);
            len(out, m.0.len());
            for l in &m.0 {
                coords(out, l);
            }
        }
        Geometry::MultiPolygon(m) => {
            out.push(MULTI_POLYGON);
            len(out, m.0.len());
            for p in &m.0 {
                polygon(out, p);
            }
        }
        Geometry::GeometryCollection(c) => {
            out.push(COLLECTION);
            len(out, c.0.len());
            for g in &c.0 {
                geometry(g, out);
            }
        }
        Geometry::Rect(r) => {
            out.push(RECT);
            coord(out, r.min());
            coord(out, r.max());
        }
        Geometry::Triangle(t) => {
            out.push(TRIANGLE);
            coord(out, t.v1());
            coord(out, t.v2());
            coord(out, t.v3());
        }
    }
}

/// The geometry of a record, or `None` if it is malformed.
pub(crate) fn decode(bytes: &[u8]) -> Option<Geom> {
    let bytes = plain(bytes)?;
    let mut r = Reader(&bytes);
    let crs = r.crs()?;
    let g = r.geometry(0)?;
    r.0.is_empty().then(|| Geom::new(g, crs))
}

/// The CRS and bounding box (`[min x, min y, max x, max y]`) of a record, read without
/// decoding the geometry: `Some((crs, None))` for an empty geometry, `None` if the record is
/// malformed. The box holds every coordinate (also those of polygon holes).
pub(crate) fn bbox(bytes: &[u8]) -> Option<(Crs, Option<[f64; 4]>)> {
    let bytes = plain(bytes)?;
    let mut r = Reader(&bytes);
    let crs = r.crs()?;
    let mut b = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    r.extent(0, &mut b)?;
    if !r.0.is_empty() {
        return None;
    }
    Some((crs, (b[0] <= b[2] && b[1] <= b[3]).then_some(b)))
}

/// Records of at most this many bytes get no saved box: a point or a short line, whose record
/// is about as cheap to read as a box.
pub(crate) const BOX_MIN: usize = 64;

/// The box saved for `record` (see [`BOX_MIN`]): its CRS tag and its corners, all NaN for an
/// empty geometry. `None` for a short record, or one in another CRS (whose IRI the record
/// holds): those are read from the record.
pub(crate) fn saved_box(record: &[u8]) -> Option<(u8, [u64; 4])> {
    // A packed record takes about a third of the bytes of the plain one it is read as.
    let min = if record.first() == Some(&PACKED) {
        BOX_MIN / 3
    } else {
        BOX_MIN
    };
    if record.len() <= min {
        return None;
    }
    let (crs, b) = bbox(record)?;
    let tag = match crs {
        Crs::Crs84 => CRS84,
        Crs::Epsg4326 => EPSG4326,
        Crs::Other(_) => return None,
    };
    Some((tag, b.unwrap_or([f64::NAN; 4]).map(f64::to_bits)))
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn crs(&mut self) -> Option<Crs> {
        Some(match self.u8()? {
            CRS84 => Crs::Crs84,
            EPSG4326 => Crs::Epsg4326,
            OTHER => {
                let n = self.len()?;
                Crs::Other(String::from_utf8(self.take(n)?.to_vec()).ok()?)
            }
            _ => return None,
        })
    }

    /// Reads `n` coordinates into the box `b`.
    fn fold(&mut self, n: usize, b: &mut [f64; 4]) -> Option<()> {
        for _ in 0..n {
            let c = self.coord()?;
            b[0] = b[0].min(c.x);
            b[1] = b[1].min(c.y);
            b[2] = b[2].max(c.x);
            b[3] = b[3].max(c.y);
        }
        Some(())
    }

    fn ring(&mut self, b: &mut [f64; 4]) -> Option<()> {
        let n = self.count(16)?;
        self.fold(n, b)
    }

    fn rings(&mut self, b: &mut [f64; 4]) -> Option<()> {
        let n = self.count(4)?;
        if n == 0 {
            return None;
        }
        (0..n).try_for_each(|_| self.ring(b))
    }

    /// Skips a geometry, folding its coordinates into `b` (as [`Reader::geometry`] reads it).
    fn extent(&mut self, depth: usize, b: &mut [f64; 4]) -> Option<()> {
        if depth > 64 {
            return None;
        }
        match self.u8()? {
            POINT => self.fold(1, b),
            LINE | RECT => self.fold(2, b),
            TRIANGLE => self.fold(3, b),
            LINE_STRING | MULTI_POINT => self.ring(b),
            POLYGON => self.rings(b),
            MULTI_LINE_STRING => {
                let n = self.count(4)?;
                (0..n).try_for_each(|_| self.ring(b))
            }
            MULTI_POLYGON => {
                let n = self.count(4)?;
                (0..n).try_for_each(|_| self.rings(b))
            }
            COLLECTION => {
                let n = self.count(1)?;
                (0..n).try_for_each(|_| self.extent(depth + 1, b))
            }
            _ => None,
        }
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn len(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    /// A count of items at least `size` bytes each, checked against what is left.
    fn count(&mut self, size: usize) -> Option<usize> {
        let n = self.len()?;
        (n.checked_mul(size)? <= self.0.len()).then_some(n)
    }

    fn coord(&mut self) -> Option<Coord> {
        let (b, rest) = self.0.split_first_chunk::<16>()?;
        self.0 = rest;
        let x = f64::from_le_bytes(b[..8].try_into().ok()?);
        let y = f64::from_le_bytes(b[8..].try_into().ok()?);
        Some(Coord { x, y })
    }

    fn line_string(&mut self) -> Option<LineString> {
        let n = self.count(16)?;
        (0..n)
            .map(|_| self.coord())
            .collect::<Option<Vec<_>>>()
            .map(LineString)
    }

    fn polygon(&mut self) -> Option<Polygon> {
        let n = self.count(4)?;
        if n == 0 {
            return None;
        }
        let exterior = self.line_string()?;
        let interiors = (1..n)
            .map(|_| self.line_string())
            .collect::<Option<Vec<_>>>()?;
        Some(Polygon::new(exterior, interiors))
    }

    fn geometry(&mut self, depth: usize) -> Option<Geometry> {
        // Collections nest; a malformed file must not overflow the stack.
        if depth > 64 {
            return None;
        }
        Some(match self.u8()? {
            POINT => Geometry::Point(Point(self.coord()?)),
            LINE => Geometry::Line(Line::new(self.coord()?, self.coord()?)),
            LINE_STRING => Geometry::LineString(self.line_string()?),
            POLYGON => Geometry::Polygon(self.polygon()?),
            MULTI_POINT => {
                let n = self.count(16)?;
                Geometry::MultiPoint(MultiPoint(
                    (0..n)
                        .map(|_| self.coord().map(Point))
                        .collect::<Option<_>>()?,
                ))
            }
            MULTI_LINE_STRING => {
                let n = self.count(4)?;
                Geometry::MultiLineString(MultiLineString(
                    (0..n).map(|_| self.line_string()).collect::<Option<_>>()?,
                ))
            }
            MULTI_POLYGON => {
                let n = self.count(4)?;
                Geometry::MultiPolygon(MultiPolygon(
                    (0..n).map(|_| self.polygon()).collect::<Option<_>>()?,
                ))
            }
            COLLECTION => {
                let n = self.count(1)?;
                Geometry::GeometryCollection(GeometryCollection(
                    (0..n)
                        .map(|_| self.geometry(depth + 1))
                        .collect::<Option<_>>()?,
                ))
            }
            RECT => Geometry::Rect(Rect::new(self.coord()?, self.coord()?)),
            TRIANGLE => {
                Geometry::Triangle(Triangle::new(self.coord()?, self.coord()?, self.coord()?))
            }
            _ => return None,
        })
    }
}

/// The saved geometries of a checkpoint, by dictionary ordinal: one part per dictionary piece
/// (or one for all, from a checkpoint before #218), in ordinal order.
#[derive(Default)]
pub(crate) struct Saved {
    parts: Vec<SavedPart>,
    /// The parts with records, and those with boxes, in ordinal order.
    with_records: Vec<usize>,
    with_boxes: Vec<usize>,
}

/// The saved geometries of the ordinals in one range, as read from one file (mapped).
#[derive(Default)]
pub(crate) struct SavedPart {
    ordinals: crate::buf::Buf<u32>,
    offsets: crate::buf::Buf<u64>,
    bytes: crate::buf::Buf<u8>,
    /// The saved boxes (format 3): ordinals (ascending), corners as bits, CRS tags.
    boxes: SavedBoxes,
}

/// The boxes of a [`SavedPart`], by ordinal.
#[derive(Default)]
pub(crate) struct SavedBoxes {
    ordinals: crate::buf::Buf<u32>,
    corners: crate::buf::Buf<[u64; 4]>,
    tags: crate::buf::Buf<u8>,
}

impl SavedBoxes {
    /// `None` unless the three have one entry per ordinal and the ordinals ascend.
    pub(crate) fn new(
        ordinals: impl Into<crate::buf::Buf<u32>>,
        corners: impl Into<crate::buf::Buf<[u64; 4]>>,
        tags: impl Into<crate::buf::Buf<u8>>,
    ) -> Option<Self> {
        let (ordinals, corners, tags) = (ordinals.into(), corners.into(), tags.into());
        let valid =
            corners.len() == ordinals.len() && tags.len() == ordinals.len() && ordinals.is_sorted();
        valid.then_some(Self {
            ordinals,
            corners,
            tags,
        })
    }

    fn get(&self, ordinal: u32) -> Option<(Crs, Option<[f64; 4]>)> {
        let i = self.ordinals.binary_search(&ordinal).ok()?;
        let crs = match self.tags[i] {
            CRS84 => Crs::Crs84,
            EPSG4326 => Crs::Epsg4326,
            _ => return None,
        };
        let b = self.corners[i].map(f64::from_bits);
        Some((crs, (!b[0].is_nan()).then_some(b)))
    }
}

impl SavedPart {
    /// `None` unless the offsets start at 0, end at the last byte and never go back, and the
    /// ordinals ascend.
    pub(crate) fn new(
        ordinals: impl Into<crate::buf::Buf<u32>>,
        offsets: impl Into<crate::buf::Buf<u64>>,
        bytes: impl Into<crate::buf::Buf<u8>>,
    ) -> Option<Self> {
        let (ordinals, offsets, bytes) = (ordinals.into(), offsets.into(), bytes.into());
        let valid = offsets.len() == ordinals.len() + 1
            && offsets.first() == Some(&0)
            && offsets.last() == Some(&(bytes.len() as u64))
            && offsets.is_sorted()
            && ordinals.is_sorted();
        valid.then_some(Self {
            ordinals,
            offsets,
            bytes,
            boxes: SavedBoxes::default(),
        })
    }

    /// The part with its saved boxes.
    pub(crate) fn with_boxes(self, boxes: SavedBoxes) -> Self {
        Self { boxes, ..self }
    }

    fn record(&self, ordinal: u32) -> Option<&[u8]> {
        let i = self.ordinals.binary_search(&ordinal).ok()?;
        Some(&self.bytes[self.offsets[i] as usize..self.offsets[i + 1] as usize])
    }
}

impl Saved {
    /// Parts in ordinal order, the ordinals of each above those of the one before.
    pub(crate) fn from_parts(parts: Vec<SavedPart>) -> Option<Self> {
        let parts: Vec<SavedPart> = parts
            .into_iter()
            .filter(|p| !p.ordinals.is_empty() || !p.boxes.ordinals.is_empty())
            .collect();
        let with = |ordinals: fn(&SavedPart) -> &[u32]| -> Option<Vec<usize>> {
            let at: Vec<usize> = (0..parts.len())
                .filter(|&i| !ordinals(&parts[i]).is_empty())
                .collect();
            let ordered = at
                .windows(2)
                .all(|w| ordinals(&parts[w[0]]).last() < ordinals(&parts[w[1]]).first());
            ordered.then_some(at)
        };
        let with_records = with(|p| &p.ordinals)?;
        let with_boxes = with(|p| &p.boxes.ordinals)?;
        Some(Self {
            parts,
            with_records,
            with_boxes,
        })
    }

    /// The saved geometry of `ordinal`, decoded; `None` if none was saved.
    pub(crate) fn get(&self, ordinal: u32) -> Option<Geom> {
        decode(self.record(ordinal)?)
    }

    pub(crate) fn record(&self, ordinal: u32) -> Option<&[u8]> {
        // The first part whose last ordinal is not below `ordinal`.
        let parts = &self.with_records;
        let i = parts.partition_point(|&p| self.parts[p].ordinals.last() < Some(&ordinal));
        self.parts[*parts.get(i)?].record(ordinal)
    }

    /// The saved box of `ordinal` (CRS and corners, `None` for an empty geometry), if one
    /// was saved.
    pub(crate) fn bbox(&self, ordinal: u32) -> Option<(Crs, Option<[f64; 4]>)> {
        let parts = &self.with_boxes;
        let i = parts.partition_point(|&p| self.parts[p].boxes.ordinals.last() < Some(&ordinal));
        self.parts[*parts.get(i)?].boxes.get(ordinal)
    }
}

/// The first byte of a packed record (#152): a record whose coordinates all have at most
/// [`MAX_PACKED_DECIMALS`] decimals, kept as fixed-point integers, each the difference from the
/// one before (zigzag LEB128 varints, as in TWKB and OSM PBF), with varint counts. A WKT literal
/// written with 7 decimals (OpenStreetMap's) takes 2 or 3 bytes per ordinate instead of 8.
/// [`unpack`] gives back the plain record bit for bit. A plain record never starts with it (its
/// first byte is a CRS tag).
pub(crate) const PACKED: u8 = 0x80;

/// The most decimals a packed record holds.
const MAX_PACKED_DECIMALS: u8 = 9;

const POWERS: [f64; 10] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9];

/// The plain record of `record`: itself when it is not packed, `None` when it is malformed.
pub(crate) fn plain(record: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    if record.first() == Some(&PACKED) {
        unpack(record).map(std::borrow::Cow::Owned)
    } else {
        Some(std::borrow::Cow::Borrowed(record))
    }
}

/// Appends the packed form of the plain `record` to `out` with `decimals` decimals, if every
/// coordinate is exactly such a number (and unpacks to the same bits): `false` (and `out`
/// unchanged) otherwise.
pub(crate) fn pack(record: &[u8], decimals: u8, out: &mut Vec<u8>) -> bool {
    if decimals > MAX_PACKED_DECIMALS {
        return false;
    }
    let at = out.len();
    out.push(PACKED);
    out.push(decimals);
    let mut t = Transcode {
        r: record,
        out,
        to_packed: true,
        scale: POWERS[decimals as usize],
        prev: [0, 0],
    };
    let ok = t.crs().is_some() && t.geometry(0).is_some() && t.r.is_empty();
    if !ok {
        out.truncate(at);
    }
    ok
}

/// Appends the packed form of `record` with the fewest decimals that hold it exactly, trying
/// `hint` first: `false` (and `out` unchanged) when none does.
pub(crate) fn pack_any(record: &[u8], hint: Option<u8>, out: &mut Vec<u8>) -> bool {
    if let Some(d) = hint
        && pack(record, d, out)
    {
        return true;
    }
    (0..=MAX_PACKED_DECIMALS).any(|d| Some(d) != hint && pack(record, d, out))
}

/// The plain record of a packed one, or `None` if it is malformed.
fn unpack(record: &[u8]) -> Option<Vec<u8>> {
    let (&decimals, rest) = record.get(1..)?.split_first()?;
    if decimals > MAX_PACKED_DECIMALS {
        return None;
    }
    let mut out = Vec::with_capacity(rest.len() * 4);
    let mut t = Transcode {
        r: rest,
        out: &mut out,
        to_packed: false,
        scale: POWERS[decimals as usize],
        prev: [0, 0],
    };
    t.crs()?;
    t.geometry(0)?;
    t.r.is_empty().then_some(out)
}

/// Reads a record in one form and writes it in the other.
struct Transcode<'a, 'o> {
    r: &'a [u8],
    out: &'o mut Vec<u8>,
    to_packed: bool,
    scale: f64,
    prev: [i64; 2],
}

impl Transcode<'_, '_> {
    fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.r.split_first()?;
        self.r = rest;
        Some(b)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn put_varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.out.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }

    /// A count, copied from one form to the other; a packed count is checked against what is
    /// left (each item takes at least `min` bytes), so a malformed one allocates nothing.
    fn count(&mut self, min: usize) -> Option<usize> {
        if self.to_packed {
            let n = u32::from_le_bytes(self.r.get(..4)?.try_into().ok()?) as usize;
            self.r = &self.r[4..];
            self.put_varint(n as u64);
            Some(n)
        } else {
            let n = usize::try_from(self.varint()?).ok()?;
            if n.checked_mul(min)? > self.r.len() {
                return None;
            }
            self.out
                .extend_from_slice(&u32::try_from(n).ok()?.to_le_bytes());
            Some(n)
        }
    }

    fn crs(&mut self) -> Option<()> {
        let tag = self.u8()?;
        self.out.push(tag);
        match tag {
            CRS84 | EPSG4326 => Some(()),
            OTHER => {
                let n = self.count(1)?;
                let (iri, rest) = self.r.split_at_checked(n)?;
                self.out.extend_from_slice(iri);
                self.r = rest;
                Some(())
            }
            _ => None,
        }
    }

    fn coords(&mut self, n: usize) -> Option<()> {
        for _ in 0..n {
            for axis in 0..2 {
                if self.to_packed {
                    let (b, rest) = self.r.split_first_chunk::<8>()?;
                    self.r = rest;
                    let x = f64::from_le_bytes(*b);
                    let scaled = (x * self.scale).round();
                    if !scaled.is_finite() || scaled.abs() >= 9_007_199_254_740_992.0 {
                        return None;
                    }
                    let v = scaled as i64;
                    // Exactly the same bits back, so -0.0 and inexact numbers stay plain.
                    if (v as f64 / self.scale).to_bits() != x.to_bits() {
                        return None;
                    }
                    let d = v.wrapping_sub(self.prev[axis]);
                    self.prev[axis] = v;
                    self.put_varint(((d << 1) ^ (d >> 63)) as u64);
                } else {
                    let z = self.varint()?;
                    let d = ((z >> 1) as i64) ^ -((z & 1) as i64);
                    let v = self.prev[axis].wrapping_add(d);
                    self.prev[axis] = v;
                    if v.unsigned_abs() >= 1 << 53 {
                        return None;
                    }
                    self.out
                        .extend_from_slice(&(v as f64 / self.scale).to_le_bytes());
                }
            }
        }
        Some(())
    }

    fn ring(&mut self) -> Option<()> {
        let n = self.count(2)?;
        self.coords(n)
    }

    fn rings(&mut self) -> Option<()> {
        let n = self.count(1)?;
        (0..n).try_for_each(|_| self.ring())
    }

    fn geometry(&mut self, depth: usize) -> Option<()> {
        if depth > 64 {
            return None;
        }
        let tag = self.u8()?;
        self.out.push(tag);
        match tag {
            POINT => self.coords(1),
            LINE | RECT => self.coords(2),
            TRIANGLE => self.coords(3),
            LINE_STRING | MULTI_POINT => self.ring(),
            POLYGON => self.rings(),
            MULTI_LINE_STRING => {
                let n = self.count(1)?;
                (0..n).try_for_each(|_| self.ring())
            }
            MULTI_POLYGON => {
                let n = self.count(1)?;
                (0..n).try_for_each(|_| self.rings())
            }
            COLLECTION => {
                let n = self.count(1)?;
                (0..n).try_for_each(|_| self.geometry(depth + 1))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{Literal, NamedNode, Term};
    use tripletrail_geo::parse_wkt_literal_uncached;

    fn parse(wkt: &str) -> Geom {
        let lit = Literal::new_typed_literal(
            wkt,
            NamedNode::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
        );
        (*parse_wkt_literal_uncached(&Term::Literal(lit)).unwrap()).clone()
    }

    /// Packed records (#152) unpack to the plain record bit for bit, are smaller for
    /// fixed-point coordinates, and are refused when a coordinate has more decimals.
    #[test]
    fn packed_records_unpack_exactly() {
        for (wkt, decimals) in [
            ("POINT(1.5 -2.25)", 2),
            (
                "LINESTRING(7.5240150 58.1965462, 7.5236656 58.1967575, 7.5226876 58.1973698)",
                7,
            ),
            (
                "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 1))",
                0,
            ),
            ("MULTIPOINT((0 0), (1 1))", 0),
            ("MULTILINESTRING((0 0, 1 1), (2 2, 3 3, 4 4))", 0),
            (
                "MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5)))",
                0,
            ),
            ("GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 3 3))", 0),
            ("GEOMETRYCOLLECTION EMPTY", 0),
            (
                "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(50.123456789 4)",
                9,
            ),
            (
                "<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(150000.25 170000)",
                2,
            ),
            ("POINT(-180 -90)", 0),
        ] {
            let mut plain_record = Vec::new();
            assert!(encode(&parse(wkt), &mut plain_record), "{wkt}");
            let mut packed = Vec::new();
            assert!(pack_any(&plain_record, None, &mut packed), "{wkt}");
            assert_eq!(packed[1], decimals, "{wkt}: the fewest decimals");
            assert_eq!(&*plain(&packed).unwrap(), &plain_record[..], "{wkt}");
            assert_eq!(
                decode(&packed).unwrap().geometry,
                parse(wkt).geometry,
                "{wkt}"
            );
            assert_eq!(bbox(&packed), bbox(&plain_record), "{wkt}");
            if plain_record.len() > 40 && !wkt.starts_with('<') {
                assert!(
                    packed.len() * 2 < plain_record.len(),
                    "{wkt}: {} bytes",
                    packed.len()
                );
            }
            // A hint that holds it is taken as it is.
            let mut hinted = Vec::new();
            assert!(pack_any(&plain_record, Some(9), &mut hinted));
            assert_eq!(&*plain(&hinted).unwrap(), &plain_record[..]);
        }
        // Not exactly a number of 9 decimals or fewer, or -0.0: stays plain.
        for wkt in [
            "POINT(0.1234567891 1)",
            "POINT(-0.0 1)",
            "POINT(1e300 1)",
            "POINT(35.335507039952923 25.1)",
        ] {
            let mut plain_record = Vec::new();
            assert!(encode(&parse(wkt), &mut plain_record), "{wkt}");
            let mut packed = vec![7];
            assert!(!pack_any(&plain_record, Some(7), &mut packed), "{wkt}");
            assert_eq!(packed, [7], "{wkt}: out unchanged");
        }
        // Malformed packed records are refused, not panicked on.
        let mut packed = Vec::new();
        let mut plain_record = Vec::new();
        assert!(encode(
            &parse("LINESTRING(0 0, 1 1, 2 2)"),
            &mut plain_record
        ));
        assert!(pack(&plain_record, 0, &mut packed));
        for cut in 0..packed.len() {
            assert!(plain(&packed[..cut]).is_none() || cut == 0, "cut at {cut}");
        }
        assert!(plain(&[PACKED, 0, CRS84, LINE_STRING, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_none());
        assert!(plain(&[PACKED, 10, CRS84, POINT, 0, 0]).is_none());
    }

    #[test]
    fn geometries_round_trip() {
        for wkt in [
            "POINT(1.5 -2.25)",
            "LINESTRING(0 0, 1 1, 2 0.5)",
            "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 1))",
            "MULTIPOINT((0 0), (1 1))",
            "MULTILINESTRING((0 0, 1 1), (2 2, 3 3, 4 4))",
            "MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5)))",
            "GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 3 3))",
            "GEOMETRYCOLLECTION EMPTY",
            "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(50 4)",
            "<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(150000 170000)",
        ] {
            let g = parse(wkt);
            let mut out = Vec::new();
            assert!(encode(&g, &mut out), "{wkt}");
            let back = decode(&out).unwrap();
            assert_eq!(back.geometry, g.geometry, "{wkt}");
            assert_eq!(back.crs, g.crs, "{wkt}");
            // The box read from the record is the geometry's (no holes stick out here).
            use geo::BoundingRect;
            let (crs, b) = bbox(&out).unwrap();
            assert_eq!(crs, g.crs, "{wkt}");
            let rect = g.geometry.bounding_rect();
            assert_eq!(
                b,
                rect.map(|r| [r.min().x, r.min().y, r.max().x, r.max().y]),
                "{wkt}"
            );
            // A cut record is rejected, not misread.
            assert!(decode(&out[..out.len() - 1]).is_none(), "{wkt}");
            assert!(bbox(&out[..out.len() - 1]).is_none(), "{wkt}");
        }
        // Z ordinates are not held: the literal is parsed from its text instead.
        assert!(!encode(&parse("POINT Z(1 2 3)"), &mut Vec::new()));
    }
}
