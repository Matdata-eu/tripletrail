//! The RDF4J REST API (#223), as RDF4J's `HTTPRepository` and Workbench use it. The same
//! scenario was run against RDF4J 5.1.2's own Java client during development.

use serde_json::Value;
use tripletrail::Engine;
use tripletrail_server::{Options, Server};

const TOKEN: &str = "update-token";

fn start(location: Option<std::path::PathBuf>) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 2,
            update_token: Some(TOKEN.to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    // A dataset with a directory keeps its namespaces there.
    server
        .handle()
        .attach("kg", Engine::new_tripletrail(), location);
    let base = format!("http://{}/rdf4j-server", server.address());
    server.spawn();
    base
}

fn get(url: &str) -> String {
    ureq::get(url).call().unwrap().into_string().unwrap()
}

fn write(method: &str, url: &str) -> ureq::Request {
    ureq::request(method, url).set("Authorization", &format!("Bearer {TOKEN}"))
}

fn size(repo: &str, query: &str) -> u64 {
    get(&format!("{repo}/size{query}")).trim().parse().unwrap()
}

fn bindings(body: &str) -> Vec<Value> {
    let json: Value = serde_json::from_str(body).unwrap();
    json["results"]["bindings"].as_array().unwrap().clone()
}

/// A binary RDF (version 2) document with one statement per (s, p, o, context), `None` being
/// the null value.
fn binary_rdf(statements: &[[Option<&str>; 4]]) -> Vec<u8> {
    fn string(out: &mut Vec<u8>, s: &str) {
        assert!(s.len() < 128);
        out.push(s.len() as u8);
        out.extend_from_slice(s.as_bytes());
    }
    let mut d = b"BRDF\0\0\0\x02".to_vec();
    string(&mut d, "UTF-8");
    for statement in statements {
        d.push(1);
        for (i, value) in statement.iter().enumerate() {
            match value {
                None => d.push(0),
                Some(v) if i == 2 && !v.starts_with("urn:") && !v.starts_with("http") => {
                    d.push(3);
                    string(&mut d, v);
                }
                Some(v) => {
                    d.push(1);
                    string(&mut d, v);
                }
            }
        }
    }
    d.push(127);
    d
}

#[test]
fn statements_contexts_queries_and_namespaces() {
    let base = start(None);
    let protocol = get(&format!("{base}/protocol"));
    assert_eq!(protocol.trim(), "12");

    let repos = bindings(
        &ureq::get(&format!("{base}/repositories"))
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    );
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["id"]["value"], "kg");
    let repo = format!("{base}/repositories/kg");

    // Writes need the update token.
    let e = ureq::post(&format!("{repo}/statements"))
        .set("Content-Type", "text/turtle")
        .send_string("<urn:a> <urn:p> 1 .")
        .unwrap_err();
    assert!(matches!(e, ureq::Error::Status(401 | 403, _)), "{e}");

    write("POST", &format!("{repo}/statements"))
        .set("Content-Type", "text/turtle")
        .send_string("<urn:a> <urn:p> 1 . <urn:b> <urn:p> 2 .")
        .unwrap();
    write("POST", &format!("{repo}/statements?context=%3Curn%3Ag%3E"))
        .set("Content-Type", "application/n-triples")
        .send_string("<urn:c> <urn:p> \"three\" .\n")
        .unwrap();
    write("POST", &format!("{repo}/statements"))
        .set("Content-Type", "application/x-binary-rdf")
        .send_bytes(&binary_rdf(&[[
            Some("urn:d"),
            Some("urn:p"),
            Some("four"),
            Some("urn:h"),
        ]]))
        .unwrap();
    assert_eq!(size(&repo, ""), 4);
    assert_eq!(size(&repo, "?context=%3Curn%3Ag%3E"), 1);
    assert_eq!(size(&repo, "?context=null"), 2);

    let contexts = bindings(
        &ureq::get(&format!("{repo}/contexts"))
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    );
    let mut names: Vec<_> = contexts
        .iter()
        .map(|b| b["contextID"]["value"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["urn:g", "urn:h"]);

    // Queries, by GET and by form POST.
    let rows = bindings(
        &ureq::get(&format!(
            "{repo}?query=SELECT%20%3Fs%20WHERE%20%7B%20%3Fs%20%3Curn%3Ap%3E%201%20%7D"
        ))
        .set("Accept", "application/sparql-results+json")
        .call()
        .unwrap()
        .into_string()
        .unwrap(),
    );
    assert_eq!(rows[0]["s"]["value"], "urn:a");
    let ask = ureq::post(&repo)
        .set("Accept", "application/sparql-results+json")
        .send_form(&[("query", "ASK { GRAPH <urn:g> { ?s ?p ?o } }")])
        .unwrap()
        .into_string()
        .unwrap();
    assert!(ask.contains("true"), "{ask}");

    // Statements by pattern.
    let nt = ureq::get(&format!("{repo}/statements?subj=%3Curn%3Ac%3E"))
        .set("Accept", "application/n-quads")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(nt.trim(), "<urn:c> <urn:p> \"three\" <urn:g> .");

    // SPARQL update through /statements, then removal by pattern.
    write("POST", &format!("{repo}/statements"))
        .send_form(&[("update", "INSERT DATA { <urn:e> <urn:p> 5 }")])
        .unwrap();
    assert_eq!(size(&repo, ""), 5);
    write(
        "DELETE",
        &format!("{repo}/statements?pred=%3Curn%3Ap%3E&context=null"),
    )
    .call()
    .unwrap();
    assert_eq!(size(&repo, ""), 2);

    // PUT replaces a context.
    write("PUT", &format!("{repo}/statements?context=%3Curn%3Ag%3E"))
        .set("Content-Type", "text/turtle")
        .send_string("<urn:x> <urn:p> 1 . <urn:y> <urn:p> 2 .")
        .unwrap();
    assert_eq!(size(&repo, "?context=%3Curn%3Ag%3E"), 2);
    assert_eq!(size(&repo, ""), 3);

    // Namespaces.
    write("PUT", &format!("{repo}/namespaces/ex"))
        .send_string("urn:ex:")
        .unwrap();
    assert_eq!(get(&format!("{repo}/namespaces/ex")), "urn:ex:");
    let prefixes = ureq::get(&format!("{repo}/namespaces"))
        .set("Accept", "application/sparql-results+json")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        prefixes.contains("\"ex\"") && prefixes.contains("\"rdf\""),
        "{prefixes}"
    );
    write("DELETE", &format!("{repo}/namespaces/ex"))
        .call()
        .unwrap();
    assert!(matches!(
        ureq::get(&format!("{repo}/namespaces/ex")).call(),
        Err(ureq::Error::Status(404, _))
    ));
}

