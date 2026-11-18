//! Per-dataset limits (#225): concurrency with a queue, then 429; timeouts and row limits per
//! dataset; reported in the statistics and metrics; the file is read again when it changes.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use tripletrail::Engine;
use tripletrail_server::{Options, Server};

fn status(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

fn write_limits(path: &Path, busy_max: usize) {
    std::fs::write(
        path,
        format!(
            r#"{{
  "default": {{ "queueTimeout": 0.3 }},
  "datasets": {{
    "busy": {{ "maxConcurrent": {busy_max}, "maxQueued": 1, "maxTimeout": 1 }},
    "free": {{ "maxRows": 2 }}
  }}
}}"#
        ),
    )
    .unwrap();
}

/// A chain of 3000 nodes: its closure takes far longer than a second to count.
fn chain() -> String {
    (0..3000)
        .map(|i| {
            format!(
                "<http://example.com/s{i}> <http://example.com/p> <http://example.com/s{}> .\n",
                i + 1
            )
        })
        .collect()
}

const SLOW: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/p>* ?b }";

#[test]
fn dataset_limits() {
    let dir = tripletrail_testdir::tempdir("limits");
    let file = dir.join("limits.json");
    write_limits(&file, 1);
    let server = Server::bind(
        "127.0.0.1:0",
        ["busy", "free"]
            .into_iter()
            .map(|n| (n.to_owned(), Engine::new().unwrap())),
        Options {
            threads: 4,
            limits: Some(file.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();

    let nt = chain();
    for ds in ["busy", "free"] {
        ureq::post(&format!("{base}/{ds}/data?default"))
            .set("Content-Type", "application/n-triples")
            .send_string(&nt)
            .unwrap();
    }

    // The slow query asks for 100 s but the dataset caps it at 1 s.
    let started = Instant::now();
    let holder = {
        let url = format!("{base}/busy/query");
        thread::spawn(move || {
            status(
                ureq::get(&url)
                    .query("query", SLOW)
                    .query("timeout", "100")
                    .call(),
            )
        })
    };
    thread::sleep(Duration::from_millis(200));

    // One request may wait for a turn; it gives up after the queue timeout.
    let waiter = {
        let url = format!("{base}/busy/query");
        thread::spawn(move || {
            let t = Instant::now();
            let r = ureq::get(&url).query("query", "ASK {}").call();
            (status(r), t.elapsed())
        })
    };
    thread::sleep(Duration::from_millis(20));
    // With the queue full, the next is refused at once, with Retry-After.
    let t = Instant::now();
    match ureq::get(&format!("{base}/busy/query"))
        .query("query", "ASK {}")
        .call()
    {
        Err(ureq::Error::Status(429, r)) => {
            assert_eq!(r.header("Retry-After"), Some("1"));
            assert!(r.into_string().unwrap().contains("busy"));
        }
        other => panic!("expected 429, got {:?}", other.map(|r| r.status())),
    }
    assert!(
        t.elapsed() < Duration::from_millis(250),
        "{:?}",
        t.elapsed()
    );

    // Another dataset still answers at once.
    let t = Instant::now();
    assert_eq!(
        status(
            ureq::get(&format!("{base}/free/query"))
                .query("query", "ASK {}")
                .call()
        ),
        200
    );
    assert!(
        t.elapsed() < Duration::from_millis(250),
        "{:?}",
        t.elapsed()
    );

    let (code, waited) = waiter.join().unwrap();
    assert_eq!(code, 429);
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert_eq!(holder.join().unwrap(), 503);
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the cap did not apply: {took:?}"
    );

    // Once the turn is free, the dataset answers again.
    assert_eq!(
        status(
            ureq::get(&format!("{base}/busy/query"))
                .query("query", "ASK {}")
                .call()
        ),
        200
    );

    // A per-dataset row limit.
    assert_eq!(
        status(
            ureq::get(&format!("{base}/free/query"))
                .query("query", "SELECT * { ?s ?p ?o } LIMIT 3")
                .call()
        ),
        413
    );
    assert_eq!(
        status(
            ureq::get(&format!("{base}/busy/query"))
                .query("query", "SELECT * { ?s ?p ?o } LIMIT 3")
                .call()
        ),
        200
    );

    // Statistics and metrics show the limits and the refusals.
    let stats: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/$/stats/busy"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    let busy = &stats["datasets"]["/busy"];
    assert_eq!(busy["limits"]["maxConcurrent"], 1);
    assert_eq!(busy["limits"]["maxTimeout"], 1.0);
    assert_eq!(busy["rejectedBusy"], 2);
    assert_eq!(busy["queuedTotal"], 1);
    assert_eq!(busy["running"], 0);
    let metrics = ureq::get(&format!("{base}/$/metrics"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        metrics.contains("tripletrail_dataset_requests_rejected_total{dataset=\"busy\"} 2"),
        "{metrics}"
    );

    // A changed file applies without a restart.
    thread::sleep(Duration::from_millis(1100));
    write_limits(&file, 5);
    thread::sleep(Duration::from_millis(1100));
    let stats: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/$/stats/busy"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stats["datasets"]["/busy"]["limits"]["maxConcurrent"], 5);
}

#[test]
fn caller_limits() {
    use tripletrail_server::auth::{sha256, to_hex};
    let hash = |key: &str| to_hex(&sha256(key.as_bytes()));
    let dir = tripletrail_testdir::tempdir("callers");
    let auth = dir.join("guard.json");
    std::fs::write(
        &auth,
        format!(
            r#"{{"auth": {{"apiKeys": [
                {{"sha256": "{}", "agent": "urn:a", "groups": ["partners"]}},
                {{"sha256": "{}", "agent": "urn:b", "groups": ["partners"]}},
                {{"sha256": "{}", "agent": "urn:c"}}
            ], "anonymous": true}}}}"#,
            hash("a-key"),
            hash("b-key"),
            hash("c-key"),
        ),
    )
    .unwrap();
    let limits = dir.join("limits.json");
    std::fs::write(
        &limits,
        r#"{"callers": {
            "anonymous": {"requestsPerMinute": 60, "burst": 2},
            "agents": {"urn:c": {"requestsPerMinute": 60, "burst": 1}},
            "groups": {"partners": {"maxConcurrent": 1}}
        }}"#,
    )
    .unwrap();
    let engine = Engine::new().unwrap();
    engine
        .load(
            tripletrail::oxigraph::io::RdfFormat::NTriples,
            chain().as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 4,
            auth_config: Some(auth),
            limits: Some(limits),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    // Status, Retry-After and body of an ASK as `key`.
    let ask = |key: Option<&str>| -> (u16, Option<String>, String) {
        let mut r = ureq::get(&format!("{base}/kg/query")).query("query", "ASK {}");
        if let Some(key) = key {
            r = r.set("X-API-Key", key);
        }
        let r = match r.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        let retry = r.header("Retry-After").map(str::to_owned);
        (r.status(), retry, r.into_string().unwrap())
    };

    // Anonymous callers: a bucket of 2, refilled at one a second.
    assert_eq!(ask(None).0, 200);
    assert_eq!(ask(None).0, 200);
    let (code, retry, _) = ask(None);
    assert_eq!((code, retry.as_deref()), (429, Some("1")));
    // Health checks are never limited.
    assert_eq!(status(ureq::get(&format!("{base}/$/ping")).call()), 200);
    // An agent's own limit.
    assert_eq!(ask(Some("c-key")).0, 200);
    assert_eq!(ask(Some("c-key")).0, 429);

    // A group's members share its limit.
    let holder = {
        let url = format!("{base}/kg/query");
        thread::spawn(move || {
            status(
                ureq::get(&url)
                    .set("X-API-Key", "a-key")
                    .query("query", SLOW)
                    .query("timeout", "1")
                    .call(),
            )
        })
    };
    thread::sleep(Duration::from_millis(200));
    let (code, _, body) = ask(Some("b-key"));
    assert_eq!(code, 429);
    assert!(body.contains("group partners"), "{body}");
    holder.join().unwrap();
    assert_eq!(ask(Some("b-key")).0, 200);

    let metrics = ureq::get(&format!("{base}/$/metrics"))
        .set("X-API-Key", "b-key")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        metrics.contains("tripletrail_caller_requests_rejected_total{reason=\"rate\"} 2"),
        "{metrics}"
    );
    assert!(
        metrics.contains("tripletrail_caller_requests_rejected_total{reason=\"concurrency\"} 1"),
        "{metrics}"
    );
    // A client that sends one request after the other never runs into its own turn: the turn
    // ends before the answer's last bytes go out (CI saw a 429 here).
    for i in 0..300 {
        assert_eq!(ask(Some("b-key")).0, 200, "request {i}");
    }
}

