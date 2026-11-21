//! The server over HTTP, as a SPARQL client would use it.

use std::time::Duration;
use tripletrail::Engine;
use tripletrail_server::{Options, Server};

fn start(datasets: &[&str], timeout: Option<Duration>) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        datasets
            .iter()
            .map(|n| (n.to_string(), Engine::new().unwrap())),
        Options {
            timeout,
            threads: 4,
            cors: false,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn body(r: ureq::Response) -> String {
    r.into_string().unwrap()
}

fn status(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

const TTL: &str = r#"@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:a geo:hasGeometry ex:ga . ex:ga geo:asWKT "POINT(1 1)"^^geo:wktLiteral .
ex:b geo:hasGeometry ex:gb . ex:gb geo:asWKT "POINT(50 50)"^^geo:wktLiteral .
"#;

#[test]
fn query_update_and_graph_store() {
    let base = start(&["kg"], None);
    // Graph Store Protocol: PUT a named graph, then read it back.
    let r = ureq::put(&format!("{base}/kg/data?graph=http://example.com/g"))
        .set("Content-Type", "text/turtle")
        .send_string(TTL)
        .unwrap();
    assert_eq!(r.status(), 201);
    let r = ureq::get(&format!("{base}/kg/data?graph=http://example.com/g"))
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/n-triples");
    assert_eq!(body(r).lines().count(), 4);

    // A spatial query, GET with the query in the URL, JSON results by default.
    let q = r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
        SELECT ?f WHERE { GRAPH ?g { ?f geo:hasGeometry/geo:asWKT ?w }
          FILTER(geof:sfWithin(?w, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral)) }"#;
    let r = ureq::get(&format!("{base}/kg/query"))
        .query("query", q)
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/sparql-results+json");
    let json = body(r);
    assert!(
        json.contains("http://example.com/a") && !json.contains("example.com/b"),
        "{json}"
    );

    // POST application/sparql-query, CSV by Accept.
    let r = ureq::post(&format!("{base}/kg/sparql"))
        .set("Content-Type", "application/sparql-query")
        .set("Accept", "text/csv")
        .send_string(q)
        .unwrap();
    assert_eq!(body(r).replace('\r', ""), "f\nhttp://example.com/a\n");

    // An update as a form, then a CONSTRUCT as Turtle.
    let r = ureq::post(&format!("{base}/kg/update"))
        .send_form(&[(
            "update",
            "INSERT DATA { GRAPH <http://example.com/g> { <http://example.com/a> <http://example.com/p> 1 } }",
        )])
        .unwrap();
    assert_eq!(r.status(), 200);
    // As Fuseki answers an update: 200 and a page.
    assert_eq!(r.content_type(), "text/html");
    assert!(body(r).contains("Update succeeded"));
    let r = ureq::post(&format!("{base}/kg/query"))
        .send_form(&[(
            "query",
            "CONSTRUCT { ?s ?p ?o } WHERE { GRAPH ?g { ?s <http://example.com/p> ?o BIND(<http://example.com/p> AS ?p) } }",
        )])
        .unwrap();
    assert_eq!(r.content_type(), "text/turtle");
    assert!(body(r).contains("<http://example.com/p> 1"));

    // ASK as XML.
    let r = ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK { GRAPH ?g { ?s ?p ?o } }")
        .set("Accept", "application/sparql-results+xml")
        .call()
        .unwrap();
    assert!(body(r).contains("<boolean>true</boolean>"));

    // The whole dataset as N-Quads; then DELETE the graph.
    let r = ureq::get(&format!("{base}/kg/data")).call().unwrap();
    assert_eq!(r.content_type(), "application/n-quads");
    assert_eq!(body(r).lines().count(), 5);
    assert_eq!(
        status(ureq::delete(&format!("{base}/kg/data?graph=http://example.com/g")).call()),
        204
    );
    assert_eq!(
        status(ureq::get(&format!("{base}/kg/data?graph=http://example.com/g")).call()),
        404
    );
}

#[test]
fn post_appends_n_triples_to_the_default_graph_and_explain_reports() {
    let base = start(&["a", "b"], None);
    let nt = (0..1000)
        .map(|i| format!("<http://example.com/s{i}> <http://example.com/p> \"{i}\" .\n"))
        .collect::<String>();
    for _ in 0..2 {
        let r = ureq::post(&format!("{base}/b/data?default"))
            .set("Content-Type", "application/n-triples")
            .send_string(&nt)
            .map_err(|e| match e {
                ureq::Error::Status(_, r) => r.into_string().unwrap(),
                e => e.to_string(),
            })
            .unwrap();
        assert_eq!(r.status(), 204);
    }
    let count = |ds: &str| {
        body(
            ureq::get(&format!("{base}/{ds}/query"))
                .query("query", "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }")
                .set("Accept", "text/tab-separated-values")
                .call()
                .unwrap(),
        )
    };
    assert_eq!(count("b").lines().nth(1), Some("1000"));
    assert_eq!(count("a").lines().nth(1), Some("0"));
    let r = ureq::get(&format!("{base}/b/explain"))
        .query("query", "SELECT * { ?s ?p ?o }")
        .call()
        .unwrap();
    let json = body(r);
    assert!(json.starts_with("{\"rows\":1000,"), "{json}");
    // With two datasets, paths need the dataset name.
    assert_eq!(
        status(ureq::get(&format!("{base}/query?query=ASK{{}}")).call()),
        404
    );
    let json = body(ureq::get(&format!("{base}/$/datasets")).call().unwrap());
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    let names: Vec<&str> = json["datasets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["ds.name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["/a", "/b"]);
}

/// RDF/JSON and TriX, as Jena reads and writes them (#831): uploaded, read back from the
/// Graph Store, and as CONSTRUCT results.
#[test]
fn graph_store_and_construct_speak_rdf_json_and_trix() {
    let base = start(&["kg"], None);
    let rj = r#"{ "http://e/s": { "http://e/p": [
        { "type": "literal", "value": "chat", "lang": "fr" },
        { "type": "bnode", "value": "_:b1" } ] },
      "_:b1": { "http://e/q": [ { "type": "literal", "value": "7",
        "datatype": "http://www.w3.org/2001/XMLSchema#integer" } ] } }"#;
    let r = ureq::put(&format!("{base}/kg/data?graph=http://e/g"))
        .set("Content-Type", "application/rdf+json")
        .send_string(rj)
        .unwrap();
    assert_eq!(r.status(), 201);
    let trix = r#"<TriX xmlns="http://www.w3.org/2004/03/trix/trix-1/">
      <graph><uri>http://e/h</uri>
        <triple><uri>http://e/s</uri><uri>http://e/p</uri><plainLiteral>in h</plainLiteral></triple>
      </graph></TriX>"#;
    let r = ureq::post(&format!("{base}/kg/data"))
        .set("Content-Type", "application/trix")
        .send_string(trix)
        .unwrap();
    assert_eq!(r.status(), 204);
    let ask = |q: &str| {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", q)
            .set("Accept", "text/csv")
            .call()
            .unwrap();
        body(r)
    };
    assert!(ask("ASK { GRAPH <http://e/g> { <http://e/s> <http://e/p> ?b . ?b <http://e/q> 7 } GRAPH <http://e/h> { <http://e/s> <http://e/p> \"in h\" } }").contains("true"));
    // One graph as RDF/JSON, by Accept.
    let r = ureq::get(&format!("{base}/kg/data?graph=http://e/g"))
        .set("Accept", "application/rdf+json")
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/rdf+json");
    let json: serde_json::Value = serde_json::from_str(&body(r)).unwrap();
    assert_eq!(
        json["http://e/s"]["http://e/p"].as_array().unwrap().len(),
        2
    );
    // The dataset as TriX, by ?format=.
    let r = ureq::get(&format!("{base}/kg/data?format=trix"))
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/trix");
    let text = body(r);
    assert!(
        text.contains("<uri>http://e/h</uri>")
            && text.contains("<plainLiteral xml:lang=\"fr\">chat</plainLiteral>"),
        "{text}"
    );
    // A CONSTRUCT answers RDF/JSON when asked for it; RDF/JSON is one graph, so the dataset
    // isn't offered in it.
    let r = ureq::get(&format!("{base}/kg/query"))
        .query(
            "query",
            "CONSTRUCT { ?s ?p ?o } WHERE { GRAPH <http://e/h> { ?s ?p ?o } }",
        )
        .set("Accept", "application/rdf+json")
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/rdf+json");
    let json: serde_json::Value = serde_json::from_str(&body(r)).unwrap();
    assert_eq!(json["http://e/s"]["http://e/p"][0]["value"], "in h");
    assert_eq!(
        status(
            ureq::get(&format!("{base}/kg/data"))
                .set("Accept", "application/rdf+json")
                .call()
        ),
        406
    );
    // A document that isn't RDF/JSON is a 400 that says why, and changes nothing.
    let bad = ureq::post(&format!("{base}/kg/data?default"))
        .set("Content-Type", "application/rdf+json")
        .send_string("[1]");
    match bad {
        Err(ureq::Error::Status(400, r)) => {
            assert!(r.into_string().unwrap().contains("not an object"))
        }
        other => panic!("{other:?}"),
    }
}

