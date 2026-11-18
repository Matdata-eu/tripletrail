//! `SERVICE <https://…>` to remote SPARQL endpoints (#192), with a second server as the remote.

#![cfg(feature = "remote-service")]

use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::remote::RemoteConfig;
use tripletrail_server::{Options, Server};

fn engine(nt: &str) -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    e
}

fn serve(nt: &str, remote: RemoteConfig) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine(nt))],
        Options {
            threads: 4,
            remote_services: remote,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

/// Status and body of a query, the body as TSV.
fn query(base: &str, q: &str) -> (u16, String) {
    let r = ureq::get(&format!("{base}/kg/query"))
        .query("query", q)
        .set("Accept", "text/tab-separated-values")
        .call();
    let r = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{e}"),
    };
    (r.status(), r.into_string().unwrap())
}

/// The queries the remote has answered, once the count holds still: a server counts a query
/// after its answer has gone out, so the last one may not be counted yet when the caller
/// already has its answer.
fn remote_queries(base: &str) -> u64 {
    let mut count = counted_queries(base);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let now = counted_queries(base);
        if now == count {
            return count;
        }
        count = now;
    }
}

fn counted_queries(base: &str) -> u64 {
    let stats: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/$/stats/kg"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    stats["datasets"]["/kg"]["queries"].as_u64().unwrap()
}

