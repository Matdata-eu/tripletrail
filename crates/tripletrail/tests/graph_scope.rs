//! `GRAPH ?g { ... }` as the SPARQL algebra has it (issue #364): `?g` is not in scope inside,
//! it ranges over every named graph even when the inside matches no quad of the active graph,
//! and a sub-select inside is evaluated once per named graph.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const DATA: &str = "@prefix ex: <http://example.org/> .
GRAPH ex:g1 { ex:a ex:p ex:o . ex:b ex:p ex:o . ex:c ex:p ex:g1 . }
GRAPH ex:g2 { ex:a ex:p ex:o . ex:d ex:p ex:g1 . }
GRAPH ex:g3 { ex:e ex:q ex:o . }
ex:x ex:p ex:o .
";

fn engine() -> Engine {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    engine
}

/// The rows, sorted, as `var=value` joined by spaces, with `http://example.org/` as `ex:`.
fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let query = format!("PREFIX ex: <http://example.org/>\n{query}");
    let QueryResults::Solutions(solutions) = engine.query(&query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut rows: Vec<String> = solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
                .replace("<http://example.org/", "ex:")
                .replace('>', "")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn the_graph_variable_is_not_in_scope_inside() {
    let e = engine();
    assert!(rows(&e, "SELECT * { GRAPH ?g { FILTER(BOUND(?g)) } }").is_empty());
    assert!(rows(&e, "SELECT * { GRAPH ?g { ?s ex:p ?o FILTER(BOUND(?g)) } }").is_empty());
    // Bound inside, the row is kept only if it is the graph's own name.
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?s { GRAPH ?g { ?s ?p ?o OPTIONAL { ?s ex:q ?g } } }"
        ),
        [
            "g=ex:g1 s=ex:a",
            "g=ex:g1 s=ex:b",
            "g=ex:g1 s=ex:c",
            "g=ex:g2 s=ex:a",
            "g=ex:g2 s=ex:d"
        ]
    );
    assert_eq!(
        rows(&e, "SELECT ?g ?s { GRAPH ?g { ?s ex:p ?g } }"),
        ["g=ex:g1 s=ex:c"]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?x { GRAPH ?g { BIND(ex:g2 AS ?g) BIND(1 AS ?x) } }"
        ),
        ["g=ex:g2 x=\"1\"^^<http://www.w3.org/2001/XMLSchema#integer"]
    );
}

#[test]
fn the_graph_variable_ranges_over_every_named_graph() {
    let e = engine();
    assert_eq!(
        rows(&e, "SELECT ?g ?x { GRAPH ?g { VALUES ?x { 1 } } }").len(),
        3
    );
    // Nested: the inner GRAPH is evaluated against all named graphs.
    assert_eq!(
        rows(
            &e,
            "SELECT ?g1 ?g0 ?s ?o { GRAPH ?g1 { GRAPH ?g0 { ?s ex:q ?o } } }"
        ),
        [
            "g1=ex:g1 g0=ex:g3 s=ex:e o=ex:o",
            "g1=ex:g2 g0=ex:g3 s=ex:e o=ex:o",
            "g1=ex:g3 g0=ex:g3 s=ex:e o=ex:o"
        ]
    );
    assert_eq!(
        rows(&e, "SELECT * { GRAPH ?g { GRAPH ex:g3 { ?s ex:q ?o } } }").len(),
        3
    );
    // A graph name outside the dataset gives nothing, even with no triple pattern inside.
    assert!(rows(&e, "SELECT * { GRAPH ex:nope { VALUES ?x { 1 } } }").is_empty());
    assert!(
        rows(
            &e,
            "SELECT * { VALUES ?g { ex:nope ex:g1 } GRAPH ?g { VALUES ?x { 1 } } }"
        ) == ["g=ex:g1 x=\"1\"^^<http://www.w3.org/2001/XMLSchema#integer"]
    );
}

#[test]
fn sub_selects_run_once_per_named_graph() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?c { GRAPH ?g { SELECT (COUNT(*) AS ?c) { ?s ex:p ex:o } } }"
        ),
        [
            "g=ex:g1 c=\"2\"^^<http://www.w3.org/2001/XMLSchema#integer",
            "g=ex:g2 c=\"1\"^^<http://www.w3.org/2001/XMLSchema#integer",
            "g=ex:g3 c=\"0\"^^<http://www.w3.org/2001/XMLSchema#integer"
        ]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?s { GRAPH ?g { SELECT ?s { ?s ex:p ?o } ORDER BY ?s LIMIT 1 } }"
        ),
        ["g=ex:g1 s=ex:a", "g=ex:g2 s=ex:a"]
    );
}

#[test]
fn plain_graph_patterns_are_unchanged() {
    let e = engine();
    assert_eq!(
        rows(&e, "SELECT ?g ?s { GRAPH ?g { ?s ex:p ex:o } }"),
        ["g=ex:g1 s=ex:a", "g=ex:g1 s=ex:b", "g=ex:g2 s=ex:a"]
    );
    assert_eq!(
        rows(&e, "SELECT ?s { GRAPH ex:g2 { ?s ex:p ?o } }"),
        ["s=ex:a", "s=ex:d"]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?s { ?s ex:p ex:o GRAPH ?g { ?s ex:p ?o FILTER NOT EXISTS { ?s ex:q ?x } } }"
        ),
        Vec::<String>::new()
    );
}

/// `MINUS` inside `GRAPH ?g` runs per graph, where the graph is no variable: a right row
/// removes a left row only when they share another bound variable (W3C graph-minus,
/// issues #425 and #427).
#[test]
fn minus_inside_a_graph_does_not_share_the_graph_variable() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::TriG,
        "@prefix ex: <http://example.org/> .
        GRAPH ex:g1 { ex:a ex:p ex:o . ex:a ex:q ex:x . }
        GRAPH ex:g2 { ex:c ex:p ex:o . ex:g1 ex:q ex:z . ex:g2 ex:q ex:w . }
        ex:d ex:p ex:o ."
            .as_bytes(),
    )
    .unwrap();
    let both = ["g=ex:g1 a=ex:a", "g=ex:g2 a=ex:c"];
    // No variable in common.
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?a { GRAPH ?g { ?a ex:p ex:o MINUS { ?b ex:p ex:o } } }"
        ),
        both
    );
    // ?a in common, but unbound in every right row (#427).
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?a { GRAPH ?g { ?a ex:p ex:o
                MINUS { ?b ex:p ex:o OPTIONAL { ?a ex:nope ?z } } } }"
        ),
        both
    );
    // ?a in common and bound: ex:a has ex:q in ex:g1.
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?a { GRAPH ?g { ?a ex:p ex:o MINUS { ?a ex:q ?b } } }"
        ),
        ["g=ex:g2 a=ex:c"]
    );
    // A ?g written inside is a variable of its own, not the graph's name.
    assert_eq!(
        rows(
            &e,
            "SELECT ?g ?a { GRAPH ?g { ?a ex:p ex:o MINUS { ?g ex:q ?b } } }"
        ),
        both
    );
    // Outside the GRAPH, ?g is shared.
    assert!(
        rows(
            &e,
            "SELECT ?g ?a { GRAPH ?g { ?a ex:p ex:o } MINUS { GRAPH ?g { ?x ex:q ?b } } }"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?a { GRAPH ex:g1 { ?a ex:p ex:o MINUS { ?b ex:p ex:o } } }"
        ),
        ["a=ex:a"]
    );
}