/// Jena's RDF Thrift and RDF Protobuf (#831): the Graph Store reads and writes them, and
/// SELECT answers in Thrift and Protobuf results.
#[test]
fn graph_store_and_select_speak_rdf_thrift_and_protobuf() {
    use tripletrail_server::jena_binary::{Encoding, read, read_results, write};
    let base = start(&["kg"], None);
    let quads: Vec<tripletrail::oxigraph::model::Quad> =
        tripletrail::oxigraph::io::RdfParser::from_format(tripletrail::oxigraph::io::RdfFormat::NQuads)
            .for_slice(
                b"<http://e/s> <http://e/p> \"x\"@en <http://e/g> .\n<http://e/s> <http://e/q> _:b <http://e/g> .\n",
            )
            .collect::<Result<_, _>>()
            .unwrap();
    let r = ureq::post(&format!("{base}/kg/data"))
        .set("Content-Type", "application/rdf+protobuf")
        .send_bytes(&write(Encoding::Protobuf, &quads, true).unwrap())
        .unwrap();
    assert_eq!(r.status(), 204);
    let bytes = |r: ureq::Response| {
        let mut v = Vec::new();
        std::io::Read::read_to_end(&mut r.into_reader(), &mut v).unwrap();
        v
    };
    let r = ureq::get(&format!("{base}/kg/data"))
        .set("Accept", "application/rdf+thrift")
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/rdf+thrift");
    let back = read(Encoding::Thrift, &bytes(r)).unwrap();
    assert_eq!(back.len(), 2);
    assert!(
        back.iter()
            .all(|q| q.graph_name.to_string() == "<http://e/g>"),
        "{back:?}"
    );
    // One graph: triple rows.
    let r = ureq::get(&format!("{base}/kg/data?graph=http://e/g&format=protobuf"))
        .call()
        .unwrap();
    let back = read(Encoding::Protobuf, &bytes(r)).unwrap();
    assert!(back.iter().all(|q| q.graph_name.is_default_graph()) && back.len() == 2);
    // SELECT, with an unbound value.
    for (accept, encoding) in [
        ("application/sparql-results+thrift", Encoding::Thrift),
        ("application/sparql-results+protobuf", Encoding::Protobuf),
    ] {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query(
                "query",
                "SELECT ?o ?none { GRAPH ?g { ?s <http://e/p> ?o } OPTIONAL { ?s <http://e/nothing> ?none } }",
            )
            .set("Accept", accept)
            .call()
            .unwrap();
        assert_eq!(r.content_type(), accept);
        let (vars, rows) = read_results(encoding, &bytes(r)).unwrap();
        assert_eq!(vars, ["o", "none"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_ref().unwrap().to_string(), "\"x\"@en");
        assert!(rows[0][1].is_none());
    }
}

/// Each upload's blank nodes are its own, as on Fuseki (#840): the same body posted twice
/// adds two blank nodes, for a small body (one transaction) and a large one (a bulk load,
/// parsed in parts), and the label shared by its first and last triple stays one node.
#[test]
fn each_upload_has_its_own_blank_nodes() {
    let base = start(&["kg"], None);
    let count = |query: &str| {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", query)
            .set("Accept", "text/tab-separated-values")
            .call()
            .unwrap();
        body(r).lines().nth(1).unwrap().to_owned()
    };
    for (n, graph) in [(10, "small"), (100_010, "large")] {
        let mut nt = String::from("_:shared <http://e/name> \"first\" .\n");
        for i in 0..n {
            nt += &format!("<http://e/s{i}> <http://e/p> \"{i}\" .\n");
        }
        nt += "<http://e/last> <http://e/p> _:shared .\n";
        for _ in 0..2 {
            let r = ureq::post(&format!("{base}/kg/data"))
                .query("graph", &format!("http://e/{graph}"))
                .set("Content-Type", "application/n-triples")
                .send_string(&nt)
                .unwrap();
            assert!(matches!(r.status(), 201 | 204), "{}", r.status());
        }
        let g = format!("<http://e/{graph}>");
        assert_eq!(
            count(&format!(
                "SELECT (COUNT(DISTINCT ?b) AS ?n) {{ GRAPH {g} {{ ?b <http://e/name> ?x }} }}"
            )),
            "2",
            "{graph}"
        );
        assert_eq!(
            count(&format!(
                "SELECT (COUNT(*) AS ?n) {{ GRAPH {g} {{ <http://e/last> <http://e/p> ?b . \
                 ?b <http://e/name> ?x }} }}"
            )),
            "2",
            "{graph}"
        );
    }
}

#[test]
fn errors_and_timeouts() {
    let base = start(&["ds"], Some(Duration::from_secs(60)));
    // A single dataset also answers without its name.
    assert_eq!(
        status(ureq::get(&format!("{base}/query?query=ASK{{}}")).call()),
        200
    );
    assert_eq!(
        status(ureq::get(&format!("{base}/ds/query?query=SELEC")).call()),
        400
    );
    // A POST without a query is an error; a GET without one gets the service description.
    assert_eq!(
        status(ureq::post(&format!("{base}/ds/query")).send_form(&[("timeout", "5")])),
        400
    );
    assert_eq!(
        status(ureq::get(&format!("{base}/nope/query?query=ASK{{}}")).call()),
        404
    );
    assert_eq!(
        status(
            ureq::post(&format!("{base}/ds/data?default"))
                .set("Content-Type", "application/x-unknown")
                .send_string("x")
        ),
        415
    );
    // A query far longer than its 0.2 s timeout is cancelled with 503. (Cancellation is
    // checked on store access, so the query reads the store in its inner loop.)
    let nt = (0..3000)
        .map(|i| {
            format!(
                "<http://example.com/s{i}> <http://example.com/p> <http://example.com/s{}> .\n",
                i + 1
            )
        })
        .collect::<String>();
    ureq::post(&format!("{base}/ds/data?default"))
        .set("Content-Type", "application/n-triples")
        .send_string(&nt)
        .unwrap();
    // A chain of 3000 nodes: the closure has 4.5 million pairs, each step a store lookup.
    let slow = "SELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/p>* ?b }";
    let started = std::time::Instant::now();
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", slow)
        .query("timeout", "0.2")
        .call();
    assert_eq!(status(r), 503);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // Counted as a timeout, not as a memory trip (#189). Counted just after the reply.
    let timeout = r#"tripletrail_timeouts_total{dataset="ds"} 1"#;
    let memory = r#"tripletrail_budget_trips_total{dataset="ds",budget="memory"} 0"#;
    let mut metrics = String::new();
    for _ in 0..500 {
        metrics = body(ureq::get(&format!("{base}/$/metrics")).call().unwrap());
        if metrics.lines().any(|l| l == timeout) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(metrics.lines().any(|l| l == timeout), "{metrics}");
    assert!(metrics.lines().any(|l| l == memory), "{metrics}");
    // A timeout once the rows are being sent cuts the body, and the client sees it cut
    // (#549): no last chunk, the connection closed. Also in CSV and N-Triples, where a
    // shorter body would read as a whole result. The client is not left waiting. The
    // evaluator streams its rows; the id executor (#11) has the whole answer before the first
    // byte, so its timeout is a status instead.
    let agent = ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(20))
        .build();
    for (query, accept, start) in [
        (
            "#pragma tt.executor spareval\nSELECT * WHERE { ?a ?b ?c . ?d ?e ?f }",
            "application/sparql-results+json",
            &b"{\"head\""[..],
        ),
        (
            "#pragma tt.executor spareval\nSELECT * WHERE { ?a ?b ?c . ?d ?e ?f }",
            "text/csv",
            b"a,b,c,d,e,f",
        ),
        (
            "CONSTRUCT { ?a ?b ?d } WHERE { ?a ?b ?c . ?d ?e ?f }",
            "application/n-triples",
            b"<http",
        ),
    ] {
        let started = std::time::Instant::now();
        let r = agent
            .get(&format!("{base}/ds/query"))
            .query("query", query)
            .query("timeout", "0.3")
            .set("Accept", accept)
            .call()
            .unwrap();
        assert_eq!(r.status(), 200);
        let mut sent = Vec::new();
        let read = std::io::Read::read_to_end(&mut r.into_reader(), &mut sent);
        assert!(read.is_err(), "{accept}: a cut body read as whole");
        assert!(sent.starts_with(start), "{accept}");
        assert!(sent.len() > 64 * 1024, "{accept}: {}", sent.len());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{accept}: {:?}",
            started.elapsed()
        );
        // The agent's next request opens a new connection and is answered.
        assert_eq!(
            status(agent.get(&format!("{base}/ds/query?query=ASK{{}}")).call()),
            200
        );
    }
}