#[test]
fn transactions_commit_and_roll_back() {
    let base = start(None);
    let repo = format!("{base}/repositories/kg");
    write("POST", &format!("{repo}/statements"))
        .set("Content-Type", "text/turtle")
        .send_string("<urn:a> <urn:p> 1 . <urn:a> <urn:q> 2 .")
        .unwrap();
    write("POST", &format!("{repo}/statements?context=%3Curn%3Ag%3E"))
        .set("Content-Type", "text/turtle")
        .send_string("<urn:a> <urn:p> 3 .")
        .unwrap();

    let begin = write("POST", &format!("{repo}/transactions"))
        .call()
        .unwrap();
    assert_eq!(begin.status(), 201);
    let txn = begin.header("Location").unwrap().to_owned();
    assert!(txn.starts_with(&format!("{repo}/transactions/")), "{txn}");

    write("PUT", &format!("{txn}?action=ADD"))
        .set("Content-Type", "text/turtle")
        .send_string("<urn:b> <urn:p> 4 .")
        .unwrap();
    // remove(urn:a, null, null, urn:g) as RDF4J's client sends it: a wildcard statement.
    write("PUT", &format!("{txn}?action=DELETE"))
        .set("Content-Type", "application/x-binary-rdf")
        .send_bytes(&binary_rdf(&[[
            Some("urn:a"),
            Some("http://www.openrdf.org/schema/sesame#wildcard"),
            Some("http://www.openrdf.org/schema/sesame#wildcard"),
            Some("urn:g"),
        ]]))
        .unwrap();
    // remove(null, urn:q, null, (Resource) null): only the default graph.
    write("PUT", &format!("{txn}?action=DELETE"))
        .set("Content-Type", "application/x-binary-rdf")
        .send_bytes(&binary_rdf(&[[
            Some("http://www.openrdf.org/schema/sesame#wildcard"),
            Some("urn:q"),
            Some("http://www.openrdf.org/schema/sesame#wildcard"),
            Some("http://rdf4j.org/schema/rdf4j#nil"),
        ]]))
        .unwrap();
    write("PUT", &format!("{txn}?action=UPDATE"))
        .send_form(&[("update", "INSERT DATA { <urn:c> <urn:p> 5 }")])
        .unwrap();
    // A query in the transaction, as the client sends it: an empty `query` in the URL.
    let ask = write("PUT", &format!("{txn}?action=QUERY&queryLn=SPARQL&query"))
        .set("Content-Type", "application/sparql-query; charset=utf-8")
        .set("Accept", "application/sparql-results+json")
        .send_string("ASK { <urn:a> <urn:p> 1 }")
        .unwrap()
        .into_string()
        .unwrap();
    assert!(ask.contains("true"), "{ask}");
    // Nothing shows before the commit.
    assert_eq!(size(&repo, ""), 3);
    write("PUT", &format!("{txn}?action=COMMIT"))
        .call()
        .unwrap();
    assert_eq!(size(&repo, ""), 3);
    assert_eq!(size(&repo, "?context=%3Curn%3Ag%3E"), 0);
    let rows = bindings(
        &ureq::post(&repo)
            .set("Accept", "application/sparql-results+json")
            .send_form(&[("query", "SELECT ?s WHERE { ?s ?p ?o } ORDER BY ?s")])
            .unwrap()
            .into_string()
            .unwrap(),
    );
    let subjects: Vec<_> = rows.iter().map(|r| r["s"]["value"].clone()).collect();
    assert_eq!(subjects, ["urn:a", "urn:b", "urn:c"]);
    // A committed transaction is gone.
    assert!(matches!(
        write("PUT", &format!("{txn}?action=COMMIT")).call(),
        Err(ureq::Error::Status(404, _))
    ));

    // Write rights are checked on every write, not only at begin.
    let txn = write("POST", &format!("{repo}/transactions"))
        .call()
        .unwrap()
        .header("Location")
        .unwrap()
        .to_owned();
    for (method, url) in [
        ("PUT", format!("{txn}?action=UPDATE&update=CLEAR%20ALL")),
        ("PUT", format!("{txn}?action=COMMIT")),
    ] {
        assert!(matches!(
            ureq::request(method, &url).call(),
            Err(ureq::Error::Status(401, _))
        ));
    }
    write("DELETE", &txn).call().unwrap();
    assert_eq!(size(&repo, ""), 3);

    // Rollback drops the changes.
    let txn = write("POST", &format!("{repo}/transactions"))
        .call()
        .unwrap()
        .header("Location")
        .unwrap()
        .to_owned();
    write("PUT", &format!("{txn}?action=ADD"))
        .set("Content-Type", "text/turtle")
        .send_string("<urn:never> <urn:p> 1 .")
        .unwrap();
    write("DELETE", &txn).call().unwrap();
    assert_eq!(size(&repo, ""), 3);
}

