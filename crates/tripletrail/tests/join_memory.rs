//! Past the id-join memory budget (#479), a query is evaluated row by row and gives the same
//! answer. A test file of its own: the budget is for the whole process.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn count(engine: &Engine, query: &str) -> String {
    let QueryResults::Solutions(mut s) = engine.query(query).unwrap() else {
        panic!()
    };
    s.next().unwrap().unwrap().get("n").unwrap().to_string()
}

#[test]
fn the_same_answer_past_the_budget() {
    let d: String = (0..100)
        .map(|i| {
            format!(
                "<urn:s{i}> <urn:p> {i} .\n<urn:s{i}> <urn:q> <urn:s{}> .\n",
                (i + 1) % 100
            )
        })
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let queries = [
        "SELECT (COUNT(*) AS ?n) { ?a <urn:p> ?x . ?b <urn:p> ?y . ?c <urn:p> ?z FILTER(?x + ?y + ?z < 50) }",
        "SELECT (COUNT(*) AS ?n) { ?a <urn:q> ?b . ?b <urn:p> ?x . ?c <urn:p> ?y FILTER(?x < ?y) }",
        "SELECT (COUNT(*) AS ?n) { ?a <urn:p> ?x OPTIONAL { ?a <urn:q> ?b . ?b <urn:p> ?y } ?c <urn:p> ?z FILTER(?z > ?x) }",
    ];
    let expected: Vec<String> = queries.iter().map(|q| count(&engine, q)).collect();
    tripletrail::store::set_join_memory(16 << 10);
    for (q, e) in queries.iter().zip(&expected) {
        assert_eq!(&count(&engine, q), e, "{q}");
    }
    tripletrail::store::set_join_memory(tripletrail::store::DEFAULT_JOIN_MEMORY);
}
