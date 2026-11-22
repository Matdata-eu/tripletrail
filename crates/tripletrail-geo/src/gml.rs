//! `geo:gmlLiteral` (GML 3.2 simple features) and `geo:kmlLiteral` (KML 2.2) input.

use crate::literal::{Crs, Geom, crs_from_iri};
use geo::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon, Rect,
};
use quick_xml::Reader;
use quick_xml::events::Event;

/// An XML element, with names stripped of their namespace prefix.
#[derive(Debug, Default)]
struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Element>,
    text: String,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
}

fn local(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    match name.rsplit_once(':') {
        Some((_, l)) => l.to_owned(),
        None => name.into_owned(),
    }
}

/// An attribute's value with its references resolved and whitespace normalised (XML 1.0).
pub(crate) fn attribute_value(a: &quick_xml::events::attributes::Attribute<'_>) -> Option<String> {
    a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .ok()
        .map(|v| v.into_owned())
}

/// The text a reference in element content stands for: a character reference (`&#65;`) or a
/// predefined entity (`&amp;`). Other entities need a DTD and are not supported.
pub(crate) fn reference_text(r: &quick_xml::events::BytesRef<'_>) -> Option<String> {
    if let Some(c) = r.resolve_char_ref().ok()? {
        return Some(c.to_string());
    }
    quick_xml::escape::resolve_predefined_entity(&r.decode().ok()?).map(str::to_owned)
}

fn parse_xml(text: &str) -> Option<Element> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Element> = vec![Element::default()];
    let start = |e: &quick_xml::events::BytesStart<'_>| -> Option<Element> {
        let mut el = Element {
            name: local(e.name().as_ref()),
            ..Element::default()
        };
        for a in e.attributes() {
            let a = a.ok()?;
            el.attrs.push((local(a.key.as_ref()), attribute_value(&a)?));
        }
        Some(el)
    };
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => stack.push(start(&e)?),
            Event::Empty(e) => {
                let el = start(&e)?;
                stack.last_mut()?.children.push(el);
            }
            Event::End(_) => {
                let el = stack.pop()?;
                stack.last_mut()?.children.push(el);
            }
            Event::Text(t) => stack.last_mut()?.text.push_str(&t.decode().ok()?),
            Event::GeneralRef(r) => stack.last_mut()?.text.push_str(&reference_text(&r)?),
            Event::CData(t) => stack
                .last_mut()?
                .text
                .push_str(&String::from_utf8_lossy(&t)),
            Event::Eof => break,
            _ => {}
        }
    }
    let mut root = stack.pop()?;
    if !stack.is_empty() || root.children.len() != 1 {
        return None;
    }
    root.children.pop()
}

