//! OpenTelemetry traces (#220): a request's spans reach an OTLP/HTTP collector as JSON, join
//! the caller's trace from `traceparent`, and follow its sampling choice.
#![cfg(feature = "otel")]

use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::otel::OtlpOptions;
use tripletrail_server::{Options, Server};

/// The bodies a collector got, with the `x-api-key` header of each.
type Received = Arc<Mutex<Vec<(String, Value)>>>;

/// A collector that keeps the bodies posted to it.
fn collector() -> (String, Received) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let got = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&got);
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("x-api-key"))
                .map(|h| h.value.to_string())
                .unwrap_or_default();
            keep.lock()
                .unwrap()
                .push((auth, serde_json::from_str(&body).unwrap()));
            let _ = request.respond(tiny_http::Response::empty(200));
        }
    });
    (url, got)
}

/// The spans received so far, by trace id.
fn spans(got: &Mutex<Vec<(String, Value)>>, trace: &str) -> Vec<Value> {
    got.lock()
        .unwrap()
        .iter()
        .flat_map(|(_, body)| {
            body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|s| s["traceId"] == trace)
        .collect()
}

fn wait_for(got: &Mutex<Vec<(String, Value)>>, trace: &str, n: usize) -> Vec<Value> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let found = spans(got, trace);
        if found.len() >= n || Instant::now() > until {
            return found;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn attribute<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .map(|a| &a["value"])
}

#[test]
fn requests_are_traced_to_an_otlp_collector() {
    let (url, got) = collector();
    let engine = Engine::new_tripletrail();
    engine
        .load(RdfFormat::NTriples, &b"<urn:s> <urn:p> \"o\" .\n"[..])
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 2,
            otlp: OtlpOptions {
                endpoint: Some(OtlpOptions::traces_url(&url)),
                headers: vec![("x-api-key".to_owned(), "secret".to_owned())],
                service_name: "tripletrail-test".to_owned(),
                // New traces are not kept: only callers that ask (sampled traceparent) are.
                sample: 0.0,
            },
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();

    let caller_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let caller_span = "00f067aa0ba902b7";
    let rows = ureq::get(&format!("{base}/kg/query"))
        .query("query", "SELECT ?o WHERE { ?s ?p ?o }")
        .set("Accept", "text/csv")
        .set(
            "traceparent",
            &format!("00-{caller_trace}-{caller_span}-01"),
        )
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(rows.trim(), "o\r\no");
    // A query that fails to parse: a 400 is the caller's error, not the span's.
    let bad_trace = "5bf92f3577b34da6a3ce929d0e0e4736";
    let bad = ureq::get(&format!("{base}/kg/query"))
        .query("query", "SELEC nothing")
        .set("traceparent", &format!("00-{bad_trace}-{caller_span}-01"))
        .call();
    assert!(matches!(bad, Err(ureq::Error::Status(400, _))));
    // An update.
    let update_trace = "6bf92f3577b34da6a3ce929d0e0e4736";
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .set(
            "traceparent",
            &format!("00-{update_trace}-{caller_span}-01"),
        )
        .send_string("INSERT DATA { <urn:a> <urn:b> <urn:c> }")
        .unwrap();
    // Not sampled by the caller, and new traces are not kept: nothing is sent.
    let unsampled = "7bf92f3577b34da6a3ce929d0e0e4736";
    ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK {}")
        .set("traceparent", &format!("00-{unsampled}-{caller_span}-00"))
        .call()
        .unwrap();
    ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK {}")
        .call()
        .unwrap();

    let query = wait_for(&got, caller_trace, 4);
    let names: Vec<&str> = query.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "sparql.prepare",
            "sparql.evaluate",
            "http.send",
            "GET /{dataset}/query"
        ],
        "{query:#?}"
    );
    let root = &query[3];
    // The request's span is a child of the caller's, a SERVER span; the steps are its children.
    assert_eq!(root["parentSpanId"], caller_span);
    assert_eq!(root["kind"], 2);
    for step in &query[..3] {
        assert_eq!(step["parentSpanId"], root["spanId"], "{step}");
        assert_eq!(step["kind"], 1);
        let (start, end) = (
            step["startTimeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap(),
            step["endTimeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap(),
        );
        let (root_start, root_end) = (
            root["startTimeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap(),
            root["endTimeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap(),
        );
        assert!(
            root_start <= start && start <= end && end <= root_end,
            "{step}"
        );
    }
    assert_eq!(
        attribute(root, "http.response.status_code"),
        Some(&serde_json::json!({"intValue": "200"}))
    );
    assert_eq!(
        attribute(root, "db.namespace"),
        Some(&serde_json::json!({"stringValue": "kg"}))
    );
    assert_eq!(
        attribute(root, "db.operation.name"),
        Some(&serde_json::json!({"stringValue": "query"}))
    );
    assert!(attribute(root, "tripletrail.query_hash").is_some());
    assert!(root.get("status").is_none(), "{root}");

    let bad = wait_for(&got, bad_trace, 2);
    let prepare = bad
        .iter()
        .find(|s| s["name"] == "sparql.prepare")
        .expect("{bad:#?}");
    assert_eq!(
        prepare["status"]["code"], 2,
        "the step that failed: {prepare}"
    );
    let root = bad.iter().find(|s| s["kind"] == 2).unwrap();
    assert_eq!(
        attribute(root, "http.response.status_code"),
        Some(&serde_json::json!({"intValue": "400"}))
    );
    assert!(root.get("status").is_none(), "{root}");

    let update = wait_for(&got, update_trace, 3);
    let names: Vec<&str> = update.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"sparql.update"), "{names:?}");
    assert!(names.contains(&"POST /{dataset}/update"), "{names:?}");

    // The resource and the export request's headers.
    let (auth, body) = got.lock().unwrap()[0].clone();
    assert_eq!(auth, "secret");
    assert_eq!(
        body["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
        "tripletrail-test"
    );
    std::thread::sleep(Duration::from_millis(2500));
    assert!(spans(&got, unsampled).is_empty());
    let traces: std::collections::HashSet<String> = got
        .lock()
        .unwrap()
        .iter()
        .flat_map(|(_, b)| {
            b["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|s| s["traceId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        traces.len(),
        3,
        "only the three sampled requests: {traces:?}"
    );
}
