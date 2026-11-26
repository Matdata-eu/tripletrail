//! The Model Context Protocol endpoint (#153): JSON-RPC over HTTP, read-only tools, under
//! the same access rules and limits as queries.

use serde_json::{Value, json};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::auth::{sha256, to_hex};
use tripletrail_server::{Options, Server};

const DATA: &str = "@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:bxc a ex:Station ; rdfs:label \"Brussels-Central\" ; ex:tracks 6 .
ex:bxn a ex:Station ; rdfs:label \"Brussels-North\" ; ex:tracks 12 .
ex:ant a ex:Station ; rdfs:label \"Antwerp-Central\" ; ex:tracks 14 .
";

fn serve(engines: Vec<(String, Engine)>, options: Options) -> String {
    let server = Server::bind("127.0.0.1:0", engines, options).unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn start(mcp: bool) -> String {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let other = Engine::new_tripletrail();
    serve(
        vec![("kg".to_owned(), engine), ("other".to_owned(), other)],
        Options {
            threads: 2,
            mcp,
            ..Options::default()
        },
    )
}

/// Posts a JSON-RPC message; the status and the answer (`Null` when there is none).
fn post(url: &str, key: Option<&str>, message: &Value) -> (u16, Value) {
    let mut request = ureq::post(url).set("Content-Type", "application/json");
    if let Some(key) = key {
        request = request.set("X-API-Key", key);
    }
    match request.send_string(&message.to_string()) {
        Ok(r) => {
            let status = r.status();
            let text = r.into_string().unwrap();
            let value = if text.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&text).unwrap()
            };
            (status, value)
        }
        Err(ureq::Error::Status(code, _)) => (code, Value::Null),
        Err(e) => panic!("{e}"),
    }
}

/// Calls a tool; its text, and whether it is an error.
fn tool(url: &str, key: Option<&str>, name: &str, arguments: Value) -> (String, bool) {
    let (status, answer) = post(
        url,
        key,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                 "params": { "name": name, "arguments": arguments } }),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["id"], 7);
    let result = &answer["result"];
    (
        result["content"][0]["text"].as_str().unwrap().to_owned(),
        result["isError"].as_bool().unwrap(),
    )
}

