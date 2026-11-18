//! GeoParquet import and export (cargo feature `geoparquet`).
//!
//! **Export** ([`Engine::export_geoparquet`]) writes the solutions of a SPARQL `SELECT` as a
//! GeoParquet 1.1 file: one column per variable, typed from its values. Columns whose values are
//! all geometry literals become WKB geometry columns, with their CRS, geometry types and extent
//! in the `geo` metadata; the first one is the primary column and gets a `bbox` covering column,
//! which lets readers skip row groups outside an area.
//!
//! **Import** ([`Engine::load_geoparquet`]) maps each row to triples: a subject from an IRI
//! template over the row's columns (a blank node without one), one predicate per column, and
//! each geometry column as `geo:hasGeometry` / `geo:asWKT`. An optional bounding box keeps only
//! the rows whose geometry intersects it, skipping whole row groups by their `bbox` statistics
//! when the file has a covering column.

use crate::{Engine, Error};
use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Float64Builder, Int64Builder, StringBuilder,
};
use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type};
use arrow_array::{Array, ArrayRef, RecordBatch, StructArray};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, Field, Fields, Schema};
use geo::{BoundingRect, Geometry};
use oxigraph::model::vocab::{rdf, xsd};
use oxigraph::model::{
    BlankNode, GraphName, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, Quad, Term,
};
use oxigraph::sparql::QueryResults;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use parquet::file::reader::ChunkReader;
use parquet::file::statistics::Statistics;
use rayon::prelude::*;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use tripletrail_geo::{Crs, Geom, parse_wkt_literal_uncached};

pub use bytes::Bytes;
pub use parquet;

const GEO: &str = "http://www.opengis.net/ont/geosparql#";
const HEX_BINARY: &str = "http://www.w3.org/2001/XMLSchema#hexBinary";
const EPSG: &str = "http://www.opengis.net/def/crs/EPSG/0/";

/// PROJJSON of EPSG:4326, as PROJ writes it. CRS84, the GeoParquet default, is written as no
/// `crs` at all.
const EPSG_4326_PROJJSON: &str = r#"{"type":"GeographicCRS","name":"WGS 84","datum_ensemble":{"name":"World Geodetic System 1984 ensemble","members":[{"name":"World Geodetic System 1984 (Transit)","id":{"authority":"EPSG","code":1166}},{"name":"World Geodetic System 1984 (G730)","id":{"authority":"EPSG","code":1152}},{"name":"World Geodetic System 1984 (G873)","id":{"authority":"EPSG","code":1153}},{"name":"World Geodetic System 1984 (G1150)","id":{"authority":"EPSG","code":1154}},{"name":"World Geodetic System 1984 (G1674)","id":{"authority":"EPSG","code":1155}},{"name":"World Geodetic System 1984 (G1762)","id":{"authority":"EPSG","code":1156}},{"name":"World Geodetic System 1984 (G2139)","id":{"authority":"EPSG","code":1309}},{"name":"World Geodetic System 1984 (G2296)","id":{"authority":"EPSG","code":1383}}],"ellipsoid":{"name":"WGS 84","semi_major_axis":6378137,"inverse_flattening":298.257223563},"accuracy":"2.0","id":{"authority":"EPSG","code":6326}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"},{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"}]},"scope":"Horizontal component of 3D system.","area":"World.","bbox":{"south_latitude":-90,"west_longitude":-180,"north_latitude":90,"east_longitude":180},"id":{"authority":"EPSG","code":4326}}"#;

fn err(message: impl std::fmt::Display) -> Error {
    Error::GeoParquet(message.to_string())
}

/// Options of [`Engine::export_geoparquet`].
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Rows per row group (and per bbox statistics block).
    pub row_group_size: usize,
    /// Writes the `bbox` covering column of the primary geometry column.
    pub bbox_column: bool,
    /// PROJJSON to write for geometry columns whose CRS is neither CRS84 nor EPSG:4326 (there is
    /// no PROJ database here). Without it, such a column gets a PROJJSON object holding only
    /// the CRS's `id`, which some readers (pyproj, GeoPandas) refuse.
    pub projjson: Option<String>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            row_group_size: 64 * 1024,
            bbox_column: true,
            projjson: None,
        }
    }
}

/// What [`Engine::export_geoparquet`] wrote.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportStats {
    pub rows: usize,
    /// The geometry columns, the primary one first.
    pub geometry_columns: Vec<String>,
}

