//! Under `--executor ids` (#11), the result cache keeps an answer on ids as it is: a hit gives
//! the same solutions, still attached as ids for the writers, and a write empties it.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::sync::Arc;
use tripletrail::{CacheStatus, Engine, IdSolutions, QueryOptions};

fn run(engine: &Engine, query: &str) -> (Vec<String>, bool, CacheStatus) {
    let (results, status) = engine
        .query_with_cache_status(query, QueryOptions::default())
        .unwrap();
    let QueryResults::Solutions(solutions) = results else {
        panic!("not a SELECT")
    };
    let ids = solutions
        .attachment()
        .is_some_and(|a| Arc::clone(a).downcast::<IdSolutions>().is_ok());
    let rows = solutions
        .map(|s| {
            let s = s.unwrap();
            s.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    (rows, ids, status)
}

#[test]
fn the_cache_keeps_answers_on_ids() {
    Engine::set_default_executor_ids(true);
    let engine = Engine::new_tripletrail();
    engine.set_result_cache(64 << 20);
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..500 {
        ttl.push_str(&format!("ex:s{i} ex:p ex:o{} ; ex:n \"n{i}\" .\n", i % 40));
    }
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let query = "PREFIX ex: <http://example.org/> \
                 SELECT ?s ?o ?n WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?n } } ORDER BY ?s";
    let (first, ids, status) = run(&engine, query);
    assert!(ids, "answered on ids");
    assert!(matches!(status, CacheStatus::Miss));
    assert_eq!(first.len(), 500);
    let (again, ids, status) = run(&engine, query);
    assert!(ids, "the hit is still on ids");
    assert!(matches!(status, CacheStatus::Hit));
    assert_eq!(first, again);
    engine
        .update("PREFIX ex: <http://example.org/> INSERT DATA { ex:s0 ex:p ex:new }")
        .unwrap();
    let (after, _, status) = run(&engine, query);
    assert!(matches!(status, CacheStatus::Miss));
    assert_eq!(after.len(), 501);
}