/// The `value`s of `variable` in SPARQL JSON results, sorted.
fn values(results: &str, variable: &str) -> Vec<String> {
    let json: Value = serde_json::from_str(results).unwrap();
    let mut out: Vec<String> = json["results"]["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b[variable]["value"].as_str().unwrap().to_owned())
        .collect();
    out.sort();
    out
}

#[test]
fn agents_query_describe_and_search_a_dataset() {
    let base = start(true);
    let url = format!("{base}/kg/mcp");

    let (status, answer) = post(
        &url,
        None,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" } } }),
    );
    assert_eq!(status, 200);
    assert_eq!(answer["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(answer["result"]["serverInfo"]["name"], "TripleTrail");
    assert!(answer["result"]["capabilities"]["tools"].is_object());
    // A notification is accepted without an answer.
    let (status, answer) = post(
        &url,
        None,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    assert_eq!((status, answer), (202, Value::Null));

    let (_, answer) = post(
        &url,
        None,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    );
    let mut names: Vec<&str> = answer["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "describe",
            "list_datasets",
            "list_shapes",
            "search",
            "shape",
            "sparql_query"
        ]
    );

    let (text, error) = tool(
        &url,
        None,
        "sparql_query",
        json!({ "query": "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:tracks ?t FILTER(?t > 10) }" }),
    );
    assert!(!error);
    assert_eq!(
        values(&text, "s"),
        ["http://example.com/ant", "http://example.com/bxn"]
    );
    // At most max_rows rows, saying so.
    let (text, _) = tool(
        &url,
        None,
        "sparql_query",
        json!({ "query": "SELECT ?s WHERE { ?s a ?c }", "max_rows": 2 }),
    );
    assert!(
        text.ends_with("(Only the first 2 rows: add a LIMIT, or narrow the query.)"),
        "{text}"
    );
    // Updates are refused.
    let (text, error) = tool(
        &url,
        None,
        "sparql_query",
        json!({ "query": "INSERT DATA { <urn:a> <urn:b> <urn:c> }" }),
    );
    assert!(error, "{text}");
    assert!(text.starts_with("Not a SPARQL query"), "{text}");

    let (text, error) = tool(
        &url,
        None,
        "describe",
        json!({ "iri": "http://example.com/bxn" }),
    );
    assert!(!error);
    assert!(text.contains("Brussels-North"), "{text}");

    let (text, error) = tool(
        &url,
        None,
        "search",
        json!({ "text": "brussels", "limit": 5 }),
    );
    assert!(!error, "{text}");
    assert_eq!(
        values(&text, "resource"),
        ["http://example.com/bxc", "http://example.com/bxn"]
    );

    // Without a shape set, the shapes profiled from the data.
    let (text, error) = tool(&url, None, "list_shapes", json!({}));
    assert!(!error, "{text}");
    let shapes: Value = serde_json::from_str(&text).unwrap();
    let station = &shapes[0];
    assert_eq!(
        station["targetClass"],
        json!(["http://example.com/Station"]),
        "{shapes}"
    );
    assert_eq!(
        station["paths"],
        json!([
            "http://example.com/tracks",
            "http://www.w3.org/2000/01/rdf-schema#label"
        ])
    );
    let (text, error) = tool(&url, None, "shape", json!({ "iri": station["iri"] }));
    assert!(!error, "{text}");
    assert!(text.contains("sh:path"), "{text}");

    let (text, _) = tool(&url, None, "list_datasets", json!({}));
    let datasets: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(datasets[0]["name"], "kg");
    assert_eq!(datasets[0]["mcp"], format!("{base}/kg/mcp"));
    assert_eq!(datasets[1]["name"], "other");

    // Protocol errors.
    let (_, answer) = post(
        &url,
        None,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }),
    );
    assert_eq!(answer["error"]["code"], -32601);
    let (_, answer) = post(
        &url,
        None,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "drop" } }),
    );
    assert_eq!(answer["error"]["code"], -32602);
    // A browser page from another origin is refused (DNS rebinding).
    match ureq::post(&url)
        .set("Origin", "http://evil.example")
        .send_string(r#"{"jsonrpc":"2.0","id":5,"method":"tools/list"}"#)
    {
        Err(ureq::Error::Status(403, _)) => {}
        other => panic!("{other:?}"),
    }
    // Nothing to GET.
    match ureq::get(&url).call() {
        Err(ureq::Error::Status(405, _)) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_endpoint_is_off_unless_asked_for() {
    let base = start(false);
    let (status, _) = post(
        &format!("{base}/kg/mcp"),
        None,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(status, 404);
}

/// A caller granted one named graph (#201) reads only that graph through every tool.
#[test]
fn tools_keep_to_the_callers_graphs() {
    let dir = tripletrail_testdir::tempdir("mcp-graph-grants");
    std::fs::write(
        dir.join("assets.ttl"),
        "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .\n@prefix ex: <http://example.org/> .\n\
         ex:Contractors a sg:Policy ; sg:group \"contractor-x\" ; sg:readGraph <urn:release> .",
    )
    .unwrap();
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{{"sha256": "{}", "agent": "https://example.org/people/c", "groups": ["contractor-x"]}}]}},
                "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            to_hex(&sha256(b"contractor"))
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            &b"<urn:release> { <urn:a> <http://www.w3.org/2000/01/rdf-schema#label> \"released bridge\" }
               <urn:wip> { <urn:w> <http://www.w3.org/2000/01/rdf-schema#label> \"secret bridge\" }"[..],
        )
        .unwrap();
    let base = serve(
        vec![("assets".to_owned(), engine)],
        Options {
            threads: 2,
            mcp: true,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    );
    let url = format!("{base}/assets/mcp");
    // Without a key: refused as a query would be.
    let (status, _) = post(
        &url,
        None,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(status, 401);
    let key = Some("contractor");
    let (text, _) = tool(
        &url,
        key,
        "sparql_query",
        json!({ "query": "SELECT ?l WHERE { GRAPH ?g { ?s ?p ?l } }" }),
    );
    assert_eq!(values(&text, "l"), ["released bridge"]);
    let (text, _) = tool(&url, key, "describe", json!({ "iri": "urn:w" }));
    assert!(!text.contains("secret"), "{text}");
    let (text, error) = tool(&url, key, "search", json!({ "text": "bridge" }));
    assert!(!error, "{text}");
    assert_eq!(values(&text, "resource"), ["urn:a"]);
}

/// A query a tool runs is recorded as a query to `/{ds}/sparql` is: in the slow-query log
/// with its text and hash, and the request log with its hash.
#[test]
fn tool_queries_land_in_the_slow_query_log() {
    use std::io::Read;
    let dir = tripletrail_testdir::tempdir("mcp-slow-query");
    std::fs::write(dir.join("kg.ttl"), DATA).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .args(["serve", "--bind", &format!("127.0.0.1:{port}")])
        .args([
            "--dataset",
            "kg",
            "--mcp",
            "--slow-query-ms",
            "0",
            "--log-requests",
        ])
        .arg("--load")
        .arg(dir.join("kg.ttl"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while ureq::get(&format!("{base}/$/ready")).call().is_err() {
        assert!(std::time::Instant::now() < deadline, "not ready");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let query = "SELECT ?s WHERE { ?s a <http://example.com/Station> }";
    let (text, error) = tool(
        &format!("{base}/kg/mcp"),
        None,
        "sparql_query",
        json!({ "query": query }),
    );
    assert!(!error, "{text}");
    let _ = child.kill();
    let _ = child.wait();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let slow: Value = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|l| l["event"] == "slow_query")
        .unwrap_or_else(|| panic!("{stderr}"));
    assert_eq!(slow["text"], query);
    assert_eq!(slow["operation"], "query");
    assert_eq!(slow["dataset"], "kg");
    let hash = slow["query_hash"].as_str().unwrap();
    let logged: Value = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|l| l["path"] == "/kg/mcp")
        .unwrap_or_else(|| panic!("{stdout}"));
    assert_eq!(logged["query_hash"], hash);
}