/// How [`Engine::load_geoparquet`] maps rows to triples.
#[derive(Debug, Clone)]
pub struct ImportMapping {
    /// The subject IRI of a row, with `{column}` placeholders (values percent-encoded), e.g.
    /// `http://example.org/station/{id}`, or `{+column}` for values inserted as they are (a
    /// column of IRIs: `{+feature}`; such a column is not also mapped to a predicate). Without
    /// one, each row is a blank node.
    pub subject: Option<String>,
    /// Prepended to a column name to make its predicate, e.g. `http://example.org/def#`
    /// (`urn:x-tripletrail:column:` by default).
    pub predicate_prefix: String,
    /// An `rdf:type` of every row.
    pub class: Option<NamedNode>,
    /// The graph to load into; the default graph without one.
    pub graph: Option<NamedNode>,
    /// Only rows whose geometry intersects this box (min x, min y, max x, max y, in the
    /// coordinates of the file's primary geometry column).
    pub bbox: Option<[f64; 4]>,
    /// The geometry columns to read; those of the `geo` metadata by default.
    pub geometry_columns: Option<Vec<String>>,
}

impl Default for ImportMapping {
    fn default() -> Self {
        Self {
            subject: None,
            predicate_prefix: "urn:x-tripletrail:column:".into(),
            class: None,
            graph: None,
            bbox: None,
            geometry_columns: None,
        }
    }
}

/// What [`Engine::load_geoparquet`] loaded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportStats {
    pub rows: usize,
    pub triples: usize,
    /// Rows left out by the bounding box (row groups skipped by their statistics included).
    pub skipped_rows: usize,
    /// Geometry values that are not valid WKB, left out.
    pub invalid_geometries: usize,
}

// --- Export ------------------------------------------------------------------------------------

/// The type of an exported column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Geometry,
    Bool,
    Long,
    Decimal,
    Double,
    String,
}

fn kind_of(t: &Term) -> Kind {
    let Term::Literal(l) = t else {
        return Kind::String;
    };
    let dt = l.datatype();
    if tripletrail_geo::is_geometry_datatype(dt.as_str()) && parse_wkt_literal_uncached(t).is_some()
    {
        Kind::Geometry
    } else if dt == xsd::BOOLEAN && matches!(l.value(), "true" | "false" | "1" | "0") {
        Kind::Bool
    } else if is_integer_type(dt) && l.value().parse::<i64>().is_ok() {
        Kind::Long
    } else if dt == xsd::DECIMAL && decimal_parts(l.value()).is_some() {
        Kind::Decimal
    } else if (dt == xsd::DOUBLE || dt == xsd::FLOAT) && l.value().parse::<f64>().is_ok() {
        Kind::Double
    } else {
        Kind::String
    }
}

fn is_integer_type(dt: NamedNodeRef<'_>) -> bool {
    [
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
    .contains(&dt)
}

fn merge(a: Option<Kind>, b: Kind) -> Kind {
    match (a, b) {
        (None, b) => b,
        (Some(a), b) if a == b => a,
        (Some(Kind::Long), Kind::Decimal) | (Some(Kind::Decimal), Kind::Long) => Kind::Decimal,
        (Some(Kind::Long | Kind::Decimal), Kind::Double)
        | (Some(Kind::Double), Kind::Long | Kind::Decimal) => Kind::Double,
        _ => Kind::String,
    }
}

/// The digits of an `xsd:decimal` (or integer) lexical form: sign, integer digits, fraction
/// digits.
fn decimal_parts(v: &str) -> Option<(bool, &str, &str)> {
    let (negative, v) = match v.as_bytes().first()? {
        b'-' => (true, &v[1..]),
        b'+' => (false, &v[1..]),
        _ => (false, v),
    };
    let (int, frac) = v.split_once('.').unwrap_or((v, ""));
    (!(int.is_empty() && frac.is_empty())
        && int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()))
    .then_some((negative, int, frac))
}

/// A decimal value scaled to `scale` fraction digits, if it fits.
fn scaled_decimal(t: &Term, scale: usize) -> Option<i128> {
    let Term::Literal(l) = t else { return None };
    let (negative, int, frac) = decimal_parts(l.value())?;
    let frac = frac.trim_end_matches('0');
    (frac.len() <= scale).then_some(())?;
    let digits = format!("{int}{frac:0<scale$}");
    let v: i128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    Some(if negative { -v } else { v })
}

fn text_of(t: &Term) -> String {
    match t {
        Term::Literal(l) => l.value().to_owned(),
        Term::NamedNode(n) => n.as_str().to_owned(),
        other => other.to_string(),
    }
}

