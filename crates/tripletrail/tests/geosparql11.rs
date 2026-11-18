//! GeoSPARQL 1.1 functions beyond the Simple Features relations, through SPARQL.

use tripletrail::Engine;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
"#;

fn value(e: &Engine, expr: &str) -> Option<String> {
    let q = format!("{PREFIXES} SELECT ({expr} AS ?v) WHERE {{}}");
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    s.next().unwrap().unwrap().get("v").map(|t| match t {
        tripletrail::oxigraph::model::Term::Literal(l) => l.value().to_owned(),
        other => other.to_string(),
    })
}

fn wkt(s: &str) -> String {
    format!("\"{s}\"^^geo:wktLiteral")
}

#[test]
fn egenhofer_and_rcc8() {
    let e = Engine::new().unwrap();
    let a = wkt("POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))");
    let inner = wkt("POLYGON((1 1, 2 1, 2 2, 1 2, 1 1))");
    let touching_inner = wkt("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))");
    let neighbour = wkt("POLYGON((4 0, 6 0, 6 4, 4 4, 4 0))");
    let far = wkt("POLYGON((10 10, 11 10, 11 11, 10 11, 10 10))");
    let overlapping = wkt("POLYGON((3 3, 5 3, 5 5, 3 5, 3 3))");
    let cases = [
        ("ehEquals", &a, &a, true),
        ("ehDisjoint", &a, &far, true),
        ("ehDisjoint", &a, &neighbour, false),
        ("ehMeet", &a, &neighbour, true),
        ("ehMeet", &a, &far, false),
        ("ehOverlap", &a, &overlapping, true),
        ("ehCovers", &a, &touching_inner, true),
        ("ehCovers", &a, &inner, false), // GeoSPARQL's ehCovers needs boundary contact
        ("ehCoveredBy", &touching_inner, &a, true),
        ("ehInside", &inner, &a, true),
        ("ehInside", &touching_inner, &a, false),
        ("ehContains", &a, &inner, true),
        ("rcc8eq", &a, &a, true),
        ("rcc8dc", &a, &far, true),
        ("rcc8ec", &a, &neighbour, true),
        ("rcc8po", &a, &overlapping, true),
        ("rcc8tpp", &touching_inner, &a, true),
        ("rcc8tppi", &a, &touching_inner, true),
        ("rcc8ntpp", &inner, &a, true),
        ("rcc8ntppi", &a, &inner, true),
        ("rcc8ntpp", &touching_inner, &a, false),
    ];
    for (f, x, y, expected) in cases {
        assert_eq!(
            value(&e, &format!("geof:{f}({x}, {y})")),
            Some(expected.to_string()),
            "{f}"
        );
    }
}

#[test]
fn accessors() {
    let e = Engine::new().unwrap();
    let line3d = wkt("LINESTRING Z(0 0 5, 1 1 7)");
    assert_eq!(
        value(&e, &format!("geof:is3D({line3d})")).as_deref(),
        Some("true")
    );
    assert_eq!(
        value(&e, &format!("geof:minZ({line3d})")).as_deref(),
        Some("5")
    );
    assert_eq!(
        value(&e, &format!("geof:maxZ({line3d})")).as_deref(),
        Some("7")
    );
    assert_eq!(
        value(&e, &format!("geof:coordinateDimension({line3d})")).as_deref(),
        Some("3")
    );
    let m = wkt("POINT M(1 2 3)");
    assert_eq!(
        value(&e, &format!("geof:isMeasured({m})")).as_deref(),
        Some("true")
    );
    assert_eq!(
        value(&e, &format!("geof:spatialDimension({m})")).as_deref(),
        Some("2")
    );
    assert_eq!(
        value(&e, &format!("geof:is3D({})", wkt("POINT(1 2)"))).as_deref(),
        Some("false")
    );
    assert_eq!(
        value(&e, &format!("geof:minZ({})", wkt("POINT(1 2)"))),
        None
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:geometryType({})", wkt("MULTIPOINT((1 2),(3 4))"))
        )
        .as_deref(),
        Some("http://www.opengis.net/ont/sf#MultiPoint")
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:geometryN({}, 2)", wkt("MULTIPOINT((1 2),(3 4))"))
        )
        .as_deref(),
        Some("POINT(3 4)")
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:geometryN({}, 3)", wkt("MULTIPOINT((1 2),(3 4))"))
        ),
        None
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:isSimple({})", wkt("LINESTRING(0 0, 2 2, 2 0, 0 2)"))
        )
        .as_deref(),
        Some("false")
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:isSimple({})", wkt("LINESTRING(0 0, 1 1, 2 0)"))
        )
        .as_deref(),
        Some("true")
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:isSimple({})", wkt("LINESTRING(0 0, 1 0, 1 1, 0 0)"))
        )
        .as_deref(),
        Some("true")
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:boundary({})", wkt("LINESTRING(0 0, 1 1, 2 0)"))
        )
        .as_deref(),
        Some("MULTIPOINT((0 0),(2 0))")
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:boundary({})", wkt("POLYGON((0 0, 1 0, 1 1, 0 0))"))
        )
        .as_deref(),
        Some("LINESTRING(0 0,1 0,1 1,0 0)")
    );
}

