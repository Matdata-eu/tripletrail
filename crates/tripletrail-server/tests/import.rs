//! The Import page's endpoints (#20): a file kept in memory, its Facade-X tree, a drafted
//! CONSTRUCT, its preview, and running it into a named graph; writers only.

use serde_json::Value;
use tripletrail::Engine;
use tripletrail_server::{Options, Server};

fn start() -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            update_token: Some("w".to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn status(r: Result<ureq::Response, ureq::Error>) -> (u16, String) {
    match r {
        Ok(r) | Err(ureq::Error::Status(_, r)) => (r.status(), r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

fn post(base: &str, path: &str, body: &[u8]) -> (u16, String) {
    status(
        ureq::post(&format!("{base}{path}"))
            .set("Authorization", "Bearer w")
            .send_bytes(body),
    )
}

fn ask(base: &str, query: &str) -> String {
    ureq::get(&format!("{base}/kg/query"))
        .query("query", query)
        .set("Accept", "text/csv")
        .call()
        .unwrap()
        .into_string()
        .unwrap()
}

const CSV: &[u8] = b"id,name,speed (km/h)\nS01,Gent - Brugge,160\nS02,Brugge - Oostende,120\n";

#[test]
fn a_file_is_read_mapped_and_added_to_a_graph() {
    let base = start();
    let (code, body) = post(&base, "/kg/import?name=sections.csv", CSV);
    assert_eq!(code, 201, "{body}");
    let upload: Value = serde_json::from_str(&body).unwrap();
    let id = upload["id"].as_str().unwrap();
    assert_eq!(upload["options"], "csv.headers=true");
    assert_eq!(upload["bytes"], CSV.len());
    let tree = upload["tree"].as_str().unwrap();
    assert!(
        tree.contains("<http://sparql.xyz/facade-x/data/name> \"Gent - Brugge\""),
        "{tree}"
    );
    // The drafted query maps every column, the odd one under a name of its own.
    let query = upload["query"].as_str().unwrap();
    assert!(
        query.contains("SERVICE <x-sparql-anything:location=upload:sections.csv,csv.headers=true>"),
        "{query}"
    );
    for key in [
        "ex:id ?id",
        "ex:name ?name",
        "ex:speed__km_h_ ?speed__km_h_",
    ] {
        assert!(query.contains(key), "{key} in {query}");
    }

    // Its preview: the triples it makes, nothing written.
    let preview = status(
        ureq::post(&format!("{base}/kg/import/{id}/preview"))
            .set("Authorization", "Bearer w")
            .set("Accept", "application/n-triples")
            .send_string(query),
    );
    assert_eq!(preview.0, 200, "{}\n{query}", preview.1);
    assert!(
        preview.1.contains(
            "<https://example.org/import/record/2> <https://example.org/import/name> \"Brugge - Oostende\""
        ),
        "{}",
        preview.1
    );
    assert_eq!(
        ask(&base, "SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }"),
        "n\r\n0\r\n"
    );

    // Run into a named graph: the graph's size before and after.
    let (code, body) = post(
        &base,
        &format!("/kg/import/{id}/run?graph=urn:x:sections"),
        query.as_bytes(),
    );
    assert_eq!(code, 200, "{body}");
    let ran: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (ran["before"].as_u64(), ran["after"].as_u64()),
        (Some(0), Some(8))
    );
    assert_eq!(
        ask(
            &base,
            "SELECT ?s ?v { GRAPH <urn:x:sections> { ?s <https://example.org/import/speed__km_h_> ?v } } ORDER BY ?s"
        ),
        "s,v\r\nhttps://example.org/import/record/1,160\r\nhttps://example.org/import/record/2,120\r\n"
    );

    // Not a CONSTRUCT, or one with FROM: refused. Forgotten: gone.
    let (code, _) = post(
        &base,
        &format!("/kg/import/{id}/run?graph=urn:x:g"),
        b"SELECT * { ?s ?p ?o }",
    );
    assert_eq!(code, 400);
    let deleted = status(
        ureq::delete(&format!("{base}/kg/import/{id}"))
            .set("Authorization", "Bearer w")
            .call(),
    );
    assert_eq!(deleted.0, 204);
    let (code, _) = post(&base, &format!("/kg/import/{id}/preview"), query.as_bytes());
    assert_eq!(code, 404);
}

#[test]
fn only_writers_import_and_an_upload_reads_nothing_else() {
    let base = start();
    // Without the update token: refused, on every step.
    let anonymous = status(ureq::post(&format!("{base}/kg/import?name=a.csv")).send_bytes(CSV));
    assert_eq!(anonymous.0, 401, "{}", anonymous.1);
    let (code, body) = post(&base, "/kg/import?name=a.json", br#"[{"k": 1}, {"k": 2}]"#);
    assert_eq!(code, 201, "{body}");
    let upload: Value = serde_json::from_str(&body).unwrap();
    let id = upload["id"].as_str().unwrap();
    let anonymous = status(
        ureq::post(&format!("{base}/kg/import/{id}/preview")).send_string("CONSTRUCT {} {}"),
    );
    assert_eq!(anonymous.0, 401);
    // A mapping may name only its upload: no files, no other uploads.
    for location in ["location=/etc/hostname", "location=upload:b.csv"] {
        let q = format!(
            "CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <x-sparql-anything:{location}> {{ ?s ?p ?o }} }}"
        );
        let (code, body) = post(&base, &format!("/kg/import/{id}/preview"), q.as_bytes());
        assert_ne!(code, 200, "{location}: {body}");
    }
    // The upload is not readable from the dataset's own queries.
    let q = "SELECT * WHERE { SERVICE <x-sparql-anything:location=upload:a.json> { ?s ?p ?o } }";
    let r = status(
        ureq::get(&format!("{base}/kg/query"))
            .query("query", q)
            .call(),
    );
    assert_ne!(r.0, 200, "{}", r.1);
    // A name with a path in it, and a file that is no CSV: refused.
    let (code, _) = post(&base, "/kg/import?name=../x.csv", CSV);
    assert_eq!(code, 400);
    let (code, body) = post(&base, "/kg/import?name=broken.json", b"{ not json");
    assert_eq!(code, 422, "{body}");
}
