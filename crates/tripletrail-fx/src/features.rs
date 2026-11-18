//! Geodata files as features, TripleTrail's own formats (SPARQL Anything has none of them):
//! Shapefile (`.shp`, with its `.shx` and `.dbf`), KML and GPX. The root holds one container
//! per feature (`rdf:_1`, `rdf:_2`, ...; `<root>#feature<n>` with `blank-nodes=false`). A
//! feature's attributes are keys; it has `geo:hasGeometry` to a geometry container
//! (`<feature>/geometry`) with `geo:asWKT`. `geo.crs` names the CRS of a Shapefile's
//! coordinates (KML and GPX are always longitude/latitude, the WKT default).
//!
//! - Shapefile: one feature per record. Attributes are typed by their dBase field type:
//!   character and memo `xsd:string`, numeric and float `xsd:double`, integer `xsd:int`,
//!   logical `xsd:boolean`, date `xsd:date`, timestamp `xsd:dateTime`. Empty values give
//!   no triple. Z and M values are dropped.
//! - KML: one feature per `Placemark`, with `name`, `description`, and the `Data` and
//!   `SimpleData` of its `ExtendedData` as keys; `Point`, `LineString`, `LinearRing`,
//!   `Polygon` and `MultiGeometry` geometries. Altitudes are dropped.
//! - GPX: one feature per waypoint (`wpt`, a point), route (`rte`, a line through its
//!   `rtept`s) and track (`trk`, a line, or a multi-line with several `trkseg`s); the text of
//!   its other child elements (`name`, `desc`, `ele`, `time`, `type`, ...) are keys.
//!   Strings, as written.

use crate::FxError;
use crate::geo;
use crate::options::Options;
use crate::tree::{Sink, xsd_double};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{BlankNode, Literal, NamedNode, NamedOrBlankNode, Term};
use quick_xml::events::Event;
use std::fmt::Write;
use std::path::Path;

/// Adds the features to the root, one at a time.
struct Features<'s, 'a> {
    sink: &'s mut Sink<'a>,
    root: NamedOrBlankNode,
    n: usize,
    crs: String,
}

impl<'s, 'a> Features<'s, 'a> {
    fn new(sink: &'s mut Sink<'a>, options: &Options) -> Result<Self, FxError> {
        let root = sink.root();
        Ok(Self {
            sink,
            root,
            n: 0,
            crs: geo::crs_prefix(options)?,
        })
    }

    /// A new feature, in the root's next slot.
    fn feature(&mut self) -> NamedOrBlankNode {
        self.n += 1;
        let node: NamedOrBlankNode = if self.sink.common.blank_nodes {
            BlankNode::default().into()
        } else {
            let r = &self.sink.common.root;
            let sep = if r.ends_with('/') || r.ends_with('#') {
                ""
            } else {
                "#"
            };
            NamedNode::new_unchecked(format!("{r}{sep}feature{}", self.n)).into()
        };
        self.sink.slot(&self.root, self.n, node.clone().into());
        node
    }

    fn key(&mut self, feature: &NamedOrBlankNode, key: &str, value: Option<Term>) {
        if let Some(v) = value {
            self.sink.key(feature, key, v);
        }
    }

    /// `feature geo:hasGeometry [ geo:asWKT wkt ]`, for a geometry that is left once
    /// [`Geometry::clean`] has dropped its empty parts. `format` names the source in errors.
    fn geometry(
        &mut self,
        feature: &NamedOrBlankNode,
        geometry: Option<Geometry>,
        format: &str,
    ) -> Result<(), FxError> {
        let Some(geometry) = geometry else {
            return Ok(());
        };
        if !geometry.finite() {
            return Err(FxError::Parse(format!(
                "{format}: a coordinate is not a finite number"
            )));
        }
        let Some(geometry) = geometry.clean() else {
            return Ok(());
        };
        let wkt = geometry.wkt();
        let has = self.sink.predicate(geo::HAS_GEOMETRY.to_owned());
        let as_wkt = self.sink.predicate(geo::AS_WKT.to_owned());
        if has.is_none() && as_wkt.is_none() {
            return Ok(());
        }
        let g = self.geometry_node(feature);
        if let Some(has) = has {
            self.sink.emit(feature, &has, g.clone().into());
        }
        if let Some(as_wkt) = as_wkt {
            let literal = Literal::new_typed_literal(
                format!("{}{wkt}", self.crs),
                NamedNode::new_unchecked(geo::WKT_LITERAL),
            );
            self.sink.emit(&g, &as_wkt, literal.into());
        }
        Ok(())
    }

