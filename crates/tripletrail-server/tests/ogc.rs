//! OGC API – Features over a dataset (#138).

use serde_json::{Value, json};
use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

fn start(engine: Engine) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn get(url: &str) -> (u16, String, Value) {
    match ureq::get(url).call() {
        Ok(r) => {
            let kind = r.content_type().to_owned();
            (
                r.status(),
                kind,
                serde_json::from_str(&r.into_string().unwrap()).unwrap(),
            )
        }
        Err(ureq::Error::Status(code, r)) => {
            (code, String::new(), Value::String(r.into_string().unwrap()))
        }
        Err(e) => panic!("{e}"),
    }
}

/// The collection at `url` once its extent is made (in the background), or after 5 s.
fn with_extent(url: &str) -> Value {
    let mut collection = Value::Null;
    for _ in 0..500 {
        collection = get(url).2;
        if !collection["extent"].is_null() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    collection
}

fn href(links: &Value, rel: &str) -> Option<String> {
    links
        .as_array()?
        .iter()
        .find(|l| l["rel"] == rel)
        .map(|l| l["href"].as_str().unwrap().to_owned())
}

#[test]
fn features_from_the_shapes() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..25 {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:name \"S{i}\" ; ex:tracks {} ; ex:open {} ; ex:limit {} ; ex:operator ex:o1 ;\n\
             geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n",
            i % 3,
            i % 2 == 0,
            i % 2
        ));
    }
    // Every other station has a second geometry.
    for i in (0..25).step_by(2) {
        ttl.push_str(&format!(
            "ex:s{i} geo:hasGeometry [ geo:asWKT \"POINT({i} 3.25)\"^^geo:wktLiteral ] .\n"
        ));
    }
    ttl.push_str("ex:o1 a ex:Operator ; ex:name \"O1\" .\n");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let ogc = format!("{base}/kg/ogc");

    let (status, kind, landing) = get(&ogc);
    assert_eq!(status, 200);
    assert_eq!(kind, "application/json");
    assert_eq!(
        href(&landing["links"], "data").unwrap(),
        format!("{ogc}/collections")
    );
    let (_, _, conformance) = get(&format!("{ogc}/conformance"));
    assert!(
        conformance["conformsTo"]
            .to_string()
            .contains("conf/geojson")
    );
    let (status, kind, api) = get(&format!("{ogc}/api"));
    assert_eq!(status, 200);
    assert!(
        kind.starts_with("application/vnd.oai.openapi+json"),
        "{kind}"
    );
    assert!(
        api["paths"]["/collections/Station/items"].is_object(),
        "{api}"
    );
    // What Spectral's OpenAPI rules ask: a contact, a description, and every operation tagged
    // with a declared tag and described.
    assert!(api["info"]["contact"].is_object() && api["info"]["description"].is_string());
    let declared: Vec<&str> = api["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for (path, item) in api["paths"].as_object().unwrap() {
        for op in item.as_object().unwrap().values() {
            let tag = op["tags"][0].as_str().unwrap_or_else(|| panic!("{path}"));
            assert!(declared.contains(&tag), "{path}: {tag}");
            assert!(!op["description"].as_str().unwrap().is_empty(), "{path}");
        }
    }
    assert_eq!(
        api["paths"]["/collections/Station/items"]["get"]["tags"],
        json!(["Station"])
    );

    // Stations have a geometry; operators do not, so they are no collection.
    let (_, _, collections) = get(&format!("{ogc}/collections"));
    let ids: Vec<&str> = collections["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["Station"], "{collections}");
    let (status, _, station) = get(&format!("{ogc}/collections/Station"));
    assert_eq!(status, 200);
    assert_eq!(station["itemType"], "feature");
    assert_eq!(get(&format!("{ogc}/collections/Operator")).0, 404);

    // Pages of ten features, then the rest: a feature with two geometries is one feature.
    let (status, kind, page) = get(&format!("{ogc}/collections/Station/items"));
    assert_eq!(status, 200);
    assert_eq!(kind, "application/geo+json");
    assert_eq!(page["type"], "FeatureCollection");
    assert_eq!(page["numberReturned"], 10);
    let mut seen = 0;
    let mut next = Some(format!("{ogc}/collections/Station/items?limit=7"));
    let mut ids = std::collections::HashSet::new();
    while let Some(url) = next {
        let (_, _, page) = get(&url);
        assert_eq!(
            page["numberReturned"],
            if seen < 21 { 7 } else { 4 },
            "{page}"
        );
        for f in page["features"].as_array().unwrap() {
            assert!(ids.insert(f["id"].as_str().unwrap().to_owned()));
            seen += 1;
        }
        next = href(&page["links"], "next");
    }
    assert_eq!(seen, 25);

    // A box around x = 2..4.
    let (_, _, boxed) = get(&format!(
        "{ogc}/collections/Station/items?bbox=1.5,0,4.5,2&limit=100"
    ));
    let mut names: Vec<&str> = boxed["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["S2", "S3", "S4"], "{boxed}");
    let s2 = boxed["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["properties"]["name"] == "S2")
        .unwrap();
    // One of its two geometries: the most detailed, here the one with more digits.
    assert_eq!(s2["geometry"]["type"], "Point", "{s2}");
    assert_eq!(s2["geometry"]["coordinates"][1], 3.25);
    // Both, though only one is in the box, when asked for.
    let (_, _, all) = get(&format!(
        "{ogc}/collections/Station/items?bbox=1.5,0,2.5,2&geometries=all"
    ));
    let s2 = &all["features"][0];
    assert_eq!(s2["geometry"]["type"], "GeometryCollection", "{s2}");
    assert_eq!(s2["geometry"]["geometries"].as_array().unwrap().len(), 2);
    assert_eq!(
        get(&format!("{ogc}/collections/Station/items?geometries=some")).0,
        400
    );
    let s3 = boxed["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["properties"]["name"] == "S3")
        .unwrap();
    assert_eq!(s3["geometry"]["type"], "Point");
    assert_eq!(s3["geometry"]["coordinates"][0], 3.0);
    assert_eq!(s2["properties"]["tracks"], 2);
    assert_eq!(s2["properties"]["open"], true);
    assert_eq!(s2["properties"]["operator"], "http://example.com/o1");

    // One feature, by its percent-encoded IRI.
    let one = href(&s2["links"], "self").unwrap();
    assert!(
        one.ends_with("/items/http%3A%2F%2Fexample.com%2Fs2"),
        "{one}"
    );
    let (status, _, feature) = get(&one);
    assert_eq!(status, 200);
    // The OGC test suite leaves the IRI's slashes unencoded.
    let (status, _, same) = get(&one.replace("%2Fs2", "/s2"));
    assert_eq!(status, 200, "{same}");
    assert_eq!(same["id"], feature["id"]);
    assert_eq!(feature["id"], "http://example.com/s2");
    assert_eq!(feature["properties"]["name"], "S2");
    assert_eq!(
        get(&format!(
            "{ogc}/collections/Station/items/http%3A%2F%2Fexample.com%2Fo1"
        ))
        .0,
        404
    );

    // Other CRSs (Part 2): EPSG:4326 is latitude first, a box may be in Web Mercator.
    let epsg = |code: u32| format!("http%3A%2F%2Fwww.opengis.net%2Fdef%2Fcrs%2FEPSG%2F0%2F{code}");
    let r = ureq::get(&format!(
        "{ogc}/collections/Station/items?bbox=2.5,0.5,3.5,1.5&crs={}",
        epsg(4326)
    ))
    .call()
    .unwrap();
    assert_eq!(
        r.header("Content-Crs"),
        Some("<http://www.opengis.net/def/crs/EPSG/0/4326>")
    );
    let latlon: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(
        latlon["features"][0]["geometry"]["coordinates"],
        serde_json::json!([1.0, 3.0])
    );
    // x 2.5..3.5 and y 0.5..1.5 degrees, in metres.
    let (_, _, mercator) = get(&format!(
        "{ogc}/collections/Station/items?bbox=278298,55660,389618,166998&bbox-crs={}&crs={}",
        epsg(3857),
        epsg(3857)
    ));
    let features = mercator["features"].as_array().unwrap();
    assert_eq!(features.len(), 1, "{mercator}");
    let x = features[0]["geometry"]["coordinates"][0].as_f64().unwrap();
    assert!((x - 333_958.47).abs() < 0.1, "{x}");
    let (_, _, station) = get(&format!("{ogc}/collections/Station"));
    assert_eq!(station["crs"].as_array().unwrap().len(), 3);
    // The extent: the box around every geometry, no time without tts:time.
    let station = with_extent(&format!("{ogc}/collections/Station"));
    assert_eq!(
        station["extent"]["spatial"]["bbox"],
        serde_json::json!([[0.0, 1.0, 24.0, 3.25]]),
        "{station}"
    );
    assert!(station["extent"]["temporal"].is_null(), "{station}");
    let page = ureq::get(&format!("{ogc}/collections/Station?f=html"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        page.contains("<dt>Extent</dt><dd>0.0, 1.0, 24.0, 3.25</dd>"),
        "{page}"
    );
    assert_eq!(
        get(&format!("{ogc}/collections/Station/items?crs=EPSG:4326")).0,
        400
    );
    assert!(
        conformance["conformsTo"]
            .to_string()
            .contains("ogcapi-features-2/1.0/conf/crs")
    );

    // Property filters on the facets: tracks has a few values, name does not.
    let (status, _, two) = get(&format!(
        "{ogc}/collections/Station/items?tracks=2&limit=100"
    ));
    assert_eq!(status, 200, "{two}");
    let mut names: Vec<&str> = two["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["S11", "S14", "S17", "S2", "S20", "S23", "S5", "S8"],
        "{two}"
    );
    let (_, _, both) = get(&format!(
        "{ogc}/collections/Station/items?tracks=2&open=true&limit=100"
    ));
    assert_eq!(both["numberReturned"], 4, "{both}");
    // Names are a text facet, so they filter too (the whole value, as property filters do).
    let (_, _, s2) = get(&format!("{ogc}/collections/Station/items?name=S2"));
    assert_eq!(s2["numberReturned"], 1, "{s2}");
    assert!(
        api["paths"]["/collections/Station/items"]["get"]["parameters"]
            .to_string()
            .contains("\"tracks\"")
    );
    // Query parameters as OGC API – Features defines them: one comma-separated value.
    for p in api["paths"]["/collections/Station/items"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["in"] == "query")
    {
        assert_eq!(
            (&p["style"], &p["explode"]),
            (&"form".into(), &false.into()),
            "{p}"
        );
    }
    // A facet whose key is a parameter of the items (`limit`) stays a parameter only.
    let (_, _, ten) = get(&format!("{ogc}/collections/Station/items?limit=10"));
    assert_eq!(ten["numberReturned"], 10, "{ten}");
    assert_eq!(
        api["paths"]["/collections/Station/items"]["get"]["parameters"]
            .to_string()
            .matches("\"limit\"")
            .count(),
        1
    );

    // Unknown parameters and bad values are refused.
    assert_eq!(
        get(&format!("{ogc}/collections/Station/items?colour=red")).0,
        400
    );
    assert_eq!(
        get(&format!("{ogc}/collections/Station/items?bbox=1,2,3")).0,
        400
    );
    assert_eq!(
        get(&format!("{ogc}/collections/Station/items?limit=0")).0,
        400
    );
    assert_eq!(
        get(&format!("{ogc}/collections/Station/items?f=xml")).0,
        406
    );
    assert_eq!(get(&format!("{ogc}/nope")).0, 404);
}

/// `GET url` with `accept`: status, content type, body.
fn page(url: &str, accept: &str) -> (u16, String, String) {
    match ureq::get(url).set("Accept", accept).call() {
        Ok(r) => {
            let kind = r.content_type().to_owned();
            (r.status(), kind, r.into_string().unwrap())
        }
        Err(ureq::Error::Status(code, r)) => (code, String::new(), r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn html_pages_for_browsers() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:s1 a ex:Station ; ex:name \"Ås <1>\" ; geo:hasGeometry [ geo:asWKT \"POINT(10 60)\"^^geo:wktLiteral ] .\n\
         ex:s2 a ex:Station ; ex:name \"S2\" ; geo:hasGeometry [ geo:asWKT \"POINT(11 61)\"^^geo:wktLiteral ] .\n\
         ex:t1 a ex:Track ; ex:name \"T1\" ; geo:hasGeometry [ geo:asWKT \"LINESTRING(10 60, 11 61)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let ogc = format!("{base}/kg/ogc");
    let browser = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

    // A browser gets HTML, a client asking for JSON (or nothing) gets JSON.
    let (status, kind, landing) = page(&ogc, browser);
    assert_eq!(status, 200, "{landing}");
    assert!(kind.starts_with("text/html"), "{kind}");
    assert!(
        landing.contains(&format!("href=\"{ogc}/collections\"")),
        "{landing}"
    );
    assert!(landing.contains("?f=json"), "{landing}");
    let (_, kind, _) = page(&ogc, "application/json");
    assert_eq!(kind, "application/json");
    assert_eq!(get(&ogc).1, "application/json");
    // `f` wins over `Accept`, and the links of an `f=html` page keep it.
    let (_, kind, collections) = page(&format!("{ogc}/collections?f=html"), "*/*");
    assert!(kind.starts_with("text/html"));
    assert!(collections.contains("Station") && collections.contains("Track"));
    assert!(
        collections.contains(&format!("href=\"{ogc}/collections/Station/items?f=html\"")),
        "{collections}"
    );
    let (_, kind, _) = page(&format!("{ogc}/collections?f=json"), browser);
    assert_eq!(kind, "application/json");

    // The JSON documents link their HTML form.
    let (_, _, items) = get(&format!("{ogc}/collections/Station/items"));
    assert!(
        href(&items["links"], "alternate")
            .unwrap()
            .ends_with("items?f=html")
    );
    // Typed `text/html` without parameters, as clients match it.
    assert!(
        items["links"]
            .to_string()
            .contains("\"type\":\"text/html\"")
    );
    let (_, _, conformance) = get(&format!("{ogc}/conformance"));
    assert!(conformance["conformsTo"].to_string().contains("conf/html"));

    // Items: a sketch of the geometries, a table of the properties, text escaped.
    let (status, _, items) = page(&format!("{ogc}/collections/Station/items"), browser);
    assert_eq!(status, 200, "{items}");
    assert!(items.contains("<svg"), "{items}");
    assert_eq!(items.matches("<circle").count(), 2, "{items}");
    assert!(items.contains("Ås &lt;1&gt;"), "{items}");
    assert!(!items.contains("<1>"), "{items}");
    let (_, _, tracks) = page(&format!("{ogc}/collections/Track/items"), browser);
    assert!(tracks.contains("<polyline"), "{tracks}");

    // A feature, and the other resources.
    let one = format!(
        "{ogc}/collections/Station/items/{}",
        "http%3A%2F%2Fexample.com%2Fs2"
    );
    let (status, kind, feature) = page(&one, browser);
    assert_eq!(status, 200, "{feature}");
    assert!(kind.starts_with("text/html"));
    assert!(
        feature.contains("<circle") && feature.contains("S2"),
        "{feature}"
    );
    for path in ["conformance", "api", "collections/Station"] {
        let (status, kind, body) = page(&format!("{ogc}/{path}"), browser);
        assert_eq!(status, 200, "{path}: {body}");
        assert!(kind.starts_with("text/html"), "{path}");
    }
    // Errors stay plain text, and say they depend on `Accept` too.
    let missing = ureq::get(&format!("{ogc}/collections/Nope"))
        .set("Accept", browser)
        .call();
    match missing {
        Err(ureq::Error::Status(404, r)) => assert_eq!(r.header("Vary"), Some("Accept")),
        other => panic!("expected a 404, got {other:?}"),
    }
    // A polygon's hole stays empty, however its rings are wound.
    assert!(items.contains("fill-rule:evenodd"));
}

fn tile(url: &str) -> (u16, Vec<u8>) {
    match ureq::get(url).call() {
        Ok(r) => {
            assert_eq!(r.content_type(), "application/vnd.mapbox-vector-tile");
            let status = r.status();
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut r.into_reader(), &mut body).unwrap();
            (status, body)
        }
        Err(ureq::Error::Status(code, _)) => (code, Vec::new()),
        Err(e) => panic!("{e}"),
    }
}

fn holds(tile: &[u8], text: &str) -> bool {
    tile.windows(text.len()).any(|w| w == text.as_bytes())
}

#[test]
fn a_tile_has_the_same_bytes_whatever_order_the_data_came_in() {
    let station = |i: u32| {
        format!(
            "ex:s{i} a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT({} 50.85)\"^^geo:wktLiteral ] .\n",
            4.0 + f64::from(i) / 100.0
        )
    };
    let prefixes = "@prefix ex: <http://example.com/> .\n\
                    @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n";
    let tile_of = |ids: Vec<u32>| {
        let ttl: String = std::iter::once(prefixes.to_owned())
            .chain(ids.into_iter().map(station))
            .collect();
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
        let base = start(engine);
        tile(&format!("{base}/kg/tiles/Station/3/4/2.mvt"))
    };
    let (status, forward) = tile_of((0..60).collect());
    assert_eq!(status, 200);
    assert!(holds(&forward, "http://example.com/s59"));
    let (_, backward) = tile_of((0..60).rev().collect());
    assert_eq!(forward, backward);
}

#[test]
fn vector_tiles_from_the_shapes() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:s1 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(4.35 50.85)\"^^geo:wktLiteral ] .\n\
               ex:l1 a ex:Line ; geo:hasGeometry [ geo:asWKT \"LINESTRING(4.3 50.8, 4.4 50.9)\"^^geo:wktLiteral ] .\n\
               ex:z1 a ex:Zone ; ex:area [ geo:asWKT \"POLYGON((4 50, 5 50, 5 51, 4 51, 4 50))\"^^geo:wktLiteral ] .\n\
               ex:o1 a ex:Operator .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let tiles = format!("{base}/kg/tiles");

    // The whole world: a layer per collection, each feature with its IRI.
    let (status, world) = tile(&format!("{tiles}/all/0/0/0.mvt"));
    assert_eq!(status, 200);
    assert!(holds(&world, "Station") && holds(&world, "Line"));
    assert!(holds(&world, "http://example.com/s1"));
    assert!(holds(&world, "http://example.com/l1"));
    // A collection with another geometry path, read by its own query.
    assert!(holds(&world, "Zone") && holds(&world, "http://example.com/z1"));
    // One layer; Brussels at z10 is tile 524/343.
    let (_, station) = tile(&format!("{tiles}/Station/10/524/343.mvt"));
    assert!(holds(&station, "http://example.com/s1"));
    assert!(!holds(&station, "Line"));
    // Elsewhere, an empty tile.
    let (status, empty) = tile(&format!("{tiles}/all/10/0/0.mvt"));
    assert_eq!((status, empty.len()), (200, 0));
    // A write shows in the next tile, cached or not.
    assert_eq!(
        tile(&format!("{tiles}/Station/0/0/0.mvt")).1,
        tile(&format!("{tiles}/Station/0/0/0.mvt")).1
    );
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(
            "PREFIX ex: <http://example.com/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
             INSERT DATA { ex:s2 a ex:Station ; geo:hasGeometry ex:g2 . \
             ex:g2 geo:asWKT \"POINT(-70 -30)\"^^geo:wktLiteral }",
        )
        .unwrap();
    assert!(holds(
        &tile(&format!("{tiles}/Station/0/0/0.mvt")).1,
        "http://example.com/s2"
    ));

    assert_eq!(tile(&format!("{tiles}/Operator/0/0/0.mvt")).0, 404);
    assert_eq!(tile(&format!("{tiles}/all/1/2/0.mvt")).0, 404);
    assert_eq!(tile(&format!("{tiles}/all/a/0/0.mvt")).0, 400);
    assert_eq!(tile(&format!("{tiles}/all/0/0")).0, 404);
}

#[test]
fn a_style_draws_each_collection_over_the_tiles() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:s1 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(4.35 50.85)\"^^geo:wktLiteral ] .\n\
               ex:s2 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(4.36 50.86)\"^^geo:wktLiteral ] .\n\
               ex:l1 a ex:Line ; geo:hasGeometry [ geo:asWKT \"LINESTRING(4.3 50.8, 4.4 50.9)\"^^geo:wktLiteral ] .\n\
               ex:o1 a ex:Operator .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let (status, kind, style) = get(&format!("{base}/kg/tiles/style.json"));
    assert_eq!((status, kind.as_str()), (200, "application/json"));
    assert_eq!(style["version"], 8);
    assert_eq!(
        style["sources"]["tripletrail"]["tiles"][0],
        format!("{base}/kg/tiles/all/{{z}}/{{x}}/{{y}}.mvt")
    );
    // No base map unless the server names one: a plain background.
    assert!(style["sources"]["basemap"].is_null());
    assert_eq!(
        style["metadata"]["tripletrail:basemaps"],
        serde_json::json!([])
    );
    assert_eq!(style["layers"][0]["type"], "background");
    // A collection per class with a geometry, for the legend, in its generated colour.
    let legend = style["metadata"]["tripletrail:collections"]
        .as_array()
        .unwrap();
    let ids: Vec<&str> = legend.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["Line", "Station"]);
    let station = &legend[1];
    assert_eq!(station["class"], "http://example.com/Station");
    assert_eq!(station["instances"], 2);
    assert_eq!(station["minZoom"], 0);
    let colour = station["color"].as_str().unwrap();
    assert!(colour.starts_with('#') && colour.len() == 7, "{colour}");
    // Three layers each, areas under lines under points, in the collection's colour; within
    // each, the collection with more features below.
    let layers: Vec<&Value> = style["layers"].as_array().unwrap().iter().skip(1).collect();
    let ids: Vec<&str> = layers.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "Station-fill",
            "Line-fill",
            "Station-line",
            "Line-line",
            "Station-point",
            "Line-point"
        ]
    );
    let point = layers[4];
    assert_eq!(point["source-layer"], "Station");
    assert_eq!(point["paint"]["circle-color"], colour);
    // A server with a base map puts it under the layers.
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 2,
            map_tiles: Some("https://tile.example.org/{z}/{x}/{y}.png".to_owned()),
            map_attribution: Some("© Example".to_owned()),
            map_basemaps: vec![tripletrail_server::Basemap {
                title: "Aerial".to_owned(),
                tiles: "https://aerial.example.org/{z}/{y}/{x}.jpg".to_owned(),
                attribution: "© Aerial".to_owned(),
                maxzoom: 17,
            }],
            ..Options::default()
        },
    )
    .unwrap();
    let other = format!("http://{}", server.address());
    server.spawn();
    let style = get(&format!("{other}/kg/tiles/style.json")).2;
    assert_eq!(style["layers"][0]["id"], "basemap");
    assert_eq!(style["sources"]["basemap"]["attribution"], "© Example");
    // The base maps the Map page switches between: the server's own, then the others.
    let basemaps = &style["metadata"]["tripletrail:basemaps"];
    assert_eq!(basemaps[0]["key"], "map");
    assert_eq!(basemaps[0]["title"], "Base map");
    assert_eq!(basemaps[1]["key"], "basemap-1");
    assert_eq!(basemaps[1]["title"], "Aerial");
    assert_eq!(
        basemaps[1]["tiles"],
        "https://aerial.example.org/{z}/{y}/{x}.jpg"
    );
    assert_eq!(basemaps[1]["attribution"], "© Aerial");
    // The last zoom of each one's tiles, for the page to enlarge them beyond it (#52).
    assert_eq!(basemaps[0]["maxzoom"], 19);
    assert_eq!(basemaps[1]["maxzoom"], 17);
}