fn bool_of(t: &Term) -> Option<bool> {
    match t {
        Term::Literal(l) => Some(matches!(l.value(), "true" | "1")),
        _ => None,
    }
}

fn number_of<T: std::str::FromStr>(t: &Term) -> Option<T> {
    match t {
        Term::Literal(l) => l.value().parse().ok(),
        _ => None,
    }
}

/// GeoParquet's name of a geometry type.
fn type_name(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) => "Point",
        Geometry::Line(_) | Geometry::LineString(_) => "LineString",
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => "Polygon",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
    }
}

/// The PROJJSON of a CRS: `None` for CRS84, GeoParquet's default.
fn projjson(crs: &Crs, custom: Option<&str>) -> Result<Option<Value>, Error> {
    let parse =
        |s: &str| serde_json::from_str::<Value>(s).map_err(|e| err(format!("PROJJSON: {e}")));
    match crs {
        Crs::Crs84 => Ok(None),
        Crs::Epsg4326 => parse(EPSG_4326_PROJJSON).map(Some),
        Crs::Other(_) if custom.is_some() => parse(custom.unwrap_or_default()).map(Some),
        Crs::Other(iri) => {
            // http://www.opengis.net/def/crs/{authority}/{version}/{code}
            let parts: Vec<&str> = iri
                .strip_prefix("http://www.opengis.net/def/crs/")
                .map(|r| r.split('/').collect())
                .unwrap_or_default();
            let id = match parts.as_slice() {
                [authority, _, code] => match code.parse::<i64>() {
                    Ok(n) => json!({"authority": authority, "code": n}),
                    Err(_) => json!({"authority": authority, "code": code}),
                },
                _ => return Err(err(format!("no PROJJSON for the CRS {iri}"))),
            };
            Ok(Some(json!({ "id": id })))
        }
    }
}

/// One exported geometry column.
struct GeomColumn {
    crs: Option<Crs>,
    types: Vec<&'static str>,
    /// A geometry type GeoParquet cannot name (with M, or a 3-D surface): types are unknown.
    unknown_types: bool,
    extent: Option<[f64; 4]>,
}

impl Engine {
    /// Runs a SPARQL `SELECT` and writes its solutions as a GeoParquet 1.1 file (see the
    /// [module documentation](crate::geoparquet)). The solutions are held in memory to type
    /// the columns.
    pub fn export_geoparquet<W: Write + Send>(
        &self,
        query: &str,
        writer: W,
        options: &ExportOptions,
    ) -> Result<ExportStats, Error> {
        let QueryResults::Solutions(solutions) = self.query(query)? else {
            return Err(err("GeoParquet export needs a SELECT query"));
        };
        let names: Vec<String> = solutions
            .variables()
            .iter()
            .map(|v| v.as_str().to_owned())
            .collect();
        let mut rows = Vec::new();
        for solution in solutions {
            rows.push(solution?.values().to_vec());
        }
        write_geoparquet(&names, &rows, writer, options)
    }
}

