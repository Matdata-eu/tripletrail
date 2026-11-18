//! The evaluator's hash joins under a memory limit and a timeout (#476): a join whose built
//! side passes the limit fails the query at once, and a join that matches one probe row with
//! many built rows makes them one at a time. A test file of its own: the limit is for the
//! whole process.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

/// Subqueries joined on ?k (7 values over `n` subjects): hash joins in the evaluator.
const QUERY: &str = "SELECT (COUNT(*) AS ?n) { { SELECT ?k ?a { ?a <urn:r> ?k } } { SELECT ?k ?b { ?b <urn:r> ?k } } { SELECT ?k ?c { ?c <urn:r> ?k } } FILTER(STR(?a) != STR(?c)) }";

fn engine(n: usize) -> Engine {
    let d: String = (0..n)
        .map(|i| format!("<urn:s{i}> <urn:r> <urn:k{}> .\n", i % 7))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    engine
}

fn count(engine: &Engine, query: &str) -> Result<String, String> {
    let QueryResults::Solutions(mut s) = engine.query(query).map_err(|e| e.to_string())? else {
        panic!()
    };
    let row = s.next().unwrap().map_err(|e| e.to_string())?;
    Ok(row.get("n").unwrap().to_string())
}

#[test]
fn a_join_past_the_memory_limit_fails() {
    let engine = engine(140);
    // 20 per key: 7 × 20³ = 56,000 rows, of which 7 × 20 × 20 have ?a = ?c.
    let expected = "\"53200\"^^<http://www.w3.org/2001/XMLSchema#integer>";
    assert_eq!(count(&engine, QUERY).unwrap(), expected);
    tripletrail::set_query_memory(64 << 10);
    let error = count(&engine, QUERY).unwrap_err();
    assert!(error.contains("more memory than its limit"), "{error}");
    // An engine's own limit (#189) can't raise the process's: the lower of the two applies.
    let error = count(&engine.with_query_memory(64 << 20), QUERY).unwrap_err();
    assert!(error.contains("more memory than its limit"), "{error}");
    tripletrail::set_query_memory(64 << 20);
    assert_eq!(count(&engine, QUERY).unwrap(), expected);
}