/// The CRS named by a GML `srsName`: an OGC CRS IRI, `urn:ogc:def:crs:EPSG::n` or `EPSG:n`.
/// `None` when the name holds a character no IRI may hold (a space, a control character, a
/// quote, angle or curly brackets, a backslash, a caret, a backtick or `|`), since such a CRS
/// could not be written back in a WKT literal's `<...>`.
fn srs(name: &str) -> Option<Crs> {
    let name = name.trim();
    if name.chars().any(|c| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`')
    }) {
        return None;
    }
    let epsg = name
        .strip_prefix("urn:ogc:def:crs:EPSG::")
        .or_else(|| name.strip_prefix("urn:ogc:def:crs:EPSG:6.6:"))
        .or_else(|| name.strip_prefix("EPSG:"))
        .or_else(|| name.strip_prefix("http://www.opengis.net/gml/srs/epsg.xml#"));
    Some(match epsg {
        Some(code) => crs_from_iri(&format!("http://www.opengis.net/def/crs/EPSG/0/{code}")),
        None if name.ends_with("CRS84") => Crs::Crs84,
        None => crs_from_iri(name),
    })
}

struct Gml {
    swap: bool,
}

impl Gml {
    fn numbers(text: &str) -> Option<Vec<f64>> {
        text.split_whitespace().map(|n| n.parse().ok()).collect()
    }

    fn dim(el: &Element, inherited: usize) -> usize {
        el.attr("srsDimension")
            .and_then(|d| d.parse().ok())
            .unwrap_or(inherited)
    }

    fn coord(&self, x: f64, y: f64) -> Coord<f64> {
        if self.swap {
            Coord { x: y, y: x }
        } else {
            Coord { x, y }
        }
    }

    fn coords(&self, el: &Element, dim: usize) -> Option<Vec<Coord<f64>>> {
        if let Some(list) = el.child("posList") {
            let dim = Self::dim(list, dim);
            let n = Self::numbers(&list.text)?;
            if dim < 2 || n.len() % dim != 0 {
                return None;
            }
            return Some(n.chunks(dim).map(|c| self.coord(c[0], c[1])).collect());
        }
        if let Some(c) = el.child("coordinates") {
            // GML 2: "x,y x,y".
            return c
                .text
                .split_whitespace()
                .map(|t| {
                    let mut it = t.split(',').map(|v| v.parse::<f64>().ok());
                    Some(self.coord(it.next()??, it.next()??))
                })
                .collect();
        }
        el.children
            .iter()
            .filter(|c| c.name == "pos" || c.name == "pointProperty" || c.name == "Point")
            .map(|c| {
                let pos = match c.name.as_str() {
                    "pos" => c,
                    "Point" => c.child("pos")?,
                    _ => c.child("Point")?.child("pos")?,
                };
                let n = Self::numbers(&pos.text)?;
                (n.len() >= 2).then(|| self.coord(n[0], n[1]))
            })
            .collect()
    }

    fn ring(&self, boundary: &Element, dim: usize) -> Option<LineString<f64>> {
        let ring = boundary.child("LinearRing")?;
        Some(LineString::new(self.coords(ring, Self::dim(ring, dim))?))
    }

    fn polygon(&self, el: &Element, dim: usize) -> Option<Polygon<f64>> {
        let exterior = el
            .child("exterior")
            .or_else(|| el.child("outerBoundaryIs"))
            .map(|b| self.ring(b, dim))
            .unwrap_or_else(|| Some(LineString::new(Vec::new())))?;
        let interiors = el
            .children
            .iter()
            .filter(|c| c.name == "interior" || c.name == "innerBoundaryIs")
            .map(|b| self.ring(b, dim))
            .collect::<Option<Vec<_>>>()?;
        Some(Polygon::new(exterior, interiors))
    }

    /// Members of a multi-geometry: `xMember` children (one geometry each) and `xMembers`
    /// children (several).
    fn members<'a>(el: &'a Element, member: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        let plural = format!("{member}s");
        el.children.iter().flat_map(move |c| {
            if c.name == member {
                c.children.iter().take(1).collect::<Vec<_>>()
            } else if c.name == plural {
                c.children.iter().collect()
            } else {
                Vec::new()
            }
        })
    }

    fn geometry(&self, el: &Element, dim: usize) -> Option<Geometry<f64>> {
        let dim = Self::dim(el, dim);
        Some(match el.name.as_str() {
            "Point" => {
                let c = self.coords(el, dim)?;
                match c.as_slice() {
                    [c] => Geometry::Point(Point(*c)),
                    [] => Geometry::MultiPoint(MultiPoint(Vec::new())),
                    _ => return None,
                }
            }
            "LineString" | "LinearRing" => {
                Geometry::LineString(LineString::new(self.coords(el, dim)?))
            }
            "Curve" => {
                // Curve/segments/LineStringSegment*: concatenated.
                let mut coords: Vec<Coord<f64>> = Vec::new();
                for seg in el.child("segments")?.children.iter() {
                    for c in self.coords(seg, Self::dim(seg, dim))? {
                        if coords.last() != Some(&c) {
                            coords.push(c);
                        }
                    }
                }
                Geometry::LineString(LineString::new(coords))
            }
            "Polygon" => Geometry::Polygon(self.polygon(el, dim)?),
            "Surface" => {
                let patches = el.child("patches")?;
                let polys = patches
                    .children
                    .iter()
                    .map(|p| self.polygon(p, Self::dim(p, dim)))
                    .collect::<Option<Vec<_>>>()?;
                match <[Polygon<f64>; 1]>::try_from(polys) {
                    Ok([p]) => Geometry::Polygon(p),
                    Err(polys) => Geometry::MultiPolygon(MultiPolygon(polys)),
                }
            }
            "Envelope" => {
                let lower = Self::numbers(&el.child("lowerCorner")?.text)?;
                let upper = Self::numbers(&el.child("upperCorner")?.text)?;
                if lower.len() < 2 || upper.len() < 2 {
                    return None;
                }
                Geometry::Polygon(
                    Rect::new(
                        self.coord(lower[0], lower[1]),
                        self.coord(upper[0], upper[1]),
                    )
                    .to_polygon(),
                )
            }
            "MultiPoint" => Geometry::MultiPoint(MultiPoint(
                Self::members(el, "pointMember")
                    .map(|p| match self.geometry(p, dim)? {
                        Geometry::Point(p) => Some(p),
                        _ => None,
                    })
                    .collect::<Option<_>>()?,
            )),
            "MultiCurve" | "MultiLineString" => {
                let mut lines = Vec::new();
                for m in
                    Self::members(el, "curveMember").chain(Self::members(el, "lineStringMember"))
                {
                    match self.geometry(m, dim)? {
                        Geometry::LineString(l) => lines.push(l),
                        _ => return None,
                    }
                }
                Geometry::MultiLineString(MultiLineString(lines))
            }
            "MultiSurface" | "MultiPolygon" => {
                let mut polys = Vec::new();
                for m in
                    Self::members(el, "surfaceMember").chain(Self::members(el, "polygonMember"))
                {
                    match self.geometry(m, dim)? {
                        Geometry::Polygon(p) => polys.push(p),
                        Geometry::MultiPolygon(mp) => polys.extend(mp),
                        _ => return None,
                    }
                }
                Geometry::MultiPolygon(MultiPolygon(polys))
            }
            "MultiGeometry" => Geometry::GeometryCollection(GeometryCollection(
                Self::members(el, "geometryMember")
                    .map(|m| self.geometry(m, dim))
                    .collect::<Option<_>>()?,
            )),
            _ => return None,
        })
    }
}

/// Parses a GML 3.2 (or GML 2) geometry. `srsName` on the root element gives the CRS (CRS84
/// if absent); for EPSG:4326 the coordinates are latitude, longitude, as in GML.
pub(crate) fn parse_gml(value: &str) -> Option<Geom> {
    // An empty GML literal is an empty geometry (GeoSPARQL 1.0, requirement 16).
    if value.trim().is_empty() {
        return Some(Geom::new(
            geo::Geometry::GeometryCollection(geo::GeometryCollection(Vec::new())),
            Crs::Crs84,
        ));
    }
    let root = parse_xml(value)?;
    let crs = match root.attr("srsName") {
        Some(name) => srs(name)?,
        None => Crs::Crs84,
    };
    let gml = Gml {
        swap: crs == Crs::Epsg4326,
    };
    let geometry = gml.geometry(&root, 2)?;
    Some(Geom::new(geometry, crs))
}

/// Parses a KML 2.2 geometry (`Point`, `LineString`, `LinearRing`, `Polygon`,
/// `MultiGeometry`). KML coordinates are always longitude, latitude[, altitude] in WGS84.
pub(crate) fn parse_kml(value: &str) -> Option<Geom> {
    fn coords(el: &Element) -> Option<Vec<Coord<f64>>> {
        el.child("coordinates")?
            .text
            .split_whitespace()
            .map(|t| {
                let mut it = t.split(',').map(|v| v.trim().parse::<f64>().ok());
                Some(Coord {
                    x: it.next()??,
                    y: it.next()??,
                })
            })
            .collect()
    }
    fn ring(boundary: &Element) -> Option<LineString<f64>> {
        Some(LineString::new(coords(boundary.child("LinearRing")?)?))
    }
    fn geometry(el: &Element) -> Option<Geometry<f64>> {
        Some(match el.name.as_str() {
            "Point" => match coords(el)?.as_slice() {
                [c] => Geometry::Point(Point(*c)),
                _ => return None,
            },
            "LineString" | "LinearRing" => Geometry::LineString(LineString::new(coords(el)?)),
            "Polygon" => Geometry::Polygon(Polygon::new(
                ring(el.child("outerBoundaryIs")?)?,
                el.children_named("innerBoundaryIs")
                    .map(ring)
                    .collect::<Option<_>>()?,
            )),
            "MultiGeometry" => {
                let parts = el
                    .children
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.name.as_str(),
                            "Point" | "LineString" | "LinearRing" | "Polygon" | "MultiGeometry"
                        )
                    })
                    .map(geometry)
                    .collect::<Option<Vec<_>>>()?;
                homogeneous(parts)
            }
            _ => return None,
        })
    }
    let root = parse_xml(value)?;
    // Accept a bare geometry, or one wrapped in kml/Placemark.
    let mut el = &root;
    while matches!(el.name.as_str(), "kml" | "Placemark" | "Document") {
        el = el.children.iter().find(|c| {
            matches!(
                c.name.as_str(),
                "Placemark"
                    | "Document"
                    | "Point"
                    | "LineString"
                    | "LinearRing"
                    | "Polygon"
                    | "MultiGeometry"
            )
        })?;
    }
    Some(Geom::new(geometry(el)?, Crs::Crs84))
}

/// A KML MultiGeometry as a multi-geometry of one type when its parts allow it.
fn homogeneous(parts: Vec<Geometry<f64>>) -> Geometry<f64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use wkt::ToWkt;

    fn gml(s: &str) -> (String, Crs) {
        let g = parse_gml(s).expect(s);
        (g.geometry.wkt_string(), g.crs)
    }

    #[test]
    fn gml_geometries() {
        let ns = r#"xmlns:gml="http://www.opengis.net/gml/3.2""#;
        assert_eq!(
            gml(&format!(
                r#"<gml:Point {ns}><gml:pos>5 58</gml:pos></gml:Point>"#
            )),
            ("POINT(5 58)".into(), Crs::Crs84)
        );
        // EPSG:4326 is latitude, longitude in GML.
        assert_eq!(
            gml(&format!(
                r#"<gml:LineString {ns} srsName="http://www.opengis.net/def/crs/EPSG/0/4326"><gml:posList>58 5 59 6</gml:posList></gml:LineString>"#
            )),
            ("LINESTRING(5 58,6 59)".into(), Crs::Epsg4326)
        );
        let (w, crs) = gml(&format!(
            r#"<gml:Polygon {ns} srsName="urn:ogc:def:crs:EPSG::25832" srsDimension="3"><gml:exterior><gml:LinearRing><gml:posList>0 0 1 10 0 1 10 10 1 0 0 1</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon>"#
        ));
        assert_eq!(w, "POLYGON((0 0,10 0,10 10,0 0))");
        assert_eq!(
            crs,
            Crs::Other("http://www.opengis.net/def/crs/EPSG/0/25832".into())
        );
        assert_eq!(
            gml(&format!(
                r#"<gml:MultiCurve {ns}><gml:curveMember><gml:LineString><gml:pos>0 0</gml:pos><gml:pos>1 1</gml:pos></gml:LineString></gml:curveMember><gml:curveMember><gml:LineString><gml:posList>2 2 3 3</gml:posList></gml:LineString></gml:curveMember></gml:MultiCurve>"#
            ))
            .0,
            "MULTILINESTRING((0 0,1 1),(2 2,3 3))"
        );
        assert_eq!(
            gml(&format!(
                r#"<gml:Envelope {ns}><gml:lowerCorner>0 0</gml:lowerCorner><gml:upperCorner>2 1</gml:upperCorner></gml:Envelope>"#
            ))
            .0,
            "POLYGON((2 0,2 1,0 1,0 0,2 0))"
        );
        // References in text and attributes are resolved (quick-xml reports them apart from
        // the text since 0.38).
        assert_eq!(
            gml(&format!(
                r#"<gml:Point {ns} srsName="EPSG&#58;25832"><gml:pos>5&#32;58&#x20;</gml:pos></gml:Point>"#
            )),
            (
                "POINT(5 58)".into(),
                Crs::Other("http://www.opengis.net/def/crs/EPSG/0/25832".into())
            )
        );
        assert!(parse_gml("<gml:Point><gml:pos>1</gml:pos></gml:Point>").is_none());
        assert!(parse_gml("not xml").is_none());
        // An srsName that cannot be an IRI makes the literal invalid, not a CRS that a WKT
        // literal cannot hold (found by the geo_literals fuzz target).
        assert!(
            parse_gml(concat!(
                "<MultiSurface\rR(\u{e}\u{7}",
                "\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0,\0\0\0",
                "?=\r\"\u{c}\u{e}\0\"srsName\r=\"\u{e}INz>UR\0\"/>+>"
            ))
            .is_none()
        );
        for name in ["EPSG:4326 x", "urn:x>y", "http://example.org/{crs}"] {
            assert!(
                parse_gml(&format!(
                    r#"<gml:Point {ns} srsName="{name}"><gml:pos>5 58</gml:pos></gml:Point>"#
                ))
                .is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn kml_geometries() {
        let k = |s: &str| parse_kml(s).expect(s).geometry.wkt_string();
        assert_eq!(
            k("<Point><coordinates>5,58,10</coordinates></Point>"),
            "POINT(5 58)"
        );
        assert_eq!(
            k(
                r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Placemark><name>a</name><LineString><coordinates>5,58 6,59</coordinates></LineString></Placemark></kml>"#
            ),
            "LINESTRING(5 58,6 59)"
        );
        assert_eq!(
            k(
                "<Polygon><outerBoundaryIs><LinearRing><coordinates>0,0 1,0 1,1 0,0</coordinates></LinearRing></outerBoundaryIs></Polygon>"
            ),
            "POLYGON((0 0,1 0,1 1,0 0))"
        );
        assert_eq!(
            k(
                "<MultiGeometry><Point><coordinates>0,0</coordinates></Point><Point><coordinates>1,1</coordinates></Point></MultiGeometry>"
            ),
            "MULTIPOINT((0 0),(1 1))"
        );
    }
}
