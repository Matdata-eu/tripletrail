//! An engine saved to a dataset directory opens again, read-write or read-only, with the same
//! answers (spatial queries included) and no load step.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.org/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:oslo a ex:Station ; geo:hasGeometry [ geo:asWKT "POINT(10.75 59.91)"^^geo:wktLiteral ] .
ex:bergen a ex:Station ; geo:hasGeometry [ geo:asWKT "POINT(5.32 60.39)"^^geo:wktLiteral ] .
ex:trondheim a ex:Station ; geo:hasGeometry [ geo:asWKT "POINT(10.40 63.43)"^^geo:wktLiteral ] .
ex:south ex:area "POLYGON((4 58, 12 58, 12 61, 4 61, 4 58))"^^geo:wktLiteral .
"#;

const SPATIAL: &str = r#"
PREFIX ex: <http://example.org/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
SELECT ?s WHERE {
  ?s a ex:Station ; geo:hasGeometry/geo:asWKT ?w .
  ex:south ex:area ?a .
  FILTER(geof:sfWithin(?w, ?a))
} ORDER BY ?s
"#;

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("snapshot-{name}"))
}

#[test]
fn a_saved_engine_answers_the_same_after_opening() {
    for store in ["tripletrail", "oxigraph"] {
        let dir = temp_dir(store);
        let engine = if store == "tripletrail" {
            Engine::new_tripletrail()
        } else {
            Engine::new_oxigraph().unwrap()
        };
        engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        engine
            .update("INSERT DATA { <http://example.org/x> <http://example.org/p> 1 }")
            .unwrap();
        let expected = rows(&engine, SPATIAL);
        assert_eq!(expected.len(), 2, "{expected:?}");
        let target = dir.join("ds_20261107");
        engine.save(&target).unwrap();

        let ro = Engine::open_read_only(&target).unwrap();
        assert!(ro.is_read_only());
        assert_eq!(rows(&ro, SPATIAL), expected, "{store}");
        assert_eq!(
            rows(&ro, "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"),
            rows(&engine, "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }")
        );
        let err = ro
            .update("INSERT DATA { <http://example.org/y> <http://example.org/p> 2 }")
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");

        let rw = Engine::open_tripletrail(&target).unwrap();
        assert!(!rw.is_read_only());
        assert_eq!(rows(&rw, SPATIAL), expected);
        rw.update("INSERT DATA { <http://example.org/y> <http://example.org/p> 2 }")
            .unwrap();

        // A second save never lands on an existing dataset.
        assert!(engine.save(&target).is_err());
    }
}
