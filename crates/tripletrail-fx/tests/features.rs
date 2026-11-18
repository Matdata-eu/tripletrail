//! Geodata files as features (TripleTrail's own formats): Shapefile, KML and GPX.

use oxigraph::model::{Dataset, Graph};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::path::Path;
use tripletrail_fx::{FxConfig, FxService};

/// The triples of the source the option triples name, as sorted N-Triples lines.
fn triples(base: &Path, options: &str) -> Vec<String> {
    try_triples(base, options).unwrap()
}

fn try_triples(base: &Path, options: &str) -> Result<Vec<String>, String> {
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: base.to_path_buf(),
            allowed: None,
            remote: Default::default(),
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(Dataset::new()));
    let QueryResults::Graph(triples) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!(
            "PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
            CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <x-sparql-anything:> {{
                fxe:properties fxe:root \"http://ex.org/d\" ; fxe:blank-nodes false ; {options} .
                ?s ?p ?o }} }}"
        ))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?
    else {
        panic!("not a CONSTRUCT")
    };
    let g: Graph = triples
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = g
        .iter()
        .map(|t| {
            t.to_string()
                .replace("http://ex.org/d", "d")
                .replace("http://sparql.xyz/facade-x/data/", "xyz:")
                .replace("http://www.opengis.net/ont/geosparql#", "geo:")
                .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
                .replace("http://www.w3.org/2001/XMLSchema#", "xsd:")
        })
        .collect();
    lines.sort();
    Ok(lines)
}

fn content(text: &str, media_type: &str, extra: &str) -> Vec<String> {
    let text = oxigraph::model::Literal::new_simple_literal(text);
    triples(
        Path::new("."),
        &format!("fxe:content {text} ; fxe:media-type \"{media_type}\" {extra}"),
    )
}

#[test]
fn shapefiles_give_typed_attributes_and_geometries() {
    use shapefile::dbase::{FieldValue, Record, TableWriterBuilder};
    let dir = tripletrail_testdir::tempdir("fx-shp");
    let table = TableWriterBuilder::new()
        .add_character_field("name".try_into().unwrap(), 20)
        .add_numeric_field("km".try_into().unwrap(), 10, 3)
        .add_logical_field("open".try_into().unwrap());
    {
        let mut w = shapefile::Writer::from_path(dir.join("lines.shp"), table).unwrap();
        let line = shapefile::Polyline::with_parts(vec![
            vec![
                shapefile::Point::new(10.0, 59.5),
                shapefile::Point::new(10.5, 60.0),
            ],
            vec![
                shapefile::Point::new(11.0, 60.0),
                shapefile::Point::new(11.5, 60.5),
            ],
        ]);
        let mut r = Record::default();
        r.insert("name".into(), FieldValue::Character(Some("Oslo S".into())));
        r.insert("km".into(), FieldValue::Numeric(Some(1.25)));
        r.insert("open".into(), FieldValue::Logical(Some(true)));
        w.write_shape_and_record(&line, &r).unwrap();
        let line = shapefile::Polyline::new(vec![
            shapefile::Point::new(5.0, 60.0),
            shapefile::Point::new(5.5, 60.25),
        ]);
        let mut r = Record::default();
        r.insert("name".into(), FieldValue::Character(None));
        r.insert("km".into(), FieldValue::Numeric(None));
        r.insert("open".into(), FieldValue::Logical(Some(false)));
        w.write_shape_and_record(&line, &r).unwrap();
    }
    let got = triples(
        &dir,
        "fxe:location \"lines.shp\" ; fxe:geo.crs \"EPSG:4258\"",
    );
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        got,
        [
            "<d#feature1/geometry> <geo:asWKT> \"<http://www.opengis.net/def/crs/EPSG/0/4258> MULTILINESTRING((10 59.5, 10.5 60), (11 60, 11.5 60.5))\"^^<geo:wktLiteral>",
            "<d#feature1> <geo:hasGeometry> <d#feature1/geometry>",
            "<d#feature1> <xyz:km> \"1.25\"^^<xsd:double>",
            "<d#feature1> <xyz:name> \"Oslo S\"",
            "<d#feature1> <xyz:open> \"true\"^^<xsd:boolean>",
            "<d#feature2/geometry> <geo:asWKT> \"<http://www.opengis.net/def/crs/EPSG/0/4258> LINESTRING(5 60, 5.5 60.25)\"^^<geo:wktLiteral>",
            "<d#feature2> <geo:hasGeometry> <d#feature2/geometry>",
            "<d#feature2> <xyz:open> \"false\"^^<xsd:boolean>",
            "<d> <rdf:_1> <d#feature1>",
            "<d> <rdf:_2> <d#feature2>",
            "<d> <rdf:type> <http://sparql.xyz/facade-x/ns/Root>",
        ]
    );
}

