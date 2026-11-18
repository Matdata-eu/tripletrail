//! GeoParquet export and import.
#![cfg(feature = "geoparquet")]

use tripletrail::Engine;
use tripletrail::geoparquet::{Bytes, ExportOptions, ImportMapping};
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::NamedNode;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.org/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:a ex:id "a" ; ex:name "Station A" ; ex:tracks 4 ; ex:length 12.5 ; ex:open true ;
  geo:hasGeometry [ geo:asWKT "POINT(4.35 50.85)"^^geo:wktLiteral ] .
ex:b ex:id "b" ; ex:name "Line B" ; ex:tracks 2 ; ex:length 3.0 ; ex:open false ;
  geo:hasGeometry [ geo:asWKT "LINESTRING(4 50, 5 51, 6 51.5)"^^geo:wktLiteral ] .
ex:c ex:id "c" ; ex:name "Yard C" ;
  geo:hasGeometry [ geo:asWKT "POLYGON((10 45, 11 45, 11 46, 10 46, 10 45))"^^geo:wktLiteral ] .
ex:d ex:id "d" ; ex:name "Signal D" ; ex:tracks 1 ;
  geo:hasGeometry [ geo:asWKT "POINT Z(5.5 50.5 120)"^^geo:wktLiteral ] .
"#;

const QUERY: &str = r#"
PREFIX ex: <http://example.org/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
SELECT ?id ?name ?tracks ?length ?open ?geometry WHERE {
  ?s ex:id ?id ; ex:name ?name ; geo:hasGeometry/geo:asWKT ?geometry .
  OPTIONAL { ?s ex:tracks ?tracks }
  OPTIONAL { ?s ex:length ?length }
  OPTIONAL { ?s ex:open ?open }
} ORDER BY ?id
"#;

fn source() -> Engine {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    engine
}

fn export(engine: &Engine, options: &ExportOptions) -> Vec<u8> {
    let mut file = Vec::new();
    let stats = engine.export_geoparquet(QUERY, &mut file, options).unwrap();
    assert_eq!(stats.rows, 4);
    assert_eq!(stats.geometry_columns, ["geometry"]);
    file
}

fn mapping() -> ImportMapping {
    ImportMapping {
        subject: Some("http://example.org/{id}".into()),
        predicate_prefix: "http://example.org/".into(),
        ..ImportMapping::default()
    }
}

fn rows(engine: &Engine, query: &str) -> Vec<Vec<String>> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            s.unwrap()
                .values()
                .iter()
                .map(|v| v.as_ref().map_or(String::new(), ToString::to_string))
                .collect()
        })
        .collect()
}

#[test]
fn round_trip() {
    let file = export(&source(), &ExportOptions::default());
    assert_eq!(&file[..4], b"PAR1");

    let target = Engine::new_tripletrail();
    let stats = target
        .load_geoparquet(Bytes::from(file), &mapping())
        .unwrap();
    assert_eq!(stats.rows, 4);
    assert_eq!(stats.invalid_geometries, 0);
    // The same rows come back, with typed values and the same geometries.
    let expected = rows(&source(), QUERY);
    let got = rows(&target, QUERY);
    assert_eq!(got.len(), 4);
    for (e, g) in expected.iter().zip(&got) {
        assert_eq!(e[..5], g[..5]);
    }
    let geometries: Vec<&str> = got.iter().map(|r| r[5].as_str()).collect();
    assert_eq!(
        geometries,
        [
            "\"POINT(4.35 50.85)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
            "\"LINESTRING(4 50,5 51,6 51.5)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
            "\"POLYGON((10 45,11 45,11 46,10 46,10 45))\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
            "\"POINT Z(5.5 50.5 120)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
        ]
    );
    // Geometry nodes are named after their feature.
    let nodes = rows(
        &target,
        "SELECT ?g WHERE { <http://example.org/a> <http://www.opengis.net/ont/geosparql#hasGeometry> ?g }",
    );
    assert_eq!(nodes, [["<http://example.org/a/geometry>"]]);
    // The loaded geometries are spatially indexed.
    let near = rows(
        &target,
        r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
        SELECT ?id WHERE { ?s <http://example.org/id> ?id ; geo:hasGeometry/geo:asWKT ?w
          FILTER(geof:sfIntersects(?w, "POLYGON((9 44, 12 44, 12 47, 9 47, 9 44))"^^geo:wktLiteral)) }"#,
    );
    assert_eq!(near, [["\"c\""]]);
}

