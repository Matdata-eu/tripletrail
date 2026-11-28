//! Answers on the right side of a join, `OPTIONAL`, `MINUS` or `EXISTS` that is a `UNION`, `BIND`,
//! `FILTER` or `VALUES` (#342, DR 0003): the id executor gives the evaluator's answers, and answers
//! on ids. The semi-join seed is not pushed into them (measured on Sparqloscope: no gain, #342).

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn engine() -> Engine {
    let mut d = String::new();
    // Few typed subjects on the left, many triples on the right.
    for i in 0..5000 {
        d += &format!(
            "<urn:s{i}> <urn:p> {i} .\n<urn:s{i}> <urn:q> \"q{}\" .\n",
            i % 7
        );
        if i % 3 == 0 {
            d += &format!("<urn:s{i}> <urn:r> <urn:o{}> .\n", i % 11);
        }
    }
    for i in (0..5000).step_by(250) {
        d += &format!("<urn:s{i}> a <urn:T> .\n");
    }
    d += "<urn:s250> <urn:p> 7 .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    engine
}

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = engine.query(query).unwrap() else {
        panic!()
    };
    let mut rows: Vec<String> = s
        .map(|r| {
            let r = r.unwrap();
            r.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

const QUERIES: &[&str] = &[
    // UNION, one branch without the shared variable among them.
    "SELECT * { ?s a <urn:T> { ?s <urn:p> ?v } UNION { ?s <urn:r> ?v } }",
    "SELECT * { ?s a <urn:T> { ?s <urn:p> ?v } UNION { ?x <urn:r> <urn:o3> } }",
    "SELECT * { ?s a <urn:T> OPTIONAL { { ?s <urn:r> ?v } UNION { ?s <urn:q> ?v } } }",
    "SELECT * { ?s a <urn:T> MINUS { { ?s <urn:r> <urn:o0> } UNION { ?s <urn:q> \"q2\" } } }",
    // BIND.
    "SELECT * { ?s a <urn:T> OPTIONAL { ?s <urn:p> ?v BIND(?v * 2 AS ?w) } }",
    "SELECT * { ?s a <urn:T> { ?s <urn:p> ?v BIND(STR(?v) AS ?w) } }",
    // FILTER; a variable of the left side is unbound in it.
    "SELECT * { ?s a <urn:T> OPTIONAL { ?s <urn:p> ?v FILTER(?v > 1000) } }",
    "SELECT * { ?s a <urn:T> { ?s <urn:p> ?v FILTER(?v < 2000) } }",
    "SELECT * { ?s a <urn:T> MINUS { ?s <urn:p> ?v FILTER(?v >= 3000) } }",
    "SELECT * { ?s a <urn:T> ; <urn:q> ?k { ?s <urn:p> ?v FILTER(!BOUND(?k)) } }",
    // A FILTER on a BIND, rewritten under it.
    "SELECT * { ?s a <urn:T> { ?s <urn:p> ?v BIND(?v + 1 AS ?w) FILTER(?w > 2000) } }",
    // VALUES, with UNDEF.
    "SELECT * { ?s a <urn:T> OPTIONAL { VALUES (?s ?n) { (<urn:s0> 1) (<urn:s1> 2) (UNDEF 3) (<urn:s500> 4) } } }",
    "SELECT * { ?s a <urn:T> { VALUES ?s { <urn:s0> <urn:s250> <urn:nope> } } }",
    "SELECT * { ?s a <urn:T> MINUS { VALUES ?s { <urn:s750> } } }",
    // Nested: a UNION of a FILTER and a BIND.
    "SELECT * { ?s a <urn:T> OPTIONAL { { ?s <urn:p> ?v FILTER(?v > 4000) } UNION { ?s <urn:q> ?q BIND(CONCAT(?q, \"!\") AS ?v) } } }",
];

/// Shapes the id executor may leave to the evaluator: only their answers are compared.
const ANSWERS: &[&str] = &[
    // EXISTS over a UNION.
    "SELECT * { ?s a <urn:T> FILTER EXISTS { { ?s <urn:r> <urn:o1> } UNION { ?s <urn:p> 7 } } }",
    "SELECT * { ?s a <urn:T> FILTER NOT EXISTS { ?s <urn:p> ?v BIND(?v AS ?w) FILTER(?w > 10) } }",
];

#[test]
fn right_sides_give_the_evaluators_answers() {
    let engine = engine();
    for q in QUERIES.iter().chain(ANSWERS) {
        let ids = rows(&engine, &format!("#pragma tt.executor ids\n{q}"));
        let spareval = rows(&engine, &format!("#pragma tt.executor spareval\n{q}"));
        assert!(!spareval.is_empty(), "{q}");
        assert_eq!(ids, spareval, "{q}");
        if ANSWERS.contains(q) {
            continue;
        }
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{q}"))
            .unwrap();
        assert!(
            plan.contains(r#""executor":"ids""#),
            "not on ids: {q}\n{plan}"
        );
    }
}