#[test]
fn anonymous_callers_behind_a_proxy() {
    let dir = tripletrail_testdir::tempdir("proxied");
    let auth = dir.join("guard.json");
    std::fs::write(
        &auth,
        r#"{"auth": {"apiKeys": [],
            "trustedHeaders": {"agent": "x-forwarded-user", "trustedProxies": ["127.0.0.1/32"]},
            "anonymous": true}}"#,
    )
    .unwrap();
    let limits = dir.join("limits.json");
    std::fs::write(
        &limits,
        r#"{"callers": {"anonymous": {"requestsPerMinute": 60, "burst": 1}}}"#,
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new().unwrap())],
        Options {
            threads: 2,
            auth_config: Some(auth),
            limits: Some(limits),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let ask = |forwarded: &str| {
        status(
            ureq::get(&format!("{base}/kg/query"))
                .query("query", "ASK {}")
                .set("X-Forwarded-For", forwarded)
                .call(),
        )
    };
    // Each client the trusted proxy forwards for has its own bucket.
    assert_eq!(ask("192.0.2.1"), 200);
    assert_eq!(ask("192.0.2.1"), 429);
    assert_eq!(ask("192.0.2.2"), 200);
    // Only the last address, the one the proxy added, counts: earlier ones are the client's
    // to forge, so both of these are 192.0.2.3.
    assert_eq!(ask("192.0.2.2, 192.0.2.3"), 200);
    assert_eq!(ask("192.0.2.1, 192.0.2.3"), 429);
}