#[test]
fn kml_placemarks() {
    let kml = r##"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2"><Document><Folder>
  <Placemark><name>Station</name>
    <ExtendedData><Data name="code"><value>OSL</value></Data>
      <SchemaData schemaUrl="#s"><SimpleData name="tracks">19</SimpleData></SchemaData></ExtendedData>
    <Point><coordinates>10.75,59.91,0</coordinates></Point></Placemark>
  <Placemark><name>Yard</name><Polygon>
    <outerBoundaryIs><LinearRing><coordinates>0,0 4,0 4,4 0,0</coordinates></LinearRing></outerBoundaryIs>
    <innerBoundaryIs><LinearRing><coordinates>1,1 2,1 2,2 1,1</coordinates></LinearRing></innerBoundaryIs>
  </Polygon></Placemark>
  <Placemark><MultiGeometry><Point><coordinates>1,2</coordinates></Point>
    <LineString><coordinates>1,2 3,4</coordinates></LineString></MultiGeometry></Placemark>
</Folder></Document></kml>"##;
    let got = content(kml, "application/vnd.google-earth.kml+xml", "");
    let wkt: Vec<&String> = got.iter().filter(|l| l.contains("asWKT")).collect();
    assert_eq!(
        wkt,
        [
            "<d#feature1/geometry> <geo:asWKT> \"POINT(10.75 59.91)\"^^<geo:wktLiteral>",
            "<d#feature2/geometry> <geo:asWKT> \"POLYGON((0 0, 4 0, 4 4, 0 0), (1 1, 2 1, 2 2, 1 1))\"^^<geo:wktLiteral>",
            "<d#feature3/geometry> <geo:asWKT> \"GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(1 2, 3 4))\"^^<geo:wktLiteral>",
        ]
    );
    for line in [
        "<d#feature1> <xyz:name> \"Station\"",
        "<d#feature1> <xyz:code> \"OSL\"",
        "<d#feature1> <xyz:tracks> \"19\"",
        "<d#feature2> <xyz:name> \"Yard\"",
    ] {
        assert!(got.iter().any(|l| l == line), "{line} not in {got:#?}");
    }
}

#[test]
fn gpx_waypoints_routes_and_tracks() {
    let gpx = r#"<gpx version="1.1" creator="t" xmlns="http://www.topografix.com/GPX/1/1">
  <wpt lat="59.91" lon="10.75"><ele>4</ele><name>Oslo S</name></wpt>
  <rte><name>R</name><rtept lat="1" lon="2"/><rtept lat="3" lon="4"/></rte>
  <trk><name>T</name>
    <trkseg><trkpt lat="0" lon="0"/><trkpt lat="0" lon="1"/></trkseg>
    <trkseg><trkpt lat="1" lon="1"/><trkpt lat="1" lon="2"/></trkseg></trk>
</gpx>"#;
    let got = content(gpx, "application/gpx+xml", "");
    let wkt: Vec<&String> = got.iter().filter(|l| l.contains("asWKT")).collect();
    assert_eq!(
        wkt,
        [
            "<d#feature1/geometry> <geo:asWKT> \"POINT(10.75 59.91)\"^^<geo:wktLiteral>",
            "<d#feature2/geometry> <geo:asWKT> \"LINESTRING(2 1, 4 3)\"^^<geo:wktLiteral>",
            "<d#feature3/geometry> <geo:asWKT> \"MULTILINESTRING((0 0, 1 0), (1 1, 2 1))\"^^<geo:wktLiteral>",
        ]
    );
    for line in [
        "<d#feature1> <xyz:ele> \"4\"",
        "<d#feature1> <xyz:name> \"Oslo S\"",
        "<d#feature2> <xyz:name> \"R\"",
        "<d#feature3> <xyz:name> \"T\"",
    ] {
        assert!(got.iter().any(|l| l == line), "{line} not in {got:#?}");
    }
}

