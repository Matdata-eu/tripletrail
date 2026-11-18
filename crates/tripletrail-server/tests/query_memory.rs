//! A query stopped by the memory limit (#189) is counted as a memory trip in `/$/metrics`,
//! not as a timeout. A test file of its own: the limit is for the whole process.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

/// Subqueries joined on ?k (7 values over 140 subjects): 56,000 rows in hash joins.
const QUERY: &str = "SELECT (COUNT(*) AS ?n) { { SELECT ?k ?a { ?a <urn:r> ?k } } { SELECT ?k ?b { ?b <urn:r> ?k } } { SELECT ?k ?c { ?c <urn:r> ?k } } FILTER(STR(?a) != STR(?c)) }";

#[test]
fn memory_trips_are_counted_apart_from_timeouts() {
    let data: String = (0..140)
        .map(|i| format!("<urn:s{i}> <urn:r> <urn:k{}> .\n", i % 7))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("ds".to_owned(), engine)],
        Options {
            threads: 2,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    tripletrail::set_query_memory(64 << 10);
    match ureq::get(&format!("{base}/ds/query"))
        .query("query", QUERY)
        .call()
    {
        Err(ureq::Error::Status(503, r)) => {
            let message = r.into_string().unwrap();
            assert!(message.contains("more memory than its limit"), "{message}");
        }
        other => panic!("expected a 503, got {other:?}"),
    }
    let expected = [
        r#"tripletrail_requests_total{dataset="ds",operation="query",status="503"} 1"#,
        r#"tripletrail_budget_trips_total{dataset="ds",budget="memory"} 1"#,
        r#"tripletrail_budget_trips_total{dataset="ds",budget="rows"} 0"#,
        r#"tripletrail_timeouts_total{dataset="ds"} 0"#,
    ];
    let mut metrics = String::new();
    for _ in 0..500 {
        metrics = ureq::get(&format!("{base}/$/metrics"))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        if expected
            .iter()
            .all(|line| metrics.lines().any(|l| l == *line))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for line in expected {
        assert!(metrics.lines().any(|l| l == line), "{line}\n{metrics}");
    }
}