#[test]
fn service_to_a_remote_endpoint() {
    let remote = serve(
        "<http://ex/a> <http://ex/label> \"A\" .\n\
         <http://ex/b> <http://ex/label> \"B\" .\n\
         <http://ex/z> <http://ex/label> \"Z\" .\n",
        RemoteConfig::default(),
    );
    let endpoint = format!("{remote}/kg/sparql");
    let local_data = "<http://ex/a> <http://ex/type> <http://ex/T> .\n\
                      <http://ex/b> <http://ex/type> <http://ex/T> .\n";
    let local = serve(
        local_data,
        RemoteConfig {
            allow: vec![format!("{remote}/kg/")],
            cache_ttl: Duration::from_secs(60),
            max_bytes: 4096,
            ..RemoteConfig::default()
        },
    );

    // A join with the remote labels.
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?s ?l {{ ?s <http://ex/type> <http://ex/T> . \
             SERVICE <{endpoint}> {{ ?s <http://ex/label> ?l }} }} ORDER BY ?s"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body, "?s\t?l\n<http://ex/a>\t\"A\"\n<http://ex/b>\t\"B\"\n",
        "{body}"
    );

    // The same SERVICE call again comes from the cache.
    let before = remote_queries(&remote);
    let (code, _) = query(
        &local,
        &format!(
            "SELECT ?s ?l {{ ?s <http://ex/type> <http://ex/T> . \
             SERVICE <{endpoint}> {{ ?s <http://ex/label> ?l }} }}"
        ),
    );
    assert_eq!(code, 200);
    assert_eq!(remote_queries(&remote), before);

    // The endpoint can come from a variable, bound before the SERVICE in a LATERAL.
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?l {{ VALUES ?ep {{ <{endpoint}> }} \
             LATERAL {{ SERVICE ?ep {{ <http://ex/z> <http://ex/label> ?l }} }} }}"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, "?l\n\"Z\"\n");

    // Or from the patterns before it, as on Fuseki: each endpoint they bind is called; under
    // OPTIONAL, a row whose endpoint has no answer stays.
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?ep ?l {{ VALUES ?ep {{ <{endpoint}> }} \
             SERVICE ?ep {{ <http://ex/z> <http://ex/label> ?l }} }}"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, format!("?ep\t?l\n<{endpoint}>\t\"Z\"\n"));
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?s ?l {{ VALUES (?s ?ep) {{ (<http://ex/a> <{endpoint}>) (<http://ex/q> <{endpoint}>) }} \
             OPTIONAL {{ SERVICE ?ep {{ ?s <http://ex/label> ?l }} }} }} ORDER BY ?s"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, "?s\t?l\n<http://ex/a>\t\"A\"\n<http://ex/q>\t\n");

    // Also with more patterns in the OPTIONAL group than the SERVICE.
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?s ?l ?t {{ VALUES (?s ?ep) {{ (<http://ex/a> <{endpoint}>) (<http://ex/z> <{endpoint}>) }} \
             OPTIONAL {{ SERVICE ?ep {{ ?s <http://ex/label> ?l }} ?s <http://ex/type> ?t }} }} ORDER BY ?s"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body,
        "?s\t?l\t?t\n<http://ex/a>\t\"A\"\t<http://ex/T>\n<http://ex/z>\t\t\n"
    );

    // Endpoints off the allowlist are refused; SILENT turns that into no rows from it.
    let other = "http://127.0.0.1:9/sparql";
    let (code, body) = query(
        &local,
        &format!("SELECT * {{ SERVICE <{other}> {{ ?s ?p ?o }} }}"),
    );
    assert_eq!(code, 400);
    assert!(body.contains("not an allowed remote endpoint"), "{body}");
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?s {{ ?s <http://ex/type> <http://ex/T> . \
             OPTIONAL {{ SERVICE SILENT <{other}> {{ ?s ?p ?o }} }} }} ORDER BY ?s"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, "?s\n<http://ex/a>\n<http://ex/b>\n");

    // A remote error is reported with the endpoint's status and message.
    let (code, body) = query(
        &local,
        &format!("SELECT * {{ SERVICE <{remote}/kg/nothing-here> {{ ?s ?p ?o }} }}"),
    );
    assert_eq!(code, 502);
    assert!(body.contains("HTTP 404"), "{body}");

    // An answer over the size limit is an error, not a truncated result.
    let big = (0..200)
        .map(|i| format!("<http://ex/n{i}> <http://ex/label> \"a fairly long label {i}\" .\n"))
        .collect::<String>();
    let big_remote = serve(&big, RemoteConfig::default());
    let local = serve(
        local_data,
        RemoteConfig {
            allow: vec![big_remote.clone()],
            max_bytes: 4096,
            ..RemoteConfig::default()
        },
    );
    let (code, body) = query(
        &local,
        &format!("SELECT * {{ SERVICE <{big_remote}/kg/sparql> {{ ?s ?p ?o }} }}"),
    );
    assert_eq!(code, 502);
    assert!(body.contains("larger than 4096 bytes"), "{body}");
}

#[test]
fn remote_services_are_off_by_default() {
    let local = serve(
        "<http://ex/a> <http://ex/type> <http://ex/T> .\n",
        RemoteConfig::default(),
    );
    let (code, body) = query(
        &local,
        "SELECT * { SERVICE <https://query.wikidata.org/sparql> { ?s ?p ?o } }",
    );
    assert_eq!(code, 400);
    assert!(body.contains("must be allowed"), "{body}");
}

#[test]
fn bind_join_sends_the_joining_values() {
    let labels = (0..300)
        .map(|i| format!("<http://ex/n{i}> <http://ex/label> \"L{i}\" .\n"))
        .collect::<String>();
    let remote = serve(&labels, RemoteConfig::default());
    let endpoint = format!("{remote}/kg/sparql");
    // 250 local things with a remote label, 50 without one.
    let local_data = (0..250)
        .map(|i| format!("<http://ex/n{i}> <http://ex/type> <http://ex/T> .\n"))
        .chain((0..50).map(|i| format!("<http://ex/m{i}> <http://ex/type> <http://ex/T> .\n")))
        .collect::<String>();
    let local = serve(
        &local_data,
        RemoteConfig {
            allow: vec![remote.clone()],
            ..RemoteConfig::default()
        },
    );
    let lines = |body: &str| body.lines().count() - 1;

    // 300 values to join on, so three calls of 100; only the 250 labels that join come back.
    let before = remote_queries(&remote);
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?s ?l {{ ?s <http://ex/type> <http://ex/T> . \
             SERVICE <{endpoint}> {{ ?s <http://ex/label> ?l }} }}"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(lines(&body), 250);
    assert!(body.contains("<http://ex/n249>\t\"L249\""), "{body}");
    assert!(!body.contains("L250"), "{body}");
    assert_eq!(remote_queries(&remote) - before, 3);

    // The same for an OPTIONAL SERVICE: every local thing, with a label where there is one.
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?s ?l {{ ?s <http://ex/type> <http://ex/T> . \
             OPTIONAL {{ SERVICE <{endpoint}> {{ ?s <http://ex/label> ?l }} }} }}"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(lines(&body), 300);
    assert_eq!(body.matches("\"L").count(), 250);

    // A SILENT service that fails stays one empty solution: each local row comes back once,
    // also with more values than one batch holds.
    let (code, body) = query(
        &serve(
            &local_data,
            RemoteConfig {
                allow: vec!["http://127.0.0.1:9/".to_owned()],
                ..RemoteConfig::default()
            },
        ),
        "SELECT ?s ?l { ?s <http://ex/type> <http://ex/T> . \
         SERVICE SILENT <http://127.0.0.1:9/sparql> { ?s <http://ex/label> ?l } }",
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(lines(&body), 300, "{body}");

    // Nothing to join with: no call at all.
    let before = remote_queries(&remote);
    let (code, body) = query(
        &local,
        &format!(
            "SELECT ?s ?l {{ ?s <http://ex/type> <http://ex/Nothing> . \
             SERVICE <{endpoint}> {{ ?s <http://ex/label> ?l }} }}"
        ),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(lines(&body), 0);
    assert_eq!(remote_queries(&remote), before);
}
