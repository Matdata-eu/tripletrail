//! Deeply nested or very long queries (#483): those within the limits of
//! `tripletrail::complexity` answer, on the server's query threads, where they used to abort the
//! whole process with a stack overflow; larger ones get a 400, and the server stays up.

use tripletrail::Engine;
use tripletrail_server::{Options, Server};

fn post(base: &str, path: &str, kind: &str, body: &str) -> (u16, String) {
    match ureq::post(&format!("{base}{path}"))
        .set("Content-Type", kind)
        .send_string(body)
    {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

/// Queries of each shape that used to overflow a 2 MB stack, `n` levels or parts long.
fn shapes(n: usize) -> Vec<(&'static str, String)> {
    let p = "PREFIX : <urn:> ";
    let list =
        |item: &dyn Fn(usize) -> String, sep: &str| (0..n).map(item).collect::<Vec<_>>().join(sep);
    vec![
        (
            "not",
            format!(
                "{p}SELECT * {{ ?s :x ?o FILTER({}true{}) }}",
                "!(".repeat(n),
                ")".repeat(n)
            ),
        ),
        (
            "groups",
            format!(
                "{p}SELECT * {{ {}?s :x ?o{} }}",
                "{ ".repeat(n),
                " }".repeat(n)
            ),
        ),
        (
            "subqueries",
            format!(
                "{p}SELECT * {{ {}?s :x ?o{} }}",
                // Two brackets a level.
                "{ SELECT * { ".repeat(n / 2),
                " } }".repeat(n / 2)
            ),
        ),
        (
            "triples",
            format!(
                "{p}SELECT * {{ {} }}",
                list(&|i| format!("?s :p{i} ?o{i} ."), " ")
            ),
        ),
        (
            // Each pattern shares a variable with the next: planned as a chain of lookups,
            // whose types were inferred again at every level (#509).
            "chain",
            format!(
                "{p}SELECT * {{ {} }}",
                list(&|i| format!("?x{i} :p ?x{} .", i + 1), " ")
            ),
        ),
        (
            "unions",
            format!(
                "{p}SELECT * {{ {} }}",
                list(&|i| format!("{{ ?s :x {i} }}"), " UNION ")
            ),
        ),
        (
            "exists",
            format!(
                "{p}SELECT * {{ ?s :x ?o {} }}",
                list(&|i| format!("FILTER EXISTS {{ ?s :y {i} }}"), " ")
            ),
        ),
        (
            "or",
            format!(
                "{p}SELECT * {{ ?s :x ?o FILTER({}) }}",
                list(&|i| format!("?o = {i}"), " || ")
            ),
        ),
        (
            "path",
            format!(
                "{p}SELECT * {{ ?s {} ?o }}",
                list(&|i| format!(":p{i}"), "/")
            ),
        ),
    ]
}

#[test]
fn deep_and_long_queries_answer_or_are_refused_without_a_crash() {
    let server = Server::bind(
        "127.0.0.1:0",
        [("ds".to_owned(), Engine::new().unwrap())],
        Options {
            threads: 2,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    post(
        &base,
        "/ds/update",
        "application/sparql-update",
        "INSERT DATA { <urn:a> <urn:x> 1 }",
    );
    let query = "application/sparql-query";

    // Within the limits: each answers 200; on a 2 MB stack each aborted the process (900
    // levels deep, or 2,000 parts long).
    for (name, q) in shapes(900) {
        let (code, body) = post(&base, "/ds/query", query, &q);
        assert_eq!(code, 200, "{name}: {body}");
    }
    for (name, q) in shapes(2000).into_iter().skip(3) {
        let (code, body) = post(&base, "/ds/query", query, &q);
        assert_eq!(code, 200, "{name}: {body}");
    }

    // Over the limits: a 400 that says why.
    for (name, q) in shapes(1500).into_iter().take(3) {
        let (code, body) = post(&base, "/ds/query", query, &q);
        assert_eq!(code, 400, "{name}: {body}");
        assert!(body.contains("levels deep"), "{name}: {body}");
    }
    for (name, q) in shapes(12_000).into_iter().skip(3) {
        let (code, body) = post(&base, "/ds/query", query, &q);
        assert_eq!(code, 400, "{name}: {body}");
        assert!(body.contains("too large"), "{name}: {body}");
    }
    let n = 1500;
    let update = format!(
        "DELETE {{ ?s <urn:x> ?o }} WHERE {{ ?s <urn:x> ?o FILTER({}true{}) }}",
        "(".repeat(n),
        ")".repeat(n)
    );
    let (code, body) = post(&base, "/ds/update", "application/sparql-update", &update);
    assert_eq!(code, 400, "{body}");

    // Long enough to time out, with the default settings (lookup joins on): the query ends
    // with the timeout, where planning and building the evaluator walked the whole chain
    // again at every step (#509: chains of 8,000 ran for minutes).
    for (name, pattern) in [("chain", "?x{i} :p ?x{j} ."), ("star", "?s :p ?x{i} .")] {
        let q = format!(
            "PREFIX : <urn:> ASK {{ {} }}",
            (0..8000)
                .map(|i| pattern
                    .replace("{i}", &i.to_string())
                    .replace("{j}", &(i + 1).to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let start = std::time::Instant::now();
        let (code, body) = post(&base, "/ds/query?timeout=2", query, &q);
        let took = start.elapsed();
        assert!(code == 200 || code == 503, "{name}: {code} {body}");
        assert!(took.as_secs() < 10, "{name}: {took:?}");
    }

    // The server is still up and the data unchanged.
    let (code, body) = post(&base, "/ds/query", query, "ASK { <urn:a> <urn:x> 1 }");
    assert_eq!(code, 200);
    assert!(body.contains("true"), "{body}");
}
