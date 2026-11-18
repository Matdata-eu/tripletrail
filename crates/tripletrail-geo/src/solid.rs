//! True 3-D geometries: polyhedral surfaces, TINs, triangles and solids, and 3-D results (a
//! vertical face, a vertical line) that the (x, y)-keyed Z of [`crate::literal::Zm`] cannot
//! hold.
//!
//! A [`Surface`] is a list of patches, each a list of closed 3-D rings (the exterior first),
//! plus a solid flag. It is read from and written to WKT (`POLYHEDRALSURFACE`, `TIN`,
//! `TRIANGLE`, and SFCGAL's `SOLID`, which is how a literal keeps PostGIS's solid flag) and
//! WKB. 2-D functions see its footprint (see [`Surface::footprint`]).

use geo::{
    Area, Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Orient, Point,
};

/// A 3-D point.
pub type P3 = [f64; 3];
/// A ring (closed: the last point repeats the first) or, for lines, a line.
pub type Ring3 = Vec<P3>;
/// A polygon: its exterior ring, then its holes. A point is a one-point ring; a line a ring.
pub type Patch = Vec<Ring3>;

/// What a [`Surface`] is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceKind {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    Triangle,
    PolyhedralSurface,
    Tin,
}

/// A geometry with true 3-D coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub kind: SurfaceKind,
    /// Polygons (exterior ring, then holes); for the point and line kinds, one point or line
    /// per patch.
    pub patches: Vec<Patch>,
    /// PostGIS's solid flag: the patches bound a volume.
    pub solid: bool,
    /// Whether the coordinates have Z (a 2-D `TIN` has Z = 0 and is written in 2-D).
    pub has_z: bool,
}

impl Surface {
    pub fn new(kind: SurfaceKind, patches: Vec<Patch>) -> Self {
        Self {
            kind,
            patches,
            solid: false,
            has_z: true,
        }
    }

    pub fn is_polygonal(&self) -> bool {
        !matches!(
            self.kind,
            SurfaceKind::Point
                | SurfaceKind::MultiPoint
                | SurfaceKind::LineString
                | SurfaceKind::MultiLineString
        )
    }

    /// The same geometry with every point mapped by `f`, in order.
    pub fn map(&self, mut f: impl FnMut(P3) -> P3) -> Self {
        Self {
            patches: self
                .patches
                .iter()
                .map(|p| {
                    p.iter()
                        .map(|r| r.iter().map(|&q| f(q)).collect())
                        .collect()
                })
                .collect(),
            ..self.clone()
        }
    }

    /// The 2-D form that 2-D functions see: for surfaces the union of the patches projected
    /// on the XY plane (vertical patches have no area and drop out; a surface that is all
    /// vertical gives the outlines of its patches), otherwise the projected points or lines.
    pub fn footprint(&self) -> Geometry<f64> {
        let xy =
            |r: &Ring3| LineString::new(r.iter().map(|p| Coord { x: p[0], y: p[1] }).collect());
        match self.kind {
            SurfaceKind::Point | SurfaceKind::MultiPoint => {
                let pts: Vec<Point<f64>> = self
                    .patches
                    .iter()
                    .flatten()
                    .flatten()
                    .map(|p| Point::new(p[0], p[1]))
                    .collect();
                if self.kind == SurfaceKind::Point && pts.len() == 1 {
                    Geometry::Point(pts[0])
                } else {
                    Geometry::MultiPoint(MultiPoint(pts))
                }
            }
            SurfaceKind::LineString | SurfaceKind::MultiLineString => {
                let lines: Vec<LineString<f64>> = self.patches.iter().flatten().map(xy).collect();
                if self.kind == SurfaceKind::LineString && lines.len() == 1 {
                    Geometry::LineString(lines.into_iter().next().unwrap())
                } else {
                    Geometry::MultiLineString(MultiLineString(lines))
                }
            }
            SurfaceKind::Polygon | SurfaceKind::MultiPolygon => {
                // Exactly the projected polygons, as for a polygon in the (x, y)-keyed model.
                let polys: Vec<geo::Polygon<f64>> = self
                    .patches
                    .iter()
                    .filter(|p| !p.is_empty())
                    .map(|p| geo::Polygon::new(xy(&p[0]), p[1..].iter().map(xy).collect()))
                    .collect();
                if self.kind == SurfaceKind::Polygon && polys.len() == 1 {
                    Geometry::Polygon(polys.into_iter().next().unwrap())
                } else {
                    Geometry::MultiPolygon(MultiPolygon(polys))
                }
            }
            _ => {
                let scale = self.scale();
                let polys: Vec<geo::Polygon<f64>> = self
                    .patches
                    .iter()
                    .filter(|p| !p.is_empty())
                    .map(|p| geo::Polygon::new(xy(&p[0]), p[1..].iter().map(xy).collect()))
                    .filter(|p| p.unsigned_area() > 1e-12 * scale * scale)
                    // Downward and upward faces alike: the union counts windings.
                    .map(|p| p.orient(geo::orient::Direction::Default))
                    .collect();
                if polys.is_empty() && !self.patches.is_empty() {
                    return Geometry::MultiLineString(MultiLineString(
                        self.patches
                            .iter()
                            .filter_map(|p| p.first())
                            .map(xy)
                            .collect(),
                    ));
                }
                match polys.len() {
                    0 => Geometry::MultiPolygon(MultiPolygon(Vec::new())),
                    1 if self.kind == SurfaceKind::Triangle => {
                        Geometry::Polygon(polys.into_iter().next().unwrap())
                    }
                    1 => Geometry::MultiPolygon(MultiPolygon(polys)),
                    _ => Geometry::MultiPolygon(geo::algorithm::unary_union(&polys)),
                }
            }
        }
    }