    fn geometry_node(&self, feature: &NamedOrBlankNode) -> NamedOrBlankNode {
        match feature {
            NamedOrBlankNode::NamedNode(n) => {
                NamedNode::new_unchecked(format!("{}/geometry", n.as_str())).into()
            }
            NamedOrBlankNode::BlankNode(_) => BlankNode::default().into(),
        }
    }
}

// --- WKT ---------------------------------------------------------------------------------

fn number(out: &mut String, v: f64) {
    let _ = write!(out, "{v}");
}

fn points(out: &mut String, ps: &[(f64, f64)]) {
    out.push('(');
    for (i, (x, y)) in ps.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        number(out, *x);
        out.push(' ');
        number(out, *y);
    }
    out.push(')');
}

fn rings(out: &mut String, rs: &[Vec<(f64, f64)>]) {
    out.push('(');
    for (i, r) in rs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        points(out, r);
    }
    out.push(')');
}

/// A geometry, before it is written as WKT.
enum Geometry {
    Point(f64, f64),
    Line(Vec<(f64, f64)>),
    Polygon(Vec<Vec<(f64, f64)>>),
    MultiPoint(Vec<(f64, f64)>),
    MultiLine(Vec<Vec<(f64, f64)>>),
    MultiPolygon(Vec<Vec<Vec<(f64, f64)>>>),
    Collection(Vec<Geometry>),
}

/// The points a ring needs: three corners and the first again.
const RING_POINTS: usize = 4;

impl Geometry {
    /// Whether every coordinate is a finite number.
    fn finite(&self) -> bool {
        let ok = |ps: &[(f64, f64)]| ps.iter().all(|(x, y)| x.is_finite() && y.is_finite());
        match self {
            Geometry::Point(x, y) => x.is_finite() && y.is_finite(),
            Geometry::Line(ps) | Geometry::MultiPoint(ps) => ok(ps),
            Geometry::Polygon(rs) | Geometry::MultiLine(rs) => rs.iter().all(|r| ok(r)),
            Geometry::MultiPolygon(ps) => ps.iter().flatten().all(|r| ok(r)),
            Geometry::Collection(gs) => gs.iter().all(Geometry::finite),
        }
    }

    /// The geometry as valid WKT can say it: lines need two points (a line of one is that
    /// point), rings four (a polygon whose outer ring is too short is dropped, and so are
    /// short holes), and parts that are left empty are dropped. `None` when nothing is left.
    fn clean(self) -> Option<Geometry> {
        fn polygon(mut rings: Vec<Vec<(f64, f64)>>) -> Option<Vec<Vec<(f64, f64)>>> {
            if rings.first().is_none_or(|r| r.len() < RING_POINTS) {
                return None;
            }
            rings.retain(|r| r.len() >= RING_POINTS);
            Some(rings)
        }
        /// Lines too short to be lines become their points: next to longer lines, in a
        /// collection with them.
        fn lines(lines: Vec<Vec<(f64, f64)>>) -> Option<Geometry> {
            let (mut long, short): (Vec<_>, Vec<_>) = lines.into_iter().partition(|l| l.len() > 1);
            let mut points: Vec<(f64, f64)> = short.into_iter().flatten().collect();
            let points = match points.len() {
                0 => None,
                1 => {
                    let (x, y) = points.remove(0);
                    Some(Geometry::Point(x, y))
                }
                _ => Some(Geometry::MultiPoint(points)),
            };
            let lines = match long.len() {
                0 => None,
                1 => Some(Geometry::Line(long.remove(0))),
                _ => Some(Geometry::MultiLine(long)),
            };
            match (lines, points) {
                (Some(l), Some(p)) => Some(Geometry::Collection(vec![l, p])),
                (l, p) => l.or(p),
            }
        }
        match self {
            Geometry::Point(..) => Some(self),
            Geometry::Line(ps) => lines(vec![ps]),
            Geometry::MultiLine(ls) => lines(ls),
            Geometry::MultiPoint(ps) => (!ps.is_empty()).then_some(Geometry::MultiPoint(ps)),
            Geometry::Polygon(rs) => polygon(rs).map(Geometry::Polygon),
            Geometry::MultiPolygon(ps) => {
                let mut ps: Vec<_> = ps.into_iter().filter_map(polygon).collect();
                match ps.len() {
                    0 => None,
                    1 => Some(Geometry::Polygon(ps.remove(0))),
                    _ => Some(Geometry::MultiPolygon(ps)),
                }
            }
            Geometry::Collection(gs) => {
                let mut gs: Vec<_> = gs.into_iter().filter_map(Geometry::clean).collect();
                match gs.len() {
                    0 => None,
                    1 => Some(gs.remove(0)),
                    _ => Some(Geometry::Collection(gs)),
                }
            }
        }
    }

