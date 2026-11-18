//! A cross product stops soon after the query is cancelled (a timeout, #467): the evaluator's
//! cross join makes its rows one at a time and checks the token while making them, as no quad
//! is read then, and the id joins check it every few rows too.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::{CancellationToken, QueryResults};
use std::time::{Duration, Instant};
use tripletrail::Engine;

fn engine(n: usize) -> Engine {
    let d: String = (0..n)
        .map(|i| format!("<urn:s{i}> <urn:p> {i} .\n"))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    engine
}

/// Runs `query` on `engine`, cancelled after 200 ms, and returns how long it took and whether
/// it ended with an error.
fn cancelled_after_200ms(engine: &Engine, query: &str) -> (Duration, bool) {
    let token = CancellationToken::new();
    let engine = engine.with_cancellation_token(token.clone());
    let start = Instant::now();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        token.cancel();
    });
    let failed = match engine.query(query) {
        Err(_) => true,
        Ok(QueryResults::Solutions(mut s)) => s.any(|r| r.is_err()),
        Ok(_) => false,
    };
    canceller.join().unwrap();
    (start.elapsed(), failed)
}

#[test]
fn cross_joins_stop_when_cancelled() {
    let engine = engine(300);
    for query in [
        // The evaluator (BIND keeps it out of the id joins): 8.1e9 rows.
        "SELECT (COUNT(*) AS ?n) { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i . ?j ?k ?l BIND(1 AS ?x) }",
        // The id joins first, then the evaluator.
        "SELECT (COUNT(*) AS ?n) { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i . ?j ?k ?l FILTER(?c + ?l > 0) }",
    ] {
        let (took, failed) = cancelled_after_200ms(&engine, query);
        assert!(failed, "{query}");
        eprintln!("{took:?} {query}");
        assert!(took < Duration::from_secs(2), "{query}: {took:?}");
    }
}

#[test]
fn a_cancelled_hash_join_ends() {
    // Subqueries joined on ?k with 7 values over 1,000 subjects: hash joins of about 1.4e11
    // rows. Once cancelled, the join gives its error once and ends: a consumer that collects
    // errors and keeps pulling (the COUNT) must not get another error forever (#472 review).
    let d: String = (0..1000)
        .map(|i| format!("<urn:s{i}> <urn:r> <urn:k{}> .\n", i % 7))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let query = "SELECT (COUNT(*) AS ?n) { { SELECT ?k ?a { ?a <urn:r> ?k } } { SELECT ?k ?b { ?b <urn:r> ?k } } { SELECT ?k ?c { ?c <urn:r> ?k } } { SELECT ?k ?d { ?d <urn:r> ?k } } FILTER(STR(?a) != STR(?d)) }";
    let (took, failed) = cancelled_after_200ms(&engine, query);
    eprintln!("{took:?} {query}");
    assert!(failed);
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn cross_join_counts_need_no_rows() {
    // 300^3 rows, counted from the number of rows each side has.
    let engine = engine(300);
    let QueryResults::Solutions(mut s) = engine
        .query("SELECT (COUNT(*) AS ?n) { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }")
        .unwrap()
    else {
        panic!()
    };
    let n = s.next().unwrap().unwrap();
    assert_eq!(
        n.get("n").unwrap().to_string(),
        "\"27000000\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    );
}

#[test]
fn a_long_geo_function_stops_when_cancelled() {
    // The functions are registered once per engine and shared by its queries (#186): a
    // long function still stops with the query that calls it, read from the call, not from
    // a token it was registered with.
    let engine = engine(1);
    // A sliver whose bounding box it hardly fills: a million points take about 10^9 tries.
    let query = "PREFIX st: <http://w3id.org/tripletrail/st#> \
         SELECT ?m { BIND(st:ST_GeneratePoints(\"POLYGON((0 0, 1000 1000, 1000 1000.000001, 0 0))\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>, 1000000) AS ?m) }";
    // A query of the same engine that is not cancelled first: the shared functions keep
    // no token of their own.
    let quick = "PREFIX st: <http://w3id.org/tripletrail/st#> \
         SELECT ?m { BIND(st:ST_GeometricMedian(\"MULTIPOINT(0 0, 2 0)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>) AS ?m) }";
    let Ok(QueryResults::Solutions(s)) = engine.query(quick) else {
        panic!("a solution")
    };
    let rows = s.collect::<Result<Vec<_>, _>>().unwrap();
    assert!(rows[0].get("m").is_some(), "{rows:?}");

    let (took, _) = cancelled_after_200ms(&engine, query);
    assert!(took < Duration::from_secs(2), "{took:?}");
}
