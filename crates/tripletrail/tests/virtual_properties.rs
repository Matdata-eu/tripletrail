//! GeoSPARQL properties computed from geometries (`?g geo:hasMetricLength ?l`).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix ex: <http://example.org/> .

ex:track geo:hasDefaultGeometry ex:trackGeom .
ex:trackGeom geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/25833> LINESTRING(0 0, 300 400)"^^geo:wktLiteral .
ex:yard geo:hasGeometry ex:yardGeom .
ex:yardGeom geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/25833> POLYGON((0 0, 10 0, 10 20, 0 20, 0 0))"^^geo:wktLiteral .
ex:signal geo:hasDefaultGeometry ex:signalGeom .
ex:signalGeom geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(5 5)"^^geo:wktLiteral ;
    geo:hasMetricSize 1.5 .
"#;

fn rows(e: &Engine, q: &str) -> Vec<Vec<String>> {
    let q = format!(
        "PREFIX geo: <http://www.opengis.net/ont/geosparql#> PREFIX ex: <http://example.org/> {q}"
    );
    let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
        panic!()
    };
    let vars: Vec<_> = s.variables().to_vec();
    let mut out: Vec<Vec<String>> = s
        .map(|r| {
            let r = r.unwrap();
            vars.iter()
                .map(|v| match r.get(v) {
                    Some(tripletrail::oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
                    Some(t) => t.to_string(),
                    None => "-".to_owned(),
                })
                .collect()
        })
        .collect();
    out.sort();
    out
}

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

#[test]
fn computed_from_geometry() {
    let e = engine();
    assert_eq!(
        rows(&e, "SELECT ?l WHERE { ex:track geo:hasMetricLength ?l }"),
        [["500"]]
    );
    assert_eq!(
        rows(&e, "SELECT ?l WHERE { ex:trackGeom geo:hasLength ?l }"),
        [["500"]]
    );
    // A feature without a default geometry uses its geometries.
    assert_eq!(
        rows(
            &e,
            "SELECT ?a ?p WHERE { ex:yard geo:hasMetricArea ?a ; geo:hasMetricPerimeterLength ?p }"
        ),
        [["200", "60"]]
    );
    assert_eq!(
        rows(&e, "SELECT ?d WHERE { ex:yardGeom geo:dimension ?d }"),
        [["2"]]
    );
    // Other triples in the group join as usual, and SELECT * shows no helper variables.
    assert_eq!(
        rows(
            &e,
            "SELECT * WHERE { ?f geo:hasDefaultGeometry ?g . ?g geo:hasMetricSize ?s }"
        ),
        [
            [
                "<http://example.org/signal>",
                "<http://example.org/signalGeom>",
                "1.5"
            ],
            [
                "<http://example.org/track>",
                "<http://example.org/trackGeom>",
                "500"
            ],
        ]
    );
}

#[test]
fn asserted_values_win_and_constants_filter() {
    let e = engine();
    // ex:signalGeom asserts 1.5, which replaces the computed 0.
    assert_eq!(
        rows(&e, "SELECT ?s WHERE { ex:signalGeom geo:hasMetricSize ?s }"),
        [["1.5"]]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?g WHERE { ?g geo:asWKT ?w . ?g geo:hasMetricSize 500.0 }"
        ),
        [["<http://example.org/trackGeom>"]]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?g WHERE { ?g geo:asWKT ?w ; geo:hasMetricLength ?l FILTER(?l > 100) }"
        ),
        [["<http://example.org/trackGeom>"]]
    );
}