    /// The largest absolute ordinate, at least 1: the scale of the tolerances.
    pub fn scale(&self) -> f64 {
        self.patches
            .iter()
            .flatten()
            .flatten()
            .flat_map(|p| p.iter())
            .fold(1.0, |m, v| m.max(v.abs()))
    }

    // --- WKT --------------------------------------------------------------------------------

    /// The WKT. `postgis` writes a solid as PostGIS prints it (a `POLYHEDRALSURFACE`, without
    /// the flag); otherwise it is written as SFCGAL's `SOLID`, so that the flag survives.
    pub fn to_wkt(&self, postgis: bool) -> String {
        let z = if self.has_z { " Z" } else { "" };
        let (tag, depth) = match self.kind {
            SurfaceKind::Point => ("POINT", 0),
            SurfaceKind::LineString => ("LINESTRING", 1),
            SurfaceKind::Polygon => ("POLYGON", 2),
            SurfaceKind::MultiPoint => ("MULTIPOINT", 1),
            SurfaceKind::MultiLineString => ("MULTILINESTRING", 2),
            SurfaceKind::MultiPolygon => ("MULTIPOLYGON", 3),
            SurfaceKind::Triangle => ("TRIANGLE", 2),
            SurfaceKind::PolyhedralSurface => ("POLYHEDRALSURFACE", 3),
            SurfaceKind::Tin => ("TIN", 3),
        };
        let solid = self.solid && !postgis && !self.patches.is_empty();
        let tag = if solid { "SOLID" } else { tag };
        if self.patches.is_empty() {
            return format!("{tag}{z} EMPTY");
        }
        let mut out = format!("{tag}{z}");
        if solid {
            out.push('(');
        }
        let point = |out: &mut String, p: &P3| {
            let v = |x: f64| x + 0.0;
            if self.has_z {
                out.push_str(&format!("{} {} {}", v(p[0]), v(p[1]), v(p[2])));
            } else {
                out.push_str(&format!("{} {}", v(p[0]), v(p[1])));
            }
        };
        let ring = |out: &mut String, r: &Ring3| {
            out.push('(');
            for (i, p) in r.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                point(out, p);
            }
            out.push(')');
        };
        let patch = |out: &mut String, p: &Patch| {
            out.push('(');
            for (i, r) in p.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                ring(out, r);
            }
            out.push(')');
        };
        match depth {
            0 => {
                out.push('(');
                point(&mut out, &self.patches[0][0][0]);
                out.push(')');
            }
            1 if self.kind == SurfaceKind::MultiPoint => {
                out.push('(');
                for (i, p) in self.patches.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    ring(&mut out, &p[0]);
                }
                out.push(')');
            }
            1 => ring(&mut out, &self.patches[0][0]),
            2 if self.kind == SurfaceKind::MultiLineString => {
                out.push('(');
                for (i, p) in self.patches.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    ring(&mut out, &p[0]);
                }
                out.push(')');
            }
            2 => patch(&mut out, &self.patches[0]),
            _ => {
                out.push('(');
                for (i, p) in self.patches.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    patch(&mut out, p);
                }
                out.push(')');
            }
        }
        if solid {
            out.push(')');
        }
        out
    }

    // --- WKB --------------------------------------------------------------------------------

    /// Little-endian WKB (ISO type codes, or EWKB flags with `ewkb`). A solid is written as a
    /// polyhedral surface, as PostGIS does.
    pub fn to_wkb(&self, ewkb: bool, srid: Option<u32>) -> Vec<u8> {
        let mut out = Vec::new();
        let header = |out: &mut Vec<u8>, kind: u32, srid: Option<u32>| {
            out.push(1);
            let t = if ewkb {
                kind | if self.has_z { 0x8000_0000 } else { 0 }
                    | if srid.is_some() { 0x2000_0000 } else { 0 }
            } else {
                kind + if self.has_z { 1000 } else { 0 }
            };
            out.extend(t.to_le_bytes());
            if let Some(s) = srid {
                out.extend(s.to_le_bytes());
            }
        };
        let point = |out: &mut Vec<u8>, p: &P3| {
            out.extend(p[0].to_le_bytes());
            out.extend(p[1].to_le_bytes());
            if self.has_z {
                out.extend(p[2].to_le_bytes());
            }
        };
        let ring = |out: &mut Vec<u8>, r: &Ring3| {
            out.extend((r.len() as u32).to_le_bytes());
            r.iter().for_each(|p| point(out, p));
        };
        let polygon = |out: &mut Vec<u8>, kind: u32, p: &Patch| {
            header(out, kind, None);
            out.extend((p.len() as u32).to_le_bytes());
            p.iter().for_each(|r| ring(out, r));
        };
        let count = |out: &mut Vec<u8>, n: usize| out.extend((n as u32).to_le_bytes());
        match self.kind {
            SurfaceKind::Point => {
                header(&mut out, 1, srid);
                match self.patches.first() {
                    Some(p) => point(&mut out, &p[0][0]),
                    None => point(&mut out, &[f64::NAN; 3]),
                }
            }
            SurfaceKind::LineString => {
                header(&mut out, 2, srid);
                match self.patches.first() {
                    Some(p) => ring(&mut out, &p[0]),
                    None => count(&mut out, 0),
                }
            }
            SurfaceKind::Polygon | SurfaceKind::Triangle => {
                let kind = if self.kind == SurfaceKind::Triangle {
                    17
                } else {
                    3
                };
                header(&mut out, kind, srid);
                match self.patches.first() {
                    Some(p) => {
                        count(&mut out, p.len());
                        p.iter().for_each(|r| ring(&mut out, r));
                    }
                    None => count(&mut out, 0),
                }
            }
            SurfaceKind::MultiPoint => {
                header(&mut out, 4, srid);
                count(&mut out, self.patches.len());
                for p in &self.patches {
                    header(&mut out, 1, None);
                    point(&mut out, &p[0][0]);
                }
            }
            SurfaceKind::MultiLineString => {
                header(&mut out, 5, srid);
                count(&mut out, self.patches.len());
                for p in &self.patches {
                    header(&mut out, 2, None);
                    ring(&mut out, &p[0]);
                }
            }
            SurfaceKind::MultiPolygon | SurfaceKind::PolyhedralSurface | SurfaceKind::Tin => {
                let (kind, part) = match self.kind {
                    SurfaceKind::MultiPolygon => (6, 3),
                    SurfaceKind::PolyhedralSurface => (15, 3),
                    _ => (16, 17),
                };
                header(&mut out, kind, srid);
                count(&mut out, self.patches.len());
                self.patches.iter().for_each(|p| polygon(&mut out, part, p));
            }
        }
        out
    }
}