#[test]
fn hulls_and_circles_cover_their_input() {
    let e = Engine::new().unwrap();
    let pts = wkt("MULTIPOINT((0 0),(4 0),(4 4),(0 4),(2 1),(1 3))");
    for f in ["boundingCircle", "concaveHull", "convexHull"] {
        let covers = value(
            &e,
            &format!(
                "geof:ehCovers(geof:{f}({pts}), {pts}) || geof:sfContains(geof:{f}({pts}), {pts})"
            ),
        );
        assert_eq!(covers.as_deref(), Some("true"), "{f}");
    }
}

#[test]
fn transform_and_serialise() {
    let e = Engine::new().unwrap();
    let p = wkt("POINT(10.752245 59.91084)");
    let utm = value(
        &e,
        &format!("geof:transform({p}, <http://www.opengis.net/def/crs/EPSG/0/25832>)"),
    )
    .unwrap();
    assert!(
        utm.starts_with("<http://www.opengis.net/def/crs/EPSG/0/25832> POINT(597991.4"),
        "{utm}"
    );
    // Back to EPSG:4326, which is written latitude first.
    let back = value(
        &e,
        &format!(
            "geof:transform(\"{}\"^^geo:wktLiteral, <http://www.opengis.net/def/crs/EPSG/0/4326>)",
            utm.replace('"', "\\\"")
        ),
    )
    .unwrap();
    // Round trip within a millimetre (about 1e-8 degrees).
    let coords: Vec<f64> = back
        .rsplit("POINT(")
        .next()
        .unwrap()
        .trim_end_matches(')')
        .split(' ')
        .map(|v| v.parse().unwrap())
        .collect();
    assert!(
        (coords[0] - 59.91084).abs() < 1e-8 && (coords[1] - 10.752245).abs() < 1e-8,
        "{back}"
    );
    assert_eq!(
        value(
            &e,
            &format!("geof:asGeoJSON({})", wkt("LINESTRING(1 2, 3 4)"))
        )
        .as_deref(),
        Some(r#"{"type":"LineString","coordinates":[[1,2],[3,4]]}"#)
    );
    // GeoJSON is always lon/lat, so projected input is transformed first.
    let gj = value(&e, &format!("geof:asGeoJSON(\"{}\"^^geo:wktLiteral)", utm)).unwrap();
    assert!(
        gj.starts_with(r#"{"type":"Point","coordinates":[10.75224"#),
        "{gj}"
    );
    assert_eq!(
        value(&e, &format!("geof:asKML({})", wkt("POINT(1 2)"))).as_deref(),
        Some("<Point><coordinates>1,2</coordinates></Point>")
    );
    assert_eq!(
        value(&e, &format!("geof:asGML({})", wkt("POINT(1 2)"))).as_deref(),
        Some(
            r#"<gml:Point xmlns:gml="http://www.opengis.net/gml/3.2" srsName="http://www.opengis.net/def/crs/OGC/1.3/CRS84"><gml:pos>1 2</gml:pos></gml:Point>"#
        )
    );
    // GeoJSON literals are accepted as input everywhere.
    assert_eq!(
        value(&e, r#"geof:sfIntersects("{\"type\":\"Point\",\"coordinates\":[1,1]}"^^geo:geoJSONLiteral, "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"^^geo:wktLiteral)"#).as_deref(),
        Some("true")
    );
}

#[test]
fn aggregates() {
    let e = Engine::new().unwrap();
    let agg = |f: &str| {
        let q = format!(
            r#"{PREFIXES} SELECT (geof:{f}(?g) AS ?v) WHERE {{
                VALUES ?g {{ {} {} {} }} }}"#,
            wkt("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"),
            wkt("POLYGON((1 1, 3 1, 3 3, 1 3, 1 1))"),
            wkt("POINT(10 0)"),
        );
        let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
            panic!()
        };
        match s.next().unwrap().unwrap().get("v") {
            Some(tripletrail::oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
            other => panic!("{f}: {other:?}"),
        }
    };
    let bbox = agg("aggBoundingBox");
    let envelope = format!("geof:envelope({})", wkt("LINESTRING(0 0, 10 3)"));
    assert_eq!(
        value(&e, &format!("geof:sfEquals({}, {envelope})", wkt(&bbox))).as_deref(),
        Some("true"),
        "{bbox}"
    );
    assert!(
        agg("aggUnion").starts_with("GEOMETRYCOLLECTION(POLYGON(("),
        "{}",
        agg("aggUnion")
    );
    assert!(agg("aggCentroid").starts_with("POINT("));
    assert!(agg("aggConvexHull").starts_with("POLYGON(("));
    assert!(agg("aggConcaveHull").starts_with("POLYGON(("));
    assert!(agg("aggBoundingCircle").starts_with("POLYGON(("));
    // The union dissolves the overlap: area 4 + 4 - 1.
    let q = format!(
        r#"{PREFIXES} PREFIX st: <http://w3id.org/tripletrail/st#>
        SELECT (st:ST_Area(geof:aggUnion(?g)) AS ?v) WHERE {{ VALUES ?g {{ {} {} }} }}"#,
        wkt("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"),
        wkt("POLYGON((1 1, 3 1, 3 3, 1 3, 1 1))"),
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    let area = s.next().unwrap().unwrap().get("v").unwrap().to_string();
    assert!(area.starts_with("\"7"), "{area}");
}

#[test]
fn gml_and_kml_literals() {
    let e = Engine::new().unwrap();
    let window = wkt("POLYGON((4 57, 7 57, 7 60, 4 60, 4 57))");
    let gml = r#""<gml:Point xmlns:gml='http://www.opengis.net/gml/3.2' srsName='http://www.opengis.net/def/crs/EPSG/0/4326'><gml:pos>58 5</gml:pos></gml:Point>"^^geo:gmlLiteral"#;
    let kml = r#""<Point xmlns='http://www.opengis.net/kml/2.2'><coordinates>5,58</coordinates></Point>"^^geo:kmlLiteral"#;
    let expected = [
        (
            gml,
            "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(58 5)",
        ),
        (kml, "POINT(5 58)"),
    ];
    for (lit, as_wkt) in expected {
        assert_eq!(
            value(&e, &format!("geof:sfWithin({lit}, {window})")).as_deref(),
            Some("true"),
            "{lit}"
        );
        assert_eq!(
            value(&e, &format!("geof:asWKT({lit})")).as_deref(),
            Some(as_wkt),
            "{lit}"
        );
    }
    // asGML and asKML output parses back to the same geometry.
    let line = wkt("<http://www.opengis.net/def/crs/EPSG/0/4326> LINESTRING(58 5, 59 6)");
    for (out, dt) in [("asGML", "gmlLiteral"), ("asKML", "kmlLiteral")] {
        let text = value(&e, &format!("geof:{out}({line})")).unwrap();
        let back = format!("\"{}\"^^geo:{dt}", text.replace('"', "\\\""));
        assert_eq!(
            value(&e, &format!("geof:sfEquals({back}, {line})")).as_deref(),
            Some("true"),
            "{text}"
        );
    }
}

#[test]
fn dggs_cells() {
    let e = Engine::new().unwrap();
    let h3 = "<http://w3id.org/tripletrail/dggs#h3Literal>";
    // A point is one cell; resolution 9 cells of Oslo Central are about 0.1 km².
    let cell = value(
        &e,
        &format!("geof:asDGGS({}, {h3})", wkt("POINT(10.7522 59.9111)")),
    )
    .unwrap();
    let list = cell
        .strip_prefix("CELLLIST ((")
        .and_then(|s| s.strip_suffix("))"))
        .unwrap();
    assert_eq!(list.split(' ').count(), 1);
    assert!(list.starts_with("89"), "{list}"); // resolution 9
    // The cell list is a geometry: it contains the point it came from.
    let literal = format!("\"{cell}\"^^{h3}");
    assert_eq!(
        value(
            &e,
            &format!(
                "geof:sfContains({literal}, {})",
                wkt("POINT(10.7522 59.9111)")
            )
        )
        .as_deref(),
        Some("true")
    );
    // A track segment is a chain of cells; a polygon is covered completely.
    let track = wkt("LINESTRING(10.70 59.90, 10.80 59.95)");
    let line = value(&e, &format!("geof:asDGGS({track}, {h3}, 8)")).unwrap();
    assert!(line.matches(' ').count() > 5, "{line}");
    let area = wkt("POLYGON((10.70 59.90, 10.80 59.90, 10.80 59.95, 10.70 59.95, 10.70 59.90))");
    let cover = value(&e, &format!("geof:asDGGS({area}, {h3}, 7)")).unwrap();
    let cover = format!("\"{cover}\"^^{h3}");
    assert_eq!(
        value(&e, &format!("geof:sfContains({cover}, {area})")).as_deref(),
        Some("true")
    );
    // Projected input is transformed first; unsupported datatypes give no value.
    let utm = wkt("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(262000 6650000)");
    assert!(value(&e, &format!("geof:asDGGS({utm}, {h3})")).is_some());
    assert_eq!(
        value(
            &e,
            &format!("geof:asDGGS({}, geo:wktLiteral)", wkt("POINT(1 2)"))
        ),
        None
    );
}