/// Writes rows of terms (`None` for unbound) as GeoParquet.
pub fn write_geoparquet<W: Write + Send>(
    names: &[String],
    rows: &[Vec<Option<Term>>],
    writer: W,
    options: &ExportOptions,
) -> Result<ExportStats, Error> {
    // Column kinds, from all values.
    let kinds: Vec<Kind> = (0..names.len())
        .map(|c| {
            rows.iter()
                .filter_map(|r| r[c].as_ref())
                .fold(None, |k, t| Some(merge(k, kind_of(t))))
                .unwrap_or(Kind::String)
        })
        .collect();
    let geometry_columns: Vec<usize> = (0..names.len())
        .filter(|&c| kinds[c] == Kind::Geometry)
        .collect();
    let primary = geometry_columns.first().copied();

    let mut fields = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();
    let mut geo_columns = serde_json::Map::new();
    let mut primary_boxes: Vec<Option<[f64; 4]>> = Vec::new();
    for (c, name) in names.iter().enumerate() {
        let values = rows.iter().map(|r| r[c].as_ref());
        let (data_type, array): (DataType, ArrayRef) = match kinds[c] {
            Kind::Geometry => {
                let mut b = BinaryBuilder::new();
                let mut col = GeomColumn {
                    crs: None,
                    types: Vec::new(),
                    unknown_types: false,
                    extent: None,
                };
                for v in values {
                    let Some(g) = v.and_then(parse_wkt_literal_uncached) else {
                        b.append_null();
                        if Some(c) == primary {
                            primary_boxes.push(None);
                        }
                        continue;
                    };
                    let bbox = add_geometry(&mut col, &g, name)?;
                    if Some(c) == primary {
                        primary_boxes.push(bbox);
                    }
                    b.append_value(tripletrail_geo::wkb(&g));
                }
                let mut meta = serde_json::Map::new();
                meta.insert("encoding".into(), json!("WKB"));
                let types = if col.unknown_types {
                    Vec::new()
                } else {
                    col.types
                };
                meta.insert("geometry_types".into(), json!(types));
                if let Some(e) = col.extent {
                    meta.insert("bbox".into(), json!(e));
                }
                if let Some(p) = projjson(
                    col.crs.as_ref().unwrap_or(&Crs::Crs84),
                    options.projjson.as_deref(),
                )? {
                    meta.insert("crs".into(), p);
                }
                geo_columns.insert(name.clone(), Value::Object(meta));
                (DataType::Binary, Arc::new(b.finish()))
            }
            Kind::Bool => {
                let mut b = BooleanBuilder::new();
                values.for_each(|v| b.append_option(v.and_then(bool_of)));
                (DataType::Boolean, Arc::new(b.finish()))
            }
            Kind::Long => {
                let mut b = Int64Builder::new();
                values.for_each(|v| b.append_option(v.and_then(number_of::<i64>)));
                (DataType::Int64, Arc::new(b.finish()))
            }
            Kind::Decimal => {
                // Scale: the most fraction digits; precision 38 (Decimal128).
                let parts = || {
                    rows.iter()
                        .filter_map(|r| r[c].as_ref())
                        .filter_map(|t| match t {
                            Term::Literal(l) => decimal_parts(l.value()).map(|(_, i, f)| {
                                (
                                    i.trim_start_matches('0').len(),
                                    f.trim_end_matches('0').len(),
                                )
                            }),
                            _ => None,
                        })
                };
                let scale = parts().map(|p| p.1).max().unwrap_or(0);
                let int = parts().map(|p| p.0).max().unwrap_or(0);
                if scale + int <= 38 {
                    let array = values
                        .map(|v| v.and_then(|t| scaled_decimal(t, scale)))
                        .collect::<arrow_array::Decimal128Array>()
                        .with_precision_and_scale(38, scale as i8)
                        .map_err(|e| err(e.to_string()))?;
                    (DataType::Decimal128(38, scale as i8), Arc::new(array))
                } else {
                    let mut b = Float64Builder::new();
                    values.for_each(|v| b.append_option(v.and_then(number_of::<f64>)));
                    (DataType::Float64, Arc::new(b.finish()))
                }
            }
            Kind::Double => {
                let mut b = Float64Builder::new();
                values.for_each(|v| b.append_option(v.and_then(number_of::<f64>)));
                (DataType::Float64, Arc::new(b.finish()))
            }
            Kind::String => {
                let mut b = StringBuilder::new();
                values.for_each(|v| b.append_option(v.map(text_of)));
                (DataType::Utf8, Arc::new(b.finish()))
            }
        };
        fields.push(Field::new(name, data_type, true));
        arrays.push(array);
    }

    // The covering column of the primary geometry column.
    if let (Some(p), true) = (primary, options.bbox_column) {
        let mut bbox_name = "bbox".to_owned();
        while names.contains(&bbox_name) {
            bbox_name.insert(0, '_');
        }
        let corners = ["xmin", "ymin", "xmax", "ymax"];
        let child_fields: Fields = corners
            .iter()
            .map(|n| Field::new(*n, DataType::Float64, true))
            .collect();
        let children: Vec<ArrayRef> = (0..4)
            .map(|i| {
                Arc::new(
                    primary_boxes
                        .iter()
                        .map(|b| b.map(|b| b[i]))
                        .collect::<arrow_array::Float64Array>(),
                ) as ArrayRef
            })
            .collect();
        let bbox = StructArray::try_new(child_fields.clone(), children, None)
            .map_err(|e| err(e.to_string()))?;
        fields.push(Field::new(&bbox_name, DataType::Struct(child_fields), true));
        arrays.push(Arc::new(bbox));
        let covering: serde_json::Map<String, Value> = corners
            .iter()
            .map(|c| (c.to_string(), json!([bbox_name, c])))
            .collect();
        if let Some(Value::Object(meta)) = geo_columns.get_mut(&names[p]) {
            meta.insert("covering".into(), json!({ "bbox": covering }));
        }
    }

    let schema = Arc::new(Schema::new(fields));
    let mut properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_row_count(Some(options.row_group_size.max(1)));
    if let Some(p) = primary {
        let geo = json!({
            "version": "1.1.0",
            "primary_column": names[p],
            "columns": geo_columns,
        });
        properties = properties
            .set_key_value_metadata(Some(vec![KeyValue::new("geo".into(), geo.to_string())]));
    }
    let mut w = ArrowWriter::try_new(writer, schema.clone(), Some(properties.build()))
        .map_err(|e| err(e.to_string()))?;
    let batch = RecordBatch::try_new(schema, arrays).map_err(|e| err(e.to_string()))?;
    w.write(&batch).map_err(|e| err(e.to_string()))?;
    w.close().map_err(|e| err(e.to_string()))?;
    Ok(ExportStats {
        rows: rows.len(),
        geometry_columns: geometry_columns.iter().map(|&c| names[c].clone()).collect(),
    })
}