#[test]
fn namespaces_persist_with_a_location() {
    let dir = tripletrail_testdir::tempdir("rdf4j");
    let base = start(Some(dir.to_path_buf()));
    write("PUT", &format!("{base}/repositories/kg/namespaces/rinf"))
        .send_string("http://data.europa.eu/949/")
        .unwrap();
    let base = start(Some(dir.to_path_buf()));
    assert_eq!(
        get(&format!("{base}/repositories/kg/namespaces/rinf")),
        "http://data.europa.eu/949/"
    );
}

#[test]
fn queries_read_like_rdf4j() {
    let base = start(None);
    let repo = format!("{base}/repositories/kg");
    write("POST", &format!("{repo}/statements?context=%3Curn%3Ag%3E"))
        .set("Content-Type", "text/turtle")
        .send_string(
            "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
             <urn:a> rdfs:label \"a\" ; <urn:n> \"1\"^^<http://www.w3.org/2001/XMLSchema#int> .
             <urn:b> rdfs:label \"b\" .",
        )
        .unwrap();
    let select = |query: &str, extra: &[(&str, &str)]| -> Vec<Value> {
        let mut form = vec![("query", query)];
        form.extend_from_slice(extra);
        bindings(
            &ureq::post(&repo)
                // As RDF4J's client asks: CSV first, at the same weight as JSON.
                .set(
                    "Accept",
                    "text/csv;q=0.8, application/sparql-results+json;q=0.8, \
                     application/x-binary-rdf-results-table",
                )
                .send_form(&form)
                .unwrap()
                .into_string()
                .unwrap(),
        )
    };
    // The named graph is part of the default graph, rdfs: needs no declaration, and the
    // results keep their datatypes.
    let rows = select("SELECT ?n { ?s rdfs:label \"a\" ; <urn:n> ?n }", &[]);
    assert_eq!(
        rows[0]["n"]["datatype"],
        "http://www.w3.org/2001/XMLSchema#int"
    );
    // baseURI, and a binding of a variable the query does not select.
    let rows = select(
        "SELECT ?s { ?s rdfs:label ?l }",
        &[("baseURI", "urn:x"), ("$l", "\"b\"")],
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["s"]["value"], "urn:b");
    let rows = select("SELECT ?l { <b> rdfs:label ?l }", &[("baseURI", "urn:b")]);
    assert_eq!(
        rows[0]["l"]["value"], "b",
        "<b> resolves against baseURI urn:b"
    );
    // A dataset: only default graphs, so no named ones.
    let rows = select("SELECT ?s { ?s ?p ?o }", &[("default-graph-uri", "null")]);
    assert!(rows.is_empty());
    let rows = select(
        "SELECT ?g { GRAPH ?g { ?s ?p ?o } }",
        &[("default-graph-uri", "urn:g")],
    );
    assert!(rows.is_empty());
    // A query's own FROM is kept.
    let rows = select("SELECT ?s FROM <urn:none> { ?s ?p ?o }", &[]);
    assert!(rows.is_empty());

    // Errors name their RDF4J type, which the client maps to its exceptions.
    let error = |r: Result<ureq::Response, ureq::Error>| match r {
        Err(ureq::Error::Status(_, r)) => r.into_string().unwrap(),
        other => panic!("{other:?}"),
    };
    assert!(
        error(ureq::post(&repo).send_form(&[("query", "SELECT {")]))
            .starts_with("MALFORMED QUERY:")
    );
    assert!(
        error(
            write("POST", &format!("{repo}/statements"))
                .set("Content-Type", "text/turtle")
                .send_string("bad")
        )
        .starts_with("MALFORMED DATA:")
    );
    assert!(
        error(write("POST", &format!("{repo}/statements")).send_form(&[
            ("update", "DELETE WHERE { ?s ?p ?o }"),
            ("using-graph-uri", "urn:g")
        ]))
        .contains("not supported")
    );
}

