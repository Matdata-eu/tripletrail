//! TripleTrail's geometry extensions, off unless asked for with an option, so queries written
//! for SPARQL Anything give the same graph:
//!
//! - `geo=true` on a JSON source: every GeoJSON geometry object (`{"type": "Point",
//!   "coordinates": …}`, and the other six types) gets `geo:asWKT` with its WKT, the
//!   coordinates written as they are in the file, and every GeoJSON `Feature` gets
//!   `geo:hasGeometry` to its geometry's container.
//! - `geo.wkt=<column>`, or `geo.x=<column>` and `geo.y=<column>` (and `geo.z`), on a CSV
//!   source: every row with a value there gets `geo:asWKT`. Columns are named by their
//!   header, or by number from 1.
//! - `geo.crs`: the CRS of the coordinates (`EPSG:25833`, or a CRS IRI); the WKT literal then
//!   starts with the CRS IRI. GeoJSON is longitude/latitude (CRS84) by default.

use crate::FxError;
use crate::options::Options;

pub(crate) const AS_WKT: &str = "http://www.opengis.net/ont/geosparql#asWKT";
pub(crate) const HAS_GEOMETRY: &str = "http://www.opengis.net/ont/geosparql#hasGeometry";
pub(crate) const WKT_LITERAL: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";

/// The `<IRI> ` prefix of WKT literals for `geo.crs`, empty for CRS84 or none.
pub(crate) fn crs_prefix(options: &Options) -> Result<String, FxError> {
    let Some(crs) = options.get("geo.crs").map(str::trim) else {
        return Ok(String::new());
    };
    let iri = if let Some(code) = crs
        .strip_prefix("EPSG:")
        .or_else(|| crs.strip_prefix("epsg:"))
    {
        if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(FxError::Option(format!(
                "geo.crs {crs:?}: not an EPSG code"
            )));
        }
        format!("http://www.opengis.net/def/crs/EPSG/0/{code}")
    } else if crs.eq_ignore_ascii_case("CRS84") || crs.eq_ignore_ascii_case("OGC:CRS84") {
        CRS84.to_owned()
    } else if crs.contains("://") && oxiri::Iri::parse(crs).is_ok() {
        crs.to_owned()
    } else {
        return Err(FxError::Option(format!(
            "geo.crs {crs:?}: give EPSG:<code> or a CRS IRI"
        )));
    };
    Ok(if iri == CRS84 {
        String::new()
    } else {
        format!("<{iri}> ")
    })
}

/// A column named by `geo.*`: by header name, or by number from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Column {
    Name(String),
    Number(usize),
}

impl Column {
    fn parse(s: &str) -> Self {
        match s.trim().parse::<usize>() {
            Ok(n) if n >= 1 => Column::Number(n),
            _ => Column::Name(s.trim().to_owned()),
        }
    }

    /// The position (from 0) of the column among `headers`.
    pub(crate) fn index(&self, headers: &[Option<String>]) -> Option<usize> {
        match self {
            Column::Number(n) => Some(n - 1),
            Column::Name(name) => headers.iter().position(|h| h.as_deref() == Some(name)),
        }
    }
}

/// The geometry columns of a table source.
#[derive(Debug, Clone)]
pub(crate) struct Columns {
    pub wkt: Option<Column>,
    /// x, y and maybe z.
    pub xyz: Option<(Column, Column, Option<Column>)>,
    pub crs: String,
}

impl Columns {
    /// `None` when no `geo.*` column option is given.
    pub(crate) fn from_options(options: &Options) -> Result<Option<Self>, FxError> {
        let wkt = options.get("geo.wkt").map(Column::parse);
        let x = options.get("geo.x").map(Column::parse);
        let y = options.get("geo.y").map(Column::parse);
        let z = options.get("geo.z").map(Column::parse);
        let xyz = match (x, y) {
            (Some(x), Some(y)) => Some((x, y, z)),
            (None, None) if z.is_none() => None,
            _ => {
                return Err(FxError::Option(
                    "geo.x and geo.y go together (and geo.z with them)".into(),
                ));
            }
        };
        if wkt.is_some() && xyz.is_some() {
            return Err(FxError::Option("geo.wkt or geo.x/geo.y, not both".into()));
        }
        if wkt.is_none() && xyz.is_none() {
            return Ok(None);
        }
        Ok(Some(Self {
            wkt,
            xyz,
            crs: crs_prefix(options)?,
        }))
    }