    fn wkt(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Geometry::Point(x, y) => {
                out.push_str("POINT");
                points(out, &[(*x, *y)]);
            }
            Geometry::Line(ps) => {
                out.push_str("LINESTRING");
                points(out, ps);
            }
            Geometry::Polygon(rs) => {
                out.push_str("POLYGON");
                rings(out, rs);
            }
            Geometry::MultiPoint(ps) => {
                out.push_str("MULTIPOINT(");
                for (i, p) in ps.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    points(out, std::slice::from_ref(p));
                }
                out.push(')');
            }
            Geometry::MultiLine(ls) => {
                out.push_str("MULTILINESTRING");
                rings(out, ls);
            }
            Geometry::MultiPolygon(ps) => {
                out.push_str("MULTIPOLYGON(");
                for (i, p) in ps.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    rings(out, p);
                }
                out.push(')');
            }
            Geometry::Collection(gs) => {
                out.push_str("GEOMETRYCOLLECTION(");
                for (i, g) in gs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    g.write(out);
                }
                out.push(')');
            }
        }
    }
}

// --- Shapefile ---------------------------------------------------------------------------

pub(crate) fn shapefile(
    path: &Path,
    options: &Options,
    sink: &mut Sink<'_>,
) -> Result<(), FxError> {
    let error = |e: shapefile::Error| FxError::Parse(format!("Shapefile {}: {e}", path.display()));
    let mut reader = shapefile::Reader::from_path(path).map_err(error)?;
    let mut f = Features::new(sink, options)?;
    for item in reader.iter_shapes_and_records() {
        let (shape, record) = item.map_err(error)?;
        let feature = f.feature();
        for (name, value) in record {
            let term = field(f.sink, value);
            f.key(&feature, &name, term);
        }
        f.geometry(&feature, shape_geometry(shape), "Shapefile")?;
    }
    Ok(())
}

fn field(sink: &Sink<'_>, value: shapefile::dbase::FieldValue) -> Option<Term> {
    use shapefile::dbase::FieldValue as V;
    let double = |v: f64| sink.typed(&xsd_double(v), xsd::DOUBLE);
    match value {
        V::Character(s) => sink.string(s.as_deref()?.trim_end()),
        V::Memo(s) => sink.string(&s),
        V::Numeric(v) => double(v?),
        V::Float(v) => double(f64::from(v?)),
        V::Double(v) | V::Currency(v) => double(v),
        V::Integer(v) => sink.typed(&v.to_string(), xsd::INT),
        V::Logical(v) => sink.typed(if v? { "true" } else { "false" }, xsd::BOOLEAN),
        V::Date(d) => {
            let d = d?;
            sink.typed(
                &format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
                xsd::DATE,
            )
        }
        V::DateTime(t) => {
            let (d, t) = (t.date(), t.time());
            sink.typed(
                &format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                    d.year(),
                    d.month(),
                    d.day(),
                    t.hours(),
                    t.minutes(),
                    t.seconds()
                ),
                xsd::DATE_TIME,
            )
        }
    }
}