/// The 3-D form of a point, line or polygon geometry whose coordinates, in order, have the
/// Z values `z`; `None` for other geometries or if the counts differ.
pub(crate) fn from_geometry(g: &Geometry<f64>, z: &[f64]) -> Option<Surface> {
    use geo::CoordsIter;
    if g.coords_count() != z.len() {
        return None;
    }
    let mut zs = z.iter();
    let mut lift = |c: &Coord<f64>| [c.x, c.y, *zs.next().unwrap_or(&0.0)];
    let mut ring = |l: &LineString<f64>| -> Ring3 { l.0.iter().map(&mut lift).collect() };
    let (kind, patches): (SurfaceKind, Vec<Patch>) = match g {
        Geometry::Point(p) => (
            SurfaceKind::Point,
            vec![vec![ring(&LineString::new(vec![p.0]))]],
        ),
        Geometry::LineString(l) => (SurfaceKind::LineString, vec![vec![ring(l)]]),
        Geometry::Polygon(p) => (
            SurfaceKind::Polygon,
            vec![
                std::iter::once(p.exterior())
                    .chain(p.interiors())
                    .map(&mut ring)
                    .collect(),
            ],
        ),
        Geometry::MultiPoint(m) => (
            SurfaceKind::MultiPoint,
            m.0.iter()
                .map(|p| vec![ring(&LineString::new(vec![p.0]))])
                .collect(),
        ),
        Geometry::MultiLineString(m) => (
            SurfaceKind::MultiLineString,
            m.0.iter().map(|l| vec![ring(l)]).collect(),
        ),
        Geometry::MultiPolygon(m) => (
            SurfaceKind::MultiPolygon,
            m.0.iter()
                .map(|p| {
                    std::iter::once(p.exterior())
                        .chain(p.interiors())
                        .map(&mut ring)
                        .collect()
                })
                .collect(),
        ),
        _ => return None,
    };
    Some(Surface::new(kind, patches))
}