#[test]
fn disk_and_memory_quotas() {
    const TOKEN: &str = "admin-secret";
    let dir = tripletrail_testdir::tempdir("quotas");
    let limits = dir.join("limits.json");
    std::fs::write(
        &limits,
        r#"{"datasets": {
            "disk": {"maxDisk": "4KB"},
            "mem": {"maxMemory": "100KiB"}
        }}"#,
    )
    .unwrap();
    let mem = Engine::new().unwrap();
    // Memory is only measured for TripleTrail's own store (not with TRIPLETRAIL_STORE=oxigraph).
    let measured = matches!(mem.store(), tripletrail::store::Dataset::TripleTrail(_));
    let server = Server::bind(
        "127.0.0.1:0",
        [("mem".to_owned(), mem)],
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            limits: Some(limits),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    status(
        ureq::post(&format!("{base}/$/datasets"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .query("name", "disk")
            .query("type", "disk")
            .call(),
    );
    let upload = |ds: &str, nt: &str| {
        status(
            ureq::post(&format!("{base}/{ds}/data?default"))
                .set("Content-Type", "application/n-triples")
                .send_string(nt),
        )
    };
    let update = |ds: &str, u: &str| {
        let r = ureq::post(&format!("{base}/{ds}/update"))
            .set("Content-Type", "application/sparql-update")
            .send_string(u);
        match r {
            Ok(r) => (r.status(), String::new()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
            Err(e) => panic!("{e}"),
        }
    };
    let count = |ds: &str| {
        ureq::get(&format!("{base}/{ds}/query"))
            .query("query", "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }")
            .set("Accept", "text/tab-separated-values")
            .call()
            .unwrap()
            .into_string()
            .unwrap()
    };
    let stats = |ds: &str| -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_str(
            &ureq::get(&format!("{base}/$/stats/{ds}"))
                .set("Authorization", &format!("Bearer {TOKEN}"))
                .call()
                .unwrap()
                .into_string()
                .unwrap(),
        )
        .unwrap();
        v["datasets"][format!("/{ds}")].clone()
    };
    const INSERT: &str = "INSERT DATA { <http://example.com/x> <http://example.com/p> 1 }";

    let quota_bound = if measured {
        &["disk", "mem"][..]
    } else {
        &["disk"]
    };
    for &ds in quota_bound {
        // Under the quota, writes go through; the one that crosses it completes.
        assert_eq!(upload(ds, &chain()), 204, "{ds}");
        // Over it, writes that add data are refused, and the dataset stays readable.
        let (code, body) = update(ds, INSERT);
        assert_eq!(code, 507, "{ds}: {body}");
        assert!(body.contains("quota"), "{body}");
        assert_eq!(
            upload(
                ds,
                "<http://example.com/a> <http://example.com/b> <http://example.com/c> .\n"
            ),
            507
        );
        assert!(count(ds).contains("3000"), "{ds}");
        // Removing data is allowed, so the dataset can be brought back under.
        let (code, body) = update(
            ds,
            "DELETE WHERE { <http://example.com/s0> <http://example.com/p> ?o }",
        );
        assert_eq!(code, 200, "{ds}: {body}");
        assert_eq!(
            status(ureq::delete(&format!("{base}/{ds}/data?default")).call()),
            204,
            "{ds}"
        );
        assert!(count(ds).contains("\n0"), "{ds}");
        let s = stats(ds);
        assert_eq!(s["rejectedQuota"], 2, "{ds}");
    }
    assert_eq!(stats("disk")["limits"]["maxDisk"], 4000);
    assert_eq!(stats("mem")["limits"]["maxMemory"], 102_400);
    if measured {
        let s = stats("mem");
        assert!(s["memoryBytes"].as_u64().unwrap() > 0);
        // By part, adding up to the total.
        let parts = s["memory"].as_object().unwrap();
        assert!(parts["dictionary"].as_u64().unwrap() > 0, "{s}");
        assert!(parts.contains_key("spatialIndex") && parts.contains_key("textIndex"));
        let sum: u64 = parts.values().map(|v| v.as_u64().unwrap()).sum();
        assert_eq!(sum, s["memoryBytes"].as_u64().unwrap());
    }

    // Once the files shrink under the quota (here: a higher quota), writes are taken again.
    thread::sleep(Duration::from_millis(1100));
    std::fs::write(
        dir.join("limits.json"),
        r#"{"datasets": {"disk": {"maxDisk": "1GB"}}}"#,
    )
    .unwrap();
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(update("disk", INSERT).0, 200);
    let metrics = ureq::get(&format!("{base}/$/metrics"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        metrics.contains("tripletrail_dataset_writes_rejected_quota_total{dataset=\"disk\"} 2"),
        "{metrics}"
    );
    if measured {
        assert!(
            metrics
                .contains("tripletrail_dataset_memory_bytes{dataset=\"mem\",part=\"dictionary\"}"),
            "{metrics}"
        );
    }
}

#[test]
fn limits_over_the_admin_api() {
    const TOKEN: &str = "admin-secret";
    let dir = tripletrail_testdir::tempdir("limits-api");
    let file = dir.join("limits.json");
    std::fs::write(&file, r#"{"default": {"maxRows": 1000}}"#).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new().unwrap())],
        Options {
            threads: 2,
            admin_token: Some(TOKEN.to_owned()),
            limits: Some(file.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let admin = |method: &str, path: &str| {
        ureq::request(method, &format!("{base}/$/limits{path}"))
            .set("Authorization", &format!("Bearer {TOKEN}"))
    };
    let json = |r: Result<ureq::Response, ureq::Error>| -> (u16, serde_json::Value) {
        let r = match r {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        let code = r.status();
        let text = r.into_string().unwrap();
        (
            code,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
        )
    };

    let (code, v) = json(admin("GET", "").call());
    assert_eq!(code, 200);
    assert_eq!(v["default"]["maxRows"], 1000);

    // One dataset's entry, applied at once and written to the file.
    let (code, v) = json(admin("PUT", "/kg").send_string(r#"{"maxRows": 2, "maxDisk": "1GB"}"#));
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["maxRows"], 2);
    assert_eq!(v["maxDisk"], 1_000_000_000u64);
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("\"maxDisk\": \"1GB\"")
    );
    assert_eq!(
        status(
            ureq::get(&format!("{base}/kg/query"))
                .query("query", "SELECT * { VALUES ?x { 1 2 3 } }")
                .call()
        ),
        413
    );
    let (_, v) = json(admin("GET", "/kg").call());
    assert_eq!(v["written"]["maxRows"], 2);
    assert_eq!(v["applied"]["maxRows"], 2);

    // An invalid entry is refused and changes nothing.
    let (code, v) = json(admin("PUT", "/kg").send_string(r#"{"maxRows": "many"}"#));
    assert_eq!(code, 400);
    assert!(v.as_str().unwrap().contains("maxRows"), "{v}");
    assert_eq!(json(admin("GET", "/kg").call()).1["applied"]["maxRows"], 2);

    // Removing it falls back to the default; the whole file can be replaced.
    assert_eq!(status(admin("DELETE", "/kg").call()), 204);
    assert_eq!(
        json(admin("GET", "/kg").call()).1["applied"]["maxRows"],
        1000
    );
    let (code, _) = json(admin("PUT", "").send_string(r#"{"datasets": {"kg": {"maxRows": 5}}}"#));
    assert_eq!(code, 200);
    assert_eq!(json(admin("GET", "/kg").call()).1["applied"]["maxRows"], 5);

    // An edit by hand just before a PUT is kept, not overwritten from the copy in memory.
    std::fs::write(&file, r#"{"datasets": {"kg": {"maxRows": 7}}}"#).unwrap();
    let (code, _) = json(admin("PUT", "/other").send_string(r#"{"maxRows": 9}"#));
    assert_eq!(code, 200);
    let (_, v) = json(admin("GET", "").call());
    assert_eq!(v["datasets"]["kg"]["maxRows"], 7, "{v}");
    assert_eq!(v["datasets"]["other"]["maxRows"], 9, "{v}");

    // Changes need the admin token.
    assert_eq!(
        status(ureq::put(&format!("{base}/$/limits/kg")).send_string("{}")),
        401
    );
}

/// #225's acceptance check: with `busy` held to one request at once, a flood of heavy queries
/// on it (16 clients, retrying 20 ms after each 429) keeps the p95 of light queries on `free`
/// near its idle value. The limit has to leave cores free: on a 4-core machine, with
/// `maxConcurrent: 2` the light p95 doubled (CPU, not workers). Timing-dependent, so run by
/// hand: `cargo test --release -p tripletrail-server --test limits -- --ignored`.
#[test]
#[ignore]
fn a_flood_on_one_dataset_leaves_the_other_fast() {
    let dir = tripletrail_testdir::tempdir("flood");
    let file = dir.join("limits.json");
    std::fs::write(
        &file,
        r#"{"datasets": {"busy": {"maxConcurrent": 1, "maxQueued": 2, "queueTimeout": 1, "maxTimeout": 2}}}"#,
    )
    .unwrap();
    let light_data = (0..2000)
        .map(|i| format!("<http://example.com/s{i}> <http://example.com/p> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n"))
        .collect::<String>();
    let busy = Engine::new().unwrap();
    busy.load(
        tripletrail::oxigraph::io::RdfFormat::NTriples,
        chain().as_bytes(),
    )
    .unwrap();
    let free = Engine::new().unwrap();
    free.load(
        tripletrail::oxigraph::io::RdfFormat::NTriples,
        light_data.as_bytes(),
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("busy".to_owned(), busy), ("free".to_owned(), free)],
        Options {
            threads: 8,
            limits: Some(file),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let agent = ureq::AgentBuilder::new().build();
    let light = |n: usize| {
        let mut times: Vec<Duration> = (0..n)
            .map(|_| {
                let t = Instant::now();
                let r = agent
                    .get(&format!("{base}/free/query"))
                    .query(
                        "query",
                        "SELECT ?s { ?s <http://example.com/p> ?o FILTER(?o > 1990) }",
                    )
                    .call();
                assert_eq!(status(r), 200);
                t.elapsed()
            })
            .collect();
        times.sort();
        times[times.len() * 95 / 100]
    };
    light(50);
    let idle = light(400);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flood: Vec<_> = (0..16)
        .map(|_| {
            let url = format!("{base}/busy/query");
            let stop = stop.clone();
            thread::spawn(move || {
                let agent = ureq::AgentBuilder::new().build();
                let mut refused = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let code = status(agent.get(&url).query("query", SLOW).call());
                    if code == 429 {
                        refused += 1;
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                refused
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(500));
    let flooded = light(400);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let refused: u32 = flood.into_iter().map(|t| t.join().unwrap()).sum();
    eprintln!(
        "light p95 idle {idle:?}, during the flood {flooded:?}; {refused} heavy queries refused"
    );
    // 20% of the idle p95, with a millisecond's allowance for timer noise at these sizes.
    assert!(
        flooded <= idle.mul_f64(1.2) + Duration::from_millis(1),
        "idle {idle:?}, flooded {flooded:?}"
    );
}

/// Subqueries joined on ?k (7 values over 140 subjects): hash joins whose held rows take about
/// 0.5 MB.
const JOINS: &str = "SELECT (COUNT(*) AS ?n) { { SELECT ?k ?a { ?a <urn:r> ?k } } { SELECT ?k ?b { ?b <urn:r> ?k } } { SELECT ?k ?c { ?c <urn:r> ?k } } FILTER(STR(?a) != STR(?c)) }";

/// A dataset's `queryMemory` (#189) fails a query whose hash joins hold more with 503, and a
/// request may ask for less with `query-memory`, never for more.
#[test]
fn query_memory_per_dataset_and_request() {
    let dir = tripletrail_testdir::tempdir("qmem");
    let file = dir.join("limits.json");
    std::fs::write(
        &file,
        r#"{ "datasets": { "small": { "queryMemory": "64KiB" } } }"#,
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        ["small", "big"]
            .into_iter()
            .map(|n| (n.to_owned(), Engine::new().unwrap())),
        Options {
            threads: 4,
            limits: Some(file.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let nt: String = (0..140)
        .map(|i| format!("<urn:s{i}> <urn:r> <urn:k{}> .\n", i % 7))
        .collect();
    for ds in ["small", "big"] {
        ureq::post(&format!("{base}/{ds}/data?default"))
            .set("Content-Type", "application/n-triples")
            .send_string(&nt)
            .unwrap();
    }
    let ask = |ds: &str, memory: Option<&str>| {
        let mut r = ureq::get(&format!("{base}/{ds}/query"))
            .query("query", JOINS)
            .set("Accept", "text/csv");
        if let Some(m) = memory {
            r = r.query("query-memory", m);
        }
        status(r.call())
    };
    assert_eq!(ask("small", None), 503);
    // Asking for more than the dataset allows changes nothing.
    assert_eq!(ask("small", Some("1GB")), 503);
    assert_eq!(ask("big", None), 200);
    assert_eq!(ask("big", Some("64KiB")), 503);
    assert_eq!(ask("big", Some("64MiB")), 200);
    assert_eq!(ask("big", Some("lots")), 400);
    // 0 would lift the dataset's limit, so it is refused.
    for zero in ["0", "0B", "0.4"] {
        assert_eq!(ask("small", Some(zero)), 400, "{zero}");
    }
    let limits: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/$/stats/small"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        limits["datasets"]["/small"]["limits"]["queryMemory"], 65536,
        "{limits}"
    );
}
