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

/// How many of the stations a tile names by IRI.
fn stations(tile: &[u8]) -> usize {
    let needle = b"http://example.com/p";
    tile.windows(needle.len()).filter(|w| w == needle).count()
}

fn varint(b: &[u8], i: &mut usize) -> u64 {
    let (mut out, mut shift) = (0, 0);
    loop {
        let byte = b[*i];
        *i += 1;
        out |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte < 0x80 {
            return out;
        }
    }
}

/// The fields of a protocol buffer message: (field, varint value or bytes).
fn fields(b: &[u8]) -> Vec<(u64, u64, &[u8])> {
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        let key = varint(b, &mut i);
        match key & 7 {
            0 => out.push((key >> 3, varint(b, &mut i), &b[..0])),
            2 => {
                let n = usize::try_from(varint(b, &mut i)).unwrap();
                out.push((key >> 3, 0, &b[i..i + n]));
                i += n;
            }
            other => panic!("wire type {other}"),
        }
    }
    out
}

/// The ids of a tile's features (`None` for one without), over its layers.
fn feature_ids(tile: &[u8]) -> Vec<Option<u64>> {
    let mut out = Vec::new();
    for (f, _, layer) in fields(tile) {
        if f != 3 {
            continue;
        }
        for (lf, _, feature) in fields(layer) {
            if lf == 2 {
                out.push(fields(feature).iter().find(|x| x.0 == 1).map(|x| x.1));
            }
        }
    }
    out
}

/// FNV-1a 64 of `iri`, cut to 40 bits: the number a generalised tile gives a feature.
fn number(iri: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in iri.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash & 0xff_ffff_ffff
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
    let ids = feature_ids(&low);
    let drawn = ids.len();
    assert!(drawn > 100 && drawn < 1200, "{drawn}");
    assert!(low.len() * 2 < full.len(), "{} {}", low.len(), full.len());
    // Its features carry a number in place of their IRI (#261), and the full tile's none.
    assert_eq!(stations(&low), 0);
    let stations_iris: Vec<String> = (0..40)
        .flat_map(|i| (0..30).map(move |j| format!("http://example.com/p{i}_{j}")))
        .collect();
    let numbers: std::collections::HashMap<u64, &str> = stations_iris
        .iter()
        .map(|s| (number(s), s.as_str()))
        .collect();
    assert_eq!(numbers.len(), 1200);
    assert!(
        ids.iter()
            .all(|n| n.is_some_and(|n| numbers.contains_key(&n)))
    );
    assert!(feature_ids(&full).iter().all(Option::is_none));
    // The numbers turn back into the IRIs; one no station has is left out.
    let some: Vec<u64> = ids.iter().take(3).map(|n| n.unwrap()).collect();
    let query: Vec<String> = some.iter().map(|n| format!("id={n}")).collect();
    let found: serde_json::Value = serde_json::from_reader(
        ureq::get(&format!("{base}/iris/Station?{}&id=7", query.join("&")))
            .call()
            .unwrap()
            .into_reader(),
    )
    .unwrap();
    let found = found.as_object().unwrap();
    assert_eq!(found.len(), 3, "{found:?}");
    for n in &some {
        assert_eq!(found[&n.to_string()], serde_json::json!([numbers[n]]));
    }
    assert_eq!(
        ureq::get(&format!("{base}/iris/Station?id=x"))
            .call()
            .unwrap_err()
            .into_response()
            .unwrap()
            .status(),
        400
    );
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

/// A GET of `path` with the given `Accept-Encoding`: its `Content-Encoding`, `Vary`, and the
/// body as sent. Over a plain socket, as ureq unpacks gzip and drops the header.
fn raw(address: &str, path: &str, encoding: &str) -> (Option<String>, Option<String>, Vec<u8>) {
    use std::io::{Read, Write};
    let mut socket = std::net::TcpStream::connect(address).unwrap();
    write!(
        socket,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAccept-Encoding: {encoding}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut answer = Vec::new();
    socket.read_to_end(&mut answer).unwrap();
    let end = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&answer[..end]).into_owned();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let header = |name: &str| {
        head.lines().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    };
    let (coding, vary) = (header("Content-Encoding"), header("Vary"));
    (coding, vary, answer[end + 4..].to_vec())
}

#[test]
fn tiles_and_features_go_gzipped_to_a_client_that_accepts_it() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..200 {
        ttl.push_str(&format!(
            "ex:p{i} a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT({} 50.3)\"^^geo:wktLiteral ] .\n",
            4.3 + f64::from(i) / 500.0
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
    let address = server.address().to_string();
    let base = format!("http://{address}/kg");
    server.spawn();
    let (x, y) = tile_of(4.5, 50.3, 8);
    let url = format!("/kg/tiles/Station/8/{x}/{y}.mvt");
    let raw = |path: &str, encoding: &str| raw(&address, path, encoding);
    let (coding, vary, plain) = raw(&url, "identity");
    assert_eq!(coding, None);
    assert_eq!(vary.as_deref(), Some("Accept-Encoding"));
    let (coding, _, zipped) = raw(&url, "br, gzip;q=0.8");
    assert_eq!(coding.as_deref(), Some("gzip"));
    assert!(
        zipped.len() * 2 < plain.len(),
        "{} {}",
        zipped.len(),
        plain.len()
    );
    let mut unpacked = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(&zipped[..]),
        &mut unpacked,
    )
    .unwrap();
    assert_eq!(unpacked, plain);
    // Refused with q=0, it is not used.
    assert_eq!(raw(&url, "gzip;q=0").0, None);
    // The OGC API's features too.
    let items = "/kg/ogc/collections/Station/items?limit=200";
    assert_eq!(raw(items, "gzip").0.as_deref(), Some("gzip"));
    // ureq asks for gzip and unpacks it: a client sees the same JSON.
    let json: serde_json::Value = serde_json::from_reader(
        ureq::get(&format!("{base}{}", &items[3..]))
            .call()
            .unwrap()
            .into_reader(),
    )
    .unwrap();
    assert_eq!(json["features"].as_array().unwrap().len(), 200);
}