// --- WKT parsing ---------------------------------------------------------------------------

enum Tree {
    List(Vec<Tree>),
    Coord(Vec<f64>),
}

struct Reader<'a> {
    s: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn word(&mut self) -> &str {
        self.ws();
        let start = self.i;
        while self.s.get(self.i).is_some_and(u8::is_ascii_alphabetic) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).unwrap_or("")
    }

    fn list(&mut self, depth: usize) -> Option<Vec<Tree>> {
        (depth < 8).then_some(())?;
        (self.peek()? == b'(').then_some(())?;
        self.i += 1;
        let mut items = Vec::new();
        loop {
            if self.peek()? == b'(' {
                items.push(Tree::List(self.list(depth + 1)?));
            } else {
                items.push(Tree::Coord(self.coord()?));
            }
            match self.peek()? {
                b',' => self.i += 1,
                b')' => {
                    self.i += 1;
                    return Some(items);
                }
                _ => return None,
            }
        }
    }

    fn coord(&mut self) -> Option<Vec<f64>> {
        let mut values = Vec::new();
        loop {
            self.ws();
            let start = self.i;
            while self
                .s
                .get(self.i)
                .is_some_and(|b| !b.is_ascii_whitespace() && !matches!(b, b',' | b'(' | b')'))
            {
                self.i += 1;
            }
            if start == self.i {
                break;
            }
            values.push(
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()?
                    .parse()
                    .ok()?,
            );
        }
        (values.len() >= 2).then_some(values)
    }
}

/// Parses the WKT of a polyhedral surface, TIN, triangle or SFCGAL solid (without a CRS IRI).
/// `None` if the text is of another type; `Some(None)` if it is one of these but invalid.
pub(crate) fn parse_wkt(text: &str) -> Option<Option<Surface>> {
    let mut r = Reader {
        s: text.trim().as_bytes(),
        i: 0,
    };
    let word = r.word().to_ascii_uppercase();
    let known = ["POLYHEDRALSURFACE", "TIN", "TRIANGLE", "SOLID"];
    let (name, glued) = known
        .iter()
        .find_map(|k| word.strip_prefix(k).map(|rest| (*k, rest.to_owned())))?;
    if !matches!(glued.as_str(), "" | "Z" | "M" | "ZM") {
        return None;
    }
    Some(parse_body(&mut r, name, glued))
}

