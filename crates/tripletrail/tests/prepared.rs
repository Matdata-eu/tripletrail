//! Prepared queries (#186): parsed once, run with new values for their variables.

use tripletrail::oxigraph::model::{NamedNode, Term};
use tripletrail::oxigraph::sparql::{QueryResults, Variable};
use tripletrail::{Engine, QueryOptions};

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.update(
        r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
           PREFIX ex: <http://e/>
           INSERT DATA {
             ex:area geo:hasGeometry ex:areaG .
             ex:areaG geo:asWKT "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral .
             ex:a ex:name "A" ; geo:hasGeometry ex:aG . ex:aG geo:asWKT "POINT(5 5)"^^geo:wktLiteral .
             ex:b ex:name "B" ; geo:hasGeometry ex:bG . ex:bG geo:asWKT "POINT(50 5)"^^geo:wktLiteral .
           }"#,
    )
    .unwrap();
    e
}

fn bind(v: &str, iri: &str) -> QueryOptions {
    QueryOptions {
        substitutions: vec![(
            Variable::new(v).unwrap(),
            Term::from(NamedNode::new(iri).unwrap()),
        )],
        ..Default::default()
    }
}

fn names(r: QueryResults<'static>) -> Vec<String> {
    let QueryResults::Solutions(s) = r else {
        panic!()
    };
    let mut v: Vec<String> = s
        .map(|s| s.unwrap().get("n").unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn a_prepared_query_runs_with_new_values() {
    let e = engine();
    let q = e
        .prepare_query("SELECT ?s ?n WHERE { ?s <http://e/name> ?n }", None)
        .unwrap();
    assert_eq!(
        q.variables(),
        [Variable::new("s").unwrap(), Variable::new("n").unwrap()]
    );
    assert_eq!(
        names(e.query_prepared(&q, bind("s", "http://e/a")).unwrap()),
        ["\"A\""]
    );
    assert_eq!(
        names(e.query_prepared(&q, bind("s", "http://e/b")).unwrap()),
        ["\"B\""]
    );
    assert_eq!(
        names(e.query_prepared(&q, QueryOptions::default()).unwrap()),
        ["\"A\"", "\"B\""]
    );
    // A variable the query does not select can't be bound.
    assert!(e.query_prepared(&q, bind("x", "http://e/a")).is_err());
}

#[test]
fn a_prepared_spatial_query_keeps_the_rewrite() {
    let e = engine();
    let q = e
        .prepare_query(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             SELECT ?n ?area WHERE { ?f <http://e/name> ?n ; geo:sfWithin ?area }",
            None,
        )
        .unwrap();
    assert_eq!(
        names(e.query_prepared(&q, bind("area", "http://e/area")).unwrap()),
        ["\"A\""]
    );
    assert_eq!(
        names(e.query_prepared(&q, bind("area", "http://e/b")).unwrap()),
        ["\"B\""]
    );
}

#[test]
fn prepare_reads_the_base_iri_and_refuses_bad_queries() {
    let e = engine();
    let q = e
        .prepare_query("SELECT ?n WHERE { <a> <name> ?n }", Some("http://e/"))
        .unwrap();
    assert_eq!(
        names(e.query_prepared(&q, QueryOptions::default()).unwrap()),
        ["\"A\""]
    );
    assert!(e.prepare_query("SELECT WHERE {", None).is_err());
}