#[test]
fn kml_nested_past_the_limit_is_an_error() {
    let kml = |n: usize| {
        let text = oxigraph::model::Literal::new_simple_literal(format!(
            "<kml><Document><Placemark><MultiGeometry>{}<Point><coordinates>1,2</coordinates></Point>{}</MultiGeometry></Placemark></Document></kml>",
            "<MultiGeometry>".repeat(n),
            "</MultiGeometry>".repeat(n)
        ));
        try_triples(
            Path::new("."),
            &format!(
                "fxe:content {text} ; fxe:media-type \"application/vnd.google-earth.kml+xml\""
            ),
        )
    };
    assert!(kml(100).unwrap().iter().any(|t| t.contains("POINT")));
    let e = kml(20_000).unwrap_err();
    assert!(e.contains("nest more than 512 deep"), "{e}");
}

#[cfg(unix)]
#[test]
fn shapefile_siblings_outside_the_allowed_directories_are_denied() {
    use shapefile::dbase::{FieldValue, Record, TableWriterBuilder};
    let root = tripletrail_testdir::tempdir("fx-shp-allow");
    let (inside, outside) = (root.join("inside"), root.join("outside"));
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    {
        let table = TableWriterBuilder::new().add_character_field("name".try_into().unwrap(), 20);
        let mut w = shapefile::Writer::from_path(outside.join("secret.shp"), table).unwrap();
        let mut r = Record::default();
        r.insert("name".into(), FieldValue::Character(Some("secret".into())));
        w.write_shape_and_record(&shapefile::Point::new(5.0, 60.0), &r)
            .unwrap();
    }
    std::fs::copy(outside.join("secret.shp"), inside.join("a.shp")).unwrap();
    std::fs::copy(outside.join("secret.shx"), inside.join("a.shx")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.dbf"), inside.join("a.dbf")).unwrap();
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: inside.clone(),
            allowed: Some(vec![inside.clone()]),
            remote: Default::default(),
        },
        SparqlEvaluator::new,
    );
    let result = fx
        .register(SparqlEvaluator::new())
        .parse_query(
            "PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
            CONSTRUCT { ?s ?p ?o } WHERE { SERVICE <x-sparql-anything:> {
                fxe:properties fxe:location \"a.shp\" . ?s ?p ?o } }",
        )
        .unwrap()
        .on_queryable_dataset(&*Box::leak(Box::new(Dataset::new())))
        .execute();
    let text = match result {
        Ok(QueryResults::Graph(triples)) => triples
            .map(|t| t.map(|t| t.to_string()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.join("\n"))
            .unwrap_or_else(|e| e),
        Ok(_) => unreachable!(),
        Err(e) => e.to_string(),
    };
    std::fs::remove_dir_all(&root).unwrap();
    assert!(!text.contains("secret"), "{text}");
    assert!(text.contains("outside the directories"), "{text}");
}

/// The WKT literals of `got`, one per feature that has a geometry.
fn wkts(got: &[String]) -> Vec<String> {
    got.iter()
        .filter(|l| l.contains("asWKT"))
        .map(|l| l.split('"').nth(1).unwrap().to_owned())
        .collect()
}

#[test]
fn gpx_empty_parts_are_dropped() {
    let gpx = r#"<gpx version="1.1" creator="t" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><name>a</name><trkseg><trkpt lat="1" lon="2"/><trkpt lat="3" lon="4"/></trkseg><trkseg/></trk>
  <trk><name>b</name><trkseg><trkpt lat="1" lon="2"/></trkseg><trkseg/></trk>
  <trk><name>c</name></trk>
  <rte><name>d</name><rtept lat="1" lon="2"/></rte>
  <rte><name>e</name></rte>
</gpx>"#;
    let got = content(gpx, "application/gpx+xml", "");
    // An empty segment is dropped, a one-point line is that point, and a track or route
    // without points has no geometry (but is still a feature).
    assert_eq!(
        wkts(&got),
        ["LINESTRING(2 1, 4 3)", "POINT(2 1)", "POINT(2 1)"]
    );
    assert!(got.iter().any(|l| l == "<d#feature5> <xyz:name> \"e\""));
    assert!(!got.iter().any(|l| l.starts_with("<d#feature3/geometry>")));
}

#[test]
fn kml_empty_parts_are_dropped() {
    let placemark = |g: &str| format!("<Placemark>{g}</Placemark>");
    let kml = format!(
        "<kml><Document>{}</Document></kml>",
        [
            "<LineString><coordinates></coordinates></LineString>",
            "<LineString><coordinates>1,2</coordinates></LineString>",
            "<LinearRing><coordinates>1,2 3,4</coordinates></LinearRing>",
            "<Polygon><innerBoundaryIs><LinearRing><coordinates>0,0 1,0 1,1 0,0</coordinates></LinearRing></innerBoundaryIs></Polygon>",
            "<Polygon><outerBoundaryIs><LinearRing><coordinates>0,0 4,0 4,4 0,0</coordinates></LinearRing></outerBoundaryIs>\
             <innerBoundaryIs><LinearRing><coordinates>1,1 2,2</coordinates></LinearRing></innerBoundaryIs></Polygon>",
            "<MultiGeometry></MultiGeometry>",
            "<MultiGeometry><LineString><coordinates></coordinates></LineString><Point><coordinates>5,6</coordinates></Point></MultiGeometry>",
        ]
        .map(placemark)
        .concat()
    );
    let got = content(&kml, "application/vnd.google-earth.kml+xml", "");
    assert_eq!(
        wkts(&got),
        [
            "POINT(1 2)",
            "LINESTRING(1 2, 3 4)",
            "POLYGON((0 0, 4 0, 4 4, 0 0))",
            "POINT(5 6)",
        ]
    );
}

#[test]
fn coordinates_that_are_not_finite_are_errors() {
    let kml = oxigraph::model::Literal::new_simple_literal(
        "<kml><Placemark><Point><coordinates>nan,inf</coordinates></Point></Placemark></kml>",
    );
    let e = try_triples(
        Path::new("."),
        &format!("fxe:content {kml} ; fxe:media-type \"application/vnd.google-earth.kml+xml\""),
    )
    .unwrap_err();
    assert!(
        e.contains("KML: a coordinate is not a finite number"),
        "{e}"
    );
    let gpx =
        oxigraph::model::Literal::new_simple_literal(r#"<gpx><wpt lat="NaN" lon="1"/></gpx>"#);
    let e = try_triples(
        Path::new("."),
        &format!("fxe:content {gpx} ; fxe:media-type \"application/gpx+xml\""),
    )
    .unwrap_err();
    assert!(
        e.contains("GPX: a coordinate is not a finite number"),
        "{e}"
    );
}

#[test]
fn shapefile_empty_and_non_finite_shapes() {
    use shapefile::dbase::{FieldValue, Record, TableWriterBuilder};
    let dir = tripletrail_testdir::tempdir("fx-shp-empty");
    let table = || TableWriterBuilder::new().add_character_field("name".try_into().unwrap(), 20);
    let record = || {
        let mut r = Record::default();
        r.insert("name".into(), FieldValue::Character(Some("a".into())));
        r
    };
    {
        let mut w = shapefile::Writer::from_path(dir.join("points.shp"), table()).unwrap();
        for _ in 0..2 {
            w.write_shape_and_record(
                &shapefile::Multipoint::new(vec![shapefile::Point::new(1.0, 2.0)]),
                &record(),
            )
            .unwrap();
        }
        let mut w = shapefile::Writer::from_path(dir.join("nan.shp"), table()).unwrap();
        w.write_shape_and_record(&shapefile::Point::new(f64::NAN, 2.0), &record())
            .unwrap();
    }
    // The writer cannot write an empty multipoint, so empty the first record by hand: its
    // content shrinks from 28 words to 20 (type, box, a count of 0) and the file by 8.
    let mut shp = std::fs::read(dir.join("points.shp")).unwrap();
    let words = |b: &[u8]| u32::from_be_bytes(b.try_into().unwrap());
    let length = words(&shp[24..28]) - 8;
    shp[24..28].copy_from_slice(&length.to_be_bytes());
    shp[104..108].copy_from_slice(&20u32.to_be_bytes());
    shp[144..148].copy_from_slice(&0u32.to_le_bytes());
    shp.drain(148..164);
    std::fs::write(dir.join("points.shp"), &shp).unwrap();
    let mut shx = std::fs::read(dir.join("points.shx")).unwrap();
    shx[104..108].copy_from_slice(&20u32.to_be_bytes());
    let second = words(&shx[108..112]) - 8;
    shx[108..112].copy_from_slice(&second.to_be_bytes());
    std::fs::write(dir.join("points.shx"), &shx).unwrap();
    let got = triples(&dir, "fxe:location \"points.shp\"");
    let nan = try_triples(&dir, "fxe:location \"nan.shp\"");
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(wkts(&got), ["MULTIPOINT((1 2))"]);
    let e = nan.unwrap_err();
    assert!(
        e.contains("Shapefile: a coordinate is not a finite number"),
        "{e}"
    );
}