fn parse_body(r: &mut Reader, name: &str, glued: String) -> Option<Surface> {
    let mut dim = glued;
    if dim.is_empty() && r.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
        let save = r.i;
        let w = r.word().to_ascii_uppercase();
        if w == "EMPTY" {
            r.i = save;
        } else {
            dim = w;
        }
    }
    let has_z_tag = match dim.as_str() {
        "" => false,
        "Z" => true,
        // Measures are not kept on 3-D geometries.
        _ => return None,
    };
    let kind = match name {
        "TIN" => SurfaceKind::Tin,
        "TRIANGLE" => SurfaceKind::Triangle,
        _ => SurfaceKind::PolyhedralSurface,
    };
    let solid = name == "SOLID";
    if r.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
        (r.word().eq_ignore_ascii_case("EMPTY")).then_some(())?;
        r.ws();
        (r.i == r.s.len()).then_some(())?;
        return Some(Surface {
            kind,
            patches: Vec::new(),
            solid,
            has_z: has_z_tag,
        });
    }
    let tree = r.list(0)?;
    r.ws();
    (r.i == r.s.len()).then_some(())?;
    let mut width = None;
    let mut ring = |t: &Tree| -> Option<Ring3> {
        let Tree::List(points) = t else { return None };
        let mut out = Vec::with_capacity(points.len() + 1);
        for p in points {
            let Tree::Coord(c) = p else { return None };
            match width {
                None => width = Some(c.len()),
                Some(w) if w != c.len() => return None,
                _ => {}
            }
            (c.iter().all(|v| v.is_finite())).then_some(())?;
            out.push([c[0], c[1], c.get(2).copied().unwrap_or(0.0)]);
        }
        if out.first() != out.last() {
            out.push(out[0]);
        }
        (out.len() >= 4).then_some(out)
    };
    let mut patch = |t: &Tree| -> Option<Patch> {
        let Tree::List(rings) = t else { return None };
        let p: Patch = rings.iter().map(&mut ring).collect::<Option<_>>()?;
        (!p.is_empty()).then_some(p)
    };
    let patches: Vec<Patch> = match name {
        "TRIANGLE" => vec![patch(&Tree::List(tree))?],
        "SOLID" => {
            let mut all = Vec::new();
            for shell in &tree {
                let Tree::List(ps) = shell else { return None };
                for p in ps {
                    all.push(patch(p)?);
                }
            }
            all
        }
        _ => tree.iter().map(&mut patch).collect::<Option<_>>()?,
    };
    if matches!(kind, SurfaceKind::Tin | SurfaceKind::Triangle)
        && patches.iter().any(|p| p.len() != 1 || p[0].len() != 4)
    {
        return None;
    }
    let has_z = match width {
        Some(2) => {
            (!has_z_tag).then_some(())?;
            false
        }
        Some(3) => true,
        _ => return None,
    };
    Some(Surface {
        kind,
        patches,
        solid,
        has_z,
    })
}

// --- WKB parsing ---------------------------------------------------------------------------

/// Reads WKB or EWKB whose top-level type is a polyhedral surface, TIN or triangle. `None` if
/// it is another type; `Some(None)` if invalid. Returns the surface and the EWKB SRID.
pub(crate) fn parse_wkb(bytes: &[u8]) -> Option<Option<(Surface, Option<u32>)>> {
    let le = *bytes.first()? == 1;
    let raw = bytes.get(1..5)?;
    let raw = u32::from_le_bytes(raw.try_into().ok()?);
    let raw = if le { raw } else { raw.swap_bytes() };
    let kind = (raw & 0x0fff_ffff) % 1000;
    if !matches!(kind, 15..=17) {
        return None;
    }
    let mut r = WkbReader {
        bytes,
        pos: 0,
        width: None,
        srid: None,
    };
    let out = r.surface().filter(|_| r.pos == bytes.len());
    Some(out.map(|s| (s, r.srid)))
}

struct WkbReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    width: Option<usize>,
    srid: Option<u32>,
}

