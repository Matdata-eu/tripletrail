//! The map made ahead of its first visitor (#92): `serve` warms each dataset's collections
//! and facets once the datasets are open, and the unfiltered counts are kept until the next
//! write (#258).

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
    // A result cache, whose counts tell how many queries the engine was asked.
    engine.set_result_cache(16 << 20);
    let store = engine.clone();
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
    let get = |path: &str| -> serde_json::Value {
        serde_json::from_str(
            &ureq::get(&format!("{base}/kg/tiles/{path}"))
                .call()
                .unwrap()
                .into_string()
                .unwrap(),
        )
        .unwrap()
    };
    // The queries the engine was asked, answered from its result cache or not.
    let asked = || {
        let stats = store.result_cache_stats();
        stats.hits + stats.misses
    };
    // Requests read what it made: the same collections and counts, without asking the
    // engine anything, however many collections a request names (#258).
    let before = asked();
    let facets = get("facets/Station");
    assert_eq!(facets["features"], 2, "{facets}");
    let both = get("facets?collection=Station&collection=Line");
    assert_eq!(both["features"], 3, "{both}");
    assert_eq!(asked(), before);
    // What it made counts in the dataset's memory (#392), as do the string ranks.
    assert!(handle.warm_ranks("kg").is_some());
    let stats: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/$/stats/kg"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    let s = &stats["datasets"]["/kg"];
    let memory = s["memory"].as_object().unwrap();
    assert!(memory["mapCaches"].as_u64().unwrap() > 0, "{s}");
    assert!(memory["stringRanks"].as_u64().unwrap() > 0, "{s}");
    let sum: u64 = memory.values().map(|v| v.as_u64().unwrap()).sum();
    assert_eq!(sum, s["memoryBytes"].as_u64().unwrap(), "{s}");
    // A filtered request is counted.
    let hubs = get("facets/Station?bbox=0.5,0.5,1.5,1.5");
    assert_eq!(hubs["features"], 1, "{hubs}");
    assert!(asked() > before);
    // A write makes the kept counts stale: the next request counts again.
    store
        .update(
            "INSERT DATA { <http://example.com/s3> a <http://example.com/Station> ; \
             <http://www.opengis.net/ont/geosparql#hasGeometry> <http://example.com/g3> . \
             <http://example.com/g3> <http://www.opengis.net/ont/geosparql#asWKT> \
             \"POINT(3 3)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> }",
        )
        .unwrap();
    let facets = get("facets/Station");
    assert_eq!(facets["features"], 3, "{facets}");
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