#[test]
fn compressed_uploads_and_fresh_blank_nodes() {
    use std::io::Write;
    let base = start(None);
    let repo = format!("{base}/repositories/kg");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(b"_:x <urn:p> \"1\" .\n").unwrap();
    let gz = gz.finish().unwrap();
    for _ in 0..2 {
        write("POST", &format!("{repo}/statements"))
            .set("Content-Type", "application/n-triples")
            .send_bytes(&gz)
            .unwrap();
    }
    // Each upload's _:x is a new blank node, as on RDF4J.
    let count = bindings(
        &ureq::post(&repo)
            .set("Accept", "application/sparql-results+json")
            .send_form(&[("query", "SELECT (COUNT(DISTINCT ?s) AS ?n) { ?s ?p ?o }")])
            .unwrap()
            .into_string()
            .unwrap(),
    );
    assert_eq!(count[0]["n"]["value"], "2");
}

/// A request sent with `Expect: 100-continue` and answered without its body being read (RDF4J's
/// client begins transactions so) must not spoil the next request on the connection.
#[test]
fn unread_expected_bodies_keep_the_connection_usable() {
    use std::io::{BufRead, BufReader, Read, Write};
    let base = start(None);
    let address = base
        .trim_start_matches("http://")
        .trim_end_matches("/rdf4j-server");
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let body = "isolation-level=http%3A%2F%2Fwww.openrdf.org%2Fschema%2Fsesame%23SNAPSHOT_READ";
    write!(
        stream,
        "POST /rdf4j-server/repositories/kg/transactions HTTP/1.1\r\nHost: localhost\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
         Expect: 100-continue\r\n\r\n",
        body.len()
    )
    .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    // The server asks for the body (or answers); a client that got a final answer first
    // would not send it, so send it only after a 100.
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    if line.contains("100") {
        let mut blank = String::new();
        while blank != "\r\n" {
            blank.clear();
            reader.read_line(&mut blank).unwrap();
        }
        stream.write_all(body.as_bytes()).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
    }
    // No token: refused, whatever was read.
    assert!(line.starts_with("HTTP/1.1 4"), "{line}");
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header == "\r\n" {
            break;
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
    }
    let mut skip = vec![0; length];
    reader.read_exact(&mut skip).unwrap();
    write!(
        stream,
        "GET /rdf4j-server/protocol HTTP/1.1\r\nHost: localhost\r\n\r\n"
    )
    .unwrap();
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}