impl WkbReader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.bytes.get(self.pos..self.pos + N)?.try_into().ok()?;
        self.pos += N;
        Some(b)
    }

    fn u32(&mut self, le: bool) -> Option<u32> {
        let b = self.take::<4>()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self, le: bool) -> Option<f64> {
        let b = self.take::<8>()?;
        Some(if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    /// Byte order, type (mod 1000) and whether it has Z, of the next header.
    fn header(&mut self) -> Option<(bool, u32, bool)> {
        let le = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let raw = self.u32(le)?;
        if raw & 0x2000_0000 != 0 {
            self.srid = Some(self.u32(le)?);
        }
        let m = raw & 0x4000_0000 != 0 || matches!((raw & 0x0fff_ffff) / 1000, 2 | 3);
        (!m).then_some(())?;
        let z = raw & 0x8000_0000 != 0 || (raw & 0x0fff_ffff) / 1000 == 1;
        let width = if z { 3 } else { 2 };
        match self.width {
            None => self.width = Some(width),
            Some(w) if w != width => return None,
            _ => {}
        }
        Some((le, (raw & 0x0fff_ffff) % 1000, z))
    }

    fn count(&mut self, le: bool) -> Option<usize> {
        let n = self.u32(le)? as usize;
        (n <= self.bytes.len()).then_some(n)
    }

    fn polygon(&mut self, le: bool, z: bool) -> Option<Patch> {
        let rings = self.count(le)?;
        (0..rings)
            .map(|_| {
                let n = self.count(le)?;
                let r: Ring3 = (0..n)
                    .map(|_| {
                        let (x, y) = (self.f64(le)?, self.f64(le)?);
                        let zv = if z { self.f64(le)? } else { 0.0 };
                        Some([x, y, zv])
                    })
                    .collect::<Option<_>>()?;
                (r.len() >= 4 && r.first() == r.last()).then_some(r)
            })
            .collect()
    }

    fn surface(&mut self) -> Option<Surface> {
        let (le, kind, z) = self.header()?;
        let patches = match kind {
            17 => {
                let p = self.polygon(le, z)?;
                if p.is_empty() { Vec::new() } else { vec![p] }
            }
            _ => {
                let n = self.count(le)?;
                (0..n)
                    .map(|_| {
                        let (le, part, _) = self.header()?;
                        (part == if kind == 16 { 17 } else { 3 }).then_some(())?;
                        self.polygon(le, z)
                    })
                    .collect::<Option<_>>()?
            }
        };
        let kind = match kind {
            15 => SurfaceKind::PolyhedralSurface,
            16 => SurfaceKind::Tin,
            _ => SurfaceKind::Triangle,
        };
        if matches!(kind, SurfaceKind::Tin | SurfaceKind::Triangle)
            && patches.iter().any(|p| p.len() != 1 || p[0].len() != 4)
        {
            return None;
        }
        Some(Surface {
            kind,
            patches,
            solid: false,
            has_z: z,
        })
    }
}

// --- Vector helpers ------------------------------------------------------------------------

pub(crate) fn sub(a: P3, b: P3) -> P3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(crate) fn add(a: P3, b: P3) -> P3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub(crate) fn scale(a: P3, k: f64) -> P3 {
    [a[0] * k, a[1] * k, a[2] * k]
}

pub(crate) fn dot(a: P3, b: P3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn cross(a: P3, b: P3) -> P3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub(crate) fn norm(a: P3) -> f64 {
    dot(a, a).sqrt()
}

/// Twice the vector area of a closed ring (Newell's method): its direction is the ring's
/// normal (counter-clockwise seen from where it points), its length twice the area.
pub(crate) fn ring_normal(r: &[P3]) -> P3 {
    let Some(&o) = r.first() else {
        return [0.0; 3];
    };
    let mut n = [0.0; 3];
    for w in r.windows(2) {
        n = add(n, cross(sub(w[0], o), sub(w[1], o)));
    }
    n
}

/// The vector area of a polygon: its exterior's, less its holes'.
pub(crate) fn area_vector(p: &Patch) -> P3 {
    let Some(ext) = p.first() else {
        return [0.0; 3];
    };
    let mut a = scale(ring_normal(ext), 0.5);
    for h in &p[1..] {
        let ah = scale(ring_normal(h), 0.5);
        a = if dot(ah, a) > 0.0 {
            sub(a, ah)
        } else {
            add(a, ah)
        };
    }
    a
}

/// The signed volume bounded by the patches (positive when their normals point outward);
/// meaningful for a closed surface.
pub(crate) fn signed_volume(patches: &[Patch]) -> f64 {
    let Some(&o) = patches
        .first()
        .and_then(|p| p.first())
        .and_then(|r| r.first())
    else {
        return 0.0;
    };
    patches
        .iter()
        .filter(|p| !p.is_empty() && !p[0].is_empty())
        .map(|p| dot(sub(p[0][0], o), area_vector(p)) / 3.0)
        .sum()
}

/// Reverses the orientation of a polygon.
pub(crate) fn reversed(p: &Patch) -> Patch {
    p.iter()
        .map(|r| r.iter().rev().copied().collect())
        .collect()
}

/// Whether every directed edge of the patches is matched by the reverse edge of another:
/// the surface is closed and consistently oriented.
pub(crate) fn is_closed(patches: &[Patch]) -> bool {
    use std::collections::HashMap;
    if patches.is_empty() {
        return false;
    }
    let key = |p: &P3| [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()];
    let mut edges: HashMap<([u64; 3], [u64; 3]), i64> = HashMap::new();
    for r in patches.iter().flatten() {
        for w in r.windows(2) {
            if w[0] == w[1] {
                continue;
            }
            let (a, b) = (key(&w[0]), key(&w[1]));
            if a < b {
                *edges.entry((a, b)).or_default() += 1;
            } else {
                *edges.entry((b, a)).or_default() -= 1;
            }
        }
    }
    edges.values().all(|&n| n == 0)
}

/// The axis to drop to project a polygon with normal `n` without folding it, and whether
/// the projection reverses its orientation.
pub(crate) fn projection(n: P3) -> (usize, bool) {
    let k = (0..3)
        .max_by(|&i, &j| n[i].abs().total_cmp(&n[j].abs()))
        .unwrap_or(2);
    (k, n[k] < 0.0)
}

/// The two kept axes when dropping `k`, in cyclic order (so the projection of a polygon
/// whose normal has a positive `k` component is counter-clockwise).
pub(crate) fn kept_axes(k: usize) -> (usize, usize) {
    ((k + 1) % 3, (k + 2) % 3)
}

/// Triangles covering a planar polygon (with holes), oriented as the polygon. A triangle
/// is returned as it is; others are triangulated (constrained Delaunay) in their plane.
pub(crate) fn triangulate(p: &Patch) -> Vec<[P3; 3]> {
    use geo::TriangulateDelaunay;
    use std::collections::HashMap;
    if p.len() == 1 && p[0].len() == 4 {
        return vec![[p[0][0], p[0][1], p[0][2]]];
    }
    let n = area_vector(p);
    if norm(n) == 0.0 {
        return Vec::new();
    }
    let (k, _) = projection(n);
    let (i, j) = kept_axes(k);
    let key = |c: Coord<f64>| (c.x.to_bits(), c.y.to_bits());
    let mut lift: HashMap<(u64, u64), P3> = HashMap::new();
    let ring2 = |r: &Ring3, lift: &mut HashMap<(u64, u64), P3>| {
        LineString::new(
            r.iter()
                .map(|q| {
                    let c = Coord { x: q[i], y: q[j] };
                    lift.entry(key(c)).or_insert(*q);
                    c
                })
                .collect(),
        )
    };
    let ext = ring2(&p[0], &mut lift);
    let holes = p[1..].iter().map(|r| ring2(r, &mut lift)).collect();
    let poly = geo::Polygon::new(ext, holes);
    let Ok(tris) = poly.constrained_triangulation(Default::default()) else {
        return Vec::new();
    };
    // A point the triangulation added lies in the polygon's plane.
    let o = p[0][0];
    let un = scale(n, 1.0 / norm(n));
    let lift_point = |c: Coord<f64>| -> P3 {
        if let Some(q) = lift.get(&key(c)) {
            return *q;
        }
        let mut q = [0.0; 3];
        q[i] = c.x;
        q[j] = c.y;
        q[k] = o[k] - (un[i] * (c.x - o[i]) + un[j] * (c.y - o[j])) / un[k];
        q
    };
    tris.into_iter()
        .map(|t| {
            let (a, b, c) = (lift_point(t.v1()), lift_point(t.v2()), lift_point(t.v3()));
            if dot(cross(sub(b, a), sub(c, a)), n) < 0.0 {
                [a, c, b]
            } else {
                [a, b, c]
            }
        })
        .collect()
}

/// A triangle as a patch.
pub(crate) fn triangle_patch(t: [P3; 3]) -> Patch {
    vec![vec![t[0], t[1], t[2], t[0]]]
}

/// Whether a polygon without holes is convex (seen along its normal).
pub(crate) fn is_convex(p: &Patch, eps: f64) -> bool {
    if p.len() != 1 {
        return false;
    }
    let n = area_vector(p);
    let len = norm(n);
    if len == 0.0 {
        return false;
    }
    let r = &p[0][..p[0].len() - 1];
    let m = r.len();
    (0..m).all(|i| {
        let (a, b, c) = (r[i], r[(i + 1) % m], r[(i + 2) % m]);
        dot(cross(sub(b, a), sub(c, b)), n) / len >= -eps
    })
}