/// The features without a geometry are left out of the facets by name when they can be
/// (#946), and by testing every feature's geometry when one is a blank node: both count the
/// same.
#[test]
fn facets_leave_out_features_without_a_geometry_either_way() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..4 {
        let status = if i < 3 { "open" } else { "closed" };
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:status ex:{status} ;\n\
             \x20 geo:hasGeometry [ geo:asWKT \"POINT(4.{i} 50.8)\"^^geo:wktLiteral ] .\n"
        ));
    }
    // Without a geometry, and a station in two graphs: left out once.
    ttl.push_str("ex:s8 a ex:Station ; ex:status ex:closed .\n");
    ttl.push_str("ex:g { ex:s8 a ex:Station }\n");
    let counts = |ttl: &str| {
        let engine = Engine::new_tripletrail();
        // The default graph is the union, as Fuseki's unionDefaultGraph: ex:s8, typed in two
        // graphs, is one feature.
        engine.set_union_default_graph(true);
        engine.load(RdfFormat::TriG, ttl.as_bytes()).unwrap();
        let base = start(engine);
        let out = get(&format!("{base}/kg/tiles/facets/Station")).2;
        let status: Vec<(String, u64)> = out["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "status")
            .unwrap()["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["value"].as_str().unwrap().to_owned(),
                    v["count"].as_u64().unwrap(),
                )
            })
            .collect();
        (out["features"].as_u64().unwrap(), status)
    };
    let expected = (
        4,
        vec![
            ("http://example.com/open".to_owned(), 3),
            ("http://example.com/closed".to_owned(), 1),
        ],
    );
    assert_eq!(counts(&ttl), expected, "named");
    ttl.push_str("[] a ex:Station ; ex:status ex:closed .\n");
    assert_eq!(counts(&ttl), expected, "tested");
}