/// Adds a geometry to its column's metadata; returns its bounding box.
fn add_geometry(col: &mut GeomColumn, g: &Geom, name: &str) -> Result<Option<[f64; 4]>, Error> {
    match &col.crs {
        None => col.crs = Some(g.crs.clone()),
        Some(crs) if *crs != g.crs => {
            return Err(err(format!(
                "column {name} mixes the CRSs {crs:?} and {:?}: a GeoParquet column has one CRS",
                g.crs
            )));
        }
        Some(_) => {}
    }
    let zm = g.zm.as_deref();
    if g.surface.is_some() || zm.is_some_and(|zm| zm.has_m) {
        col.unknown_types = true;
    } else {
        let t = type_name(&g.geometry);
        let t: &'static str = if zm.is_some_and(|zm| zm.has_z) {
            match t {
                "Point" => "Point Z",
                "LineString" => "LineString Z",
                "Polygon" => "Polygon Z",
                "MultiPoint" => "MultiPoint Z",
                "MultiLineString" => "MultiLineString Z",
                "MultiPolygon" => "MultiPolygon Z",
                _ => "GeometryCollection Z",
            }
        } else {
            t
        };
        if !col.types.contains(&t) {
            col.types.push(t);
        }
    }
    let bbox = g
        .geometry
        .bounding_rect()
        .map(|r| [r.min().x, r.min().y, r.max().x, r.max().y]);
    if let Some(b) = bbox {
        col.extent = Some(match col.extent {
            None => b,
            Some(e) => [
                e[0].min(b[0]),
                e[1].min(b[1]),
                e[2].max(b[2]),
                e[3].max(b[3]),
            ],
        });
    }
    Ok(bbox)
}

// --- Import ------------------------------------------------------------------------------------

/// The CRS a GeoParquet `crs` value names (CRS84 when absent or null).
fn crs_of_projjson(crs: Option<&Value>) -> Result<Crs, Error> {
    let Some(crs) = crs.filter(|c| !c.is_null()) else {
        return Ok(Crs::Crs84);
    };
    let id = crs.get("id").or_else(|| {
        // A bound or compound CRS: the id of its base.
        crs.get("base_crs").and_then(|b| b.get("id"))
    });
    let authority = id.and_then(|i| i.get("authority")).and_then(Value::as_str);
    let code = id.and_then(|i| i.get("code")).map(|c| match c {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    match (authority, code.as_deref()) {
        (Some("OGC"), Some("CRS84")) => Ok(Crs::Crs84),
        (Some("EPSG"), Some("4326")) => Ok(Crs::Epsg4326),
        (Some("EPSG"), Some(code)) => Ok(Crs::Other(format!("{EPSG}{code}"))),
        (Some(authority), Some(code)) => Ok(Crs::Other(format!(
            "http://www.opengis.net/def/crs/{authority}/0/{code}"
        ))),
        _ => Err(err(
            "a geometry column's CRS has no id (authority and code)",
        )),
    }
}

/// Percent-encodes a value for an IRI template.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A parsed subject template: literal text and column references, in order.
enum Part {
    Text(String),
    /// A column, and whether its value is inserted as is (`{+column}`).
    Column(usize, bool),
}

fn parse_template(template: &str, schema: &Schema) -> Result<Vec<Part>, Error> {
    let mut parts = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        parts.push(Part::Text(rest[..start].to_owned()));
        let end = rest[start..]
            .find('}')
            .ok_or_else(|| err(format!("unclosed {{ in the subject template {template}")))?;
        let name = &rest[start + 1..start + end];
        let (name, raw) = match name.strip_prefix('+') {
            Some(n) => (n, true),
            None => (name, false),
        };
        let index = schema
            .index_of(name)
            .map_err(|_| err(format!("the subject template names no column {name}")))?;
        parts.push(Part::Column(index, raw));
        rest = &rest[start + end + 1..];
    }
    parts.push(Part::Text(rest.to_owned()));
    Ok(parts)
}

/// The bytes of a binary array value.
fn bytes_at(array: &dyn Array, i: usize) -> Option<&[u8]> {
    if array.is_null(i) {
        return None;
    }
    match array.data_type() {
        DataType::Binary => Some(array.as_binary::<i32>().value(i)),
        DataType::LargeBinary => Some(array.as_binary::<i64>().value(i)),
        DataType::BinaryView => Some(array.as_binary_view().value(i)),
        _ => None,
    }
}

fn typed(value: impl Into<String>, datatype: NamedNodeRef<'_>) -> Term {
    Literal::new_typed_literal(value, datatype).into()
}

fn xsd_double(v: f64) -> Term {
    let s = if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "INF" } else { "-INF" }.to_owned()
    } else {
        format!("{v:?}")
    };
    typed(s, xsd::DOUBLE)
}