fn xy<P: shapefile::record::traits::HasXY>(p: &P) -> (f64, f64) {
    (p.x(), p.y())
}

fn shape_geometry(shape: shapefile::Shape) -> Option<Geometry> {
    use shapefile::Shape as S;
    fn line<P: shapefile::record::traits::HasXY>(parts: &[Vec<P>]) -> Geometry {
        let mut ls: Vec<Vec<(f64, f64)>> =
            parts.iter().map(|p| p.iter().map(xy).collect()).collect();
        if ls.len() == 1 {
            Geometry::Line(ls.remove(0))
        } else {
            Geometry::MultiLine(ls)
        }
    }
    fn polygon<P: shapefile::record::traits::HasXY>(
        rings: &[shapefile::PolygonRing<P>],
    ) -> Geometry {
        // An outer ring starts a polygon; inner rings are its holes.
        let mut polygons: Vec<Vec<Vec<(f64, f64)>>> = Vec::new();
        for r in rings {
            let points: Vec<(f64, f64)> = r.points().iter().map(xy).collect();
            match r {
                shapefile::PolygonRing::Outer(_) => polygons.push(vec![points]),
                shapefile::PolygonRing::Inner(_) => match polygons.last_mut() {
                    Some(p) => p.push(points),
                    None => polygons.push(vec![points]),
                },
            }
        }
        if polygons.len() == 1 {
            Geometry::Polygon(polygons.remove(0))
        } else {
            Geometry::MultiPolygon(polygons)
        }
    }
    fn multipoint<P: shapefile::record::traits::HasXY>(ps: &[P]) -> Geometry {
        Geometry::MultiPoint(ps.iter().map(xy).collect())
    }
    Some(match shape {
        S::NullShape => return None,
        S::Point(p) => Geometry::Point(p.x, p.y),
        S::PointM(p) => Geometry::Point(p.x, p.y),
        S::PointZ(p) => Geometry::Point(p.x, p.y),
        S::Polyline(l) => line(l.parts()),
        S::PolylineM(l) => line(l.parts()),
        S::PolylineZ(l) => line(l.parts()),
        S::Polygon(p) => polygon(p.rings()),
        S::PolygonM(p) => polygon(p.rings()),
        S::PolygonZ(p) => polygon(p.rings()),
        S::Multipoint(p) => multipoint(p.points()),
        S::MultipointM(p) => multipoint(p.points()),
        S::MultipointZ(p) => multipoint(p.points()),
        // Surfaces of triangles: not a simple-features geometry.
        S::Multipatch(_) => return None,
    })
}

// --- KML and GPX: a small element tree -----------------------------------------------------

#[derive(Default)]
struct Element {
    name: String,
    attributes: Vec<(String, String)>,
    text: String,
    children: Vec<Element>,
}

impl Element {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    fn all<'e>(&'e self, name: &'e str) -> impl Iterator<Item = &'e Element> + 'e {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Every element named `name` at or below this one, in document order.
    fn descendants<'e>(&'e self, name: &str, out: &mut Vec<&'e Element>) {
        if self.name == name {
            out.push(self);
        }
        for c in &self.children {
            c.descendants(name, out);
        }
    }
}

/// How deeply elements may nest: the tree is walked (and dropped) recursively.
const MAX_DEPTH: usize = 512;