#[test]
fn geo_metadata() {
    let file = export(&source(), &ExportOptions::default());
    let reader = parquet_metadata(&file);
    let geo: serde_json::Value = serde_json::from_str(&reader).unwrap();
    assert_eq!(geo["version"], "1.1.0");
    assert_eq!(geo["primary_column"], "geometry");
    let column = &geo["columns"]["geometry"];
    assert_eq!(column["encoding"], "WKB");
    assert_eq!(
        column["geometry_types"],
        serde_json::json!(["Point", "LineString", "Polygon", "Point Z"])
    );
    assert_eq!(column["bbox"], serde_json::json!([4.0, 45.0, 11.0, 51.5]));
    // CRS84 is GeoParquet's default: no crs.
    assert!(column.get("crs").is_none());
    assert_eq!(
        column["covering"]["bbox"]["xmin"],
        serde_json::json!(["bbox", "xmin"])
    );
}

fn parquet_metadata(file: &[u8]) -> String {
    use tripletrail::geoparquet::parquet::file::reader::{FileReader, SerializedFileReader};
    let reader = SerializedFileReader::new(Bytes::from(file.to_vec())).unwrap();
    let kv = reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .unwrap();
    kv.iter()
        .find(|kv| kv.key == "geo")
        .and_then(|kv| kv.value.clone())
        .unwrap()
}

#[test]
fn bbox_filter_skips_row_groups() {
    // One row per row group: the box keeps the Belgian features only.
    let file = export(
        &source(),
        &ExportOptions {
            row_group_size: 1,
            ..ExportOptions::default()
        },
    );
    let target = Engine::new_tripletrail();
    let stats = target
        .load_geoparquet(
            Bytes::from(file),
            &ImportMapping {
                bbox: Some([4.0, 50.0, 5.0, 51.0]),
                class: Some(NamedNode::new("http://example.org/Feature").unwrap()),
                graph: Some(NamedNode::new("http://example.org/g").unwrap()),
                ..mapping()
            },
        )
        .unwrap();
    assert_eq!(stats.rows, 2);
    assert_eq!(stats.skipped_rows, 2);
    let ids = rows(
        &target,
        "SELECT ?id WHERE { GRAPH <http://example.org/g> { ?s a <http://example.org/Feature> ; <http://example.org/id> ?id } } ORDER BY ?id",
    );
    assert_eq!(ids, [["\"a\""], ["\"b\""]]);
}

#[test]
fn crs_round_trip() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
            <http://example.org/x> geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(50.85 4.35)"^^geo:wktLiteral .
            <http://example.org/y> geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(150000 170000)"^^geo:wktLiteral ."#
                .as_bytes(),
        )
        .unwrap();
    for (s, expected) in [
        (
            "x",
            "\"<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(50.85 4.35)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
        ),
        (
            "y",
            "\"<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(150000 170000)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
        ),
    ] {
        let mut file = Vec::new();
        engine
            .export_geoparquet(
                &format!("SELECT ?w WHERE {{ <http://example.org/{s}> <http://www.opengis.net/ont/geosparql#asWKT> ?w }}"),
                &mut file,
                &ExportOptions::default(),
            )
            .unwrap();
        let target = Engine::new_tripletrail();
        target
            .load_geoparquet(Bytes::from(file), &ImportMapping::default())
            .unwrap();
        let got = rows(
            &target,
            "SELECT ?w WHERE { ?g <http://www.opengis.net/ont/geosparql#asWKT> ?w }",
        );
        assert_eq!(got, [[expected]]);
    }
}

#[test]
fn mixed_crs_column_is_refused() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
            <http://example.org/x> geo:asWKT "POINT(4 50)"^^geo:wktLiteral .
            <http://example.org/y> geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(150000 170000)"^^geo:wktLiteral ."#
                .as_bytes(),
        )
        .unwrap();
    let e = engine
        .export_geoparquet(
            "SELECT ?w WHERE { ?s <http://www.opengis.net/ont/geosparql#asWKT> ?w }",
            Vec::new(),
            &ExportOptions::default(),
        )
        .unwrap_err();
    assert!(e.to_string().contains("one CRS"), "{e}");
}
