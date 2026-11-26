//! Tiles of a crowded layer at low zooms (#92): fewer units and one feature per position.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

/// The tile at zoom `z` that holds a point.
fn tile_of(lon: f64, lat: f64, z: u32) -> (u32, u32) {
    let n = f64::from(1u32 << z);
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let lat = lat.to_radians();
    let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor();
    (x as u32, y as u32)
}

fn tile(url: &str) -> Vec<u8> {
    let r = ureq::get(url).call().unwrap();
    assert_eq!(r.status(), 200);
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut body).unwrap();
    body
}

/// How many of the stations a tile names: by IRI, or in a generalised layer by the part of
/// it after the namespace (`p3_7`), which the layer holds once (#261).
fn stations(tile: &[u8]) -> usize {
    let needle = b"http://example.com/p";
    let whole = tile.windows(needle.len()).filter(|w| w == needle).count();
    if whole > 0 {
        return whole;
    }
    tile.windows(3)
        .filter(|w| {
            w[0] == b'p' && w[1].is_ascii_digit() && (w[2] == b'_' || w[2].is_ascii_digit())
        })
        .count()
}

/// How many times a tile holds the stations' namespace.
fn namespaces(tile: &[u8]) -> usize {
    let needle = b"http://example.com/";
    tile.windows(needle.len()).filter(|w| w == needle).count()
}

#[test]
fn a_crowded_layer_is_generalised_below_zoom_8() {
    // 1,200 stations 0.002° apart, and a few in a sparse layer.
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..40 {
        for j in 0..30 {
            ttl.push_str(&format!(
                "ex:p{i}_{j} a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT({} {})\"^^geo:wktLiteral ] .\n",
                4.3 + f64::from(i) / 500.0,
                50.3 + f64::from(j) / 500.0
            ));
        }
    }
    for i in 0..3 {
        ttl.push_str(&format!(
            "ex:d{i} a ex:Depot ; geo:hasGeometry [ geo:asWKT \"POINT({} 50.3)\"^^geo:wktLiteral ] .\n",
            4.3 + f64::from(i) / 1000.0
        ));
    }
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
    let base = format!("http://{}/kg/tiles", server.address());
    server.spawn();
    // At zoom 8 every station has its own position.
    let (x, y) = tile_of(4.34, 50.33, 8);
    let full = tile(&format!("{base}/Station/8/{x}/{y}.mvt"));
    assert_eq!(stations(&full), 1200);
    // At zoom 7 several share a position of the 512 units: one of each is drawn, and the
    // tile is a fraction of the size.
    let (x, y) = tile_of(4.34, 50.33, 7);
    let low = tile(&format!("{base}/Station/7/{x}/{y}.mvt"));
    let drawn = stations(&low);
    assert!(drawn > 100 && drawn < 1200, "{drawn}");
    assert!(low.len() * 2 < full.len(), "{} {}", low.len(), full.len());
    // Its features name their IRI as `ns` and `local`, the namespace held once.
    assert_eq!(namespaces(&low), 1);
    assert_eq!(namespaces(&full), 1200);
    // The same tile every time: the one on top is the last by IRI.
    assert_eq!(tile(&format!("{base}/Station/7/{x}/{y}.mvt")), low);
    // A layer of a few features keeps them all, however close.
    let (x, y) = tile_of(4.3, 50.3, 0);
    let depots = tile(&format!("{base}/Depot/0/{x}/{y}.mvt"));
    let needle = b"http://example.com/d";
    assert_eq!(
        depots.windows(needle.len()).filter(|w| w == needle).count(),
        3
    );
}