#[test]
fn facets_count_and_filter_the_map() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:open rdfs:label \"Open\" .\n",
    );
    // Stations 0-3 open, 4-5 closed; even ones electrified.
    for i in 0..6 {
        let status = if i < 4 { "open" } else { "closed" };
        let power = if i % 2 == 0 { "ac" } else { "none" };
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:status ex:{status} ; ex:power ex:{power} ; ex:length {} ;\n\
             \x20 ex:name \"{} {i}\" ;\n\
             \x20 geo:hasGeometry [ geo:asWKT \"POINT(4.{i} 50.8)\"^^geo:wktLiteral ] .\n",
            i * 10,
            if i < 3 { "North" } else { "South" }
        ));
    }
    // A station without a geometry: not on the map, so not counted.
    ttl.push_str("ex:s9 a ex:Station ; ex:status ex:closed ; ex:name \"South 9\" .\n");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let facets = |query: &str| get(&format!("{base}/kg/tiles/facets/Station{query}")).2;
    let counts = |out: &Value, key: &str| -> Vec<(String, u64)> {
        out["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == key)
            .unwrap_or_else(|| panic!("{key} in {out}"))["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["label"]
                        .as_str()
                        .unwrap_or(v["value"].as_str().unwrap())
                        .to_owned(),
                    v["count"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let all = facets("");
    assert_eq!(all["features"], 6);
    assert_eq!(
        counts(&all, "status"),
        [
            ("Open".to_owned(), 4),
            ("http://example.com/closed".to_owned(), 2)
        ]
    );
    // A filter counts the other facets under it, and its own under the others only.
    let open = facets("?status=http%3A%2F%2Fexample.com%2Fopen");
    assert_eq!(open["features"], 4);
    assert_eq!(counts(&open, "status").len(), 2);
    assert_eq!(
        counts(&open, "power"),
        [
            ("http://example.com/ac".to_owned(), 2),
            ("http://example.com/none".to_owned(), 2)
        ]
    );
    let both =
        facets("?status=http%3A%2F%2Fexample.com%2Fclosed&power=http%3A%2F%2Fexample.com%2Fac");
    assert_eq!(both["features"], 1);
    // Tiles keep the features the filters keep: on the collection's own tile by key, on
    // `all` by `{collection}.{key}`.
    let (_, closed) = tile(&format!(
        "{base}/kg/tiles/all/0/0/0.mvt?Station.status=http%3A%2F%2Fexample.com%2Fclosed"
    ));
    assert!(holds(&closed, "http://example.com/s4") && holds(&closed, "http://example.com/s5"));
    assert!(!holds(&closed, "http://example.com/s0"));
    let (_, open) = tile(&format!(
        "{base}/kg/tiles/Station/0/0/0.mvt?status=http%3A%2F%2Fexample.com%2Fopen"
    ));
    assert!(holds(&open, "http://example.com/s0") && !holds(&open, "http://example.com/s4"));
    // The unfiltered tile, cached before or after, keeps them all.
    let (_, every) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt"));
    assert!(holds(&every, "http://example.com/s0") && holds(&every, "http://example.com/s4"));
    assert!(!holds(&every, "http://example.com/open"));
    // Coloured by a values facet, each feature carries its value as `value`; on `all` with
    // `color.{collection}`, on the collection's own tile with `color`.
    let (_, coloured) = tile(&format!(
        "{base}/kg/tiles/all/0/0/0.mvt?color.Station=status"
    ));
    assert!(
        holds(&coloured, "http://example.com/open")
            && holds(&coloured, "http://example.com/closed")
    );
    let (_, own) = tile(&format!("{base}/kg/tiles/Station/0/0/0.mvt?color=power"));
    assert!(holds(&own, "http://example.com/ac") && !holds(&own, "http://example.com/open"));
    // By a key that is no facet, it is left out, as a filter on such a key is.
    let (status, plain) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt?color.Station=nope"));
    assert_eq!((status, plain), (200, every.clone()));
    // By a range facet, each feature carries its highest number.
    let (_, ranged) = tile(&format!(
        "{base}/kg/tiles/all/0/0/0.mvt?color.Station=length"
    ));
    assert!(holds(&ranged, "50") && !holds(&every, "50"));
    // A text facet: searched, not counted; `.text` keeps the features whose value contains
    // it, whatever its case.
    let name = all["facets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "name")
        .unwrap_or_else(|| panic!("name in {all}"))
        .clone();
    assert_eq!(
        name,
        json!({ "key": "name", "title": "name", "text": true })
    );
    assert_eq!(facets("?name.text=SOUTH")["features"], 3);
    assert_eq!(facets("?name.text=th%201")["features"], 1);
    let (_, north) = tile(&format!(
        "{base}/kg/tiles/all/0/0/0.mvt?Station.name.text=north"
    ));
    assert!(holds(&north, "http://example.com/s2") && !holds(&north, "http://example.com/s3"));
    // Nor is a text facet a colour.
    let (_, plain) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt?color.Station=name"));
    assert_eq!(plain, every);
    // The facets' features as a query, each with its geometry, for the query page.
    let closed = facets("?status=http%3A%2F%2Fexample.com%2Fclosed");
    assert_eq!(closed["features"], 2);
    let query = closed["query"].as_str().unwrap();
    let r = ureq::get(&format!("{base}/kg/sparql"))
        .query("query", query)
        .set("Accept", "application/sparql-results+json")
        .call()
        .unwrap();
    let results: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    let rows = results["results"]["bindings"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{query}");
    assert!(rows.iter().all(|b| {
        b["geometry"]["value"]
            .as_str()
            .unwrap()
            .starts_with("POINT")
    }));
    assert_eq!(get(&format!("{base}/kg/tiles/facets/Nope")).0, 404);
    // A range facet: its span, and bounds that filter.
    let length = |out: &Value| {
        out["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "length")
            .unwrap_or_else(|| panic!("length in {out}"))
            .clone()
    };
    let span = length(&all);
    assert_eq!(
        (
            span["range"].clone(),
            span["min"].clone(),
            span["max"].clone()
        ),
        (json!(true), json!(0.0), json!(50.0))
    );
    let long = facets("?length.min=20");
    assert_eq!(long["features"], 4);
    // The span of the range itself is under the other filters only.
    assert_eq!(length(&long)["min"], json!(0.0));
    assert_eq!(facets("?length.min=20&length.max=30")["features"], 2);
    let (_, short) = tile(&format!(
        "{base}/kg/tiles/all/0/0/0.mvt?Station.length.max=10"
    ));
    assert!(holds(&short, "http://example.com/s1") && !holds(&short, "http://example.com/s2"));
    assert_eq!(
        get(&format!("{base}/kg/tiles/facets/Station?length.min=x")).0,
        400
    );
}

#[test]
fn tiles_follow_the_shapes() {
    let dir = tripletrail_testdir::tempdir("ogc-shapes");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:s1 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(4.35 50.85)\"^^geo:wktLiteral ] .\n\
               ex:l1 a ex:Line ; ex:speed 120 ; geo:hasGeometry [ geo:asWKT \"LINESTRING(4.3 50.8, 4.4 50.9)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let shapes = |method: &str, path: &str| {
        ureq::request(method, &format!("{base}/$/datasets/kg/shapes{path}"))
            .set("Authorization", "Bearer t")
    };
    let world = format!("{base}/kg/tiles/Station/0/0/0.mvt");
    assert!(holds(&tile(&world).1, "http://example.com/s1"));
    // The dataset's own shapes put the stations from zoom 5 on: the cached tile goes.
    shapes("POST", "/customise").call().unwrap();
    let station = shapes("GET", "/files/Station.ttl")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(station.contains("tts:color"), "{station}");
    let points = |style: &Value| -> Vec<String> {
        style["layers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["type"] == "circle")
            .map(|l| l["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let style = get(&format!("{base}/kg/tiles/style.json")).2;
    assert_eq!(points(&style), ["Line-point", "Station-point"]);
    let edited = station.replacen(
        "tts:color",
        "tts:minZoom 5 ;\n    tts:drawOrder -1 ;\n    tts:color",
        1,
    );
    shapes("PUT", "/files/Station.ttl")
        .send_string(&edited)
        .unwrap();
    assert_eq!(tile(&world), (200, Vec::new()));
    // Its tts:drawOrder puts the stations below the lines.
    let style = get(&format!("{base}/kg/tiles/style.json")).2;
    assert_eq!(points(&style), ["Station-point", "Line-point"]);
    assert!(holds(
        &tile(&format!("{base}/kg/tiles/Station/5/16/10.mvt")).1,
        "http://example.com/s1"
    ));
    // A shape of another IRI for the lines replaces their generated shape, with its count;
    // the legend follows sh:order, and lists last a shape for a class not in the data. A
    // plain constraint shape for the stations keeps their generated shape.
    let own = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
               @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
               @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               <urn:my:lines> a sh:NodeShape ; sh:targetClass <http://example.com/Line> ;\n\
                 tts:key \"lines\" ; sh:name \"Lines\" ; sh:order 1 ;\n\
                 tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] ;\n\
                 sh:property [ sh:path <http://example.com/speed> ; tts:facet tts:Facet-Range ;\n\
                   <http://qudt.org/schema/qudt/hasUnit> <http://qudt.org/vocab/unit/KiloM-PER-HR> ;\n\
                   tts:colorClasses ( [ tts:upTo 100 ; rdfs:label \"slow\" ] [ rdfs:label \"fast\" ] ) ] .\n\
               <urn:my:stationrule> a sh:NodeShape ; sh:targetClass <http://example.com/Station> ;\n\
                 sh:property [ sh:path <http://example.com/len> ; sh:minCount 1 ] .\n\
               <urn:my:depots> a sh:NodeShape ; sh:targetClass <http://example.com/Depot> ;\n\
                 tts:key \"depots\" ; sh:name \"Depots\" ; sh:order 0 ;\n\
                 tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] .\n";
    shapes("DELETE", "/files/Line.ttl").call().unwrap();
    shapes("PUT", "/files/mine.ttl").send_string(own).unwrap();
    let style = get(&format!("{base}/kg/tiles/style.json")).2;
    let legend: Vec<(String, Value)> = style["metadata"]["tripletrail:collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_owned(), c["instances"].clone()))
        .collect();
    assert_eq!(
        legend,
        [
            ("lines".to_owned(), Value::from(1)),
            ("Station".to_owned(), Value::from(1)),
            ("depots".to_owned(), Value::Null),
        ]
    );
    // A range facet's unit and colour classes.
    let speed = get(&format!("{base}/kg/tiles/facets/lines")).2["facets"][0].clone();
    assert_eq!(speed["unit"], "km/h", "{speed}");
    assert_eq!(
        speed["classes"],
        json!([{ "upTo": 100.0, "label": "slow" }, { "upTo": null, "label": "fast" }])
    );
}

#[test]
fn datetime_filters_on_the_time_shapes() {
    let dir = tripletrail_testdir::tempdir("ogc-time");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:w1 a ex:Work ; ex:from \"2026-01-01\"^^xsd:date ; ex:to \"2026-03-31\"^^xsd:date ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
               ex:w2 a ex:Work ; ex:from \"2026-06-01\"^^xsd:date ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(2 2)\"^^geo:wktLiteral ] .\n\
               ex:w3 a ex:Work ; geo:hasGeometry [ geo:asWKT \"POINT(3 3)\"^^geo:wktLiteral ] .\n\
               ex:e1 a ex:Event ; ex:at \"2026-02-01T10:00:00Z\"^^xsd:dateTime ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
               ex:e2 a ex:Event ; ex:at \"2026-02-01T12:00:00\"^^xsd:dateTime ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
               ex:w4 a ex:Work ; ex:from \"2026-07-01Z\"^^xsd:date ; ex:to \"2026-07-31Z\"^^xsd:date ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(4 4)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let ogc = format!("{base}/kg/ogc");
    let ids = |collection: &str, datetime: &str| -> Vec<String> {
        let url = format!(
            "{ogc}/collections/{collection}/items?datetime={}",
            form_urlencoded::byte_serialize(datetime.as_bytes()).collect::<String>()
        );
        let (status, _, page) = get(&url);
        assert_eq!(status, 200, "{datetime}: {page}");
        let mut ids: Vec<String> = page["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().replace("http://example.com/", ""))
            .collect();
        ids.sort();
        ids
    };
    // Without tts:time in the shapes, datetime is ignored.
    assert_eq!(ids("Work", "2020-01-01"), ["w1", "w2", "w3", "w4"]);

    let shape = |class: &str, property: &str| {
        format!(
            "<urn:tripletrail:shape:http%3A%2F%2Fexample.com%2F{class}:http%3A%2F%2Fexample.com%2F{property}>"
        )
    };
    let time = format!(
        "@prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
         {} tts:time tts:Time-Start .\n{} tts:time tts:Time-End .\n{} tts:time tts:Time-Instant .\n",
        shape("Work", "from"),
        shape("Work", "to"),
        shape("Event", "at"),
    );
    let shapes = |method: &str, path: &str| {
        ureq::request(method, &format!("{base}/$/datasets/kg/shapes{path}"))
            .set("Authorization", "Bearer t")
    };
    shapes("POST", "/customise").call().unwrap();
    shapes("PUT", "/files/time.ttl").send_string(&time).unwrap();

    // The time span: the earliest start, and an open end while a work has none.
    let work = with_extent(&format!("{ogc}/collections/Work"));
    assert_eq!(
        work["extent"]["temporal"]["interval"],
        serde_json::json!([["2026-01-01T00:00:00Z", null]]),
        "{work}"
    );
    let event = with_extent(&format!("{ogc}/collections/Event"));
    assert_eq!(
        event["extent"]["temporal"]["interval"],
        serde_json::json!([["2026-02-01T10:00:00Z", "2026-02-01T12:00:00Z"]]),
        "{event}"
    );
    // Start and end: the intervals overlap, a date covering its day, no end still open,
    // no time at all left out.
    assert_eq!(ids("Work", "2026-02-01"), ["w1"]);
    assert_eq!(ids("Work", "2026-03-31"), ["w1"]);
    assert_eq!(ids("Work", "2026-03-31T22:00:00Z/.."), ["w1", "w2", "w4"]);
    assert_eq!(ids("Work", "2026-04-01/.."), ["w2", "w4"]);
    assert_eq!(ids("Work", "../2025-12-31"), Vec::<String>::new());
    assert_eq!(ids("Work", "/2026-01-01"), ["w1"]);
    assert_eq!(ids("Work", "2030-01-01"), ["w2"]);
    // An instant: in the interval; a value without a timezone is read as UTC.
    assert_eq!(ids("Event", "2026-02-01"), ["e1", "e2"]);
    assert_eq!(ids("Event", "2026-02-01T11:00:00Z/.."), ["e2"]);
    assert_eq!(ids("Event", "2026-02-01T10:00:00Z"), ["e1"]);
    // A bound without an offset is read as UTC too.
    assert_eq!(ids("Event", "2026-02-01T10:00:00"), ["e1"]);
    assert_eq!(ids("Event", "2026-02-01T11:00:00/.."), ["e2"]);
    // A date with a timezone also ends at the end of its day.
    assert_eq!(ids("Work", "2026-07-31T20:00:00Z"), ["w2", "w4"]);
    // Bad values are refused.
    for bad in ["soon", "../..", "2026-13-01", "2026-01-01/2026-02-30"] {
        let url = format!(
            "{ogc}/collections/Work/items?datetime={}",
            form_urlencoded::byte_serialize(bad.as_bytes()).collect::<String>()
        );
        assert_eq!(get(&url).0, 400, "{bad}");
    }

    // The map takes the same datetime for its tiles and counts (#37), and its style says
    // which collections have a time, and the data version to cite.
    let style = get(&format!("{base}/kg/tiles/style.json")).2;
    let legend = style["metadata"]["tripletrail:collections"]
        .as_array()
        .unwrap()
        .clone();
    let timed = |id: &str| legend.iter().find(|c| c["id"] == id).unwrap()["time"] == true;
    assert!(timed("Work") && timed("Event"), "{style}");
    assert!(
        style["metadata"]["tripletrail:dataVersion"].is_string(),
        "{style}"
    );
    let counted = |collection: &str, datetime: &str| {
        let url = format!(
            "{base}/kg/tiles/facets/{collection}?datetime={}",
            form_urlencoded::byte_serialize(datetime.as_bytes()).collect::<String>()
        );
        let (status, _, out) = get(&url);
        assert_eq!(status, 200, "{datetime}: {out}");
        out["features"].as_u64().unwrap()
    };
    assert_eq!(counted("Work", "2026-04-01/.."), 2);
    assert_eq!(counted("Work", "2026-02-01"), 1);
    assert_eq!(counted("Event", "2026-02-01T11:00:00Z/.."), 1);
    assert_eq!(counted("Work", ""), 4);
    assert_eq!(
        get(&format!("{base}/kg/tiles/facets/Work?datetime=soon")).0,
        400
    );
    // A tile keeps only the features of the time: w1 (1,1) and w2 (2,2) are in tile 0/0/0
    // either way, so compare its bytes with and without the time.
    let tile = |q: &str| {
        let r = ureq::get(&format!("{base}/kg/tiles/Work/0/0/0.mvt{q}"))
            .call()
            .unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut r.into_reader(), &mut bytes).unwrap();
        bytes
    };
    let all = tile("");
    let april = tile("?datetime=2026-04-01%2F..");
    assert!(
        !april.is_empty() && april.len() < all.len(),
        "{} {}",
        april.len(),
        all.len()
    );
    assert_eq!(tile("?datetime=..%2F2025-12-31").len(), 0);

    // JSON-FG (#497): the time of each feature, a conformsTo, and with another CRS the
    // geometry in it as place, beside the CRS84 one.
    let fg = |url: &str| -> (u16, String, Value) {
        let r = ureq::get(url)
            .set("Accept", "application/vnd.ogc.fg+json")
            .call()
            .unwrap();
        let kind = r.content_type().to_owned();
        let status = r.status();
        (
            status,
            kind,
            serde_json::from_str(&r.into_string().unwrap()).unwrap(),
        )
    };
    let (status, kind, page) = fg(&format!("{ogc}/collections/Work/items"));
    assert_eq!(
        (status, kind.as_str()),
        (200, "application/vnd.ogc.fg+json")
    );
    assert_eq!(
        page["conformsTo"],
        serde_json::json!(["http://www.opengis.net/spec/json-fg-1/1.0/conf/core"])
    );
    assert_eq!(page["featureType"], "Work");
    let time = |page: &Value, id: &str| -> Value {
        page["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == format!("http://example.com/{id}"))
            .map(|f| f["time"].clone())
            .unwrap()
    };
    assert_eq!(
        time(&page, "w1"),
        serde_json::json!({ "interval": ["2026-01-01", "2026-03-31"] })
    );
    assert_eq!(
        time(&page, "w2"),
        serde_json::json!({ "interval": ["2026-06-01", ".."] })
    );
    assert_eq!(time(&page, "w3"), Value::Null);
    assert_eq!(page["features"][0]["place"], Value::Null);
    let (_, _, events) = fg(&format!(
        "{ogc}/collections/Event/items?f=jsonfg&crs=http%3A%2F%2Fwww.opengis.net%2Fdef%2Fcrs%2FEPSG%2F0%2F3857"
    ));
    assert_eq!(
        time(&events, "e1"),
        serde_json::json!({ "timestamp": "2026-02-01T10:00:00Z" })
    );
    assert_eq!(
        time(&events, "e2"),
        serde_json::json!({ "timestamp": "2026-02-01T12:00:00Z" })
    );
    assert_eq!(
        events["coordRefSys"],
        "http://www.opengis.net/def/crs/EPSG/0/3857"
    );
    let e = &events["features"][0];
    assert_eq!(
        e["geometry"]["coordinates"],
        serde_json::json!([1, 1]),
        "{e}"
    );
    assert!(
        e["place"]["coordinates"][0].as_f64().unwrap() > 100_000.0,
        "{e}"
    );
    // One feature, by f=jsonfg; plain GeoJSON is unchanged.
    let (status, kind, one) = get(&format!(
        "{ogc}/collections/Event/items/http%3A%2F%2Fexample.com%2Fe1?f=jsonfg"
    ));
    assert_eq!(
        (status, kind.as_str()),
        (200, "application/vnd.ogc.fg+json")
    );
    assert_eq!(
        one["time"],
        serde_json::json!({ "timestamp": "2026-02-01T10:00:00Z" })
    );
    assert_eq!(
        one["conformsTo"][0],
        "http://www.opengis.net/spec/json-fg-1/1.0/conf/core"
    );
    let (_, kind, plain) = get(&format!("{ogc}/collections/Event/items"));
    assert_eq!(kind, "application/geo+json");
    assert!(plain.get("conformsTo").is_none() && plain["features"][0].get("time").is_none());
}

#[test]
fn extents_reproject_and_skip_empty_geometries() {
    // The same places in three CRSs, and an empty geometry: the box is in CRS84 and the
    // empty one does not void it.
    let ttl = "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:a a ex:Site ; geo:hasGeometry [ geo:asWKT \"<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(60 10)\"^^geo:wktLiteral ] .\n\
         ex:b a ex:Site ; geo:hasGeometry [ geo:asWKT \"<http://www.opengis.net/def/crs/EPSG/0/25832> POINT(500000 6651411)\"^^geo:wktLiteral ] .\n\
         ex:c a ex:Site ; geo:hasGeometry [ geo:asWKT \"POINT(11 61)\"^^geo:wktLiteral ] .\n\
         ex:d a ex:Site ; geo:hasGeometry [ geo:asWKT \"POINT EMPTY\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let site = with_extent(&format!("{base}/kg/ogc/collections/Site"));
    let bbox: Vec<f64> = site["extent"]["spatial"]["bbox"][0]
        .as_array()
        .unwrap_or_else(|| panic!("{site}"))
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert!((bbox[0] - 9.0).abs() < 1e-6, "{bbox:?}");
    assert!((bbox[1] - 60.0).abs() < 0.01, "{bbox:?}");
    assert_eq!(bbox[2..], [11.0, 61.0], "{bbox:?}");
}

#[test]
fn collections_follow_writes() {
    // After a write the collections made before it answer while new ones are made in the
    // background; a class the write added shows up soon after.
    let ttl = "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:a a ex:Site ; geo:hasGeometry [ geo:asWKT \"POINT(1 2)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine.clone());
    let url = format!("{base}/kg/ogc/collections");
    let ids = |c: &Value| -> Vec<String> {
        c["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(&get(&url).2), ["Site"]);
    engine
        .update(
            "PREFIX ex: <http://example.com/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
             INSERT DATA { ex:d a ex:Depot ; geo:hasGeometry ex:g . \
             ex:g geo:asWKT \"POINT(3 4)\"^^geo:wktLiteral }",
        )
        .unwrap();
    // At once after the write, while the collections may be made again: whatever these
    // answer must not be kept past the rebuild.
    let world = format!("{base}/kg/tiles/all/0/0/0.mvt");
    assert_eq!(tile(&world).0, 200);
    assert_eq!(get(&format!("{url}/Site")).0, 200);
    let mut seen = Vec::new();
    for _ in 0..500 {
        let (status, _, collections) = get(&url);
        assert_eq!(status, 200);
        seen = ids(&collections);
        if seen.len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    seen.sort();
    assert_eq!(seen, ["Depot", "Site"]);
    let (status, _, depot) = get(&format!("{url}/Depot/items"));
    assert_eq!(status, 200);
    assert_eq!(depot["features"].as_array().unwrap().len(), 1, "{depot}");
    // Once the new collections are in, the tile has the new layer and the new class its extent.
    assert!(holds(&tile(&world).1, "http://example.com/d"));
    let depot = with_extent(&format!("{url}/Depot"));
    assert_eq!(
        depot["extent"]["spatial"]["bbox"],
        json!([[3.0, 4.0, 3.0, 4.0]]),
        "{depot}"
    );
}

#[test]
fn cql2_filters_on_the_queryables() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..6 {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:tracks {} ; ex:open {} ;\n\
             geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n",
            i % 3,
            i % 2 == 0,
        ));
        // The last one has no name.
        if i < 5 {
            ttl.push_str(&format!("ex:s{i} ex:name \"S{i}\" .\n"));
        }
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let ogc = format!("{base}/kg/ogc");

    let (status, kind, queryables) = get(&format!("{ogc}/collections/Station/queryables"));
    assert_eq!((status, kind.as_str()), (200, "application/schema+json"));
    assert_eq!(
        queryables["properties"]["geometry"]["format"],
        "geometry-any"
    );
    assert_eq!(
        queryables["properties"]["tracks"]["type"], "integer",
        "{queryables}"
    );
    assert_eq!(queryables["properties"]["name"]["type"], "string");
    let (_, _, station) = get(&format!("{ogc}/collections/Station"));
    assert!(
        href(
            &station["links"],
            "http://www.opengis.net/def/rel/ogc/1.0/queryables"
        )
        .is_some()
    );
    let (_, _, conformance) = get(&format!("{ogc}/conformance"));
    assert!(conformance.to_string().contains("conf/cql2-text"));
    let (status, kind, html) = page(
        &format!("{ogc}/collections/Station/queryables"),
        "text/html",
    );
    assert_eq!((status, kind.as_str()), (200, "text/html"));
    assert!(html.contains("<code>tracks</code>"), "{html}");

    let ids = |query: &[(&str, &str)]| -> Vec<String> {
        let mut url = format!("{ogc}/collections/Station/items?");
        let mut q = form_urlencoded::Serializer::new(String::new());
        for (k, v) in query {
            q.append_pair(k, v);
        }
        url.push_str(&q.finish());
        let (status, _, page) = get(&url);
        assert_eq!(status, 200, "{query:?}: {page}");
        let mut ids: Vec<String> = page["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                f["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("http://example.com/")
                    .to_owned()
            })
            .collect();
        ids.sort();
        ids
    };
    let text = |f: &str| ids(&[("filter", f)]);
    assert_eq!(text("tracks = 2"), ["s2", "s5"]);
    assert_eq!(text("tracks > 0 AND open"), ["s2", "s4"]);
    assert_eq!(text("NOT tracks = 2 AND tracks <> 0"), ["s1", "s4"]);
    assert_eq!(
        text("name LIKE 'S_' AND tracks BETWEEN 1 AND 1"),
        ["s1", "s4"]
    );
    assert_eq!(
        text("name IN ('S1', 'S3') OR name IS NULL"),
        ["s1", "s3", "s5"]
    );
    assert_eq!(text("CASEI(name) = CASEI('s3')"), ["s3"]);
    // An IN of strings on its own is a join with a VALUES table.
    assert_eq!(text("name IN ('S1', 'S3', 'S9')"), ["s1", "s3"]);
    assert_eq!(
        text("CASEI(name) IN (CASEI('s2'), CASEI('S4')) AND tracks = 1"),
        ["s4"]
    );
    assert_eq!(text("open = false AND \"tracks\" >= 1"), ["s1", "s5"]);
    assert_eq!(
        text("S_INTERSECTS(geometry, BBOX(1.5, 0, 4.5, 2))"),
        ["s2", "s3", "s4"]
    );
    assert_eq!(text("S_INTERSECTS(geometry, POINT(3 1))"), ["s3"]);
    assert_eq!(
        ids(&[
            ("filter", "S_INTERSECTS(geometry, BBOX(0, 1.5, 2, 4.5))"),
            ("filter-crs", "http://www.opengis.net/def/crs/EPSG/0/4326"),
        ]),
        ["s2", "s3", "s4"]
    );
    assert_eq!(
        ids(&[
            ("filter-lang", "cql2-json"),
            (
                "filter",
                r#"{"op":"and","args":[{"op":"s_intersects","args":[{"property":"geometry"},
                   {"type":"Polygon","coordinates":[[[1.5,0],[4.5,0],[4.5,2],[1.5,2],[1.5,0]]]}]},
                   {"op":"=","args":[{"property":"tracks"},0]}]}"#
            ),
        ]),
        ["s3"]
    );
    // Errors: an unknown property, bad syntax, an unsupported function or language.
    let status = |query: &str| get(&format!("{ogc}/collections/Station/items?{query}")).0;
    assert_eq!(status("filter=height%20%3E%201"), 400);
    assert_eq!(status("filter=tracks%20%3D"), 400);
    assert_eq!(status("filter=T_AFTER(tracks%2C%201)"), 400);
    assert_eq!(status("filter=tracks%3D1&filter-lang=ecql"), 400);
    assert_eq!(status("filter-lang=cql2-text"), 400);
    // A filter that nests deep or has many predicates is refused, and the server lives on.
    let encoded = |f: &str| form_urlencoded::byte_serialize(f.as_bytes()).collect::<String>();
    let parens = format!("{}TRUE{}", "(".repeat(5000), ")".repeat(5000));
    assert_eq!(status(&format!("filter={}", encoded(&parens))), 400);
    let nots = format!("{}TRUE", "NOT ".repeat(1000));
    assert_eq!(status(&format!("filter={}", encoded(&nots))), 400);
    let ands = vec!["TRUE"; 5000].join(" AND ");
    assert_eq!(status(&format!("filter={}", encoded(&ands))), 400);
    assert_eq!(text("tracks = 2"), ["s2", "s5"]);
}

/// A string `IN` matches a value whatever form it comes in (a string, a language-tagged
/// string, an IRI), as text (#497), and other features with that value don't count.
#[test]
fn cql2_string_in_matches_each_form_of_the_values() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:s0 a ex:Station ; ex:name \"A\"@nb .\n\
         ex:s1 a ex:Station ; ex:name \"A\"@en , \"B\" .\n\
         ex:s2 a ex:Station ; ex:name \"B\"@nb .\n\
         ex:s3 a ex:Station ; ex:name \"C\" .\n\
         ex:s4 a ex:Station ; ex:name <http://example.com/c> .\n\
         ex:s5 a ex:Station ; ex:name [ ex:x 1 ] .\n\
         ex:o a ex:Other ; ex:name \"A\" .\n";
    let mut ttl = ttl.to_owned();
    for i in 0..6 {
        ttl.push_str(&format!(
            "ex:s{i} geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let text = |f: &str| -> Vec<String> {
        let q: String = form_urlencoded::byte_serialize(f.as_bytes()).collect();
        let (status, _, page) = get(&format!(
            "{base}/kg/ogc/collections/Station/items?filter={q}"
        ));
        assert_eq!(status, 200, "{f}: {page}");
        let mut ids: Vec<String> = page["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                f["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("http://example.com/")
                    .to_owned()
            })
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(text("name IN ('A', 'zz')"), ["s0", "s1"]);
    assert_eq!(text("name IN ('B')"), ["s1", "s2"]);
    assert_eq!(text("name IN ('http://example.com/c', 'C')"), ["s3", "s4"]);
    assert_eq!(text("name IN ('zz', 'y')"), Vec::<String>::new());
    assert_eq!(text("NOT name IN ('A', 'B')"), ["s3", "s4", "s5"]);
    assert_eq!(text("CASEI(name) IN (CASEI('a'))"), ["s0", "s1"]);
}

/// With values of another datatype, a string `IN` compares their text, as before.
#[test]
fn cql2_string_in_compares_the_text_of_other_datatypes() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         ex:s0 a ex:Station ; ex:code \"7\"^^xsd:integer .\n\
         ex:s1 a ex:Station ; ex:code \"07\"^^xsd:integer .\n\
         ex:s2 a ex:Station ; ex:code \"7.0\"^^xsd:decimal .\n\
         ex:s3 a ex:Station ; ex:code \"A\"@nb .\n";
    let mut ttl = ttl.to_owned();
    for i in 0..4 {
        ttl.push_str(&format!(
            "ex:s{i} geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let text = |f: &str| -> Vec<String> {
        let q: String = form_urlencoded::byte_serialize(f.as_bytes()).collect();
        let (status, _, page) = get(&format!(
            "{base}/kg/ogc/collections/Station/items?filter={q}"
        ));
        assert_eq!(status, 200, "{f}: {page}");
        let mut ids: Vec<String> = page["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                f["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("http://example.com/")
                    .to_owned()
            })
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(text("code IN ('7', 'A')"), ["s0", "s3"]);
    assert_eq!(text("code IN ('7.0', '07')"), ["s1", "s2"]);
}

#[test]
fn cql2_temporal_functions() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:e0 a ex:Event ; ex:at \"2026-01-01T10:00:00Z\"^^xsd:dateTime ; geo:hasGeometry [ geo:asWKT \"POINT(0 0)\"^^geo:wktLiteral ] .\n\
         ex:e1 a ex:Event ; ex:at \"2026-02-01T10:00:00\"^^xsd:dateTime ; geo:hasGeometry [ geo:asWKT \"POINT(1 0)\"^^geo:wktLiteral ] .\n\
         ex:e2 a ex:Event ; ex:at \"2026-03-01\"^^xsd:date ; geo:hasGeometry [ geo:asWKT \"POINT(2 0)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let ids = |query: &[(&str, &str)]| -> Vec<String> {
        let mut q = form_urlencoded::Serializer::new(String::new());
        for (k, v) in query {
            q.append_pair(k, v);
        }
        let (status, _, page) = get(&format!(
            "{base}/kg/ogc/collections/Event/items?{}",
            q.finish()
        ));
        assert_eq!(status, 200, "{query:?}: {page}");
        let mut ids: Vec<String> = page["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                f["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("http://example.com/")
                    .to_owned()
            })
            .collect();
        ids.sort();
        ids
    };
    let text = |f: &str| ids(&[("filter", f)]);
    assert_eq!(
        text("T_AFTER(at, TIMESTAMP('2026-01-15T00:00:00Z'))"),
        ["e1", "e2"]
    );
    // A date covers its day: e1 is on 1 February, so not before it.
    assert_eq!(text("T_BEFORE(at, DATE('2026-02-01'))"), ["e0"]);
    assert_eq!(
        text("T_DURING(at, INTERVAL('2026-01-01', '2026-02-28'))"),
        ["e0", "e1"]
    );
    assert_eq!(
        text("T_INTERSECTS(at, INTERVAL('2026-03-01T12:00:00Z', '..'))"),
        ["e2"]
    );
    assert_eq!(text("T_DISJOINT(at, INTERVAL('..', '2026-02-01'))"), ["e2"]);
    assert_eq!(
        text("NOT T_EQUALS(at, TIMESTAMP('2026-01-01T10:00:00Z'))"),
        ["e1", "e2"]
    );
    assert_eq!(
        ids(&[
            ("filter-lang", "cql2-json"),
            (
                "filter",
                r#"{"op":"t_after","args":[{"property":"at"},{"interval":["2026-01-01","2026-02-15T00:00:00Z"]}]}"#
            ),
        ]),
        ["e2"]
    );
    // Meeting (#497): a day ends where the next begins, at midnight.
    assert_eq!(
        text("T_MEETS(INTERVAL('2026-02-01', '2026-02-28'), at)"),
        ["e2"]
    );
    assert_eq!(text("T_METBY(INTERVAL('2026-03-02', '..'), at)"), ["e2"]);
    assert_eq!(
        text("T_MEETS(at, TIMESTAMP('2026-03-02T00:00:00Z'))"),
        ["e2"]
    );
    assert_eq!(
        text("T_MEETS(INTERVAL('2026-01-01', '2026-01-01T10:00:00Z'), at)"),
        ["e0"]
    );
    assert_eq!(
        text("T_MEETS(at, DATE('2026-03-01'))"),
        Vec::<String>::new()
    );
    let status = |f: &str| {
        let q: String = form_urlencoded::byte_serialize(f.as_bytes()).collect();
        get(&format!("{base}/kg/ogc/collections/Event/items?filter={q}")).0
    };
    assert_eq!(status("T_AFTER(at, '..')"), 400);
    assert_eq!(status("T_AFTER(at, 5)"), 400);
    assert_eq!(status("T_AFTER(at, INTERVAL('2026-13-01', '..'))"), 400);
}

#[test]
fn the_map_finds_features_by_name() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
               ex:a a ex:Station ; rdfs:label \"Brussels Central\" ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(4.357 50.845)\"^^geo:wktLiteral ] .\n\
               ex:b a ex:Station ; rdfs:label \"Brussels North (main hall)\" ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(4.36 50.86)\"^^geo:wktLiteral ] .\n\
               ex:c a ex:Station ; rdfs:label \"Antwerp Central\" ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(4.42 51.21)\"^^geo:wktLiteral ] .\n\
               ex:d a ex:Station ; rdfs:label \"Brussels South\" .\n\
               ex:e a ex:Person ; rdfs:label \"Brussels Fan\" .\n\
               ex:g a ex:Station ; rdfs:label \"Gent Sint Pieters\" ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(3.71 51.04)\"^^geo:wktLiteral ] .\n";
    let mut ttl = ttl.to_owned();
    // Better matches without a geometry take none of the five places of their class.
    for i in 0..6 {
        ttl.push_str(&format!("ex:g{i} a ex:Station ; rdfs:label \"Gent\" .\n"));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let find = |q: &str| -> Vec<(String, Value)> {
        let url = format!("{base}/kg/tiles/search?q={}", q.replace(' ', "%20"));
        let (status, body, out) = get(&url);
        assert_eq!(status, 200, "{body}");
        out["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["label"].as_str().unwrap().to_owned(), r["bbox"].clone()))
            .collect()
    };
    // All the words, the last as a prefix, whatever the case and accents; the shortest
    // label first; only features the map draws.
    let found = find("BRUSSELS");
    let labels: Vec<&str> = found.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, ["Brussels Central", "Brussels North (main hall)"]);
    assert_eq!(found[0].1, json!([4.357, 50.845, 4.357, 50.845]));
    let labels: Vec<String> = find("brüssels cen").into_iter().map(|(l, _)| l).collect();
    assert_eq!(labels, ["Brussels Central"]);
    assert_eq!(find("central").len(), 2);
    // Nothing to search for, or only the search's and SPARQL's own syntax.
    assert!(find("").is_empty());
    assert!(find("%22%5C%22*").is_empty());
    assert!(find("nowhere").is_empty());
    let labels: Vec<String> = find("gent").into_iter().map(|(l, _)| l).collect();
    assert_eq!(labels, ["Gent Sint Pieters"]);
}

/// With a base IRI, a feature's id is its IRI's local part (#497); the whole IRI still finds
/// it, and an IRI outside the base keeps its whole IRI as its id.
#[test]
fn feature_ids_are_local_under_the_base_iri() {
    let ttl = "@prefix ex: <https://data.example.org/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:s1 a ex:Station ; ex:name \"S1\" ; geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
         <http://other.example.com/s2> a ex:Station ; ex:name \"S2\" ; \
         geo:hasGeometry [ geo:asWKT \"POINT(2 1)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            base_iris: vec![("https://data.example.org/".to_owned(), "kg".to_owned())],
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let items = format!("{base}/kg/ogc/collections/Station/items");
    let (status, _, page) = get(&format!("{items}?f=json"));
    assert_eq!(status, 200, "{page}");
    let mut ids: Vec<(String, String)> = page["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["id"].as_str().unwrap().to_owned(),
                f["links"][0]["href"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        [
            (
                "http://other.example.com/s2".to_owned(),
                format!("{items}/http%3A%2F%2Fother.example.com%2Fs2")
            ),
            ("s1".to_owned(), format!("{items}/s1")),
        ]
    );
    for url in [
        format!("{items}/s1"),
        format!("{items}/https%3A%2F%2Fdata.example.org%2Fs1"),
    ] {
        let (status, _, feature) = get(&format!("{url}?f=json"));
        assert_eq!(status, 200, "{url}: {feature}");
        assert_eq!(feature["id"], "s1");
        assert_eq!(feature["properties"]["name"], "S1");
    }
    let (status, _, feature) = get(&format!(
        "{items}/http%3A%2F%2Fother.example.com%2Fs2?f=json"
    ));
    assert_eq!(
        (status, feature["id"].as_str()),
        (200, Some("http://other.example.com/s2"))
    );
    assert_eq!(get(&format!("{items}/s9?f=json")).0, 404);
}

fn post(url: &str, kind: &str, body: &str) -> (u16, String) {
    match ureq::post(url).set("Content-Type", kind).send_string(body) {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn proposed_shapes_are_previewed_on_the_map_without_saving_them() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
               ex:s1 a ex:Station ; rdfs:label \"Leuven\" ; geo:hasGeometry [ geo:asWKT \"POINT(4.35 50.85)\"^^geo:wktLiteral ] .\n\
               ex:l1 a ex:Line ; geo:hasGeometry [ geo:asWKT \"LINESTRING(4.3 50.8, 4.4 50.9)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let ids = |style: &Value| -> Vec<(String, String)> {
        style["metadata"]["tripletrail:collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["id"].as_str().unwrap().to_owned(),
                    c["color"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let shape = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                 @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                 @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
                 <http://example.com/shapes#Stop> a sh:NodeShape ;\n\
                   sh:targetClass <http://example.com/Station> ;\n\
                   tts:key \"Stop\" ; tts:color \"#000000\" ;\n\
                   sh:property [ sh:path <http://www.w3.org/2000/01/rdf-schema#label> ; sh:name \"Halt\" ] ;\n\
                   tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] .\n";
    // As JSON files, or as one Turtle body.
    let body = json!({"files": [{"name": "stop.ttl", "text": shape}]}).to_string();
    let (status, made) = post(
        &format!("{base}/kg/tiles/preview"),
        "application/json",
        &body,
    );
    assert_eq!(status, 201, "{made}");
    let made: Value = serde_json::from_str(&made).unwrap();
    let id = made["id"].as_str().unwrap();
    assert_eq!(id.len(), 32);
    let preview = format!("{base}/kg/tiles/preview/{id}");
    assert_eq!(made["style"], format!("{preview}/style.json"));
    let (status, _, style) = get(&format!("{preview}/style.json"));
    assert_eq!(status, 200);
    let collections = ids(&style);
    assert!(
        collections.contains(&("Stop".into(), "#000000".into())),
        "{collections:?}"
    );
    assert!(
        !collections.iter().any(|(c, _)| c == "Station"),
        "{collections:?}"
    );
    // Its tiles, search and facets use the proposed collections.
    assert_eq!(
        style["sources"]["tripletrail"]["tiles"][0],
        format!("{preview}/all/{{z}}/{{x}}/{{y}}.mvt")
    );
    let (status, stop) = tile(&format!("{preview}/Stop/0/0/0.mvt"));
    assert_eq!(status, 200);
    assert!(holds(&stop, "Stop") && holds(&stop, "http://example.com/s1"));
    let (_, _, found) = get(&format!("{preview}/search?q=leu"));
    assert_eq!(found["results"][0]["collection"], "Stop", "{found}");
    let (status, _, facets) = get(&format!("{preview}/facets/Stop"));
    assert_eq!((status, facets["features"].clone()), (200, json!(1)));
    // A describe page takes the preview's shapes with preview=; an unknown one is a 404.
    let page = |query: &str| match ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2Fs1{query}"
    ))
    .set("Accept", "text/html")
    .call()
    {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, _)) => (code, String::new()),
        Err(e) => panic!("{e}"),
    };
    let (status, html) = page(&format!("&preview={id}"));
    assert_eq!(status, 200);
    assert!(html.contains("Halt"), "{html}");
    assert!(!page("").1.contains("Halt"));
    assert_eq!(page(&format!("&preview={}", "0".repeat(32))).0, 404);
    // So do search and the REST API, for reading only.
    let text = |url: &str| match ureq::get(url).call() {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("{e}"),
    };
    for path in ["search/classes", "api/v1", "api/openapi.json"] {
        let (status, body) = text(&format!("{base}/kg/{path}?preview={id}"));
        assert_eq!(status, 200, "{path}: {body}");
        assert!(body.contains("Stop"), "{path}: {body}");
        assert!(
            !text(&format!("{base}/kg/{path}")).1.contains("\"Stop"),
            "{path}"
        );
    }
    assert_eq!(
        text(&format!(
            "{base}/kg/search/classes?preview={}",
            "0".repeat(32)
        ))
        .0,
        404
    );
    let write = ureq::post(&format!("{base}/kg/api/v1/Stop?preview={id}"))
        .set("Content-Type", "application/json")
        .send_string("{}");
    assert!(
        matches!(write, Err(ureq::Error::Status(400, _))),
        "{write:?}"
    );
    // The dataset's own map is unchanged.
    let (_, _, own) = get(&format!("{base}/kg/tiles/style.json"));
    let own: Vec<String> = ids(&own).into_iter().map(|(c, _)| c).collect();
    assert_eq!(own, ["Line", "Station"]);
    assert_eq!(get(&format!("{base}/kg/tiles/Stop/0/0/0.mvt")).0, 404);
    // Turtle too; and files that do not load are refused, with what is wrong.
    let (status, _) = post(&format!("{base}/kg/tiles/preview"), "text/turtle", shape);
    assert_eq!(status, 201);
    let (status, said) = post(
        &format!("{base}/kg/tiles/preview"),
        "text/turtle",
        "not turtle",
    );
    assert_eq!(status, 422, "{said}");
    assert!(said.contains("preview.ttl"), "{said}");
    let (status, _) = post(
        &format!("{base}/kg/tiles/preview"),
        "application/json",
        "{}",
    );
    assert_eq!(status, 400);
    // An unknown or another dataset's preview is a 404.
    assert_eq!(
        get(&format!(
            "{base}/kg/tiles/preview/{}/style.json",
            "0".repeat(32)
        ))
        .0,
        404
    );
}

/// A collection whose id is "preview" keeps its tiles: `preview/{z}/{x}/{y}` is not read as a
/// preview's id.
#[test]
fn a_collection_called_preview_keeps_its_tiles() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:p1 a ex:preview ; geo:hasGeometry [ geo:asWKT \"POINT(4.35 50.85)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let (status, _, style) = get(&format!("{base}/kg/tiles/style.json"));
    assert_eq!(status, 200);
    assert!(
        style["metadata"]["tripletrail:collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "preview"),
        "{style}"
    );
    let (status, world) = tile(&format!("{base}/kg/tiles/preview/0/0/0.mvt"));
    assert_eq!(status, 200);
    assert!(holds(&world, "http://example.com/p1"));
    // A preview id that is not there is still a 404.
    let (status, _) = tile(&format!(
        "{base}/kg/tiles/preview/{}/style.json",
        "0".repeat(32)
    ));
    assert_eq!(status, 404);
}

#[test]
fn a_line_between_two_linked_features() {
    let dir = tripletrail_testdir::tempdir("ogc-line");
    // Two sections between the same stations: the first has no geometry of its own.
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:a a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(4.3 50.8)\"^^geo:wktLiteral ] .\n\
               ex:b a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(4.5 50.8)\"^^geo:wktLiteral ] .\n\
               ex:straight a ex:Section ; ex:from ex:a ; ex:to ex:b .\n\
               ex:drawn a ex:Section ; ex:from ex:a ; ex:to ex:b ;\n\
                 geo:hasGeometry [ geo:asWKT \"LINESTRING(4.3 50.9, 4.5 50.9)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let section = "@prefix ex: <http://example.com/> .\n\
                   @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
                   @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                   @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                   ex:SectionShape a sh:NodeShape ; sh:targetClass ex:Section ; tts:key \"section\" ;\n\
                     tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ; sh:order 0 ] ,\n\
                       [ tts:lineBetween ( ( ex:from geo:hasGeometry geo:asWKT ) ( ex:to geo:hasGeometry geo:asWKT ) ) ; sh:order 1 ] .\n";
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Section.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(section)
        .unwrap();
    let items = format!("{base}/kg/ogc/collections/section/items?f=json");
    let (status, _, page) = get(&items);
    assert_eq!(status, 200, "{page}");
    let geometry = |page: &Value, id: &str| -> Value {
        page["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"].as_str().unwrap().ends_with(id))
            .map(|f| f["geometry"].clone())
            .unwrap_or(Value::Null)
    };
    // The section without a geometry is the line between its stations; the other keeps its own.
    assert_eq!(
        geometry(&page, "straight"),
        json!({ "type": "LineString", "coordinates": [[4.3, 50.8], [4.5, 50.8]] }),
        "{page}"
    );
    assert_eq!(
        geometry(&page, "drawn")["coordinates"],
        json!([[4.3, 50.9], [4.5, 50.9]]),
        "{page}"
    );
    // A box between the stations meets only the line between them: first while the lines'
    // boxes are read (the line way is read whole), then from the boxes.
    for _ in 0..2 {
        let (_, _, boxed) = get(&format!("{items}&bbox=4.39,50.79,4.41,50.81"));
        assert_eq!(boxed["features"].as_array().unwrap().len(), 1, "{boxed}");
        assert!(!geometry(&boxed, "straight").is_null(), "{boxed}");
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // A box that meets no line and no geometry finds nothing.
    let (_, _, none) = get(&format!("{items}&bbox=10,10,11,11"));
    assert_eq!(none["features"].as_array().unwrap().len(), 0, "{none}");
    // A write makes the lines' boxes again: a new section in the box is found.
    ureq::post(&format!("{base}/kg/update"))
        .set("Authorization", "Bearer t")
        .set("Content-Type", "application/sparql-update")
        .send_string(
            "PREFIX ex: <http://example.com/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
             INSERT DATA { ex:c a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(10.2 10.5)\"^^geo:wktLiteral ] . \
             ex:d a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(10.8 10.5)\"^^geo:wktLiteral ] . \
             ex:later a ex:Section ; ex:from ex:c ; ex:to ex:d . }",
        )
        .unwrap();
    let (_, _, later) = get(&format!("{items}&bbox=10.4,10.4,10.6,10.6"));
    assert_eq!(later["features"].as_array().unwrap().len(), 1, "{later}");
    assert!(!geometry(&later, "later").is_null(), "{later}");
    // The map's tiles draw it, and the REST API's GeoJSON has it.
    assert!(holds(
        &tile(&format!("{base}/kg/tiles/section/0/0/0.mvt")).1,
        "http://example.com/straight"
    ));
    let rest: Value = serde_json::from_str(
        &ureq::get(&format!("{base}/kg/api/v1/section"))
            .query("f", "geojson")
            .query("bbox", "4.39,50.79,4.41,50.81")
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(geometry(&rest, "straight")["type"], "LineString", "{rest}");
    assert_eq!(rest["features"].as_array().unwrap().len(), 1, "{rest}");
}

/// Map views (`tts:MapView`) in the map's style: the collections each shows, and the filters
/// and colour it sets on the facets they have; a switched-off view is left out.
#[test]
fn map_views_in_the_style() {
    let dir = tripletrail_testdir::tempdir("ogc-views");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:a a ex:Station ; ex:kind ex:Hub ; ex:tracks 6 ; geo:hasGeometry [ geo:asWKT \"POINT(4.3 50.8)\"^^geo:wktLiteral ] .\n\
               ex:b a ex:Station ; ex:kind ex:Halt ; ex:tracks 2 ; geo:hasGeometry [ geo:asWKT \"POINT(4.5 50.8)\"^^geo:wktLiteral ] .\n\
               ex:l a ex:Line ; geo:hasGeometry [ geo:asWKT \"LINESTRING(4.3 50.9, 4.5 50.9)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let shapes = "@prefix ex: <http://example.com/> .\n\
                  @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
                  @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                  @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
                  @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                  @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                  ex:StationShape a sh:NodeShape ; sh:targetClass ex:Station ; tts:key \"station\" ;\n\
                    tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ; sh:order 0 ] ;\n\
                    sh:property [ sh:path ex:kind ; tts:key \"kind\" ; tts:facet tts:Facet-Values ] ,\n\
                      [ sh:path ex:tracks ; tts:key \"tracks\" ; tts:facet tts:Facet-Range ; sh:datatype xsd:integer ] .\n\
                  ex:big a tts:MapView ; tts:key \"big\" ; rdfs:label \"Big stations\" ;\n\
                    rdfs:comment \"Stations with four tracks or more.\" ; sh:order 1 ;\n\
                    tts:showsClass ex:Station, ex:Nowhere ; tts:colorFacet \"kind\" ;\n\
                    tts:listsFacets ( \"tracks\" \"missing\" \"kind\" ) ;\n\
                    tts:filter [ tts:key \"tracks\" ; sh:minInclusive 4 ] , [ tts:key \"kind\" ; sh:in ( ex:Hub ) ] ,\n\
                      [ tts:key \"missing\" ; sh:in ( \"x\" ) ] .\n\
                  ex:off a tts:MapView ; tts:key \"off\" ; sh:deactivated true ; tts:showsClass ex:Station .\n";
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Station.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(shapes)
        .unwrap();
    let (status, _, style) = get(&format!("{base}/kg/tiles/style.json"));
    assert_eq!(status, 200, "{style}");
    assert_eq!(
        style["metadata"]["tripletrail:views"],
        json!([{
            "key": "big",
            "title": "Big stations",
            "description": "Stations with four tracks or more.",
            "collections": ["station"],
            "filters": { "station": { "tracks": { "min": "4", "max": "" }, "kind": ["http://example.com/Hub"] } },
            "colors": { "station": "kind" },
            "facets": { "station": ["tracks", "kind"] },
        }]),
        "{style}"
    );
    // The view's filters as the map sends them select the big station.
    let (status, _, facets) = get(&format!(
        "{base}/kg/tiles/facets/station?station.tracks.min=4&station.kind=http%3A%2F%2Fexample.com%2FHub"
    ));
    assert_eq!(status, 200, "{facets}");
    assert_eq!(facets["features"], 1, "{facets}");
}

/// A facet computed from the data (`tts:select`): counted, filtered on and coloured by on the
/// map, and no property of the OGC API's features.
#[test]
fn a_slow_computed_facet_is_not_counted_again_until_a_write() {
    let dir = tripletrail_testdir::tempdir("ogc-slow");
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..10 {
        ttl.push_str(&format!(
            "ex:t{i} a ex:Track ; ex:speed {i} ; \
             geo:hasGeometry [ geo:asWKT \"POINT(4.{i} 50.8)\"^^geo:wktLiteral ] .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            timeout: Some(Duration::from_secs(1)),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    // Every feature joins all the triples six times over: far longer than a second.
    let shapes = r#"@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .
ex:TrackShape a sh:NodeShape ; sh:targetClass ex:Track ; tts:key "track" ;
  tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ; sh:order 0 ] ;
  sh:property [ sh:path ex:speed ; tts:key "speed" ; tts:facet tts:Facet-Values ] ,
    [ sh:path ex:speed ; tts:key "slow" ; tts:facet tts:Facet-Values ;
      tts:select """$this ?p ?o . ?a ?b ?c . ?d ?e ?g . ?h ?i ?j . ?k ?l ?m . ?n ?q ?r .
        BIND(?o AS ?value)""" ] .
"#;
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Track.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(shapes)
        .unwrap();
    let facets = || -> (Value, Duration) {
        let started = std::time::Instant::now();
        let (status, _, facets) = get(&format!("{base}/kg/tiles/facets/track"));
        assert_eq!(status, 200, "{facets}");
        let slow = facets["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "slow")
            .unwrap()
            .clone();
        assert_eq!(facets["features"], 10, "{facets}");
        (slow, started.elapsed())
    };
    let (slow, took) = facets();
    assert!(
        slow["error"].as_str().unwrap().contains("timeout"),
        "{slow}"
    );
    assert!(took >= Duration::from_secs(1), "{took:?}");
    // Remembered: answered at once.
    let (again, took) = facets();
    assert_eq!(again, slow);
    assert!(took < Duration::from_millis(900), "{took:?}");
    // A write: counted again (and timed out again).
    ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <urn:x> <urn:p> 1 }")
        .unwrap();
    let mut counted = false;
    for _ in 0..5 {
        let (after, took) = facets();
        assert!(after["error"].is_string(), "{after}");
        if took >= Duration::from_secs(1) {
            counted = true;
            break;
        }
    }
    assert!(counted, "the write did not count the facet again");
}

#[test]
fn a_filter_on_a_slow_computed_facet_is_capped_too() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..10 {
        ttl.push_str(&format!(
            "ex:t{i} a ex:Track ; ex:speed {i} ; \
             geo:hasGeometry [ geo:asWKT \"POINT(4.{i} 50.8)\"^^geo:wktLiteral ] .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let dir = tripletrail_testdir::tempdir("ogc-slowf");
    // No timeout of the server's own: only the computed facets' cap ends the count.
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let shapes = r#"@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .
ex:TrackShape a sh:NodeShape ; sh:targetClass ex:Track ; tts:key "track" ;
  tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ; sh:order 0 ] ;
  sh:property [ sh:path ex:speed ; tts:key "slow" ; tts:facet tts:Facet-Values ;
      tts:select """$this ?p ?o . ?a ?b ?c . ?d ?e ?g . ?h ?i ?j . ?k ?l ?m . ?n ?q ?r . ?s ?t ?u .
        BIND(?o AS ?value)""" ] .
"#;
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Track.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(shapes)
        .unwrap();
    let started = std::time::Instant::now();
    let (status, _, body) = get(&format!("{base}/kg/tiles/facets/track?slow=1"));
    let took = started.elapsed();
    assert_eq!(status, 503, "{body}");
    assert!(took < Duration::from_secs(20), "{took:?}");
    // A tile coloured by it too.
    let started = std::time::Instant::now();
    let (status, _) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt?color.track=slow"));
    let took = started.elapsed();
    assert_eq!(status, 503);
    assert!(took < Duration::from_secs(20), "{took:?}");
}

#[test]
fn a_computed_facet_on_the_map() {
    let dir = tripletrail_testdir::tempdir("ogc-select");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:a a ex:Track ; ex:speed 200 ; geo:hasGeometry [ geo:asWKT \"POINT(4.3 50.8)\"^^geo:wktLiteral ] .\n\
               ex:b a ex:Track ; ex:speed 100 ; geo:hasGeometry [ geo:asWKT \"POINT(4.4 50.8)\"^^geo:wktLiteral ] .\n\
               ex:c a ex:Track ; geo:hasGeometry [ geo:asWKT \"POINT(4.5 50.8)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let shapes = r#"@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .
ex:TrackShape a sh:NodeShape ; sh:targetClass ex:Track ; tts:key "track" ;
  tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ; sh:order 0 ] ;
  sh:property [ sh:path ex:speed ; tts:key "speed" ; tts:facet tts:Facet-Range ] ,
    [ sh:path ex:speed ; tts:key "fast" ; tts:facet tts:Facet-Values ;
      tts:select """OPTIONAL { $this <http://example.com/speed> ?s }
        BIND(IF(!BOUND(?s), <http://example.com/NoData>,
          IF(?s >= 160, <http://example.com/Meets>, <http://example.com/DoesNotMeet>)) AS ?value)""" ] .
"#;
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Track.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(shapes)
        .unwrap();
    let values = |query: &str| -> Value {
        let (status, _, facets) = get(&format!("{base}/kg/tiles/facets/track{query}"));
        assert_eq!(status, 200, "{facets}");
        facets
    };
    let all = values("");
    let fast = all["facets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "fast")
        .unwrap();
    let mut counted: Vec<(String, i64)> = fast["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["value"].as_str().unwrap().to_owned(),
                v["count"].as_i64().unwrap(),
            )
        })
        .collect();
    counted.sort();
    assert_eq!(
        counted,
        [
            ("http://example.com/DoesNotMeet".to_owned(), 1),
            ("http://example.com/Meets".to_owned(), 1),
            ("http://example.com/NoData".to_owned(), 1),
        ],
        "{all}"
    );
    // Filtered on, with another filter: the variables of the pattern meet nothing outside it.
    let met = values("?track.fast=http%3A%2F%2Fexample.com%2FMeets&track.speed.min=150");
    assert_eq!(met["features"], 1, "{met}");
    let none = values("?track.fast=http%3A%2F%2Fexample.com%2FMeets&track.speed.max=150");
    assert_eq!(none["features"], 0, "{none}");
    // A tile coloured by it.
    let tile = ureq::get(&format!("{base}/kg/tiles/all/0/0/0.mvt?color.track=fast"))
        .call()
        .unwrap();
    assert_eq!(tile.status(), 200);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut tile.into_reader(), &mut bytes).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("http://example.com/Meets") && text.contains("http://example.com/NoData")
    );
    // No property of the OGC API's features, nor a filter there.
    let (_, _, items) = get(&format!("{base}/kg/ogc/collections/track/items?f=json"));
    let properties = &items["features"][0]["properties"];
    assert!(properties.get("fast").is_none(), "{items}");
    assert_eq!(properties["speed"], 200, "{items}");
}

/// Facets and tiles narrowed to a map area (`bbox`), to named graphs (`graph`) and to a text
/// (`q`), the same filters for both (#92).
#[test]
fn facets_and_tiles_by_area_graph_and_text() {
    let mut trig = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n",
    );
    // Stations 0-3 in Belgium (lon 4.x) in graph be, 4-5 in Norway (lon 10.x) in graph no.
    for i in 0..6 {
        let (graph, lon, name) = if i < 4 {
            (
                "be",
                4.0,
                [
                    "Bruxelles Midi",
                    "Gent Sint-Pieters",
                    "Brugge",
                    "Bruxelles Nord",
                ][i],
            )
        } else {
            ("no", 10.0, ["Oslo S", "Bergen"][i - 4])
        };
        let status = if i % 2 == 0 { "open" } else { "closed" };
        trig.push_str(&format!(
            "ex:{graph} {{ ex:s{i} a ex:Station ; ex:status ex:{status} ; rdfs:label \"{name}\" ;\n\
             \x20 geo:hasGeometry [ geo:asWKT \"POINT({} 50.8)\"^^geo:wktLiteral ] . }}\n",
            lon + f64::from(i as u32) / 10.0
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.set_union_default_graph(true);
    engine.load(RdfFormat::TriG, trig.as_bytes()).unwrap();
    let base = start(engine);
    let facets = |query: &str| get(&format!("{base}/kg/tiles/facets/Station{query}")).2;
    assert_eq!(facets("")["features"], 6);
    // The map area: Belgium's box keeps 0-3.
    let area = facets("?bbox=3,50,5,51");
    assert_eq!(area["features"], 4, "{area}");
    let status = |out: &Value| -> Vec<u64> {
        out["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "status")
            .unwrap()["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["count"].as_u64().unwrap())
            .collect()
    };
    assert_eq!(status(&area), [2, 2]);
    // A named graph, or several.
    assert_eq!(
        facets("?graph=http%3A%2F%2Fexample.com%2Fno")["features"],
        2
    );
    assert_eq!(
        facets("?graph=http%3A%2F%2Fexample.com%2Fno&graph=http%3A%2F%2Fexample.com%2Fbe")["features"],
        6
    );
    assert_eq!(
        get(&format!(
            "{base}/kg/tiles/facets/Station?graph=not%20an%20iri"
        ))
        .0,
        400
    );
    assert_eq!(
        get(&format!("{base}/kg/tiles/facets/Station?bbox=1,2")).0,
        400
    );
    // A text: every word, the last as a prefix, accents and case aside.
    assert_eq!(facets("?q=bruxelles")["features"], 2);
    assert_eq!(facets("?q=BRUXELLES%20mi")["features"], 1);
    assert_eq!(
        facets("?q=bruxelles&status=http%3A%2F%2Fexample.com%2Fopen")["features"],
        1
    );
    // All three together, with a facet.
    assert_eq!(
        facets("?q=bruxelles&bbox=3,50,5,51&graph=http%3A%2F%2Fexample.com%2Fbe")["features"],
        2
    );
    assert_eq!(facets("?q=oslo&bbox=3,50,5,51")["features"], 0);
    // Tiles keep the same features, on the collection's tile and on `all`, each filter kept
    // apart in the cache.
    let (_, every) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt"));
    assert!(holds(&every, "http://example.com/s0") && holds(&every, "http://example.com/s5"));
    let (_, norway) = tile(&format!(
        "{base}/kg/tiles/Station/0/0/0.mvt?graph=http%3A%2F%2Fexample.com%2Fno"
    ));
    assert!(holds(&norway, "http://example.com/s4") && !holds(&norway, "http://example.com/s0"));
    let (_, gent) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt?q=gent"));
    assert!(holds(&gent, "http://example.com/s1") && !holds(&gent, "http://example.com/s0"));
    let (_, area) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt?bbox=9,50,12,51"));
    assert!(holds(&area, "http://example.com/s5") && !holds(&area, "http://example.com/s3"));
    let (_, again) = tile(&format!("{base}/kg/tiles/all/0/0/0.mvt"));
    assert_eq!(again, every);
}

/// Facets of several collections in one request (#92): each one's counts, and the facets they
/// share summed over them, under the same filters.
#[test]
fn facets_of_several_collections_at_once() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    // Four stations and six sections, both with a country and a length; stations a kind.
    for i in 0..4 {
        let country = if i < 3 { "BE" } else { "NL" };
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:country ex:{country} ; ex:length {} ; ex:kind ex:halt ;\n\
             \x20 ex:opened \"2020-0{}-01\"^^xsd:date ;\n\
             \x20 geo:hasGeometry [ geo:asWKT \"POINT(4.{i} 50.8)\"^^geo:wktLiteral ] .\n",
            i * 10,
            i + 2
        ));
    }
    for i in 0..6 {
        let country = if i < 1 { "BE" } else { "NL" };
        ttl.push_str(&format!(
            "ex:l{i} a ex:Section ; ex:country ex:{country} ; ex:length {} ;\n\
             \x20 ex:opened \"2019-0{m}-01\"^^xsd:date ;\n\
             \x20 geo:hasGeometry [ geo:asWKT \"LINESTRING(4.{i} 50.8, 4.{i} 50.9)\"^^geo:wktLiteral ] .\n",
            100 + i * 50,
            m = i + 1
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let facets = |query: &str| get(&format!("{base}/kg/tiles/facets?{query}")).2;
    let both = facets("collection=Station&collection=Section");
    assert_eq!(both["features"], 10, "{both}");
    let ids: Vec<&str> = both["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["collection"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["Station", "Section"]);
    assert_eq!(both["collections"][1]["features"], 6);
    let shared = |out: &Value, key: &str| -> Value {
        out["shared"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == key)
            .cloned()
            .unwrap_or(Value::Null)
    };
    // Country: summed, most first.
    let country = shared(&both, "country");
    assert_eq!(
        country["values"],
        json!([
            { "value": "http://example.com/NL", "label": null, "count": 6 },
            { "value": "http://example.com/BE", "label": null, "count": 4 }
        ]),
        "{both}"
    );
    assert_eq!(country["collections"], json!(["Station", "Section"]));
    // Length: one span over both.
    let length = shared(&both, "length");
    assert_eq!(
        (
            length["min"].as_f64(),
            length["max"].as_f64(),
            length["count"].as_u64()
        ),
        (Some(0.0), Some(350.0), Some(10))
    );
    // Dates: one span over both, compared as dates (stations 2020-02..05, sections 2019-01..06).
    let opened = shared(&both, "opened");
    assert_eq!(
        (
            opened["min"].as_str(),
            opened["max"].as_str(),
            opened["count"].as_u64()
        ),
        (Some("2019-01-01"), Some("2020-05-01"), Some(10)),
        "{both}"
    );
    // A facet of one collection only is in its own answer, not shared.
    assert!(shared(&both, "kind").is_null());
    // A bare filter applies to every collection with that facet.
    let nl = facets("collection=Station&collection=Section&country=http%3A%2F%2Fexample.com%2FNL");
    assert_eq!(nl["features"], 6);
    // One collection's own filter, to it alone.
    let mixed = facets(
        "collection=Station&collection=Section&Section.country=http%3A%2F%2Fexample.com%2FBE",
    );
    assert_eq!(
        (
            mixed["collections"][0]["features"].as_u64(),
            mixed["collections"][1]["features"].as_u64()
        ),
        (Some(4), Some(1))
    );
    // And the scope filters (#92) too.
    assert_eq!(
        facets("collection=Station&collection=Section&bbox=4.05,50,4.15,51")["features"],
        2
    );
    assert_eq!(get(&format!("{base}/kg/tiles/facets")).0, 400);
    assert_eq!(
        get(&format!("{base}/kg/tiles/facets?collection=Nope")).0,
        404
    );
}

/// A set that shows only its own classes on the map (#92): `tts:hidesGenerated` keeps the
/// generated shapes it does not take over from the map, and from no other reader.
#[test]
fn a_set_can_hide_the_generated_shapes_from_the_map() {
    let dir = tripletrail_testdir::tempdir("ogc-hides-generated");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:s1 a ex:Station ; ex:name \"A\" ; geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
               ex:l1 a ex:Line ; geo:hasGeometry [ geo:asWKT \"LINESTRING(1 1, 2 2)\"^^geo:wktLiteral ] .\n\
               ex:d1 a ex:Depot ; geo:hasGeometry [ geo:asWKT \"POINT(3 3)\"^^geo:wktLiteral ] .\n\
               ex:r1 a ex:Ref ; geo:hasGeometry [ geo:asWKT \"POINT(4 4)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let on_map = || -> Vec<String> {
        let style = get(&format!("{base}/kg/tiles/style.json")).2;
        let mut ids: Vec<String> = style["metadata"]["tripletrail:collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    };
    let in_ogc = || -> Vec<String> {
        let page = get(&format!("{base}/kg/ogc/collections")).2;
        let mut ids: Vec<String> = page["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(on_map(), ["Depot", "Line", "Ref", "Station"]);
    // Its own shape for the stations, one refining the generated depots' shape, and the
    // generated lines and references only from the OGC API.
    let own = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
               @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               <urn:my:set> a tts:ShapeSet ; tts:hidesGenerated tts:Reader-Map .\n\
               <urn:my:stations> a sh:NodeShape ; sh:targetClass <http://example.com/Station> ;\n\
                 tts:key \"stations\" ; tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] .\n\
               <urn:my:depots> a sh:NodeShape ; sh:targetClass <http://example.com/Depot> ;\n\
                 tts:refines true ; tts:color \"#112233\" .\n";
    let put = |text: &str| -> (u16, String) {
        match ureq::put(&format!("{base}/$/datasets/kg/shapes/files/mine.ttl"))
            .set("Authorization", "Bearer t")
            .send_string(text)
        {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(put(own).0, 200);
    assert_eq!(on_map(), ["Depot", "stations"]);
    assert_eq!(in_ogc(), ["Depot", "Line", "Ref", "stations"]);
    // A pack may say it too; the own set is emptied first.
    ureq::delete(&format!("{base}/$/datasets/kg/shapes/files/mine.ttl"))
        .set("Authorization", "Bearer t")
        .call()
        .unwrap();
    assert_eq!(on_map(), ["Depot", "Line", "Ref", "Station"]);
    let pack = json!({ "name": "p", "version": "1", "files": {
        "_set.ttl": "@prefix tts: <http://w3id.org/tripletrail/shapes#> . <urn:p> a tts:ShapeSet ; tts:hidesGenerated tts:Reader-Map .",
        "Station.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . <urn:p:Station> a sh:NodeShape ; sh:targetClass <http://example.com/Station> ; tts:refines true ; tts:color \"#d62728\" ."
    } });
    let r = ureq::put(&format!("{base}/$/datasets/kg/shapes/pack"))
        .set("Authorization", "Bearer t")
        .send_string(&pack.to_string())
        .unwrap();
    assert_eq!(r.status(), 200);
    // The map follows the pack at once, the refined stations alone.
    assert_eq!(on_map(), ["Station"]);
    assert_eq!(in_ogc(), ["Depot", "Line", "Ref", "Station"]);
    // Only readers are named.
    let wrong = own.replace("tts:Reader-Map", "tts:Map");
    let (code, body) = put(&wrong);
    assert_eq!(code, 422, "{body}");
    assert!(body.contains("is not a reader"), "{body}");
}

/// The legend of a map coloured by a range (#92): each colour class with the features drawn
/// in it under the filters, by each feature's highest number, as the tiles colour them.
#[test]
fn legend_counts_of_a_map_coloured_by_a_range() {
    let dir = tripletrail_testdir::tempdir("ogc-legend");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:l1 a ex:Line ; ex:speed 50 ; ex:len 10 ; geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
               ex:l2 a ex:Line ; ex:speed 100 ; ex:len 120 ; geo:hasGeometry [ geo:asWKT \"POINT(2 2)\"^^geo:wktLiteral ] .\n\
               ex:l3 a ex:Line ; ex:speed 120 ; ex:len 250 ; geo:hasGeometry [ geo:asWKT \"POINT(3 3)\"^^geo:wktLiteral ] .\n\
               ex:l4 a ex:Line ; ex:speed 200 ; ex:len 350 ; geo:hasGeometry [ geo:asWKT \"POINT(4 4)\"^^geo:wktLiteral ] .\n\
               ex:l5 a ex:Line ; ex:speed 80, 150 ; ex:len 200 ; geo:hasGeometry [ geo:asWKT \"POINT(5 5)\"^^geo:wktLiteral ] .\n\
               ex:l6 a ex:Line ; ex:len 0 ; geo:hasGeometry [ geo:asWKT \"POINT(6 6)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let shapes = "@prefix ex: <http://example.com/> .\n\
                  @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
                  @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                  @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                  @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                  ex:LineShape a sh:NodeShape ; sh:targetClass ex:Line ; tts:key \"line\" ;\n\
                    tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] ;\n\
                    sh:property [ sh:path ex:speed ; tts:key \"speed\" ; tts:facet tts:Facet-Range ;\n\
                        tts:colorClasses ( [ tts:upTo 100 ; rdfs:label \"slow\" ] [ rdfs:label \"fast\" ] ) ] ,\n\
                      [ sh:path ex:len ; tts:key \"len\" ; tts:facet tts:Facet-Range ] .\n";
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Line.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(shapes)
        .unwrap();
    let facet = |query: &str, key: &str| -> Value {
        let (status, _, out) = get(&format!("{base}/kg/tiles/facets/line?{query}"));
        assert_eq!(status, 200, "{query}: {out}");
        out["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == key)
            .cloned()
            .unwrap()
    };
    // Not coloured by it: the shape's classes, without counts.
    assert_eq!(
        facet("", "speed")["classes"],
        json!([{ "upTo": 100.0, "label": "slow" }, { "upTo": null, "label": "fast" }])
    );
    // Coloured by it: l5 is fast by its 150; l6, with no speed, is in no class.
    assert_eq!(
        facet("color=speed", "speed")["classes"],
        json!([
            { "upTo": 100.0, "label": "slow", "count": 2 },
            { "upTo": null, "label": "fast", "count": 3 },
        ])
    );
    // Under the filters, its own too.
    assert_eq!(
        facet("color=speed&speed.min=110", "speed")["classes"],
        json!([
            { "upTo": 100.0, "label": "slow", "count": 0 },
            { "upTo": null, "label": "fast", "count": 3 },
        ])
    );
    assert_eq!(
        facet("color=speed&len.max=150", "speed")["classes"],
        json!([
            { "upTo": 100.0, "label": "slow", "count": 2 },
            { "upTo": null, "label": "fast", "count": 0 },
        ])
    );
    // Without classes in its shape, five of one round width over its values, as the map
    // draws them: 0 to 350 in steps of 100.
    assert_eq!(
        facet("color=len", "len")["classes"],
        json!([
            { "upTo": 100.0, "label": null, "count": 2 },
            { "upTo": 200.0, "label": null, "count": 2 },
            { "upTo": 300.0, "label": null, "count": 1 },
            { "upTo": null, "label": null, "count": 1 },
        ])
    );
}

/// The map's spatial filter (#33): `area` (WKT or the IRI of a resource with a geometry),
/// `rel` and `dist`, the same for the facets, the tiles and the OGC API's items.
#[test]
fn facets_tiles_and_items_by_a_spatial_relation_to_an_area() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:zone a ex:Zone ; geo:hasGeometry [ geo:asWKT \
           \"POLYGON((3.95 50.7, 4.15 50.7, 4.15 50.9, 3.95 50.9, 3.95 50.7))\"^^geo:wktLiteral ] .\n\
         ex:nowhere a ex:Zone .\n",
    );
    // Stations 0-3 at longitudes 4.0 to 4.3 (0.1° is about 7 km there), 4-5 in Norway.
    for (i, lon) in [4.0, 4.1, 4.2, 4.3, 10.4, 10.5].iter().enumerate() {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; \
             geo:hasGeometry [ geo:asWKT \"POINT({lon} 50.8)\"^^geo:wktLiteral ] .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let zone = "POLYGON((3.95%2050.7,%204.15%2050.7,%204.15%2050.9,%203.95%2050.9,%203.95%2050.7))";
    let count = |query: &str| {
        let (code, _, out) = get(&format!("{base}/kg/tiles/facets/Station?{query}"));
        assert_eq!(code, 200, "{query}: {out}");
        out["features"].as_u64().unwrap()
    };
    assert_eq!(count(""), 6);
    assert_eq!(count(&format!("area={zone}")), 2);
    assert_eq!(count(&format!("area={zone}&rel=intersects")), 2);
    assert_eq!(count(&format!("area={zone}&rel=within")), 2);
    assert_eq!(count(&format!("area={zone}&rel=contains")), 0);
    assert_eq!(count(&format!("area={zone}&rel=touches")), 0);
    assert_eq!(count(&format!("area={zone}&rel=disjoint")), 4);
    // The zone by its IRI: its geometry, read through geo:hasGeometry.
    assert_eq!(count("area=http%3A%2F%2Fexample.com%2Fzone&rel=within"), 2);
    // Within a distance of a point: s1 is about 7 km from s0, s2 about 14 km.
    let point = "POINT(4.0%2050.8)";
    assert_eq!(count(&format!("area={point}&rel=dwithin&dist=5000")), 1);
    assert_eq!(count(&format!("area={point}&rel=dwithin&dist=8000")), 2);
    assert_eq!(count(&format!("area={point}&rel=dwithin&dist=15000")), 3);
    // A CRS of its own, as a wktLiteral names it: EPSG:4326 is latitude first.
    let epsg =
        "%3Chttp%3A%2F%2Fwww.opengis.net%2Fdef%2Fcrs%2FEPSG%2F0%2F4326%3E%20POINT(50.8%204.0)";
    assert_eq!(count(&format!("area={epsg}&rel=dwithin&dist=5000")), 1);
    // With another filter: the area and the box together.
    assert_eq!(
        count(&format!("area={zone}&rel=disjoint&bbox=3,50,5,51")),
        2
    );
    // Mistakes are 400s, with the reason.
    for (query, reason) in [
        (format!("area={zone}&rel=near"), "rel=near"),
        (format!("area={zone}&dist=10"), "only with rel=dwithin"),
        (format!("area={point}&rel=dwithin"), "needs a dist"),
        (format!("area={point}&rel=dwithin&dist=-1"), "metres"),
        ("area=POLYGON((1%202".to_owned(), "not a WKT geometry"),
        ("rel=within".to_owned(), "need an area"),
        (
            "area=http%3A%2F%2Fexample.com%2Fnowhere".to_owned(),
            "no geometry",
        ),
    ] {
        let (code, _, out) = get(&format!("{base}/kg/tiles/facets/Station?{query}"));
        assert_eq!(code, 400, "{query}");
        assert!(out.as_str().unwrap().contains(reason), "{query}: {out}");
    }
    // A tile with the filter draws only what it keeps.
    let (code, all) = tile(&format!("{base}/kg/tiles/Station/0/0/0.mvt"));
    assert_eq!(code, 200);
    let (code, near) = tile(&format!(
        "{base}/kg/tiles/Station/0/0/0.mvt?area={point}&rel=dwithin&dist=8000"
    ));
    assert_eq!(code, 200);
    assert!(holds(&all, "http://example.com/s5"));
    assert!(holds(&near, "http://example.com/s1"));
    assert!(!holds(&near, "http://example.com/s2"));
    // The OGC API's items take the same parameters.
    let (code, _, page) = get(&format!(
        "{base}/kg/ogc/collections/Station/items?area={zone}&rel=disjoint"
    ));
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["numberReturned"], 4);
}

/// Select by location (#33): `area=layer:{id}`, the features of another collection, with that
/// collection's own filters, as the area.
#[test]
fn select_by_location_takes_another_layers_features() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:west a ex:Zone ; ex:kind \"west\" ; geo:hasGeometry [ geo:asWKT \
           \"POLYGON((3.95 50.7, 4.15 50.7, 4.15 50.9, 3.95 50.9, 3.95 50.7))\"^^geo:wktLiteral ] .\n\
         ex:north a ex:Zone ; ex:kind \"north\" ; geo:hasGeometry [ geo:asWKT \
           \"POLYGON((10 50, 11 50, 11 51, 10 51, 10 50))\"^^geo:wktLiteral ] .\n",
    );
    // Stations 0-3 at longitudes 4.0 to 4.3 (0.1° is about 7 km there), 4-5 further east.
    for (i, lon) in [4.0, 4.1, 4.2, 4.3, 10.4, 10.5].iter().enumerate() {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; \
             geo:hasGeometry [ geo:asWKT \"POINT({lon} 50.8)\"^^geo:wktLiteral ] .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let count = |query: &str| {
        let (code, _, out) = get(&format!("{base}/kg/tiles/facets/Station?{query}"));
        assert_eq!(code, 200, "{query}: {out}");
        out["features"].as_u64().unwrap()
    };
    assert_eq!(count("area=layer:Zone"), 4);
    // The zones as their own filters keep them; a bare filter is the stations' own.
    assert_eq!(count("area=layer:Zone&Zone.kind.text=west"), 2);
    assert_eq!(count("area=layer:Zone&Zone.kind.text=north"), 2);
    assert_eq!(count("area=layer:Zone&Zone.kind.text=west&rel=disjoint"), 4);
    // A layer as its own area: the other features, never a feature itself.
    assert_eq!(count("area=layer:Station&rel=dwithin&dist=5000"), 0);
    assert_eq!(count("area=layer:Station&rel=dwithin&dist=8000"), 6);
    let (code, _, out) = get(&format!("{base}/kg/tiles/facets/Station?area=layer:Nope"));
    assert_eq!(code, 400);
    assert!(out.as_str().unwrap().contains("no such layer"), "{out}");
    // A tile, and the OGC API's items, which take the other layer's filters too.
    let (code, north) = tile(&format!(
        "{base}/kg/tiles/Station/0/0/0.mvt?area=layer:Zone&Zone.kind.text=north"
    ));
    assert_eq!(code, 200);
    assert!(holds(&north, "http://example.com/s5"));
    assert!(!holds(&north, "http://example.com/s0"));
    let (code, _, page) = get(&format!(
        "{base}/kg/ogc/collections/Station/items?area=layer:Zone&Zone.kind.text=west"
    ));
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["numberReturned"], 2);
    let (code, _, _) = get(&format!(
        "{base}/kg/ogc/collections/Station/items?area=layer:Zone&Zone.nope=1"
    ));
    assert_eq!(code, 400);
}

/// A tile with `area=layer:{id}` gives each feature the same answer as the items, in every
/// tile it crosses: a road a degree long crosses a link drawn between two points
/// (`tts:lineBetween`) in one place only, far outside most of the tiles that draw the road.
#[test]
fn select_by_location_answers_a_long_feature_alike_in_every_tile() {
    let dir = tripletrail_testdir::tempdir("ogc-layer-long");
    let ttl = "@prefix ex: <http://example.org/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:p1 a ex:Point ; geo:asWKT \"POINT(10.5 50.0)\"^^geo:wktLiteral .\n\
               ex:p2 a ex:Point ; geo:asWKT \"POINT(10.5 50.1)\"^^geo:wktLiteral .\n\
               ex:link1 a ex:Link ; ex:from ex:p1 ; ex:to ex:p2 .\n\
               ex:road1 a ex:Road ; geo:asWKT \"LINESTRING(10.0 50.05, 11.0 50.05)\"^^geo:wktLiteral .\n\
               ex:road2 a ex:Road ; geo:asWKT \"LINESTRING(10.0 50.3, 11.0 50.3)\"^^geo:wktLiteral .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let link = "@prefix ex: <http://example.org/> .\n\
                @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
                @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                ex:LinkShape a sh:NodeShape ; sh:targetClass ex:Link ; tts:key \"link\" ;\n\
                  tts:geometry [ sh:order 0 ; sh:path ( geo:hasGeometry geo:asWKT ) ] ,\n\
                    [ sh:order 1 ; tts:lineBetween ( ( ex:from geo:asWKT ) ( ex:to geo:asWKT ) ) ] .\n";
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Link.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(link)
        .unwrap();
    let (code, _, page) = get(&format!(
        "{base}/kg/ogc/collections/Road/items?area=layer:link&f=json"
    ));
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["numberReturned"], 1, "{page}");
    assert!(
        page["features"][0]["id"]
            .as_str()
            .unwrap()
            .ends_with("road1")
    );
    // The tiles along road1, from its west end to its east end, at two zooms.
    for z in [12u32, 14] {
        let n = f64::from(1u32 << z);
        let y =
            ((1.0 - 50.05f64.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32;
        for lon in [10.1, 10.3, 10.5, 10.7, 10.95] {
            let x = ((lon + 180.0) / 360.0 * n) as u32;
            let at = format!("{base}/kg/tiles/Road/{z}/{x}/{y}.mvt?area=layer:link");
            let (code, near) = tile(&at);
            assert_eq!(code, 200, "{at}");
            assert!(holds(&near, "http://example.org/road1"), "{at}");
            let (code, outside) = tile(&format!("{at}&rel=disjoint"));
            assert_eq!(code, 200, "{at}&rel=disjoint");
            assert!(
                !holds(&outside, "http://example.org/road1"),
                "{at}&rel=disjoint"
            );
        }
    }
}

/// `area=layer:{id}&rel=disjoint` works out the matching features once, not once per feature
/// of a tile or a page: on 2,500 stations and 400 zones a tile and a page of items answer well
/// inside the server's timeout (per feature, they took minutes).
#[test]
fn select_by_location_outside_a_layer_is_one_join() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..50 {
        for j in 0..50 {
            let (lon, lat) = (4.0 + f64::from(i) * 0.01, 50.0 + f64::from(j) * 0.01);
            ttl.push_str(&format!(
                "ex:s{i}_{j} a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT({lon} {lat})\"^^geo:wktLiteral ] .\n"
            ));
        }
    }
    for i in 0..20 {
        for j in 0..20 {
            // Around station s{2i}_{2j}.
            let (lon, lat) = (4.0 + f64::from(i) * 0.02, 50.0 + f64::from(j) * 0.02);
            let (w, s, e, n) = (lon - 0.002, lat - 0.002, lon + 0.002, lat + 0.002);
            ttl.push_str(&format!(
                "ex:z{i}_{j} a ex:Zone ; geo:hasGeometry [ geo:asWKT \
                 \"POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))\"^^geo:wktLiteral ] .\n"
            ));
        }
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            timeout: Some(Duration::from_secs(20)),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    // Each zone holds one station.
    let (code, _, counts) = get(&format!(
        "{base}/kg/tiles/facets/Station?area=layer:Zone&rel=disjoint"
    ));
    assert_eq!(code, 200, "{counts}");
    assert_eq!(counts["features"], 2500 - 400);
    let (code, outside) = tile(&format!(
        "{base}/kg/tiles/Station/8/130/86.mvt?area=layer:Zone&rel=disjoint"
    ));
    assert_eq!(code, 200);
    assert!(!holds(&outside, "http://example.com/s0_0"));
    assert!(holds(&outside, "http://example.com/s1_0"));
    let (code, _, page) = get(&format!(
        "{base}/kg/ogc/collections/Station/items?area=layer:Zone&rel=disjoint&limit=1000"
    ));
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["numberReturned"], 1000);
}

/// Facets of parts rolled up to their parent (#248): a section of line matches, counts and is
/// coloured by its tracks' facets that it lacks, through `tts:rollsUpTo` on the tracks' shape.
#[test]
fn part_facets_roll_up_to_the_parent() {
    let dir = tripletrail_testdir::tempdir("ogc-rollup");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               ex:s1 a ex:Section ; ex:name \"S1\" ; geo:hasGeometry [ geo:asWKT \"LINESTRING(1 1, 2 2)\"^^geo:wktLiteral ] .\n\
               ex:s2 a ex:Section ; ex:name \"S2\" ; geo:hasGeometry [ geo:asWKT \"LINESTRING(3 3, 4 4)\"^^geo:wktLiteral ] .\n\
               ex:t1 a ex:Track ; ex:isPartOf ex:s1 ; ex:etcs \"2\" ; ex:speed 160 ; ex:name \"T1\" .\n\
               ex:t2 a ex:Track ; ex:isPartOf ex:s1 ; ex:etcs \"1\" ; ex:speed 100 ; ex:name \"T2\" .\n\
               ex:t3 a ex:Track ; ex:isPartOf ex:s2 ; ex:etcs \"1\" ; ex:speed 80 ; ex:name \"T3\" .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let shapes = "@prefix ex: <http://example.com/> .\n\
                  @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
                  @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                  @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                  ex:SectionShape a sh:NodeShape ; sh:targetClass ex:Section ; tts:key \"section\" ;\n\
                    tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] ;\n\
                    sh:property [ sh:path ex:name ; tts:key \"name\" ; tts:facet tts:Facet-Text ] .\n\
                  ex:TrackShape a sh:NodeShape ; sh:targetClass ex:Track ; tts:key \"track\" ;\n\
                    tts:geometry [ sh:path ( ex:isPartOf geo:hasGeometry geo:asWKT ) ] ;\n\
                    tts:rollsUpTo [ sh:path ex:isPartOf ; sh:class ex:Section ] ;\n\
                    sh:property [ sh:path ex:etcs ; tts:key \"etcs\" ; tts:facet tts:Facet-Values ] ,\n\
                      [ sh:path ex:speed ; tts:key \"speed\" ; tts:facet tts:Facet-Range ] ,\n\
                      [ sh:path ex:name ; tts:key \"name\" ; tts:facet tts:Facet-Text ] .\n";
    let r = ureq::put(&format!("{base}/$/datasets/kg/shapes/files/rail.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(shapes)
        .unwrap();
    assert_eq!(r.status(), 200);
    let facets = |query: &str| {
        let (status, _, out) = get(&format!("{base}/kg/tiles/facets/section{query}"));
        assert_eq!(status, 200, "{query}: {out}");
        out
    };
    let facet = |out: &Value, key: &str| -> Value {
        out["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == key)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let all = facets("");
    assert_eq!(all["features"], 2, "{all}");
    // Each section counts once per value of its tracks: s1 has "1" and "2", s2 "1".
    let counts: Vec<(String, u64)> = facet(&all, "etcs")["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["value"].as_str().unwrap().to_owned(),
                v["count"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(counts, [("1".to_owned(), 2), ("2".to_owned(), 1)], "{all}");
    // Its own facet of the same key stays its own: the section's name, not its tracks'.
    assert_eq!(facets("?name.text=t1")["features"], 0);
    assert_eq!(facets("?name.text=s1")["features"], 1);
    // A plain filter keeps the sections with a matching track.
    assert_eq!(facets("?etcs=2")["features"], 1);
    assert_eq!(facets("?speed.min=90")["features"], 1);
    let (_, tile2) = tile(&format!("{base}/kg/tiles/section/0/0/0.mvt?etcs=2"));
    assert!(holds(&tile2, "http://example.com/s1") && !holds(&tile2, "http://example.com/s2"));
    // A filter scoped to the tracks leaves the sections alone.
    let scoped = get(&format!(
        "{base}/kg/tiles/facets?collection=section&collection=track&track.etcs=2"
    ))
    .2;
    assert_eq!(scoped["collections"][0]["features"], 2, "{scoped}");
    assert_eq!(scoped["collections"][1]["features"], 1, "{scoped}");
    // Coloured by speed: each section by its fastest track (s1 160, s2 80).
    let speed = facet(&facets("?color=speed"), "speed");
    let total: u64 = speed["classes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["count"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 2, "{speed}");
    assert_eq!(
        (speed["min"].as_f64(), speed["max"].as_f64()),
        (Some(80.0), Some(160.0)),
        "{speed}"
    );
    // Only the map reads it: the OGC API's sections have no etcs.
    let queryables = get(&format!("{base}/kg/ogc/collections/section/queryables")).2;
    assert!(
        queryables["properties"].get("etcs").is_none(),
        "{queryables}"
    );
    // The term needs a path and a class.
    let wrong = shapes.replace(
        "tts:rollsUpTo [ sh:path ex:isPartOf ; sh:class ex:Section ]",
        "tts:rollsUpTo ex:Section",
    );
    let r = ureq::put(&format!("{base}/$/datasets/kg/shapes/files/rail.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(&wrong);
    match r {
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap();
            assert_eq!(code, 422, "{body}");
            assert!(body.contains("tts:rollsUpTo"), "{body}");
        }
        other => panic!("{other:?}"),
    }
}

/// The map's labels (#260): a facet value's label in the reader's language (`lang`, else the
/// browser's `Accept-Language`), else English, else one without a language, else any; and
/// without what the shapes' `tts:labelStrip` matches, in facets and search alike.
#[test]
fn map_labels_follow_the_language_and_the_label_strip() {
    let dir = tripletrail_testdir::tempdir("ogc-labels");
    let ttl = "@prefix ex: <http://example.com/> .\n\
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
               @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
               @prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n\
               ex:DEU skos:prefLabel \"ألمانيا\"@ar , \"Germany\"@en , \"Allemagne\"@fr , \"Deutschland\"@de .\n\
               ex:BEL skos:prefLabel \"بلجيكا\"@ar , \"Belgien\"@de .\n\
               ex:LUX skos:prefLabel \"Luxembourg\" , \"لوكسمبورغ\"@ar .\n\
               ex:s1 a ex:Station ; ex:country ex:DEU ; rdfs:label \"Köln Hbf (from 2022-12-11 until 2078-12-31)\" ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(6.95 50.94)\"^^geo:wktLiteral ] .\n\
               ex:s2 a ex:Station ; ex:country ex:BEL ; rdfs:label \"Bruxelles-Ouest | Brussel-West (from 2022-12-11 until 2078-12-31)\" ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(4.32 50.85)\"^^geo:wktLiteral ] .\n\
               ex:s3 a ex:Station ; ex:country ex:LUX ; rdfs:label \"Luxembourg\" ;\n\
                 geo:hasGeometry [ geo:asWKT \"POINT(6.13 49.6)\"^^geo:wktLiteral ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let own = format!(
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         <urn:my:stations> a sh:NodeShape ; sh:targetClass <http://example.com/Station> ;\n\
           tts:key \"station\" ; tts:labelStrip {:?} ;\n\
           tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] ;\n\
           sh:property [ sh:path <http://example.com/country> ; tts:key \"country\" ;\n\
             tts:facet tts:Facet-Values ] .\n",
        tripletrail::shapes::VALIDITY
    );
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/mine.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(&own)
        .unwrap();
    let countries = |query: &str, accept: Option<&str>| -> Vec<(String, String)> {
        let mut request = ureq::get(&format!("{base}/kg/tiles/facets/station{query}"));
        if let Some(accept) = accept {
            request = request.set("Accept-Language", accept);
        }
        let out: Value =
            serde_json::from_str(&request.call().unwrap().into_string().unwrap()).unwrap();
        let mut values: Vec<(String, String)> = out["facets"][0]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["value"]
                        .as_str()
                        .unwrap()
                        .replace("http://example.com/", ""),
                    v["label"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        values.sort();
        values
    };
    let pairs = |p: &[(&str, &str)]| -> Vec<(String, String)> {
        p.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    };
    // English by default; a label without a language before another language.
    assert_eq!(
        countries("", None),
        pairs(&[
            ("BEL", "Belgien"),
            ("DEU", "Germany"),
            ("LUX", "Luxembourg")
        ])
    );
    // The browser's language, then English; `lang` before the browser's.
    assert_eq!(
        countries("", Some("fr-BE,fr;q=0.9,en;q=0.8")),
        pairs(&[
            ("BEL", "Belgien"),
            ("DEU", "Allemagne"),
            ("LUX", "Luxembourg")
        ])
    );
    assert_eq!(
        countries("?lang=de", Some("fr")),
        pairs(&[
            ("BEL", "Belgien"),
            ("DEU", "Deutschland"),
            ("LUX", "Luxembourg")
        ])
    );
    // The same labels when several collections are asked at once.
    let both = get(&format!("{base}/kg/tiles/facets?collection=station")).2;
    assert!(both.to_string().contains("\"Germany\""), "{both}");
    // Search shows the labels without RINF's validity period.
    let found = get(&format!("{base}/kg/tiles/search?q=brussel")).2;
    assert_eq!(
        found["results"][0]["label"], "Bruxelles-Ouest | Brussel-West",
        "{found}"
    );
    // The map page strips the labels it looks up itself with the same pattern.
    let style = get(&format!("{base}/kg/tiles/style.json")).2;
    assert_eq!(
        style["metadata"]["tripletrail:labelStrip"],
        format!("(?:{})", tripletrail::shapes::VALIDITY),
        "{style}"
    );
}
