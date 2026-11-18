//! Linked Data publishing (#141): dataset IRIs dereference with content negotiation, as in the
//! W3C "Best Practice Recipes for Publishing RDF Vocabularies" (recipes 3 and 5).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

const DATA: &str = r#"
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix op: <https://data.example.org/op/> .
@prefix ex: <http://example.com/> .
op:BE1 a ex:OperationalPoint ; rdfs:label "Brussel-Zuid" ;
    ex:position [ ex:km 12.5 ; ex:line ex:L50 ] .
<https://data.example.org/op/v1.ttl> rdfs:label "a resource whose IRI ends in .ttl" .
<https://data.example.org/vocab> a owl:Ontology ; rdfs:label "Rail vocabulary" .
<https://data.example.org/vocab#Track> a owl:Class ; rdfs:label "Track" .
<https://data.example.org/vocab#length> a owl:DatatypeProperty .
"#;

fn start(see_other: bool) -> String {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 2,
            base_iris: vec![("https://data.example.org/".to_owned(), "kg".to_owned())],
            see_other,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

/// A request to `base` as if it came through a proxy for `https://data.example.org`.
fn get(base: &str, path: &str, accept: Option<&str>) -> ureq::Response {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut r = agent
        .get(&format!("{base}{path}"))
        .set("X-Forwarded-Host", "data.example.org")
        .set("X-Forwarded-Proto", "https");
    if let Some(a) = accept {
        r = r.set("Accept", a);
    }
    match r.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn resources_dereference_in_the_format_asked_for() {
    let base = start(false);
    let r = get(&base, "/op/BE1", Some("text/turtle"));
    assert_eq!(r.status(), 200);
    assert_eq!(r.content_type(), "text/turtle");
    assert_eq!(r.header("Vary"), Some("Accept"));
    let link = r.header("Link").unwrap().to_owned();
    assert!(
        link.contains("<https://data.example.org/op/BE1.jsonld>; rel=\"alternate\"; type=\"application/ld+json\""),
        "{link}"
    );
    let ttl = r.into_string().unwrap();
    // The blank node of the position comes with it (concise bounded description).
    assert!(
        ttl.contains("Brussel-Zuid") && ttl.contains("12.5"),
        "{ttl}"
    );

    // Suffixes and ?format= pick the format too, whatever Accept says.
    let r = get(&base, "/op/BE1.nt", Some("text/html"));
    assert_eq!(r.content_type(), "application/n-triples");
    assert_eq!(r.into_string().unwrap().lines().count(), 5);
    let r = get(&base, "/op/BE1?format=jsonld", None);
    assert_eq!(r.content_type(), "application/ld+json");
    let r = get(&base, "/op/BE1", Some("application/rdf+xml"));
    assert_eq!(r.content_type(), "application/rdf+xml");
    // No Accept: Turtle. A browser: a page.
    assert_eq!(get(&base, "/op/BE1", None).content_type(), "text/turtle");
    let r = get(
        &base,
        "/op/BE1",
        Some("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    assert_eq!(r.content_type(), "text/html");
    let page = r.into_string().unwrap();
    assert!(
        page.contains("Brussel-Zuid") && page.contains("BE1.ttl"),
        "{page}"
    );
    // Unknown resources and formats.
    assert_eq!(get(&base, "/op/BE2", None).status(), 404);
    assert_eq!(get(&base, "/op/BE1", Some("image/png")).status(), 406);
    // An IRI that itself ends in .ttl is that resource, not a document of /op/v1.
    let r = get(&base, "/op/v1.ttl", Some("application/n-triples"));
    assert_eq!(r.status(), 200);
    assert!(r.into_string().unwrap().contains("ends in .ttl"));
    // The server's own paths still answer as before.
    assert_eq!(get(&base, "/$/ping", None).status(), 200);
}

#[test]
fn etags_answer_not_modified_until_the_data_changes() {
    let base = start(false);
    let r = get(&base, "/op/BE1", Some("text/turtle"));
    let etag = r.header("ETag").unwrap().to_owned();
    let agent = ureq::AgentBuilder::new().build();
    let again = |tag: &str| match agent
        .get(&format!("{base}/op/BE1"))
        .set("X-Forwarded-Host", "data.example.org")
        .set("X-Forwarded-Proto", "https")
        .set("Accept", "text/turtle")
        .set("If-None-Match", tag)
        .call()
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(again(&etag), 304);
    // Another format has another tag.
    let json = get(&base, "/op/BE1", Some("application/ld+json"));
    assert_ne!(json.header("ETag").unwrap(), etag);
    // A write changes it.
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <https://data.example.org/op/BE1> <http://example.com/x> 1 }")
        .unwrap();
    assert_eq!(again(&etag), 200);
}

#[test]
fn recipe_3_hash_namespace_is_one_document() {
    let base = start(false);
    let r = get(&base, "/vocab", Some("application/rdf+xml"));
    assert_eq!(r.status(), 200);
    assert_eq!(r.content_type(), "application/rdf+xml");
    let xml = r.into_string().unwrap();
    for term in ["Rail vocabulary", "vocab#Track", "vocab#length"] {
        assert!(xml.contains(term), "{term}: {xml}");
    }
    let ttl = get(&base, "/vocab", Some("text/turtle"))
        .into_string()
        .unwrap();
    // Turtle names the namespaces it uses with prefixes.
    assert!(ttl.contains("#> .") && ttl.contains("vocab:Track"), "{ttl}");
}

#[test]
fn recipe_5_slash_namespace_redirects_with_303() {
    let base = start(true);
    let r = get(&base, "/op/BE1", Some("application/rdf+xml"));
    assert_eq!(r.status(), 303);
    assert_eq!(
        r.header("Location"),
        Some("https://data.example.org/op/BE1.rdf")
    );
    assert_eq!(r.header("Vary"), Some("Accept"));
    let r = get(&base, "/op/BE1", Some("text/html"));
    assert_eq!(
        r.header("Location"),
        Some("https://data.example.org/op/BE1.html")
    );
    // The documents themselves answer 200.
    let r = get(&base, "/op/BE1.rdf", None);
    assert_eq!(r.status(), 200);
    assert_eq!(r.content_type(), "application/rdf+xml");
}

#[test]
fn any_iri_can_be_described_by_query() {
    let base = start(false);
    let r = ureq::get(&format!("{base}/kg/describe"))
        .query("iri", "https://data.example.org/vocab#Track")
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/n-triples");
    assert_eq!(r.into_string().unwrap().lines().count(), 2);
    let missing = ureq::get(&format!("{base}/kg/describe")).call();
    assert!(matches!(missing, Err(ureq::Error::Status(400, _))));
}
