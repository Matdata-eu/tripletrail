//! Results of GeoSPARQL functions as other engines write them (#372): an empty literal is an
//! empty geometry, set operations on areas give clean coordinates and the simplest type,
//! convex hulls start where JTS starts them, and a GML input gives a GML result.

use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn one(query: &str) -> String {
    let e = Engine::new_tripletrail();
    let query = format!(
        "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
         PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
         {query}"
    );
    let QueryResults::Solutions(mut s) = e.query(&query).unwrap() else {
        panic!("not a SELECT")
    };
    let row = s.next().unwrap().unwrap();
    row.iter()
        .next()
        .map(|(_, t)| t.to_string())
        .unwrap_or_default()
}

const A: &str = r#""<http://www.opengis.net/def/crs/OGC/1.3/CRS84> Polygon((-83.6 34.1, -83.2 34.1, -83.2 34.5, -83.6 34.5, -83.6 34.1))"^^geo:wktLiteral"#;
const B: &str =
    r#""Polygon((-83.6 34.1, -83.4 34.1, -83.4 34.3, -83.6 34.3, -83.6 34.1))"^^geo:wktLiteral"#;

#[test]
fn empty_literals_are_empty_geometries() {
    assert_eq!(
        one(
            r#"SELECT ?x { BIND(geof:sfEquals(""^^geo:wktLiteral, "LINESTRING EMPTY"^^geo:wktLiteral) AS ?x) }"#
        ),
        r#""true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#
    );
    assert_eq!(
        one(r#"SELECT ?x { BIND(geof:sfEquals(""^^geo:gmlLiteral, ""^^geo:gmlLiteral) AS ?x) }"#),
        r#""true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#
    );
}

#[test]
fn set_operations_give_clean_polygons() {
    let r = one(&format!(
        "SELECT ?x {{ BIND(geof:intersection({A}, {B}) AS ?x) }}"
    ));
    assert_eq!(
        r,
        r#""POLYGON((-83.6 34.3,-83.4 34.3,-83.4 34.1,-83.6 34.1,-83.6 34.3))"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"#
    );
    let r = one(&format!(
        "SELECT ?x {{ BIND(geof:difference({A}, {B}) AS ?x) }}"
    ));
    assert!(r.starts_with("\"POLYGON(("), "{r}");
    assert!(!r.contains("0000000"), "{r}");
}

#[test]
fn convex_hull_starts_at_the_lowest_point_clockwise() {
    assert_eq!(
        one(&format!("SELECT ?x {{ BIND(geof:convexHull({A}) AS ?x) }}")),
        r#""POLYGON((-83.6 34.1,-83.6 34.5,-83.2 34.5,-83.2 34.1,-83.6 34.1))"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"#
    );
}

#[test]
fn gml_in_gives_gml_out() {
    let gml = r#""<gml:Polygon xmlns:gml='http://www.opengis.net/ont/gml' srsName='http://www.opengis.net/def/crs/OGC/1.3/CRS84'><gml:exterior><gml:LinearRing><gml:posList>-83.6 34.1 -83.2 34.1 -83.2 34.5 -83.6 34.5 -83.6 34.1</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon>"^^geo:gmlLiteral"#;
    let r = one(&format!("SELECT ?x {{ BIND(geof:boundary({gml}) AS ?x) }}"));
    assert!(
        r.ends_with("^^<http://www.opengis.net/ont/geosparql#gmlLiteral>"),
        "{r}"
    );
    assert!(r.contains("<gml:LineString"), "{r}");
    assert!(r.contains("http://www.opengis.net/ont/gml"), "{r}");
}

#[test]
fn geodesic_area_ignores_ring_direction() {
    let area = |wkt: &str| -> f64 {
        let r = one(&format!(
            r#"SELECT ?x {{ BIND(geof:area("{wkt}"^^geo:wktLiteral) AS ?x) }}"#
        ));
        r.trim_start_matches('"')
            .split('"')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let ccw = area("POLYGON((4 50,5 50,5 51,4 51,4 50))");
    let cw = area("POLYGON((4 50,4 51,5 51,5 50,4 50))");
    assert!((ccw - 7.89e9).abs() < 1e8, "{ccw}");
    assert!((cw - ccw).abs() < 1.0, "{cw} {ccw}");
    // Set operation results are written clockwise, as GEOS does.
    let sq = r#""POLYGON((4 50,5 50,5 51,4 51,4 50))"^^geo:wktLiteral"#;
    let r = one(&format!(
        "SELECT ?x {{ BIND(geof:area(geof:intersection({sq}, {sq})) AS ?x) }}"
    ));
    let v: f64 = r
        .trim_start_matches('"')
        .split('"')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((v - ccw).abs() < 1.0, "{v} {ccw}");
    let r = one(&format!(
        "SELECT ?x {{ BIND(geof:area(geof:convexHull({sq})) AS ?x) }}"
    ));
    let v: f64 = r
        .trim_start_matches('"')
        .split('"')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((v - ccw).abs() < 1.0, "{v} {ccw}");
}