/// The document's elements, by local name (prefixes dropped).
fn parse(bytes: &[u8], format: &str) -> Result<Element, FxError> {
    let err = |e: &dyn std::fmt::Display| FxError::Parse(format!("{format}: {e}"));
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = vec![Element::default()];
    let mut buf = Vec::new();
    let local = |n: &[u8]| -> Result<String, FxError> {
        let n = std::str::from_utf8(n).map_err(|e| err(&e))?;
        Ok(n.rsplit(':').next().unwrap_or(n).to_owned())
    };
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf).map_err(|e| err(&e))? {
            Event::Start(_) if stack.len() > MAX_DEPTH => {
                return Err(err(&format!("elements nest more than {MAX_DEPTH} deep")));
            }
            Event::Start(e) => {
                let mut el = Element {
                    name: local(e.name().as_ref())?,
                    ..Element::default()
                };
                for a in e.attributes() {
                    let a = a.map_err(|e| err(&e))?;
                    let value = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|e| err(&e))?
                        .into_owned();
                    el.attributes.push((local(a.key.as_ref())?, value));
                }
                stack.push(el);
            }
            Event::End(_) => {
                let el = stack.pop().expect("an open element");
                stack
                    .last_mut()
                    .ok_or_else(|| err(&"unbalanced elements"))?
                    .children
                    .push(el);
            }
            Event::Text(t) => {
                let s = t
                    .xml_content(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|e| err(&e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&s);
                }
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref().map_err(|e| err(&e))? {
                    Some(c) => c.to_string(),
                    None => {
                        let name = r.decode().map_err(|e| err(&e))?;
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| err(&format!("unknown entity &{name};")))?
                            .to_owned()
                    }
                };
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&resolved);
                }
            }
            Event::CData(c) => {
                let s = String::from_utf8(c.into_inner().into_owned()).map_err(|e| err(&e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&s);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(err(&"unclosed elements"));
    }
    Ok(stack.pop().expect("the document"))
}

/// KML `coordinates`: `lon,lat[,alt]` tuples separated by white space.
fn kml_coordinates(text: &str) -> Result<Vec<(f64, f64)>, FxError> {
    text.split_whitespace()
        .map(|t| {
            let mut parts = t.split(',');
            let mut next = || {
                parts
                    .next()
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .ok_or_else(|| FxError::Parse(format!("KML: coordinates {t:?}")))
            };
            Ok((next()?, next()?))
        })
        .collect()
}

fn kml_geometry(e: &Element) -> Result<Option<Geometry>, FxError> {
    let coordinates = |e: &Element| match e.child("coordinates") {
        Some(c) => kml_coordinates(&c.text),
        None => Ok(Vec::new()),
    };
    Ok(Some(match e.name.as_str() {
        "Point" => match coordinates(e)?.first() {
            Some(&(x, y)) => Geometry::Point(x, y),
            None => return Ok(None),
        },
        "LineString" | "LinearRing" => Geometry::Line(coordinates(e)?),
        "Polygon" => {
            // The outer ring first; without one the polygon is dropped.
            let mut rings = vec![Vec::new()];
            for b in e.all("outerBoundaryIs") {
                if let Some(r) = b.child("LinearRing") {
                    rings[0] = coordinates(r)?;
                }
            }
            for b in e.all("innerBoundaryIs") {
                for r in b.all("LinearRing") {
                    rings.push(coordinates(r)?);
                }
            }
            Geometry::Polygon(rings)
        }
        "MultiGeometry" => {
            let mut parts = Vec::new();
            for c in &e.children {
                if let Some(g) = kml_geometry(c)? {
                    parts.push(g);
                }
            }
            match parts.len() {
                0 => return Ok(None),
                1 => parts.remove(0),
                _ => Geometry::Collection(parts),
            }
        }
        _ => return Ok(None),
    }))
}

pub(crate) fn kml(bytes: &[u8], options: &Options, sink: &mut Sink<'_>) -> Result<(), FxError> {
    let document = parse(bytes, "KML")?;
    let mut placemarks = Vec::new();
    document.descendants("Placemark", &mut placemarks);
    let mut f = Features::new(sink, options)?;
    for p in placemarks {
        let feature = f.feature();
        for key in ["name", "description"] {
            if let Some(c) = p.child(key) {
                let v = f.sink.string(&c.text);
                f.key(&feature, key, v);
            }
        }
        if let Some(extended) = p.child("ExtendedData") {
            for d in extended.all("Data") {
                if let (Some(name), Some(value)) = (d.attribute("name"), d.child("value")) {
                    let v = f.sink.string(&value.text);
                    f.key(&feature, name, v);
                }
            }
            for s in extended.all("SchemaData") {
                for d in s.all("SimpleData") {
                    if let Some(name) = d.attribute("name") {
                        let v = f.sink.string(&d.text);
                        f.key(&feature, name, v);
                    }
                }
            }
        }
        let mut geometry = None;
        for c in &p.children {
            if let Some(g) = kml_geometry(c)? {
                geometry = Some(g);
                break;
            }
        }
        f.geometry(&feature, geometry, "KML")?;
    }
    Ok(())
}

