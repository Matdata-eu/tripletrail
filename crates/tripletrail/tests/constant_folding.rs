//! Constant expressions are evaluated once, when the query is planned (issue #375): the
//! pattern of `REPLACE` and `REGEX` is then a literal, compiled once, instead of a regular
//! expression compiled for every row (`REPLACE(STR(?x), CONCAT(STR(ex:), "_"), "")`).

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const DATA: &str = "@prefix ex: <http://example.org/> .
ex:_a ex:p 1 . ex:_b ex:p 2 . ex:c ex:p 3 .
";

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, query: &str) -> Vec<String> {
    let query = format!("PREFIX ex: <http://example.org/>\n{query}");
    let QueryResults::Solutions(solutions) = e.query(&query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut rows: Vec<String> = solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn constant_patterns_are_literals_in_the_plan() {
    let e = engine();
    let q = r#"PREFIX ex: <http://example.org/>
        SELECT ?id { ?s ex:p ?o BIND(REPLACE(STR(?s), CONCAT(STR(ex:), "_"), "") AS ?id) }"#;
    let plan = e.explain(q).unwrap();
    assert!(!plan.contains("CONCAT"), "{plan}");
    assert!(plan.contains(r#"http://example.org/_"#), "{plan}");
    assert_eq!(
        rows(
            &e,
            "SELECT ?id { ?s ex:p ?o BIND(REPLACE(STR(?s), CONCAT(STR(ex:), \"_\"), \"\") AS ?id) }"
        ),
        ["id=\"a\"", "id=\"b\"", "id=\"http://example.org/c\""]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?s { ?s ex:p ?o FILTER(REGEX(STR(?s), CONCAT(\"^\", STR(ex:), \"_\"), LCASE(\"I\"))) }"
        ),
        ["s=<http://example.org/_a>", "s=<http://example.org/_b>"]
    );
}

#[test]
fn constant_expressions_keep_their_values() {
    let e = engine();
    assert_eq!(
        rows(&e, "SELECT ?o { ?s ex:p ?o FILTER(?o > 1 + 0.5 * 2) }"),
        ["o=\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?x { BIND(STRAFTER(UCASE(\"abc\"), \"A\") AS ?x) }"
        ),
        ["x=\"BC\""]
    );
    // An expression that raises an error stays one: the BIND leaves the variable unbound.
    assert_eq!(rows(&e, "SELECT ?x { BIND(1 / 0 AS ?x) }"), [""]);
    assert_eq!(
        rows(&e, "SELECT ?o { ?s ex:p ?o FILTER(1 / 0 = 1 || ?o = 2) }").len(),
        1
    );
}

#[test]
fn functions_with_a_new_value_per_call_are_not_folded() {
    let e = engine();
    let r = rows(
        &e,
        "SELECT DISTINCT ?r { ?s ex:p ?o BIND(STR(RAND()) AS ?r) }",
    );
    assert_eq!(r.len(), 3, "{r:?}");
    let r = rows(
        &e,
        "SELECT DISTINCT ?u { ?s ex:p ?o BIND(STRUUID() AS ?u) }",
    );
    assert_eq!(r.len(), 3, "{r:?}");
}
