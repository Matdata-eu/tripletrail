//! `SELECT *` lists the variables in the order the pattern has them, as Jena does and the W3C
//! CSV and TSV tests expect (csv01 to csv04), not sorted.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

fn variables(e: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    s.variables()
        .iter()
        .map(|v| v.as_str().to_owned())
        .collect()
}

#[test]
fn select_star_keeps_the_pattern_order() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        "<http://e/s> <http://e/p> <http://e/o> .".as_bytes(),
    )
    .unwrap();
    assert_eq!(variables(&e, "SELECT * { ?s ?p ?o }"), ["s", "p", "o"]);
    assert_eq!(
        variables(&e, "SELECT * { ?z ?p ?a } ORDER BY ?a"),
        ["z", "p", "a"]
    );
    assert_eq!(
        variables(
            &e,
            "SELECT * { ?z ?p ?a OPTIONAL { ?a ?q ?b } BIND(1 AS ?c) }"
        ),
        ["z", "p", "a", "q", "b", "c"]
    );
    assert_eq!(
        variables(&e, "SELECT * { GRAPH ?g { ?s ?p ?o } }"),
        ["g", "s", "p", "o"]
    );
}