fn gpx_point(e: &Element) -> Result<(f64, f64), FxError> {
    let coordinate = |name: &str| {
        e.attribute(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .ok_or_else(|| FxError::Parse(format!("GPX: {} without a number {name}", e.name)))
    };
    Ok((coordinate("lon")?, coordinate("lat")?))
}

pub(crate) fn gpx(bytes: &[u8], options: &Options, sink: &mut Sink<'_>) -> Result<(), FxError> {
    let document = parse(bytes, "GPX")?;
    let Some(gpx) = document.child("gpx") else {
        return Err(FxError::Parse("GPX: no gpx element".into()));
    };
    let mut f = Features::new(sink, options)?;
    for e in &gpx.children {
        let geometry = match e.name.as_str() {
            "wpt" => {
                let (x, y) = gpx_point(e)?;
                Geometry::Point(x, y)
            }
            "rte" => Geometry::Line(e.all("rtept").map(gpx_point).collect::<Result<_, _>>()?),
            "trk" => {
                let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
                for s in e.all("trkseg") {
                    segments.push(s.all("trkpt").map(gpx_point).collect::<Result<_, _>>()?);
                }
                if segments.len() == 1 {
                    Geometry::Line(segments.remove(0))
                } else {
                    Geometry::MultiLine(segments)
                }
            }
            _ => continue,
        };
        let feature = f.feature();
        for c in &e.children {
            if matches!(c.name.as_str(), "rtept" | "trkseg" | "extensions" | "link")
                || !c.children.is_empty()
            {
                continue;
            }
            let v = f.sink.string(&c.text);
            f.key(&feature, &c.name, v);
        }
        f.geometry(&feature, Some(geometry), "GPX")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Geometry;

    fn wkt(g: Geometry) -> Option<String> {
        g.clean().map(|g| g.wkt())
    }

    #[test]
    fn empty_parts_are_dropped() {
        // A Shapefile polyline with no parts, or parts without points.
        assert_eq!(wkt(Geometry::MultiLine(vec![])), None);
        assert_eq!(wkt(Geometry::MultiLine(vec![vec![], vec![]])), None);
        assert_eq!(wkt(Geometry::Line(vec![])), None);
        assert_eq!(
            wkt(Geometry::MultiLine(vec![
                vec![(1.0, 2.0)],
                vec![(3.0, 4.0)]
            ]))
            .as_deref(),
            Some("MULTIPOINT((1 2), (3 4))")
        );
        // A lone point next to a line is kept beside it.
        assert_eq!(
            wkt(Geometry::MultiLine(vec![
                vec![(0.0, 0.0), (1.0, 1.0)],
                vec![(5.0, 5.0)]
            ]))
            .as_deref(),
            Some("GEOMETRYCOLLECTION(LINESTRING(0 0, 1 1), POINT(5 5))")
        );
        assert_eq!(wkt(Geometry::MultiPoint(vec![])), None);
        assert_eq!(wkt(Geometry::Polygon(vec![])), None);
        assert_eq!(
            wkt(Geometry::MultiPolygon(vec![vec![], vec![vec![]]])),
            None
        );
        assert_eq!(wkt(Geometry::Collection(vec![])), None);
        assert_eq!(
            wkt(Geometry::Collection(vec![
                Geometry::Line(vec![]),
                Geometry::Point(1.0, 2.0)
            ]))
            .as_deref(),
            Some("POINT(1 2)")
        );
    }
}
