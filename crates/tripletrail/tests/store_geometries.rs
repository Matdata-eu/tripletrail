//! On the tripletrail store, functions and the spatial rewrite read the geometries the store keeps
//! parsed by term id, not a second parse of the literal's text. (Always on that store,
//! whatever `TRIPLETRAIL_STORE` says.)

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::store::Dataset;

const TTL: &str = r#"@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:a geo:hasGeometry ex:ga . ex:ga geo:asWKT "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"^^geo:wktLiteral .
ex:b geo:hasGeometry ex:gb . ex:gb geo:asWKT "POLYGON((0 0, 3 0, 3 3, 0 3, 0 0))"^^geo:wktLiteral .
"#;

fn parsed_by_store(engine: &Engine) -> usize {
    match engine.store() {
        Dataset::TripleTrail(s) => s.memory_use().geometries,
        Dataset::Oxigraph(_) => panic!("the tripletrail store"),
    }
}

#[test]
fn functions_use_the_geometries_the_store_keeps() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
    assert_eq!(parsed_by_store(&engine), 0);
    // No spatial FILTER, so no index: the function alone asks for the geometries.
    let QueryResults::Solutions(rows) = engine
        .query(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
             SELECT ?a WHERE { ?g geo:asWKT ?w BIND(geof:area(?w) AS ?a) } ORDER BY ?a",
        )
        .unwrap()
    else {
        panic!("solutions")
    };
    let areas: Vec<String> = rows
        .map(|r| r.unwrap().get("a").unwrap().to_string())
        .collect();
    assert_eq!(areas.len(), 2, "{areas:?}");
    assert_eq!(parsed_by_store(&engine), 2);
    // A constant in the query is not in the store: still parsed.
    let QueryResults::Boolean(b) = engine
        .query(
            "PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
             PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             ASK { FILTER(geof:sfIntersects(\"POINT(1 1)\"^^geo:wktLiteral, \"POINT(1 1)\"^^geo:wktLiteral)) }",
        )
        .unwrap()
    else {
        panic!("boolean")
    };
    assert!(b);
}

#[test]
fn aggregates_use_the_geometries_the_store_keeps() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
    let QueryResults::Solutions(mut rows) = engine
        .query(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
             SELECT (SUM(geof:area(?w)) AS ?s) WHERE { ?g geo:asWKT ?w }",
        )
        .unwrap()
    else {
        panic!("solutions")
    };
    assert!(rows.next().unwrap().unwrap().get("s").is_some());
    assert_eq!(parsed_by_store(&engine), 2);
}