    /// Resolved against a table's headers: the positions (from 0) of the columns.
    pub(crate) fn resolve(&self, headers: &[Option<String>]) -> Result<Positions, FxError> {
        let find = |c: &Column| {
            c.index(headers)
                .ok_or_else(|| FxError::Option(format!("geo column {c:?}: no such column")))
        };
        Ok(match (&self.wkt, &self.xyz) {
            (Some(w), _) => Positions::Wkt(find(w)?),
            (None, Some((x, y, z))) => {
                Positions::Xyz(find(x)?, find(y)?, z.as_ref().map(find).transpose()?)
            }
            (None, None) => unreachable!("checked in from_options"),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Positions {
    Wkt(usize),
    Xyz(usize, usize, Option<usize>),
}

impl Positions {
    /// The columns it reads.
    pub(crate) fn columns(&self) -> Vec<usize> {
        match *self {
            Positions::Wkt(i) => vec![i],
            Positions::Xyz(x, y, z) => [Some(x), Some(y), z].into_iter().flatten().collect(),
        }
    }
}

impl Positions {
    /// The WKT of a row whose cell `i` is `cell(i)`, or none when the cells are empty or the
    /// coordinates not numbers.
    pub(crate) fn wkt<'a>(
        &self,
        crs: &str,
        cell: impl Fn(usize) -> Option<&'a str>,
    ) -> Option<String> {
        let value = |i: usize| cell(i).map(str::trim).filter(|v| !v.is_empty());
        match *self {
            Positions::Wkt(i) => {
                let w = value(i)?;
                Some(if w.starts_with('<') || crs.is_empty() {
                    w.to_owned()
                } else {
                    format!("{crs}{w}")
                })
            }
            Positions::Xyz(x, y, z) => {
                let n = |i: usize| value(i).filter(|v| is_number(v));
                let (x, y) = (n(x)?, n(y)?);
                Some(match z.and_then(n) {
                    Some(z) => format!("{crs}POINT Z({x} {y} {z})"),
                    None => format!("{crs}POINT({x} {y})"),
                })
            }
        }
    }
}

/// A decimal or scientific number as WKT accepts it.
pub(crate) fn is_number(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let digits = mantissa.bytes().filter(u8::is_ascii_digit).count();
    let dots = mantissa.bytes().filter(|&b| b == b'.').count();
    digits > 0
        && dots <= 1
        && digits + dots == mantissa.len()
        && exp.is_none_or(|e| {
            let e = e.strip_prefix(['-', '+']).unwrap_or(e);
            !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
        })
}

/// A GeoJSON position or nesting of positions, with numerals as written.
pub(crate) enum Coords<'a> {
    Number(&'a str),
    List(Vec<Coords<'a>>),
    Other,
}

/// The WKT of a GeoJSON geometry of type `kind` with `coordinates`, or of a
/// `GeometryCollection` with the WKT of its `geometries`; none when the shape is wrong.
pub(crate) fn geojson_wkt(kind: &str, coordinates: Option<&Coords<'_>>) -> Option<String> {
    let tag = match kind {
        "Point" => "POINT",
        "MultiPoint" => "MULTIPOINT",
        "LineString" => "LINESTRING",
        "MultiLineString" => "MULTILINESTRING",
        "Polygon" => "POLYGON",
        "MultiPolygon" => "MULTIPOLYGON",
        _ => return None,
    };
    let depth = match kind {
        "Point" => 0,
        "MultiPoint" | "LineString" => 1,
        "MultiLineString" | "Polygon" => 2,
        _ => 3,
    };
    let coordinates = coordinates?;
    let empty = matches!(coordinates, Coords::List(l) if l.is_empty());
    if empty {
        return Some(format!("{tag} EMPTY"));
    }
    let z = first_position(coordinates)?.len() >= 3;
    let mut out = String::from(tag);
    out.push_str(if z { " Z" } else { "" });
    write_nested(coordinates, depth, z, kind == "MultiPoint", &mut out)?;
    Some(out)
}

/// The numbers of the first position.
fn first_position<'a>(c: &'a Coords<'a>) -> Option<Vec<&'a str>> {
    match c {
        Coords::List(items) => match items.first()? {
            Coords::Number(_) => items
                .iter()
                .map(|i| match i {
                    Coords::Number(n) => Some(*n),
                    _ => None,
                })
                .collect(),
            inner => first_position(inner),
        },
        _ => None,
    }
}

/// `(…)` around the positions at `depth` levels below; MultiPoint positions get their own
/// parentheses.
fn write_nested(
    c: &Coords<'_>,
    depth: usize,
    z: bool,
    multipoint: bool,
    out: &mut String,
) -> Option<()> {
    let Coords::List(items) = c else { return None };
    if depth == 0 {
        out.push('(');
        write_position(items, z, out)?;
        out.push(')');
        return Some(());
    }
    out.push('(');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if depth == 1 {
            let Coords::List(p) = item else { return None };
            if multipoint {
                out.push('(');
            }
            write_position(p, z, out)?;
            if multipoint {
                out.push(')');
            }
        } else {
            write_nested(item, depth - 1, z, false, out)?;
        }
    }
    out.push(')');
    Some(())
}

fn write_position(p: &[Coords<'_>], z: bool, out: &mut String) -> Option<()> {
    let n = if z { 3 } else { 2 };
    if p.len() < n {
        return None;
    }
    for (i, c) in p.iter().take(n).enumerate() {
        let Coords::Number(v) = c else { return None };
        if i > 0 {
            out.push(' ');
        }
        out.push_str(v);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(v: &[&'static str]) -> Coords<'static> {
        Coords::List(v.iter().map(|n| Coords::Number(n)).collect())
    }

    #[test]
    fn geojson_to_wkt() {
        assert_eq!(
            geojson_wkt("Point", Some(&pos(&["10.75", "59.91"]))).unwrap(),
            "POINT(10.75 59.91)"
        );
        assert_eq!(
            geojson_wkt("Point", Some(&pos(&["1", "2", "3.0"]))).unwrap(),
            "POINT Z(1 2 3.0)"
        );
        let line = Coords::List(vec![pos(&["0", "0"]), pos(&["1", "1e3"])]);
        assert_eq!(
            geojson_wkt("LineString", Some(&line)).unwrap(),
            "LINESTRING(0 0, 1 1e3)"
        );
        assert_eq!(
            geojson_wkt("MultiPoint", Some(&line)).unwrap(),
            "MULTIPOINT((0 0), (1 1e3))"
        );
        let ring = Coords::List(vec![
            pos(&["0", "0"]),
            pos(&["1", "0"]),
            pos(&["1", "1"]),
            pos(&["0", "0"]),
        ]);
        let polygon = Coords::List(vec![ring]);
        assert_eq!(
            geojson_wkt("Polygon", Some(&polygon)).unwrap(),
            "POLYGON((0 0, 1 0, 1 1, 0 0))"
        );
        assert_eq!(
            geojson_wkt("MultiPolygon", Some(&Coords::List(vec![polygon]))).unwrap(),
            "MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)))"
        );
        assert_eq!(
            geojson_wkt("Polygon", Some(&Coords::List(vec![]))).unwrap(),
            "POLYGON EMPTY"
        );
        assert!(geojson_wkt("Point", Some(&pos(&["1"]))).is_none());
        assert!(geojson_wkt("Polygon", Some(&pos(&["1", "2"]))).is_none());
        assert!(geojson_wkt("Feature", None).is_none());
    }

    #[test]
    fn crs_and_numbers() {
        let o = |v: &str| Options([("geo.crs".to_owned(), v.to_owned())].into());
        assert_eq!(
            crs_prefix(&o("EPSG:25833")).unwrap(),
            "<http://www.opengis.net/def/crs/EPSG/0/25833> "
        );
        assert_eq!(crs_prefix(&o("CRS84")).unwrap(), "");
        assert!(crs_prefix(&o("EPSG:x")).is_err());
        assert!(is_number("-12.5") && is_number("1e-3") && is_number("7"));
        assert!(!is_number("") && !is_number("1,5") && !is_number("1.2.3") && !is_number("e5"));
    }
}
