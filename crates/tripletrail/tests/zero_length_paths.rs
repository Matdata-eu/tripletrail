//! A zero-length path (`*`, `?`) matches its constant end even when that term is in no
//! triple of the store (W3C `zero_or_more_set_start` and the like): `?s :p* :o` binds
//! `?s = :o`.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

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
                .replace("<http://example.org/", "ex:")
                .replace('>', "")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn zero_length_paths_from_terms_the_store_does_not_hold() {
    // Both stores (#814: Oxigraph's store answers through the evaluator alone).
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        e.load(
            RdfFormat::Turtle,
            "@prefix ex: <http://example.org/> . ex:a ex:p ex:b . ex:b ex:p ex:c .".as_bytes(),
        )
        .unwrap();
        assert_eq!(rows(&e, "SELECT ?s { ?s ex:p* ex:nope }"), ["s=ex:nope"]);
        assert_eq!(rows(&e, "SELECT ?o { ex:nope ex:p* ?o }"), ["o=ex:nope"]);
        assert_eq!(rows(&e, "SELECT ?s { ?s ex:p? ex:nope }"), ["s=ex:nope"]);
        assert_eq!(rows(&e, "SELECT ?o { ex:nope ex:p? ?o }"), ["o=ex:nope"]);
        assert_eq!(rows(&e, "SELECT * { ex:nope ex:p* ex:nope }"), [""]);
        assert!(rows(&e, "SELECT * { ex:nope ex:p* ex:a }").is_empty());
        assert!(rows(&e, "SELECT ?s { ?s ex:p+ ex:nope }").is_empty());
        // A value bound by the query, not a constant of the pattern: only nodes match
        // (W3C values_and_path).
        assert!(rows(&e, "SELECT * { VALUES ?v { 1 } ?v ex:p? ?v }").is_empty());
        assert!(rows(&e, "SELECT * { VALUES ?v { ex:nope } ?v ex:p* ?o }").is_empty());
        // Terms the store holds are unchanged.
        assert_eq!(
            rows(&e, "SELECT ?s { ?s ex:p* ex:c }"),
            ["s=ex:a", "s=ex:b", "s=ex:c"]
        );
        assert_eq!(
            rows(&e, "SELECT ?o { ex:a ex:p? ?o }"),
            ["o=ex:a", "o=ex:b"]
        );
    }
}

#[test]
fn zero_length_paths_from_terms_that_are_no_node() {
    // Both stores (#814: Oxigraph's store answers through the evaluator alone).
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        e.load(
            RdfFormat::TriG,
            "@prefix ex: <http://example.org/> . ex:a ex:p ex:b . ex:g { ex:b ex:p 7 . }"
                .as_bytes(),
        )
        .unwrap();
        // A predicate and a graph name are in the dictionary but are no node.
        assert_eq!(rows(&e, "SELECT ?s { ?s ex:p* ex:p }"), ["s=ex:p"]);
        assert_eq!(rows(&e, "SELECT ?o { ex:g ex:p? ?o }"), ["o=ex:g"]);
        // A number is inline: no dictionary entry at all.
        assert_eq!(
            rows(&e, "SELECT ?o { 8 ex:p* ?o }"),
            ["o=\"8\"^^<http://www.w3.org/2001/XMLSchema#integer"]
        );
        // Reversed.
        assert_eq!(rows(&e, "SELECT ?s { ?s ^(ex:p*) ex:nope }"), ["s=ex:nope"]);
        assert_eq!(rows(&e, "SELECT ?s { ?s ^ex:p? ex:nope }"), ["s=ex:nope"]);
        // Inside EXISTS.
        assert_eq!(
            rows(
                &e,
                "SELECT ?x { BIND(ex:nope AS ?x) FILTER EXISTS { ?x ex:p* ex:nope } }"
            ),
            ["x=ex:nope"]
        );
        // Nodes of a named graph only are nodes: unchanged.
        assert_eq!(
            rows(&e, "SELECT ?s { ?s ex:p* ex:b }"),
            ["s=ex:a", "s=ex:b"]
        );
    }
}
