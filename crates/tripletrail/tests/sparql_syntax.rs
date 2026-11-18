//! SPARQL grammar fixes in the vendored parser (vendor/spargebra, items 2 to 6 of its
//! TRIPLETRAIL.md), each from a W3C test that failed before.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        "@prefix : <http://e/> . :a :p 1 . :b :p 2 . :c :p 3 .".as_bytes(),
    )
    .unwrap();
    e
}

fn rows(e: &Engine, query: &str) -> Vec<String> {
    match e.query(query).unwrap() {
        QueryResults::Solutions(s) => {
            let vars: Vec<_> = s.variables().to_vec();
            s.map(|row| {
                let row = row.unwrap();
                vars.iter()
                    .map(|v| row.get(v).map_or("-".to_owned(), ToString::to_string))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
        }
        QueryResults::Graph(g) => {
            let mut t: Vec<_> = g.map(|t| t.unwrap().to_string()).collect();
            t.sort();
            t
        }
        QueryResults::Boolean(b) => vec![b.to_string()],
    }
}

#[test]
fn booleans_are_case_insensitive() {
    let b = "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> \
             \"false\"^^<http://www.w3.org/2001/XMLSchema#boolean>";
    assert_eq!(rows(&engine(), "SELECT (TRUE AS ?t) (False AS ?f) {}"), [b]);
}

#[test]
fn a_select_expression_may_use_an_earlier_one() {
    let i = |n| format!("\"{n}\"^^<http://www.w3.org/2001/XMLSchema#integer>");
    assert_eq!(
        rows(
            &engine(),
            "SELECT (COUNT(?v) AS ?n) (?n + 1 AS ?m) { ?s <http://e/p> ?v }"
        ),
        [format!("{} {}", i(3), i(4))]
    );
}

#[test]
fn construct_with_group_by_and_values() {
    assert_eq!(
        rows(
            &engine(),
            "PREFIX : <http://e/> CONSTRUCT { ?s :rank ?r } WHERE { ?s ?p ?o } GROUP BY ?s \
             VALUES (?s ?r) { (:a 1) (:b 2) (:d 4) }"
        ),
        [
            "<http://e/a> <http://e/rank> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "<http://e/b> <http://e/rank> \"2\"^^<http://www.w3.org/2001/XMLSchema#integer>",
        ]
    );
}

#[test]
fn invalid_queries_are_rejected() {
    let e = engine();
    for q in [
        // `<?a&&?b>` is one IRI token, so this is not two comparisons.
        "SELECT * { FILTER (?x<?a&&?b>?y) }",
        "SELECT (COUNT(COUNT(*)) AS ?c) {}",
        "SELECT (SUM(MAX(?v)) AS ?c) { ?s ?p ?v }",
        "SELECT * { BIND(<<( <<(<http://e/s> <http://e/p> <http://e/o>)>> <http://e/q> <http://e/z> )>> AS ?x) }",
        "SELECT * { BIND(<<( \"literal\" <http://e/q> <http://e/z> )>> AS ?x) }",
    ] {
        assert!(e.query(q).is_err(), "accepted: {q}");
    }
    // With spaces it is a comparison again, and an aggregate may be used twice side by side.
    assert!(e.query("SELECT * { FILTER (?x < ?a && ?b > ?y) }").is_ok());
    assert!(
        e.query("SELECT (COUNT(?v) AS ?a) (SUM(?v) / COUNT(?v) AS ?b) { ?s ?p ?v }")
            .is_ok()
    );
}