/// The term of a non-geometry value.
fn value_term(array: &dyn Array, formatter: &ArrayFormatter<'_>, i: usize) -> Option<Term> {
    if array.is_null(i) {
        return None;
    }
    let text = || formatter.value(i).to_string();
    Some(match array.data_type() {
        DataType::Boolean => Literal::from(array.as_boolean().value(i)).into(),
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64 => typed(text(), xsd::INTEGER),
        DataType::Float32 => xsd_double(f64::from(array.as_primitive::<Float32Type>().value(i))),
        DataType::Float64 => xsd_double(array.as_primitive::<Float64Type>().value(i)),
        DataType::Decimal32(..)
        | DataType::Decimal64(..)
        | DataType::Decimal128(..)
        | DataType::Decimal256(..) => typed(text(), xsd::DECIMAL),
        DataType::Date32 | DataType::Date64 => typed(text(), xsd::DATE),
        DataType::Timestamp(..) => typed(text(), xsd::DATE_TIME),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => {
            let hex: String = bytes_at(array, i)?
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect();
            typed(hex, NamedNodeRef::new_unchecked(HEX_BINARY))
        }
        _ => Literal::new_simple_literal(text()).into(),
    })
}

fn intersects(a: &[f64; 4], b: &[f64; 4]) -> bool {
    a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3]
}

