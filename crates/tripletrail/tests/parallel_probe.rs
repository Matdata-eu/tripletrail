//! The id-table joins, OPTIONAL, MINUS, EXISTS, FILTER and GROUP BY work large tables on several cores (#185):
//! the solutions, and their order, are those of one core.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::fmt::Write;
use tripletrail::Engine;

/// 150,000 subjects (more than `PARALLEL_ROWS`), every third with `ex:q`, every fifth with two.
fn data() -> String {
    let mut d = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..150_000 {
        writeln!(d, "ex:s{i} ex:p ex:o{} .", i % 97).unwrap();
        if i % 3 == 0 {
            writeln!(d, "ex:s{i} ex:q {i} .").unwrap();
        }
        if i % 5 == 0 {
            writeln!(d, "ex:s{i} ex:q \"x{i}\" . ex:s{i} ex:r ex:o{} .", i % 7).unwrap();
        }
    }
    d
}

fn rows(e: &Engine, query: &str) -> Vec<String> {
    let query = format!(
        "PREFIX ex: <http://example.org/>\nPREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n{query}"
    );
    let QueryResults::Solutions(solutions) = e.query(&query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[test]
fn parallel_probes_give_the_sequential_rows_in_order() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let queries = [
        "SELECT * { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } }",
        "SELECT * { ?s ex:p ?o MINUS { ?s ex:q ?v } }",
        "SELECT * { ?s ex:p ?o FILTER EXISTS { ?s ex:r ?w } }",
        "SELECT * { ?s ex:p ?o FILTER NOT EXISTS { ?s ex:q ?v } }",
        "SELECT * { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } OPTIONAL { ?s ex:r ?w } }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } MINUS { ?s ex:r ex:o3 } }",
        "SELECT ?o (COUNT(*) AS ?n) { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } } GROUP BY ?o",
        // FILTER over a large table, also with an error in some rows (?v unbound).
        "SELECT * { ?s ex:p ?o FILTER(?o != ex:o3 && ?o != ex:o5) }",
        "SELECT * { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } FILTER(?v = 3 || isLiteral(?v)) }",
        "SELECT * { ?s ex:p ?o ; ex:r ?w FILTER(isIRI(?w)) }",
        "SELECT * { ?s ex:p ?o ; ex:q ?v FILTER(DATATYPE(?v) = xsd:string) }",
        "SELECT * { ?s ex:p ?o ; ex:q ?v FILTER(LANG(?v) = \"\") }",
        // GROUP BY over a large table: few groups, a group per subject (read back as ids),
        // two keys, a key unbound in some rows, and MIN, MAX, SAMPLE and SUM per group.
        "SELECT ?o (COUNT(*) AS ?n) (MAX(?v) AS ?m) { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } } GROUP BY ?o",
        "SELECT ?s (COUNT(?v) AS ?n) (SAMPLE(?o) AS ?x) { ?s ex:p ?o OPTIONAL { ?s ex:q ?v } } GROUP BY ?s",
        "SELECT ?o ?w (COUNT(*) AS ?n) { ?s ex:p ?o OPTIONAL { ?s ex:r ?w } } GROUP BY ?o ?w",
        "SELECT ?w (MIN(?o) AS ?m) (COUNT(DISTINCT ?o) AS ?n) { ?s ex:p ?o OPTIONAL { ?s ex:r ?w } } GROUP BY ?w",
    ];
    for q in queries {
        tripletrail::store::par::set_query_threads(1);
        let one = rows(&e, q);
        tripletrail::store::par::set_query_threads(0);
        let all = rows(&e, q);
        assert!(!one.is_empty(), "{q}");
        assert_eq!(one, all, "{q}");
    }
}
