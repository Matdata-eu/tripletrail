//! Security headers on every answer (#404): `nosniff` on all, and on every HTML page a
//! `Content-Security-Policy` that runs no inline script, so values from the data that hold
//! markup or `javascript:` links stay text, on the describe, Linked Data and OGC pages alike.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

/// Literals and IRIs that would run script if a page put them in its HTML as they are.
const DATA: &str = r#"
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix ex: <http://example.com/> .
<https://data.example.org/op/X> a ex:Station ;
    rdfs:label "<script>alert(1)</script>" ;
    ex:name "\" onmouseover=\"alert(2)" ;
    ex:note "'><img src=x onerror=alert(3)>" ;
    ex:home <javascript:alert(4)> ;
    ex:page "javascript:alert(5)" ;
    ex:link <http://example.com/x?a='onload=alert(6)//> ;
    geo:hasGeometry [ geo:asWKT "POINT(4.35 50.84)"^^geo:wktLiteral ] .
<https://data.example.org/op/Y> a ex:Station ; rdfs:label "Halle" ;
    geo:hasGeometry [ geo:asWKT "POINT(4.24 50.73)"^^geo:wktLiteral ] .
"#;

const X: &str = "https%3A%2F%2Fdata.example.org%2Fop%2FX";

fn start() -> String {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            ui: true,
            base_iris: vec![("https://data.example.org/".to_owned(), "kg".to_owned())],
            map_tiles: Some("https://{s}.tile.example.org/{z}/{x}/{y}.png".to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

struct Page {
    url: String,
    content_type: String,
    csp: Option<String>,
    nosniff: bool,
    referrer: Option<String>,
    body: String,
}

fn fetch(url: &str, accept: &str, proxied: bool) -> Page {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut request = agent.get(url).set("Accept", accept);
    if proxied {
        request = request
            .set("X-Forwarded-Host", "data.example.org")
            .set("X-Forwarded-Proto", "https");
    }
    let r = match request.call() {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{url}: {e}"),
    };
    assert_eq!(r.status(), 200, "{url}");
    Page {
        url: url.to_owned(),
        content_type: r.content_type().to_owned(),
        csp: r.header("Content-Security-Policy").map(str::to_owned),
        nosniff: r.header("X-Content-Type-Options") == Some("nosniff"),
        referrer: r.header("Referrer-Policy").map(str::to_owned),
        body: r.into_string().unwrap(),
    }
}

/// The page is HTML with the data pages' policy, and none of DATA's values became markup.
fn assert_safe(page: &Page) {
    let url = &page.url;
    assert!(
        page.content_type.starts_with("text/html"),
        "{url}: {}",
        page.content_type
    );
    assert!(page.nosniff, "{url}");
    assert_eq!(page.referrer.as_deref(), Some("same-origin"), "{url}");
    let csp = page
        .csp
        .as_deref()
        .unwrap_or_else(|| panic!("{url}: no CSP"));
    // Script from the server only: no inline script, no eval.
    assert!(csp.contains("script-src 'self';"), "{url}: {csp}");
    assert!(
        csp.contains("object-src 'none'") && csp.contains("base-uri 'self'"),
        "{url}: {csp}"
    );
    let body = &page.body;
    for hostile in [
        "<script>alert",
        "\" onmouseover=",
        "<img src=x",
        "href=\"javascript:",
        "src=\"javascript:",
        "'onload=",
    ] {
        assert!(!body.contains(hostile), "{url} holds {hostile:?}:\n{body}");
    }
}

#[test]
fn every_html_page_has_the_policy_and_keeps_data_as_text() {
    let base = start();
    let browser = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
    // The describe page and the Linked Data page of the same resource.
    let describe = fetch(&format!("{base}/kg/describe?iri={X}"), browser, false);
    assert_safe(&describe);
    assert!(
        describe.body.contains("&lt;script&gt;alert(1)"),
        "{}",
        describe.body
    );
    // The map's tiles come from the --map-tiles host, and nothing else from elsewhere.
    let csp = describe.csp.as_deref().unwrap();
    assert!(
        csp.contains("connect-src 'self' https://*.tile.example.org;"),
        "{csp}"
    );
    let linked = fetch(&format!("{base}/op/X"), browser, true);
    assert_safe(&linked);
    assert!(
        linked.body.contains("&lt;script&gt;alert(1)"),
        "{}",
        linked.body
    );
    // The OGC API pages, down to the feature.
    let ogc = format!("{base}/kg/ogc");
    for path in [
        "",
        "/collections",
        "/collections/Station",
        "/collections/Station/items",
    ] {
        assert_safe(&fetch(&format!("{ogc}{path}?f=html"), browser, false));
    }
    let items: serde_json::Value = serde_json::from_str(
        &fetch(
            &format!("{ogc}/collections/Station/items?f=json"),
            "application/json",
            false,
        )
        .body,
    )
    .unwrap();
    let ids: Vec<String> = items["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2, "{items}");
    for id in &ids {
        let id: String = id
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect();
        let feature = fetch(
            &format!("{ogc}/collections/Station/items/{id}?f=html"),
            browser,
            false,
        );
        assert_safe(&feature);
    }
}

#[test]
fn the_ui_pages_keep_their_own_policy() {
    let base = start();
    // MatGUI's own policy stays as it was; the hook adds only what it lacks.
    let ui = fetch(&format!("{base}/$/ui/"), "text/html", false);
    assert_eq!(
        ui.csp.as_deref(),
        Some("frame-ancestors 'self'; base-uri 'self'; form-action 'self'")
    );
    assert!(ui.nosniff);
    assert_eq!(ui.referrer.as_deref(), Some("same-origin"));
    let api = fetch(&format!("{base}/$/ui/api/kg"), "text/html", false);
    assert!(
        api.csp
            .unwrap()
            .starts_with("default-src 'self'; style-src 'self' 'unsafe-inline'")
    );
}

#[test]
fn every_answer_has_nosniff_and_only_html_a_policy() {
    let base = start();
    let json = fetch(
        &format!("{base}/kg/sparql?query=ASK%7B%7D"),
        "application/sparql-results+json",
        false,
    );
    assert!(
        json.content_type
            .starts_with("application/sparql-results+json")
    );
    assert!(json.nosniff);
    assert_eq!(json.csp, None);
    assert_eq!(json.referrer, None);
    let turtle = fetch(&format!("{base}/op/X"), "text/turtle", true);
    assert!(turtle.content_type.starts_with("text/turtle"));
    assert!(turtle.nosniff);
    assert_eq!(turtle.csp, None);
}
