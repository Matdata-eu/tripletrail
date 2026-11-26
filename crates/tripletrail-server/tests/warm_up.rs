//! The map made ahead of its first visitor (#92): `serve` warms each dataset's collections
//! and facets once the datasets are open.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

#[test]
fn the_map_is_made_before_its_first_request() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:s1 a ex:Station ; ex:kind ex:Hub ; geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
               ex:s2 a ex:Station ; ex:kind ex:Halt ; geo:hasGeometry [ geo:asWKT \"POINT(2 2)\"^^geo:wktLiteral ] .\n\
               ex:l1 a ex:Line ; ex:speed 160 ; geo:hasGeometry [ geo:asWKT \"LINESTRING(1 1, 2 2)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 2,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    let handle = server.handle();
    server.spawn();
    let done = handle.warm_map("kg").unwrap();
    assert!(
        done.starts_with("map ready: 2 collections and their facets in "),
        "{done}"
    );
    assert!(!done.contains("failed"), "{done}");
    // Requests read what it made: the same collections and counts.
    let facets: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/kg/tiles/facets/Station"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(facets["features"], 2, "{facets}");
    assert!(
        handle
            .warm_map("nowhere")
            .unwrap_err()
            .contains("no dataset")
    );
}

#[test]
fn the_map_is_made_again_once_a_load_is_over() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:s1 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 2,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    let handle = server.handle();
    server.spawn();
    let mut watch = handle.map_watch("kg");
    handle.warm_map("kg").unwrap();
    // Nothing changed: nothing to do.
    assert!(handle.rewarm_map("kg", &mut watch).is_none());
    let more = "<http://example.com/s2> a <http://example.com/Station> ; \
                <http://www.opengis.net/ont/geosparql#hasGeometry> [ \
                <http://www.opengis.net/ont/geosparql#asWKT> \"POINT(2 2)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> ] .";
    ureq::post(&format!("{base}/kg/data"))
        .set("Content-Type", "text/turtle")
        .send_string(more)
        .unwrap();
    // The load is seen first, and warmed at the call after, once the dataset kept still.
    assert!(handle.rewarm_map("kg", &mut watch).is_none());
    let done = handle.rewarm_map("kg", &mut watch).unwrap().unwrap();
    assert!(done.starts_with("map ready: 1 collections"), "{done}");
    assert!(handle.rewarm_map("kg", &mut watch).is_none());
    let facets: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/kg/tiles/facets/Station"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(facets["features"], 2, "{facets}");
    assert!(handle.rewarm_map("nowhere", &mut watch).is_none());
}