impl Engine {
    /// Loads a GeoParquet file (or any Parquet file, whose binary geometry columns are then
    /// named by [`ImportMapping::geometry_columns`]) as triples, mapped as
    /// [`ImportMapping`] says (see the [module documentation](crate::geoparquet)).
    ///
    /// Geometries are read from WKB; GeoArrow native encodings are refused. Values that are
    /// not valid WKB are left out and counted. A read error stops the load, after the rows
    /// read so far are loaded.
    pub fn load_geoparquet<R: ChunkReader + 'static>(
        &self,
        reader: R,
        mapping: &ImportMapping,
    ) -> Result<ImportStats, Error> {
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(reader).map_err(|e| err(e.to_string()))?;
        let metadata = builder.metadata().clone();
        let schema = builder.schema().clone();
        let geo: Option<Value> = metadata
            .file_metadata()
            .key_value_metadata()
            .and_then(|kv| kv.iter().find(|kv| kv.key == "geo"))
            .and_then(|kv| kv.value.as_deref())
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| err(format!("geo metadata: {e}")))?;
        let geo_columns = geo.as_ref().and_then(|g| g.get("columns"));

        // Geometry columns: (index, CRS, name).
        let names: Vec<String> = match (&mapping.geometry_columns, geo_columns) {
            (Some(names), _) => names.clone(),
            (None, Some(Value::Object(columns))) => {
                // The primary column first.
                let primary = geo
                    .as_ref()
                    .and_then(|g| g.get("primary_column"))
                    .and_then(Value::as_str);
                let mut names: Vec<String> = columns.keys().cloned().collect();
                names.sort_by_key(|n| Some(n.as_str()) != primary);
                names
            }
            _ => Vec::new(),
        };
        let mut geometries = Vec::new();
        for name in &names {
            let index = schema
                .index_of(name)
                .map_err(|_| err(format!("no geometry column {name}")))?;
            let meta = geo_columns.and_then(|c| c.get(name));
            let encoding = meta
                .and_then(|m| m.get("encoding"))
                .and_then(Value::as_str)
                .unwrap_or("WKB");
            if !encoding.eq_ignore_ascii_case("WKB") {
                return Err(err(format!(
                    "column {name} is {encoding}-encoded: only WKB geometry columns are read"
                )));
            }
            let crs = crs_of_projjson(meta.and_then(|m| m.get("crs")))?;
            geometries.push((index, crs, name.clone()));
        }

        let template = mapping
            .subject
            .as_deref()
            .map(|t| parse_template(t, &schema))
            .transpose()?;
        // Columns that are not mapped: the geometry columns, the covering columns, and the
        // columns the subject is (`{+column}`).
        let mut skip: Vec<usize> = geometries.iter().map(|g| g.0).collect();
        skip.extend(template.iter().flatten().filter_map(|p| match p {
            Part::Column(c, true) => Some(*c),
            _ => None,
        }));
        let mut covering: Option<[String; 4]> = None;
        for (i, name) in names.iter().enumerate() {
            let cover = geo_columns
                .and_then(|c| c.get(name))
                .and_then(|m| m.get("covering"))
                .and_then(|c| c.get("bbox"));
            let Some(cover) = cover else { continue };
            let path = |corner: &str| -> Option<String> {
                let parts: Vec<&str> = cover
                    .get(corner)?
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                Some(parts.join("."))
            };
            if let (Some(a), Some(b), Some(c), Some(d)) =
                (path("xmin"), path("ymin"), path("xmax"), path("ymax"))
            {
                if let Some(top) = a.split('.').next()
                    && let Ok(index) = schema.index_of(top)
                {
                    skip.push(index);
                }
                if i == 0 {
                    covering = Some([a, b, c, d]);
                }
            }
        }

        // Row groups: those whose bbox statistics may intersect the box.
        let mut stats = ImportStats::default();
        let mut row_groups = Vec::new();
        for (g, rg) in metadata.row_groups().iter().enumerate() {
            let keep = match (&mapping.bbox, &covering) {
                (Some(bbox), Some(paths)) => {
                    let bound = |path: &str, min: bool| -> Option<f64> {
                        let column = rg
                            .columns()
                            .iter()
                            .find(|c| c.column_path().string() == path)?;
                        match column.statistics()? {
                            Statistics::Double(s) => {
                                if min { s.min_opt() } else { s.max_opt() }.copied()
                            }
                            Statistics::Float(s) => {
                                if min { s.min_opt() } else { s.max_opt() }.map(|v| f64::from(*v))
                            }
                            _ => None,
                        }
                    };
                    // Rows can only intersect if min x ≤ box max x, max x ≥ box min x, ...
                    let group = [
                        bound(&paths[0], true),
                        bound(&paths[1], true),
                        bound(&paths[2], false),
                        bound(&paths[3], false),
                    ];
                    match group {
                        [Some(a), Some(b), Some(c), Some(d)] => intersects(&[a, b, c, d], bbox),
                        _ => true,
                    }
                }
                _ => true,
            };
            if keep {
                row_groups.push(g);
            } else {
                stats.skipped_rows += rg.num_rows() as usize;
            }
        }

        let reader = builder
            .with_row_groups(row_groups)
            .with_batch_size(65536)
            .build()
            .map_err(|e| err(e.to_string()))?;
        let graph: GraphName = mapping
            .graph
            .clone()
            .map_or(GraphName::DefaultGraph, GraphName::NamedNode);
        let predicates: Vec<NamedNode> = schema
            .fields()
            .iter()
            .map(|f| {
                NamedNode::new(format!("{}{}", mapping.predicate_prefix, encode(f.name())))
                    .map_err(|e| err(format!("predicate of column {}: {e}", f.name())))
            })
            .collect::<Result<_, _>>()?;
        let has_geometry = NamedNode::new_unchecked(format!("{GEO}hasGeometry"));
        let as_wkt = NamedNode::new_unchecked(format!("{GEO}asWKT"));
        let geometry_class = NamedNode::new_unchecked(format!("{GEO}Geometry"));
        let options = FormatOptions::default();

        // The triples of one row (rows are mapped in parallel).
        #[derive(Default)]
        struct Row {
            quads: Vec<Quad>,
            invalid_geometries: usize,
            skipped: bool,
            error: Option<String>,
        }
        let map_row =
            |batch: &RecordBatch, formatters: &[Option<ArrayFormatter<'_>>], row: usize| -> Row {
                let mut out = Row::default();
                // Geometries first: the box filter needs them.
                let mut row_geometries = Vec::new();
                for (index, crs, name) in &geometries {
                    let Some(bytes) = bytes_at(batch.column(*index).as_ref(), row) else {
                        continue;
                    };
                    match tripletrail_geo::wkb_literal(bytes, Some(crs.clone())) {
                        Some(t) => row_geometries.push((name, t)),
                        None => out.invalid_geometries += 1,
                    }
                }
                if let Some(bbox) = &mapping.bbox {
                    let inside = row_geometries.first().is_some_and(|(_, t)| {
                        parse_wkt_literal_uncached(t)
                            .and_then(|g| g.geometry.bounding_rect())
                            .is_some_and(|r| {
                                intersects(&[r.min().x, r.min().y, r.max().x, r.max().y], bbox)
                            })
                    });
                    if !inside {
                        out.skipped = true;
                        return out;
                    }
                }
                let subject: NamedOrBlankNode = match &template {
                    None => BlankNode::default().into(),
                    Some(parts) => {
                        let mut iri = String::new();
                        for part in parts {
                            match part {
                                Part::Text(t) => iri.push_str(t),
                                Part::Column(c, raw) => {
                                    if let Some(f) = &formatters[*c]
                                        && !batch.column(*c).is_null(row)
                                    {
                                        let value = f.value(row).to_string();
                                        iri.push_str(&if *raw { value } else { encode(&value) });
                                    }
                                }
                            }
                        }
                        match NamedNode::new(iri) {
                            Ok(n) => n.into(),
                            Err(e) => {
                                out.error = Some(format!("subject IRI: {e}"));
                                return out;
                            }
                        }
                    }
                };
                let mut push = |s: NamedOrBlankNode, p: NamedNode, o: Term| {
                    out.quads.push(Quad::new(s, p, o, graph.clone()));
                };
                if let Some(class) = &mapping.class {
                    push(subject.clone(), rdf::TYPE.into(), class.clone().into());
                }
                for (c, column) in batch.columns().iter().enumerate() {
                    if skip.contains(&c) {
                        continue;
                    }
                    let Some(f) = &formatters[c] else { continue };
                    if let Some(o) = value_term(column.as_ref(), f, row) {
                        push(subject.clone(), predicates[c].clone(), o);
                    }
                }
                for (k, (name, literal)) in row_geometries.into_iter().enumerate() {
                    let node: NamedOrBlankNode = match &subject {
                        NamedOrBlankNode::NamedNode(n) if k == 0 => {
                            NamedNode::new_unchecked(format!("{}/geometry", n.as_str())).into()
                        }
                        NamedOrBlankNode::NamedNode(n) => NamedNode::new_unchecked(format!(
                            "{}/geometry/{}",
                            n.as_str(),
                            encode(name)
                        ))
                        .into(),
                        _ => BlankNode::default().into(),
                    };
                    push(subject.clone(), has_geometry.clone(), node.clone().into());
                    push(
                        node.clone(),
                        rdf::TYPE.into(),
                        geometry_class.clone().into(),
                    );
                    push(node, as_wkt.clone(), literal);
                }
                out
            };

        // A thread reads and maps the row groups while this one writes the previous batch's
        // triples to the store.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<Quad>>(2);
        let (loaded, (stats, failure)) = std::thread::scope(|scope| {
            let producer = scope.spawn(move || {
                let mut stats = stats;
                for batch in reader {
                    let batch = match batch {
                        Ok(b) => b,
                        Err(e) => return (stats, Some(err(e.to_string()))),
                    };
                    let rows: Vec<Row> = (0..batch.num_rows())
                        .into_par_iter()
                        .map_init(
                            || -> Vec<Option<ArrayFormatter<'_>>> {
                                batch
                                    .columns()
                                    .iter()
                                    .map(|c| ArrayFormatter::try_new(c.as_ref(), &options).ok())
                                    .collect()
                            },
                            |formatters, row| map_row(&batch, formatters, row),
                        )
                        .collect();
                    let mut quads = Vec::new();
                    for row in rows {
                        stats.invalid_geometries += row.invalid_geometries;
                        if let Some(e) = row.error {
                            return (stats, Some(err(e)));
                        }
                        if row.skipped {
                            stats.skipped_rows += 1;
                            continue;
                        }
                        stats.rows += 1;
                        quads.extend(row.quads);
                    }
                    stats.triples += quads.len();
                    if tx.send(quads).is_err() {
                        break; // the store failed
                    }
                }
                (stats, None)
            });
            let loaded = self.store().load_quad_batches(rx);
            (loaded, producer.join().expect("GeoParquet reader thread"))
        });
        // As `Engine::load`: a bulk load invalidates the spatial index (rebuilt in the
        // background or on first use) rather than patching it geometry by geometry.
        self.invalidate();
        loaded?;
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(stats)
    }
}