#[test]
fn status_says_when_the_spatial_index_is_ready() {
    let engine = Engine::new().unwrap();
    engine
        .load(tripletrail::oxigraph::io::RdfFormat::Turtle, TTL.as_bytes())
        .unwrap();
    engine.warm_up();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine.clone())],
        Options {
            timeout: None,
            threads: 2,
            cors: false,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let ready = || {
        let json = body(ureq::get(&format!("{base}/$/status")).call().unwrap());
        assert!(json.contains("\"kg\":{\"spatial_index\":"), "{json}");
        json.starts_with("{\"ready\":true")
    };
    let wait = || {
        for _ in 0..500 {
            if ready() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the spatial index was not built in the background");
    };
    wait();
    // A bulk load drops the index; it is rebuilt in the background, with the new geometry.
    engine
        .load(
            tripletrail::oxigraph::io::RdfFormat::Turtle,
            r#"<http://example.com/gc> <http://www.opengis.net/ont/geosparql#asWKT> "POINT(2 2)"^^<http://www.opengis.net/ont/geosparql#wktLiteral> ."#.as_bytes(),
        )
        .unwrap();
    wait();
    assert_eq!(engine.spatial_index().unwrap().stats().geometries, 3);
}

#[cfg(feature = "geoparquet")]
#[test]
fn select_results_as_geoparquet() {
    use std::io::Read;
    let base = start(&["ds"], None);
    ureq::post(&format!("{base}/ds/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(
            r#"INSERT DATA { <http://example.com/a> <http://www.opengis.net/ont/geosparql#asWKT>
                "POINT(1 2)"^^<http://www.opengis.net/ont/geosparql#wktLiteral> }"#,
        )
        .unwrap();
    let q = "SELECT ?s ?w WHERE { ?s <http://www.opengis.net/ont/geosparql#asWKT> ?w }";
    for request in [
        ureq::get(&format!("{base}/ds/query"))
            .query("query", q)
            .set("Accept", "application/vnd.apache.parquet"),
        ureq::get(&format!("{base}/ds/query"))
            .query("query", q)
            .query("format", "parquet"),
    ] {
        let r = request.call().unwrap();
        assert_eq!(r.content_type(), "application/vnd.apache.parquet");
        let mut file = Vec::new();
        r.into_reader().read_to_end(&mut file).unwrap();
        assert!(file.starts_with(b"PAR1") && file.ends_with(b"PAR1"));
        // Loaded back: the same geometry.
        let engine = Engine::new_tripletrail();
        let stats = engine
            .load_geoparquet(
                tripletrail::geoparquet::Bytes::from(file),
                &tripletrail::geoparquet::ImportMapping {
                    subject: Some("{+s}".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(stats.rows, 1);
    }
}

#[test]
fn bad_timeouts_are_rejected_and_workers_survive() {
    let base = start(&["ds"], None);
    // These used to panic in `Duration::from_secs_f64`, and every panic took a worker thread
    // down for good. More bad requests than workers, then the server must still answer.
    for _ in 0..3 {
        for t in ["-1", "NaN", "inf", "-inf", "1e30", "abc", ""] {
            let r = ureq::get(&format!("{base}/ds/query"))
                .query("query", "ASK {}")
                .query("timeout", t)
                .call();
            assert_eq!(status(r), 400, "timeout={t}");
        }
    }
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", "ASK {}")
        .query("timeout", "0")
        .call();
    // A zero timeout is valid; it may cancel before or after this trivial query.
    assert!(matches!(status(r), 200 | 503));
    assert_eq!(
        status(ureq::get(&format!("{base}/ds/query?query=ASK{{}}")).call()),
        200
    );
}

fn start_with(options: Options) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        [("ds".to_owned(), Engine::new().unwrap())],
        Options {
            threads: 2,
            ..options
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

#[test]
fn bodies_over_the_limits_are_413() {
    let base = start_with(Options {
        max_body: Some(64),
        max_upload: Some(256),
        ..Options::default()
    });
    let long = format!("ASK {{ ?s ?p \"{}\" }}", "x".repeat(100));
    let r = ureq::post(&format!("{base}/ds/query"))
        .set("Content-Type", "application/sparql-query")
        .send_string(&long);
    assert_eq!(status(r), 413);
    let r = ureq::post(&format!("{base}/ds/query"))
        .set("Content-Type", "application/sparql-query")
        .send_string("ASK {}");
    assert_eq!(status(r), 200);
    let nt: String = (0..20)
        .map(|i| format!("<http://example.com/s{i}> <http://example.com/p> \"{i}\" .\n"))
        .collect();
    let r = ureq::post(&format!("{base}/ds/data?default"))
        .set("Content-Type", "application/n-triples")
        .send_string(&nt);
    assert_eq!(status(r), 413);
    // The server still answers after refusing.
    assert_eq!(status(ureq::get(&format!("{base}/$/ping")).call()), 200);
}

#[test]
fn uploads_with_an_error_change_nothing() {
    let base = start_with(Options::default());
    let graph = format!("{base}/ds/data?graph=http://example.com/g");
    let good = "<http://example.com/a> <http://example.com/p> <http://example.com/b> .\n";
    let r = ureq::put(&graph)
        .set("Content-Type", "application/n-triples")
        .send_string(good);
    assert_eq!(status(r), 201);
    let bad = "<http://example.com/c> <http://example.com/p> <http://example.com/d> .\n\
               <http://example.com/e> <http://example.com/p> .\n";
    for method in ["PUT", "POST"] {
        let r = ureq::request(method, &graph)
            .set("Content-Type", "application/n-triples")
            .send_string(bad);
        assert_eq!(status(r), 400, "{method}");
        let r = ureq::get(&graph)
            .set("Accept", "application/n-triples")
            .call()
            .unwrap();
        assert_eq!(body(r), good, "{method} changed the graph");
    }
}

#[test]
fn a_request_cannot_lengthen_the_server_timeout() {
    let base = start_with(Options {
        timeout: Some(Duration::from_millis(200)),
        ..Options::default()
    });
    let nt = (0..3000)
        .map(|i| {
            format!(
                "<http://example.com/s{i}> <http://example.com/p> <http://example.com/s{}> .\n",
                i + 1
            )
        })
        .collect::<String>();
    ureq::post(&format!("{base}/ds/data?default"))
        .set("Content-Type", "application/n-triples")
        .send_string(&nt)
        .unwrap();
    let slow = "SELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/p>* ?b }";
    let started = std::time::Instant::now();
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", slow)
        .query("timeout", "600")
        .call();
    assert_eq!(status(r), 503);
    // Nor can a hint (#238).
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", &format!("#pragma tt.timeout 600s\n{slow}"))
        .call();
    assert_eq!(status(r), 503);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_timeout_hint_in_the_query_stops_it() {
    // `#pragma tt.timeout` (#238) as `?timeout=`: up to the server's timeout.
    let base = start(&["ds"], Some(Duration::from_secs(60)));
    let nt = (0..3000)
        .map(|i| {
            format!(
                "<http://example.com/s{i}> <http://example.com/p> <http://example.com/s{}> .\n",
                i + 1
            )
        })
        .collect::<String>();
    ureq::post(&format!("{base}/ds/data?default"))
        .set("Content-Type", "application/n-triples")
        .send_string(&nt)
        .unwrap();
    let slow =
        "#pragma tt.timeout 200ms\nSELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/p>* ?b }";
    let started = std::time::Instant::now();
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", slow)
        .call();
    assert_eq!(status(r), 503);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // A bad hint is a bad request.
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", "#pragma tt.timeout soon\nASK {}")
        .call();
    assert_eq!(status(r), 400);
}

#[test]
fn timeouts_stop_routing_functions() {
    let base = start(&["ds"], Some(Duration::from_secs(60)));
    // A 140 x 140 grid: 19 600 vertices, so all pairs is 19 600 full searches (minutes).
    let side = 140;
    let mut nt = String::new();
    let mut edge = 0;
    for y in 0..side {
        for x in 0..side {
            let v = y * side + x;
            for w in [
                (x + 1 < side).then_some(v + 1),
                (y + 1 < side).then_some(v + side),
            ]
            .into_iter()
            .flatten()
            {
                nt.push_str(&format!(
                    "<http://example.com/e{edge}> <http://example.com/from> <http://example.com/v{v}> .\n\
                     <http://example.com/e{edge}> <http://example.com/to> <http://example.com/v{w}> .\n"
                ));
                edge += 1;
            }
        }
    }
    ureq::post(&format!("{base}/ds/data?default"))
        .set("Content-Type", "application/n-triples")
        .send_string(&nt)
        .unwrap();
    let query = r#"
        PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
        SELECT (COUNT(*) AS ?n) WHERE {
          SERVICE <http://w3id.org/tripletrail/pgr> {
            [] pgr:function pgr:pgr_floydWarshall ; pgr:directed false ;
               pgr:start_vid ?s ; pgr:end_vid ?t ; pgr:agg_cost ?agg .
            { SELECT ?id ?source ?target ?cost WHERE {
                ?id <http://example.com/from> ?source ; <http://example.com/to> ?target .
                BIND(1.0 AS ?cost)
            } }
          }
        }"#;
    // The network is built and cached by a query that needs only one search.
    let warm = query.replace("pgr_floydWarshall", "pgr_dijkstra").replace(
        "pgr:start_vid ?s ; pgr:end_vid ?t",
        "pgr:start_vid <http://example.com/v0> ; pgr:end_vid <http://example.com/v1>",
    );
    assert_eq!(
        status(
            ureq::get(&format!("{base}/ds/query"))
                .query("query", &warm)
                .call()
        ),
        200
    );
    let started = std::time::Instant::now();
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", query)
        .query("timeout", "1")
        .call();
    assert_eq!(status(r), 503);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[test]
fn writes_need_the_update_token_and_cors_answers_listed_origins() {
    let server = Server::bind(
        "127.0.0.1:0",
        [("ds".to_owned(), Engine::new().unwrap())],
        Options {
            threads: 2,
            cors_origins: vec!["https://ok.example".to_owned()],
            update_token: Some("w-secret".to_owned()),
            admin_token: Some("a-secret".to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let update = "INSERT DATA { <http://example.com/s> <http://example.com/p> 1 }";
    let post = |auth: Option<&str>| {
        let r = ureq::post(&format!("{base}/ds/update"))
            .set("Content-Type", "application/sparql-update");
        let r = match auth {
            Some(a) => r.set("Authorization", a),
            None => r,
        };
        status(r.send_string(update))
    };
    assert_eq!(post(None), 401);
    assert_eq!(post(Some("Bearer nope")), 401);
    assert_eq!(post(Some("Bearer w-secret")), 200);
    // The admin token may write too.
    assert_eq!(post(Some("Bearer a-secret")), 200);
    // A form POST, which a web page can send without a preflight, is a write as well.
    assert_eq!(
        status(ureq::post(&format!("{base}/ds/sparql")).send_form(&[("update", "DROP ALL")])),
        401
    );
    let put = |auth: Option<&str>| {
        let r = ureq::put(&format!("{base}/ds/data?default"))
            .set("Content-Type", "application/n-triples");
        let r = match auth {
            Some(a) => r.set("Authorization", a),
            None => r,
        };
        status(r.send_string(
            "<http://example.com/a> <http://example.com/p> <http://example.com/b> .\n",
        ))
    };
    assert_eq!(put(None), 401);
    assert!(matches!(put(Some("Bearer w-secret")), 200..=204));
    assert_eq!(
        status(ureq::delete(&format!("{base}/ds/data?default")).call()),
        401
    );
    // Queries stay open.
    assert_eq!(
        status(ureq::get(&format!("{base}/ds/query?query=ASK{{}}")).call()),
        200
    );
    // CORS: a listed origin is echoed, another gets no allow header.
    let preflight = |origin: &str| {
        ureq::request("OPTIONS", &format!("{base}/ds/query"))
            .set("Origin", origin)
            .set("Access-Control-Request-Method", "POST")
            .call()
            .unwrap()
    };
    let ok = preflight("https://ok.example");
    assert_eq!(
        ok.header("Access-Control-Allow-Origin"),
        Some("https://ok.example")
    );
    assert!(ok.header("Access-Control-Allow-Methods").is_some());
    // A page may send an API key, and read whether the row budget cut an answer (#223).
    let allowed = ok
        .header("Access-Control-Allow-Headers")
        .unwrap_or_default();
    assert!(allowed.contains("X-API-Key"), "{allowed}");
    let answer = ureq::get(&format!("{base}/ds/query?query=ASK{{}}"))
        .set("Origin", "https://ok.example")
        .call()
        .unwrap();
    let exposed = answer
        .header("Access-Control-Expose-Headers")
        .unwrap_or_default();
    assert!(exposed.contains("X-Result-Incomplete"), "{exposed}");
    let other = preflight("https://evil.example");
    assert_eq!(other.header("Access-Control-Allow-Origin"), None);
    assert_eq!(other.header("Access-Control-Allow-Methods"), None);
    assert_eq!(other.header("Access-Control-Expose-Headers"), None);
}

#[test]
fn repeated_queries_say_when_the_result_cache_answered() {
    let engine = Engine::new_tripletrail();
    engine.set_result_cache(1 << 20);
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let q = "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";
    let cache = |q: &str| {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", q)
            .call()
            .unwrap();
        let header = r.header("X-Cache").unwrap_or_default().to_owned();
        (header, body(r))
    };
    let (first, empty) = cache(q);
    assert_eq!(first, "MISS");
    let (second, again) = cache(q);
    assert_eq!((second.as_str(), &again), ("HIT", &empty));
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <http://example.com/a> <http://example.com/p> 1 }")
        .unwrap();
    let (after, one) = cache(q);
    assert_eq!(after, "MISS");
    assert_ne!(one, empty);
    assert_eq!(cache("SELECT (RAND() AS ?r) WHERE {}").0, "BYPASS");
}

/// A GET of an endpoint without a query: the SPARQL 1.1 Service Description, generated from
/// what the engine registers and the server negotiates. Fails when a registered function,
/// aggregate, SERVICE or format is missing from it.
#[test]
fn endpoints_describe_themselves() {
    use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
    use tripletrail::oxigraph::model::{Graph, NamedNodeRef, TermRef};

    let base = start(&["kg"], None);
    ureq::put(&format!("{base}/kg/data?graph=http://example.com/g"))
        .set("Content-Type", "text/turtle")
        .send_string(TTL)
        .unwrap();
    let sd = |name: &str| format!("http://www.w3.org/ns/sparql-service-description#{name}");
    let has = |g: &Graph, p: &str, o: &str| {
        g.triples_for_predicate(NamedNodeRef::new_unchecked(p))
            .any(|t| match t.object {
                TermRef::NamedNode(n) => n.as_str() == o,
                TermRef::Literal(l) => l.value() == o,
                _ => false,
            })
    };

    for (path, accept, format) in [
        ("/kg/query", "text/turtle", RdfFormat::Turtle),
        ("/kg/sparql", "application/n-triples", RdfFormat::NTriples),
        ("/kg", "application/rdf+xml", RdfFormat::RdfXml),
        (
            "/kg/update",
            "application/ld+json",
            RdfFormat::JsonLd {
                profile: Default::default(),
            },
        ),
        ("/query", "*/*", RdfFormat::Turtle),
    ] {
        let r = ureq::get(&format!("{base}{path}"))
            .set("Accept", accept)
            .call()
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let text = body(r);
        let graph: Graph = RdfParser::from_format(format)
            .for_slice(text.as_bytes())
            .map(|q| tripletrail::oxigraph::model::Triple::from(q.unwrap()))
            .collect();
        assert!(
            has(&graph, &sd("endpoint"), &format!("{base}{path}")),
            "{path}: {text}"
        );
        assert!(has(&graph, &sd("supportedLanguage"), &sd("SPARQL11Query")));
        assert!(has(&graph, &sd("supportedLanguage"), &sd("SPARQL11Update")));
        assert!(has(&graph, &sd("name"), "http://example.com/g"), "{path}");

        for f in tripletrail::extension_functions() {
            assert!(has(&graph, &sd("extensionFunction"), f.as_str()), "{f}");
        }
        for f in tripletrail::extension_aggregates() {
            assert!(has(&graph, &sd("extensionAggregate"), f.as_str()), "{f}");
        }
        for s in tripletrail::extension_services() {
            assert!(has(
                &graph,
                "http://www.w3.org/2000/01/rdf-schema#seeAlso",
                s.as_str()
            ));
            // Every sd:Service has a language, as the W3C suite's conforms-to-schema checks.
            assert!(
                graph
                    .objects_for_subject_predicate(
                        NamedNodeRef::new_unchecked(s.as_str()),
                        NamedNodeRef::new_unchecked(&sd("supportedLanguage"))
                    )
                    .any(|o| o == NamedNodeRef::new_unchecked(&sd("SPARQL11Query")).into()),
                "{s}"
            );
        }
        for f in [
            "http://www.w3.org/ns/formats/SPARQL_Results_JSON",
            "http://www.w3.org/ns/formats/SPARQL_Results_XML",
            "http://www.w3.org/ns/formats/SPARQL_Results_CSV",
            "http://www.w3.org/ns/formats/SPARQL_Results_TSV",
            "http://www.w3.org/ns/formats/Turtle",
        ] {
            assert!(has(&graph, &sd("resultFormat"), f), "{f}");
        }
        for f in [
            "http://www.w3.org/ns/formats/Turtle",
            "http://www.w3.org/ns/formats/N-Triples",
            "http://www.w3.org/ns/formats/N-Quads",
            "http://www.w3.org/ns/formats/TriG",
            "http://www.w3.org/ns/formats/RDF_XML",
        ] {
            assert!(has(&graph, &sd("inputFormat"), f), "{f}");
        }
    }
    // The geo functions are there (a sanity check that the registry is not empty).
    assert!(tripletrail::extension_functions().len() > 300);

    // Every format the endpoint negotiates for SELECT is described: ask for each media type.
    let text = body(ureq::get(&format!("{base}/kg/query")).call().unwrap());
    for media in [
        "application/sparql-results+json",
        "application/sparql-results+xml",
        "text/csv",
        "text/tab-separated-values",
    ] {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", "ASK {}")
            .set("Accept", media)
            .call()
            .unwrap();
        assert!(
            r.content_type()
                .starts_with(media.split(';').next().unwrap())
        );
        let iri =
            tripletrail::oxigraph::sparql::results::QueryResultsFormat::from_media_type(media)
                .unwrap()
                .iri();
        assert!(text.contains(iri.rsplit('/').next().unwrap()), "{iri}");
    }

    // A browser is sent to the UI.
    let r = ureq::builder()
        .redirects(0)
        .build()
        .get(&format!("{base}/kg/query"))
        .set("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .call()
        .unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(r.header("Location"), Some("/$/ui/"));
}

/// A read-only dataset does not offer SPARQL Update in its description.
#[test]
fn read_only_endpoints_do_not_describe_update() {
    let dir = tripletrail_testdir::tempdir("sd-ro");
    let engine = Engine::new_tripletrail();
    engine
        .update("INSERT DATA { <http://example.com/a> <http://example.com/p> 1 }")
        .unwrap();
    engine.save(&dir).unwrap();
    drop(engine);
    let server = Server::bind(
        "127.0.0.1:0",
        [("ro".to_owned(), Engine::open_read_only(&dir).unwrap())],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let text = body(ureq::get(&format!("{base}/ro/query")).call().unwrap());
    assert!(text.contains("SPARQL11Query"), "{text}");
    assert!(!text.contains("SPARQL11Update"), "{text}");
}

/// VoID at `/.well-known/void` (every dataset) and `/{ds}/void`, with numbers equal to the
/// counting queries, and the graph sizes in the service description (#242).
#[test]
fn void_describes_every_dataset_with_partitions() {
    use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
    use tripletrail::oxigraph::model::{Graph, NamedNodeRef, NamedOrBlankNodeRef, TermRef, Triple};

    let base = start(&["kg", "other"], None);
    ureq::put(&format!("{base}/kg/data?graph=http://example.com/g"))
        .set("Content-Type", "text/turtle")
        .send_string(TTL)
        .unwrap();
    ureq::post(&format!("{base}/kg/update"))
        .send_form(&[(
            "update",
            "PREFIX ex: <http://example.com/> INSERT DATA { ex:a a ex:C . ex:b a ex:C , ex:D . ex:a ex:p 1 }",
        )])
        .unwrap();
    let void = |s: &str| format!("http://rdfs.org/ns/void#{s}");
    let parse = |text: &str| -> Graph {
        RdfParser::from_format(RdfFormat::Turtle)
            .for_slice(text.as_bytes())
            .map(|q| Triple::from(q.unwrap()))
            .collect()
    };
    let number = |g: &Graph, s: NamedOrBlankNodeRef<'_>, p: &str| -> u64 {
        match g.object_for_subject_predicate(s, NamedNodeRef::new_unchecked(p)) {
            Some(TermRef::Literal(l)) => l.value().parse().unwrap(),
            other => panic!("{p}: {other:?}"),
        }
    };
    let count = |q: &str| -> u64 {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", q)
            .set("Accept", "text/csv")
            .call()
            .unwrap();
        body(r).lines().nth(1).unwrap().trim().parse().unwrap()
    };

    let all = parse(&body(
        ureq::get(&format!("{base}/.well-known/void"))
            .call()
            .unwrap(),
    ));
    for name in ["kg", "other"] {
        let ds = NamedNodeRef::new_unchecked(&format!("{base}/{name}/void#dataset")).into_owned();
        assert!(all.contains(tripletrail::oxigraph::model::TripleRef::new(
            &ds,
            tripletrail::oxigraph::model::vocab::rdf::TYPE,
            NamedNodeRef::new_unchecked(&void("Dataset")),
        )));
    }
    let g = parse(&body(ureq::get(&format!("{base}/kg/void")).call().unwrap()));
    let ds = NamedNodeRef::new_unchecked(&format!("{base}/kg/void#dataset")).into_owned();
    let ds = NamedOrBlankNodeRef::from(&ds);
    let union = "{ { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }";
    assert_eq!(
        number(&g, ds, &void("triples")),
        count(&format!("SELECT (COUNT(*) AS ?n) WHERE {union}"))
    );
    assert_eq!(
        number(&g, ds, &void("distinctSubjects")),
        count(&format!("SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {union}"))
    );
    assert_eq!(
        number(&g, ds, &void("properties")),
        count(&format!("SELECT (COUNT(DISTINCT ?p) AS ?n) WHERE {union}"))
    );
    assert_eq!(number(&g, ds, &void("classes")), 2);
    // One partition per class and per property, with their counts.
    let partitions = |kind: &str, key: &str| -> Vec<(String, u64, NamedOrBlankNodeRef<'_>)> {
        g.objects_for_subject_predicate(ds, NamedNodeRef::new_unchecked(&void(kind)))
            .map(|p| {
                let p = match p {
                    TermRef::BlankNode(b) => NamedOrBlankNodeRef::BlankNode(b),
                    other => panic!("{other}"),
                };
                let key = g
                    .object_for_subject_predicate(p, NamedNodeRef::new_unchecked(&void(key)))
                    .unwrap()
                    .to_string();
                let n = if kind == "classPartition" {
                    "entities"
                } else {
                    "triples"
                };
                (key, number(&g, p, &void(n)), p)
            })
            .collect()
    };
    let classes = partitions("classPartition", "class");
    let mut classes: Vec<(String, u64)> = classes.into_iter().map(|(c, n, _)| (c, n)).collect();
    classes.sort();
    assert_eq!(
        classes,
        [
            ("<http://example.com/C>".to_owned(), 2),
            ("<http://example.com/D>".to_owned(), 1)
        ]
    );
    let properties = partitions("propertyPartition", "property");
    assert_eq!(properties.len(), 4, "{properties:?}");
    for (p, n, _) in &properties {
        assert_eq!(
            *n,
            count(&format!(
                "SELECT (COUNT(*) AS ?n) WHERE {{ {{ ?s {p} ?o }} UNION {{ GRAPH ?g {{ ?s {p} ?o }} }} }}"
            )),
            "{p}"
        );
    }
    // The service description gives each graph's size.
    let sd = body(ureq::get(&format!("{base}/kg/query")).call().unwrap());
    assert!(sd.contains(&format!("{base}/kg/void#dataset")), "{sd}");
    assert!(sd.contains("void:triples 4"), "{sd}");
}

#[test]
fn metrics_count_requests_in_prometheus_format() {
    let base = start(&["kg"], None);
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <http://e/a> <http://e/p> 1 }")
        .unwrap();
    for _ in 0..3 {
        ureq::get(&format!("{base}/kg/query"))
            .query("query", "SELECT * WHERE { ?s ?p ?o }")
            .call()
            .unwrap();
    }
    match ureq::get(&format!("{base}/kg/query"))
        .query("query", "SELEC")
        .call()
    {
        Err(ureq::Error::Status(400, _)) => {}
        other => panic!("expected a 400, got {other:?}"),
    }
    let expected = [
        r#"tripletrail_requests_total{dataset="kg",operation="query",status="200"} 3"#,
        r#"tripletrail_requests_total{dataset="kg",operation="query",status="400"} 1"#,
        r#"tripletrail_requests_total{dataset="kg",operation="update",status="200"} 1"#,
        r#"tripletrail_request_duration_seconds_bucket{dataset="kg",operation="query",le="+Inf"} 4"#,
        r#"tripletrail_request_duration_seconds_count{dataset="kg",operation="update"} 1"#,
        r#"tripletrail_timeouts_total{dataset="kg"} 0"#,
        "# TYPE tripletrail_request_duration_seconds histogram",
    ];
    // A request is counted after its reply is written, so the client can see the reply a
    // moment before the count, and the in-flight gauge alone does not show that every earlier
    // request is in: scrape until the expected lines are all there.
    let mut text = String::new();
    for _ in 0..500 {
        let r = ureq::get(&format!("{base}/$/metrics")).call().unwrap();
        assert!(r.content_type().starts_with("text/plain"));
        text = body(r);
        if expected.iter().all(|line| text.lines().any(|l| l == *line)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for line in expected {
        assert!(text.lines().any(|l| l == line), "{line}\n{text}");
    }
    // The quad count comes only from the tripletrail store, which keeps it (on Oxigraph's store
    // it would be a scan at every scrape).
    let triples = r#"tripletrail_triples{dataset="kg"} 1"#;
    let oxigraph = std::env::var("TRIPLETRAIL_STORE").as_deref() == Ok("oxigraph");
    assert_eq!(text.lines().any(|l| l == triples), !oxigraph, "{text}");
    for outcome in ["parallel", "too_many_values", "blank_node"] {
        let label = format!("tripletrail_update_bind_chains_total{{outcome=\"{outcome}\"}} ");
        assert!(
            text.lines().any(|l| l.starts_with(&label)),
            "{label}\n{text}"
        );
    }
    // Every sample line parses as name{labels} value.
    for l in text.lines().filter(|l| !l.starts_with('#')) {
        let value = l.rsplit(' ').next().unwrap();
        assert!(value.parse::<f64>().is_ok(), "{l}");
    }
}

#[test]
fn row_budgets_refuse_or_cut_large_results() {
    let base = start_with(Options {
        max_rows: Some(5),
        ..Options::default()
    });
    let mut data = String::new();
    for i in 0..10 {
        data += &format!("<http://e/s{i}> <http://e/p> \"{i}\" .\n");
    }
    ureq::post(&format!("{base}/ds/data?default"))
        .set("Content-Type", "application/n-triples")
        .send_string(&data)
        .unwrap();
    let select = |extra: &[(&str, &str)]| {
        let mut r = ureq::get(&format!("{base}/ds/query"))
            .query("query", "SELECT ?s WHERE { ?s ?p ?o }")
            .set("Accept", "text/csv");
        for (k, v) in extra {
            r = r.query(k, v);
        }
        r.call().map_err(|e| match e {
            ureq::Error::Status(code, _) => code,
            e => panic!("{e}"),
        })
    };
    // Over the server's budget: refused.
    assert_eq!(select(&[]).err(), Some(413));
    // Cut on request, and said so.
    let r = select(&[("partial", "true")]).unwrap();
    assert_eq!(r.header("X-Result-Incomplete"), Some("max-rows=5"));
    assert_eq!(body(r).lines().count(), 6); // header + 5 rows
    // A request may ask for less, not more.
    let r = select(&[("max-rows", "3"), ("partial", "true")]).unwrap();
    assert_eq!(body(r).lines().count(), 4);
    assert_eq!(select(&[("max-rows", "100")]).err(), Some(413));
    // Within the budget: everything, no header.
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", "SELECT ?s WHERE { ?s ?p ?o } LIMIT 5")
        .set("Accept", "text/csv")
        .call()
        .unwrap();
    assert_eq!(r.header("X-Result-Incomplete"), None);
    assert_eq!(body(r).lines().count(), 6);
    // CONSTRUCT counts triples.
    let r = ureq::get(&format!("{base}/ds/query"))
        .query("query", "CONSTRUCT WHERE { ?s ?p ?o }")
        .query("partial", "1")
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(body(r).lines().count(), 5);
    // The trips show up in the metrics, by budget (#189). A request is counted just after
    // its reply is written: scrape until the last one is in.
    let expected = [
        r#"tripletrail_requests_total{dataset="ds",operation="query",status="413"} 2"#,
        r#"tripletrail_budget_trips_total{dataset="ds",budget="rows"} 2"#,
        r#"tripletrail_budget_trips_total{dataset="ds",budget="partial"} 3"#,
        r#"tripletrail_budget_trips_total{dataset="ds",budget="memory"} 0"#,
        r#"tripletrail_timeouts_total{dataset="ds"} 0"#,
    ];
    let mut metrics = String::new();
    for _ in 0..500 {
        metrics = body(ureq::get(&format!("{base}/$/metrics")).call().unwrap());
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

#[test]
fn protocol_datasets_bases_and_bad_requests() {
    let base = start(&["kg"], None);
    let put = |graph: &str, ttl: &str| {
        ureq::put(&format!("{base}/kg/data?graph={graph}"))
            .set("Content-Type", "text/turtle")
            .send_string(ttl)
            .unwrap();
    };
    put("urn:g1", "<urn:a> <urn:p> 1 .");
    put("urn:g2", "<urn:b> <urn:p> 2 .");
    let ask = |query: &str, params: &[(&str, &str)]| -> String {
        let mut r = ureq::get(&format!("{base}/kg/query")).query("query", query);
        for (k, v) in params {
            r = r.query(k, v);
        }
        body(r.call().unwrap())
    };
    // default-graph-uri makes those graphs the default graph; named-graph-uri lists the
    // named graphs, and replaces the query's own FROM and FROM NAMED.
    let both = "ASK { <urn:a> ?p ?o . <urn:b> ?q ?v }";
    assert!(ask(both, &[]).contains("false"));
    let defaults = [
        ("default-graph-uri", "urn:g1"),
        ("default-graph-uri", "urn:g2"),
    ];
    assert!(ask(both, &defaults).contains("true"));
    let named = "ASK { GRAPH <urn:g2> { ?s ?p ?o } }";
    assert!(ask(named, &[("named-graph-uri", "urn:g1")]).contains("false"));
    assert!(ask(named, &[("named-graph-uri", "urn:g2")]).contains("true"));

    // using-graph-uri is the WHERE dataset of an update; with a USING of its own, 400.
    let update = |text: &str, params: &[(&str, &str)]| {
        let mut r = ureq::post(&format!("{base}/kg/update"));
        for (k, v) in params {
            r = r.query(k, v);
        }
        status(
            r.set("Content-Type", "application/sparql-update")
                .send_string(text),
        )
    };
    let copy = "INSERT { GRAPH <urn:out> { ?s ?p ?o } } WHERE { ?s ?p ?o }";
    assert_eq!(update(copy, &[("using-graph-uri", "urn:g1")]), 200);
    assert!(ask("ASK { GRAPH <urn:out> { <urn:a> ?p ?o } }", &[]).contains("true"));
    assert!(ask("ASK { GRAPH <urn:out> { <urn:b> ?p ?o } }", &[]).contains("false"));
    let own = "INSERT { ?s ?p ?o } USING <urn:g2> WHERE { ?s ?p ?o }";
    assert_eq!(update(own, &[("using-graph-uri", "urn:g1")]), 400);
    // The parameters of the other operation, or a non-IRI, are 400.
    assert_eq!(update(copy, &[("default-graph-uri", "urn:g1")]), 400);
    assert_eq!(update(copy, &[("using-graph-uri", "not an iri")]), 400);

    // Relative IRIs resolve against the endpoint's URL.
    assert_eq!(update("INSERT DATA { <s> <p> <o> }", &[]), 200);
    assert!(
        ask(&format!("ASK {{ <{base}/kg/s> ?p ?o }}"), &[]).contains("true"),
        "relative IRIs resolve against {base}/kg/update"
    );

    // Two queries, an operation both in the body and a parameter, or another charset: 4xx.
    let r = ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK {}")
        .query("query", "SELECT * {}")
        .call();
    assert_eq!(status(r), 400);
    let r = ureq::post(&format!("{base}/kg/query?query=ASK%20%7B%7D"))
        .set("Content-Type", "application/sparql-query")
        .send_string("ASK {}");
    assert_eq!(status(r), 400);
    let r = ureq::post(&format!("{base}/kg/query"))
        .set("Content-Type", "application/sparql-query; charset=UTF-16")
        .send_string("ASK {}");
    assert_eq!(status(r), 415);
}

#[test]
fn graph_store_takes_multipart_uploads() {
    let base = start(&["kg"], None);
    let form = "--XyZ\r\n\
        Content-Disposition: form-data; name=\"a\"; filename=\"a.ttl\"\r\n\
        Content-Type: text/turtle\r\n\r\n\
        <urn:a> <urn:p> 1 .\r\n\
        --XyZ\r\n\
        Content-Disposition: form-data; name=\"b\"; filename=\"b.nt\"\r\n\r\n\
        <urn:b> <urn:p> \"2\" .\r\n\
        --XyZ--\r\n";
    let r = ureq::post(&format!("{base}/kg/data?graph=urn:g"))
        .set("Content-Type", "multipart/form-data; boundary=XyZ")
        .send_string(form)
        .unwrap();
    assert_eq!(r.status(), 201);
    let r = ureq::get(&format!("{base}/kg/data?graph=urn:g"))
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(body(r).lines().count(), 2);
    // A part with an error leaves the graph as it was.
    let bad = form.replace("<urn:b> <urn:p> \"2\" .", "<urn:b> <urn:p> .");
    let r = ureq::put(&format!("{base}/kg/data?graph=urn:g"))
        .set("Content-Type", "multipart/form-data; boundary=XyZ")
        .send_string(&bad);
    assert_eq!(status(r), 400);
    let r = ureq::get(&format!("{base}/kg/data?graph=urn:g"))
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(body(r).lines().count(), 2);
}

/// The head of the answer read from `stream`, and its body (of `Content-Length` bytes).
fn read_answer(reader: &mut impl std::io::BufRead) -> (String, String) {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        head.push_str(&line);
    }
    let length: usize = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (head, String::from_utf8(body).unwrap())
}

/// A connection the server closes after the answer says so (#558): a client that pools
/// connections would otherwise send its next request on the closed one and lose it.
#[test]
fn an_answer_says_when_the_connection_closes() {
    use std::io::{BufReader, Write};
    let base = start(&["ds"], None);
    let address = base.trim_start_matches("http://").to_owned();
    let update = "INSERT DATA { <urn:a> <urn:b> <urn:c> }";
    let request = |connection: &str| {
        format!(
            "POST /ds/update HTTP/1.1\r\nHost: x\r\nContent-Type: application/sparql-update\r\n\
             Content-Length: {}\r\n{connection}\r\n{update}",
            update.len()
        )
    };
    let connect = || {
        let stream = std::net::TcpStream::connect(&address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
    };
    let says_close = |head: &str| {
        head.lines()
            .any(|l| l.to_ascii_lowercase().replace(' ', "") == "connection:close")
    };

    // Kept alive: no such header, and the next request on the same connection is answered.
    let mut stream = connect();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    stream.write_all(request("").as_bytes()).unwrap();
    let (head, _) = read_answer(&mut reader);
    assert!(head.starts_with("HTTP/1.1 2"), "{head}");
    assert!(!says_close(&head), "{head}");
    // The client asks to close: the answer says it, and the update was applied.
    stream
        .write_all(request("Connection: close\r\n").as_bytes())
        .unwrap();
    let (head, _) = read_answer(&mut reader);
    assert!(head.starts_with("HTTP/1.1 2"), "{head}");
    assert!(says_close(&head), "{head}");
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut rest).unwrap();
    assert!(rest.is_empty(), "the connection is closed");

    // HTTP/1.0 without keep-alive closes too, and says so.
    let mut stream = connect();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    stream
        .write_all(b"GET /ds/query?query=ASK%7B%3Curn%3Aa%3E%3Fp%3Fo%7D HTTP/1.0\r\nAccept: application/sparql-results+json\r\n\r\n")
        .unwrap();
    let (head, body) = read_answer(&mut reader);
    assert!(says_close(&head), "{head}");
    assert!(body.contains("true"), "{body}");
}

#[test]
fn graph_management_on_a_missing_or_existing_graph_is_a_bad_request() {
    let base = start(&["kg"], None);
    let update =
        |u: &str| status(ureq::post(&format!("{base}/kg/update")).send_form(&[("update", u)]));
    for u in [
        "CLEAR GRAPH <http://example.com/missing>",
        "DROP GRAPH <http://example.com/missing>",
    ] {
        assert_eq!(update(u), 400, "{u}");
    }
    assert_eq!(
        update("CLEAR SILENT GRAPH <http://example.com/missing>"),
        200
    );
    assert_eq!(
        update(
            "INSERT DATA { GRAPH <http://example.com/g> { <http://example.com/a> <http://example.com/p> 1 } }"
        ),
        200
    );
    assert_eq!(update("CREATE GRAPH <http://example.com/g>"), 400);
}

/// Many keep-alive connections opened at once are all served (#690): tiny_http's pool saw
/// its idle threads as free while they were already handed a connection, so it queued the
/// rest behind connections a pooled client never closes.
#[test]
fn many_keep_alive_connections_at_once_are_all_answered() {
    use std::io::{BufReader, Write};
    let base = start(&["ds"], None);
    let address = base.trim_start_matches("http://").to_owned();
    for _ in 0..3 {
        let streams: Vec<_> = (0..16)
            .map(|_| std::net::TcpStream::connect(&address).unwrap())
            .collect();
        for mut s in &streams {
            s.write_all(b"GET /$/ping HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
        }
        let answered = streams
            .iter()
            .filter(|s| {
                s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut reader = BufReader::new(*s);
                let mut line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut line).is_ok()
                    && line.starts_with("HTTP/1.1 200")
            })
            .count();
        // Every connection stays open (kept alive) until here.
        assert_eq!(answered, 16);
    }
}

#[test]
fn queries_and_updates_call_the_servers_functions() {
    let functions = tripletrail::udf::Functions::from_triples(
        tripletrail::oxigraph::io::RdfParser::from_format(
            tripletrail::oxigraph::io::RdfFormat::Turtle,
        )
        .for_slice(
            br#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .
                ex:double a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ] ;
                    sh:select "SELECT ($x * 2 AS ?r) WHERE {}" .
                ex:name a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ] ;
                    sh:select "SELECT ?n WHERE { $x <http://example.com/name> ?n }" ."#,
        )
        .map(|q| tripletrail::oxigraph::model::Triple::from(q.unwrap())),
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 2,
            functions,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(
            "PREFIX ex: <http://example.com/> INSERT DATA { ex:a ex:name \"A\" ; ex:v 21 }",
        )
        .unwrap();
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(
            "PREFIX ex: <http://example.com/> INSERT { ?s ex:w ?w } WHERE { ?s ex:v ?v BIND(ex:double(?v) AS ?w) }",
        )
        .unwrap();
    let r = ureq::post(&format!("{base}/kg/query"))
        .set("Content-Type", "application/sparql-query")
        .set("Accept", "text/csv")
        .send_string(
            "PREFIX ex: <http://example.com/> SELECT ?w (ex:name(?s) AS ?n) WHERE { ?s ex:w ?w }",
        )
        .unwrap();
    assert_eq!(body(r).replace('\r', ""), "w,n\n42,A\n");
}

/// The Grafana dashboard in deploy/ (#220) reads only metrics the server exports, with the
/// labels it exports them with: a renamed metric or label fails here rather than leave an
/// empty panel.
#[test]
fn the_grafana_dashboard_reads_exported_metrics() {
    let base = start(&["kg"], None);
    ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK {}")
        .call()
        .unwrap();
    let metrics = body(ureq::get(&format!("{base}/$/metrics")).call().unwrap());
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/grafana/tripletrail.json"
    );
    let dashboard: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut exprs = Vec::new();
    for panel in dashboard["panels"].as_array().unwrap() {
        for target in panel["targets"].as_array().unwrap() {
            exprs.push(target["expr"].as_str().unwrap().to_owned());
        }
    }
    assert!(exprs.len() > 10, "{exprs:?}");
    let family = |name: &str| {
        ["_bucket", "_count", "_sum"]
            .iter()
            .find_map(|s| name.strip_suffix(s))
            .unwrap_or(name)
            .to_owned()
    };
    let mut checked = 0;
    for expr in &exprs {
        let mut rest = expr.as_str();
        while let Some(at) = rest.find("tripletrail_") {
            rest = &rest[at..];
            let end = rest
                .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
                .unwrap_or(rest.len());
            let name = &rest[..end];
            assert!(
                metrics.contains(&format!("# TYPE {} ", family(name))),
                "{name} is not exported: {expr}"
            );
            // The labels it selects on, on every sample the server gives of it.
            if let Some(selector) = rest[end..].strip_prefix('{') {
                let samples: Vec<&str> = metrics
                    .lines()
                    .filter(|l| l.starts_with(&format!("{name}{{")))
                    .collect();
                for label in selector[..selector.find('}').unwrap()].split(',') {
                    let key = label.split(['=', '!']).next().unwrap().trim();
                    assert!(
                        samples.iter().all(|s| s.contains(&format!("{key}=\""))),
                        "{name} has no label {key}: {expr}"
                    );
                }
            }
            checked += 1;
            rest = &rest[end..];
        }
        // `by (…)` groups only on labels the metric has.
        if let Some(at) = expr.find("by (") {
            let labels = &expr[at + 4..at + expr[at..].find(')').unwrap()];
            let name = &expr[expr.find("tripletrail_").unwrap()..];
            let name = &name[..name.find(['{', '[', ')']).unwrap()];
            for key in labels.split(',').map(str::trim).filter(|k| *k != "le") {
                assert!(
                    metrics
                        .lines()
                        .filter(|l| l.starts_with(&format!("{name}{{")))
                        .all(|s| s.contains(&format!("{key}=\""))),
                    "{name} has no label {key}: {expr}"
                );
            }
        }
    }
    assert!(checked >= exprs.len(), "{checked}");
}

/// A wrong parameter of one of TripleTrail's own services (path, pgr, window, st) is the query's
/// fault: 400 with the reason, not 500.
#[test]
fn wrong_parameters_of_the_built_in_services_are_bad_requests() {
    let base = start(&["ds"], Some(Duration::from_secs(60)));
    for (query, says) in [
        (
            "PREFIX path: <http://w3id.org/tripletrail/path#> SELECT * { SERVICE <http://w3id.org/tripletrail/path> \
             { [] path:source <urn:a> ; path:predicate <urn:p> ; path:exceptPredicate <urn:q> ; path:end ?n } }",
            "path:exceptPredicate",
        ),
        (
            "PREFIX path: <http://w3id.org/tripletrail/path#> SELECT * { SERVICE <http://w3id.org/tripletrail/path> \
             { [] path:predicate <urn:p> ; path:end ?n } }",
            "path:source",
        ),
        (
            "PREFIX pgr: <http://w3id.org/tripletrail/pgr#> SELECT * { SERVICE <http://w3id.org/tripletrail/pgr> \
             { [] pgr:function pgr:nope ; pgr:node ?n } }",
            "pgr:",
        ),
    ] {
        let r = ureq::get(&format!("{base}/ds/query"))
            .query("query", query)
            .call();
        match r {
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap();
                assert_eq!(code, 400, "{query}: {text}");
                assert!(text.contains(says), "{query}: {text}");
            }
            Ok(r) => panic!("{query}: {} {}", r.status(), r.into_string().unwrap()),
            Err(e) => panic!("{e}"),
        }
    }
    // A stored edge query that calls a SERVICE, which nothing answers there (#861).
    ureq::post(&format!("{base}/ds/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(
            r#"INSERT DATA { <urn:net> a <http://w3id.org/tripletrail/pgr#Network> ;
                 <http://w3id.org/tripletrail/pgr#edgeQuery> "SELECT ?id ?source ?target ?cost { SERVICE <http://w3id.org/tripletrail/window> { ?id ?source ?target } BIND(1 AS ?cost) }" }"#,
        )
        .unwrap();
    let query = "PREFIX pgr: <http://w3id.org/tripletrail/pgr#> SELECT * { SERVICE <http://w3id.org/tripletrail/pgr> \
                 { [] pgr:function pgr:pgr_dijkstra ; pgr:network <urn:net> ; pgr:start_vid <urn:a> ; pgr:end_vid <urn:b> ; pgr:agg_cost ?c } }";
    match ureq::get(&format!("{base}/ds/query"))
        .query("query", query)
        .call()
    {
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap();
            assert_eq!(code, 400, "{text}");
            assert!(text.contains("not supported"), "{text}");
        }
        Ok(r) => panic!("{} {}", r.status(), r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn writes_go_on_while_the_shacl_service_validates() {
    // Enough things that the validation takes a while (#942).
    let mut data = String::new();
    for i in 0..300_000 {
        data.push_str(&format!(
            "<http://example.com/n{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.com/Thing> .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine
        .load(
            tripletrail::oxigraph::io::RdfFormat::NTriples,
            data.as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 4,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let validating = {
        let base = base.clone();
        std::thread::spawn(move || {
            ureq::post(&format!("{base}/kg/shacl?graph=default"))
                .set("Content-Type", "text/turtle")
                .send_string(
                    "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                     <http://example.com/ThingShape> a sh:NodeShape ;\n\
                       sh:targetClass <http://example.com/Thing> ;\n\
                       sh:property [ sh:path <http://example.com/name> ; sh:minCount 1 ] .\n",
                )
                .unwrap()
                .status()
        })
    };
    // Well into the run.
    std::thread::sleep(Duration::from_secs(1));
    let started = std::time::Instant::now();
    let r = ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <urn:new:1> <urn:newp:1> \"new literal 1\" }")
        .unwrap();
    let took = started.elapsed();
    assert_eq!(r.status(), 200);
    // The write did not wait for the validation, which was still running.
    assert!(
        !validating.is_finished(),
        "the validation ended too soon to tell"
    );
    assert!(took < Duration::from_secs(1), "the write took {took:?}");
    assert_eq!(validating.join().unwrap(), 200);
}

#[test]
fn vector_graph_min_decides_when_tt_nearest_builds_a_graph() {
    use tripletrail::oxigraph::model::NamedNode;
    let mut data = String::new();
    for i in 0..60 {
        let v: Vec<String> = (0..4)
            .map(|d| format!("{}", ((i * 7 + d * 3) % 11) as f64 + 0.5))
            .collect();
        data.push_str(&format!(
            "<http://example.com/r{i}> <http://example.com/embedding> \"[{}]\"^^<http://w3id.org/tripletrail/fn#vector> .\n",
            v.join(",")
        ));
    }
    let property = NamedNode::new("http://example.com/embedding").unwrap();
    for (min, graph) in [(10, true), (0, false)] {
        let engine = Engine::new_tripletrail();
        engine
            .load(
                tripletrail::oxigraph::io::RdfFormat::NTriples,
                data.as_bytes(),
            )
            .unwrap();
        let server = Server::bind(
            "127.0.0.1:0",
            [("ds".to_owned(), engine.clone())],
            Options {
                vector_graph_min: min,
                ..Options::default()
            },
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        let r = ureq::get(&format!("{base}/ds/query"))
            .query(
                "query",
                r#"SELECT ?s WHERE { ?s <http://w3id.org/tripletrail/fn#nearest> (<http://example.com/embedding> "[1, 2, 3, 4]" 3) }"#,
            )
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap();
        assert_eq!(body(r).matches("\"value\"").count(), 3);
        let index = engine.vector_index(None, &property).unwrap();
        let start = std::time::Instant::now();
        loop {
            let (nodes, building) = index.graph_status(4);
            if !graph {
                assert_eq!(
                    (nodes, building),
                    (0, false),
                    "--vector-graph-min 0 keeps it exact"
                );
                break;
            }
            if nodes == 60 {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(30), "no graph");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Retrieval for GraphRAG (#219): the resources nearest a vector or a resource, with their
/// scores and descriptions in JSON-LD.
#[test]
fn search_similar_gives_the_nearest_resources_with_their_descriptions() {
    let data = r#"@prefix ex: <http://example.com/> .
@prefix tt: <http://w3id.org/tripletrail/fn#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:crossing rdfs:label "Level crossing" ; ex:embedding "[0.9, 0.1, 0.0]"^^tt:vector .
ex:signal rdfs:label "Signal" ; ex:embedding "[0.1, 0.9, 0.1]"^^tt:vector .
ex:bridge rdfs:label "Bridge" ; ex:embedding "[0.7, 0.0, 0.7]"^^tt:vector .
ex:twice rdfs:label "Two vectors" ; ex:embedding "[1, 0, 0.1]"^^tt:vector , "[1, 0, 0.12]"^^tt:vector .
"#;
    let engine = Engine::new_tripletrail();
    engine
        .load(
            tripletrail::oxigraph::io::RdfFormat::Turtle,
            data.as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("ds".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}/ds/search/similar", server.address());
    server.spawn();
    let get = |query: &[(&str, &str)]| -> Result<serde_json::Value, u16> {
        let mut r = ureq::get(&base);
        for (k, v) in query {
            r = r.query(k, v);
        }
        match r.call() {
            Ok(r) => Ok(serde_json::from_str(&body(r)).unwrap()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("{e}"),
        }
    };
    let property = ("property", "http://example.com/embedding");
    // A resource with two vectors comes once, with its best score, and k resources still
    // come back.
    let found = get(&[property, ("vector", "[1, 0, 0.1]"), ("k", "3")]).unwrap();
    let results = found["results"].as_array().unwrap();
    let iris: Vec<&str> = results.iter().map(|r| r["iri"].as_str().unwrap()).collect();
    assert_eq!(
        iris,
        [
            "http://example.com/twice",
            "http://example.com/crossing",
            "http://example.com/bridge"
        ]
    );
    assert!((results[0]["score"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    let results = &results[1..];
    assert!(results[0]["score"].as_f64().unwrap() > results[1]["score"].as_f64().unwrap());
    let description = results[0]["description"].to_string();
    assert!(description.contains("Level crossing"), "{description}");
    let found = get(&[property, ("iri", "http://example.com/twice"), ("k", "2")]).unwrap();
    let iris: Vec<&str> = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["iri"].as_str().unwrap())
        .collect();
    assert_eq!(
        iris,
        ["http://example.com/twice", "http://example.com/crossing"]
    );
    // From a resource: itself first. Without descriptions.
    let found = get(&[
        property,
        ("iri", "http://example.com/signal"),
        ("k", "1"),
        ("describe", "false"),
    ])
    .unwrap();
    assert_eq!(found["results"][0]["iri"], "http://example.com/signal");
    assert!(found["results"][0].get("description").is_none());
    // A resource without a vector finds nothing.
    let found = get(&[property, ("iri", "http://example.com/nothing")]).unwrap();
    assert_eq!(found["results"], serde_json::json!([]));
    for bad in [
        vec![("vector", "[1, 0, 0]")],
        vec![property],
        vec![property, ("vector", "one")],
        vec![
            property,
            ("vector", "[1]"),
            ("iri", "http://example.com/signal"),
        ],
        vec![property, ("vector", "[1]"), ("k", "0")],
        vec![property, ("vector", "[1]"), ("describe", "maybe")],
    ] {
        assert_eq!(get(&bad), Err(400), "{bad:?}");
    }
    // As a form: a long vector does not fit in a URL behind most proxies. 768 numbers are
    // longer than this dataset's vectors, so they find nothing, without a 414.
    let post = |form: &[(&str, &str)]| match ureq::post(&base).send_form(form) {
        Ok(r) => Ok(serde_json::from_str::<serde_json::Value>(&body(r)).unwrap()),
        Err(ureq::Error::Status(code, _)) => Err(code),
        Err(e) => panic!("{e}"),
    };
    let found = post(&[property, ("vector", "[1, 0, 0.1]"), ("k", "1")]).unwrap();
    assert_eq!(found["results"][0]["iri"], "http://example.com/twice");
    let long = format!("[{}]", vec!["0.0123456789"; 768].join(", "));
    let found = post(&[property, ("vector", &long)]).unwrap();
    assert_eq!(found["results"], serde_json::json!([]));
    assert_eq!(post(&[("vector", "[1, 0, 0]")]), Err(400));
    let json = ureq::post(&base)
        .set("Content-Type", "application/json")
        .send_string("{}");
    assert!(matches!(json, Err(ureq::Error::Status(415, _))));
    // Media types are case-insensitive.
    let upper = ureq::post(&base)
        .set(
            "Content-Type",
            "Application/X-WWW-Form-Urlencoded; charset=UTF-8",
        )
        .send_string("property=http%3A%2F%2Fexample.com%2Fembedding&vector=%5B1,0,0.1%5D&k=1");
    assert_eq!(upper.map(|r| r.status()).ok(), Some(200));
    // Only `similar` takes a POST.
    let classes = base.replace("similar", "classes");
    let posted = ureq::post(&classes).send_form(&[]);
    assert!(matches!(posted, Err(ureq::Error::Status(405, _))));
}

/// `iri=` reads the resource's vector in the named graphs too, as the search reads theirs,
/// without the union default graph.
#[test]
fn search_similar_reads_a_resource_vector_in_a_named_graph() {
    let data = r#"@prefix ex: <http://example.com/> .
@prefix tt: <http://w3id.org/tripletrail/fn#> .
<urn:g> {
  ex:crossing ex:embedding "[0.9, 0.1, 0.0]"^^tt:vector .
  ex:signal ex:embedding "[0.1, 0.9, 0.1]"^^tt:vector .
}
"#;
    let engine = Engine::new_tripletrail();
    engine
        .load(tripletrail::oxigraph::io::RdfFormat::TriG, data.as_bytes())
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("ds".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}/ds/search/similar", server.address());
    server.spawn();
    let iris = |query: &[(&str, &str)]| -> Vec<String> {
        let mut r = ureq::get(&base).query("property", "http://example.com/embedding");
        for (k, v) in query {
            r = r.query(k, v);
        }
        let found: serde_json::Value = serde_json::from_str(&body(r.call().unwrap())).unwrap();
        found["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["iri"].as_str().unwrap().to_owned())
            .collect()
    };
    let by_vector = iris(&[("vector", "[1, 0, 0]"), ("k", "1")]);
    assert_eq!(by_vector, ["http://example.com/crossing"]);
    let by_iri = iris(&[("iri", "http://example.com/crossing"), ("k", "1")]);
    assert_eq!(by_iri, by_vector);
}
