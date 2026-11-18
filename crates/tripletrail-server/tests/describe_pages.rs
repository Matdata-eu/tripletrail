//! Describe pages (#140): any IRI of a dataset gets a readable page, from the shapes the
//! describe reader reads (generated from the profile, under the dataset's own DASH shapes).

use serde_json::Value;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

const DATA: &str = r#"
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix ex: <http://example.com/> .
ex:BE1 a ex:OperationalPoint ; rdfs:label "Brussel-Zuid"@nl , "Brussels-South"@en ;
    ex:uopid "BE00001" ; ex:secret "s3cr3t" ; ex:line ex:L50 ;
    ex:position [ ex:km 12.5 ] .
ex:BE2 a ex:OperationalPoint ; rdfs:label "Halle"@en ; ex:uopid "BE00002" .
ex:L50 rdfs:label "Line 50"@en .
ex:OperationalPoint rdfs:subClassOf ex:Place .
ex:t1 ex:at ex:BE1 ; rdfs:label "Train 1" .
ex:t2 ex:stopsAt ex:BE1 .
"#;

const IRI: &str = "http%3A%2F%2Fexample.com%2FBE1";

fn start(dir: Option<&std::path::Path>) -> String {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: dir.map(|_| "t".to_owned()),
            location: dir.map(|d| d.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn view(base: &str) -> Value {
    let r = ureq::get(&format!("{base}/kg/describe?iri={IRI}&format=view"))
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/json");
    serde_json::from_str(&r.into_string().unwrap()).unwrap()
}

fn group<'a>(view: &'a Value, label: &str) -> &'a Value {
    view["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["label"] == label)
        .unwrap_or_else(|| panic!("no group {label} in {view}"))
}

fn row<'a>(group: &'a Value, property: &str) -> Option<&'a Value> {
    group["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["property"] == property)
}

#[test]
fn any_iri_gets_a_page_without_configuration() {
    let base = start(None);
    let v = view(&base);
    assert_eq!(v["label"], "Brussels-South");
    assert_eq!(
        v["classes"][0]["iri"],
        "http://example.com/OperationalPoint"
    );
    let attributes = group(&v, "Attributes");
    let uopid = row(attributes, "http://example.com/uopid").unwrap();
    assert_eq!(uopid["values"][0]["value"], "BE00001");
    assert_eq!(uopid["viewer"], "LiteralViewer");
    let links = group(&v, "Links");
    let line = row(links, "http://example.com/line").unwrap();
    assert_eq!(line["values"][0]["label"], "Line 50");
    // A blank node is shown as its own rows.
    let position = row(links, "http://example.com/position").unwrap();
    assert_eq!(position["viewer"], "DetailsViewer");
    assert_eq!(position["values"][0]["rows"][0]["value"]["value"], "12.5");
    // The links to it, per property, with the label of where they come from.
    let incoming = v["incoming"].as_array().unwrap();
    assert_eq!(incoming.len(), 2, "{v}");
    let at = incoming
        .iter()
        .find(|i| i["property"]["iri"] == "http://example.com/at")
        .unwrap();
    assert_eq!(at["subjects"][0]["label"], "Train 1");

    // The page, for a browser, in its language.
    // en-GB picks the label tagged en.
    let r = ureq::get(&format!("{base}/kg/describe?iri={IRI}&format=html"))
        .set("Accept-Language", "en-GB")
        .call()
        .unwrap();
    assert!(
        r.into_string()
            .unwrap()
            .contains("<title>Brussels-South</title>")
    );
    let r = ureq::get(&format!("{base}/kg/describe?iri={IRI}"))
        .set("Accept", "text/html")
        .set("Accept-Language", "nl-BE,nl;q=0.9")
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "text/html");
    assert_eq!(r.header("Vary"), Some("Accept, Accept-Language"));
    let page = r.into_string().unwrap();
    assert!(page.contains("<title>Brussel-Zuid</title>"), "{page}");
    assert!(page.contains("<h2>Attributes</h2>") && page.contains("BE00001"));
    assert!(page.contains(">Line 50</a>") && page.contains("Referenced by"));
    // Links stay on the describe service, and the documents are offered.
    assert!(
        page.contains("describe?iri=http%3A%2F%2Fexample.com%2FL50"),
        "{page}"
    );
    assert!(page.contains("format=ttl"));

    // Nothing known: 404.
    let r = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2Fnone&format=view"
    ))
    .call();
    assert!(matches!(r, Err(ureq::Error::Status(404, _))));
}

#[test]
fn dash_shapes_group_order_and_hide_rows() {
    let dir = tripletrail_testdir::tempdir("describe");
    let base = start(Some(&dir));
    let shapes = |method: &str, path: &str| {
        ureq::request(method, &format!("{base}/$/datasets/kg/shapes{path}"))
            .set("Authorization", "Bearer t")
    };
    shapes("POST", "/customise").call().unwrap();
    // On the superclass: the resource's classes and their superclasses apply.
    let place = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix dash: <http://datashapes.org/dash#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix ex: <http://example.com/> .
ex:Identity a sh:PropertyGroup ; rdfs:label "Identity"@en , "Identiteit"@nl ; sh:order -1 .
ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;
    sh:property [ sh:path ex:uopid ; sh:name "UOPID" ; sh:group ex:Identity ; sh:order 1 ] ;
    sh:property [ sh:path rdfs:label ; sh:name "Name"@en ; sh:group ex:Identity ; sh:order 0 ] ;
    sh:property [ sh:path ex:secret ; dash:hidden true ] ;
    sh:property [ sh:path [ sh:inversePath ex:at ] ; sh:name "Trains here" ;
        sh:group ex:Identity ; sh:order 2 ; dash:viewer dash:URIViewer ] .
"#;
    shapes("PUT", "/files/Place.ttl")
        .send_string(place)
        .unwrap();
    let v = view(&base);
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups[0]["label"], "Identity", "{v}");
    let labels: Vec<&str> = groups[0]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["Name", "UOPID", "Trains here"]);
    let trains = row(&groups[0], "http://example.com/at").unwrap();
    assert_eq!(trains["inverse"], true);
    assert_eq!(trains["viewer"], "URIViewer");
    assert_eq!(trains["values"][0]["value"], "http://example.com/t1");
    // The hidden property is in no row, and the inverse row is no incoming link.
    for g in groups {
        assert!(row(g, "http://example.com/secret").is_none(), "{v}");
    }
    let incoming = v["incoming"].as_array().unwrap();
    assert_eq!(incoming.len(), 1, "{v}");
    assert_eq!(incoming[0]["property"]["iri"], "http://example.com/stopsAt");
    let page = ureq::get(&format!("{base}/kg/describe?iri={IRI}&format=html"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(!page.contains("s3cr3t"));
    assert!(
        page.contains("<a href=\"http://example.com/t1\">http://example.com/t1</a>"),
        "{page}"
    );
}

#[test]
fn a_resource_with_many_values_has_a_bounded_page() {
    // 150,000 values of one property: past the 100,000 triples read, quickly.
    let mut nt = String::new();
    for i in 0..150_000 {
        nt.push_str(&format!(
            "<http://example.com/big> <http://example.com/has> <http://example.com/v{i}> .\n"
        ));
    }
    nt.push_str(
        "<http://example.com/v1> <http://example.com/to> <http://example.com/only-object> .\n",
    );
    nt.push_str("<http://example.com/big> <http://example.com/home> <javascript:alert(1)> .\n");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let started = std::time::Instant::now();
    let r = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2Fbig&format=view"
    ))
    .call()
    .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    assert_eq!(v["truncated"], true);
    let has = row(group(&v, "Links"), "http://example.com/has").unwrap();
    assert_eq!(has["values"].as_array().unwrap().len(), 100);
    // 100,000 triples read in all, one of them the other property, in store order.
    let shown = 100 + has["more"].as_u64().unwrap();
    assert!((99_999..=100_000).contains(&shown), "{shown}");

    // A raw link only for http(s) IRIs.
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2Fbig&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        !page.contains("href=\"javascript:"),
        "{}",
        &page[..2000.min(page.len())]
    );
    assert!(page.contains("Only the first 100000 triples"));

    // A resource that is only an object has a page, but no documents to download.
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2Fonly-object&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains("Referenced by") && !page.contains("Download"),
        "{page}"
    );
}

#[test]
fn geometries_are_drawn_on_a_map() {
    let ttl = r#"
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix ex: <http://example.com/> .
ex:st a ex:Station ; geo:hasGeometry ex:st-geom ;
    ex:area [ geo:asWKT "POLYGON((4.34 50.84, 4.36 50.84, 4.36 50.86, 4.34 50.84))"^^geo:wktLiteral ] .
ex:st-geom geo:asWKT "POINT(4.35 50.85)"^^geo:wktLiteral .
ex:other ex:shape "<http://www.opengis.net/def/crs/EPSG/0/25832> LINESTRING(499000 6599000, 501000 6601000)"^^geo:wktLiteral ;
    ex:elsewhere "POINT(4 50)"^^geo:wktLiteral ;
    geo:hasGeometry ex:g2 .
ex:g2 geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/25832> POINT(500000 6600000)"^^geo:wktLiteral .
"#;
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let get = |iri: &str, format: &str| {
        ureq::get(&format!("{base}/kg/describe?iri={iri}&format={format}"))
            .call()
            .unwrap()
            .into_string()
            .unwrap()
    };
    let v: Value = serde_json::from_str(&get("http%3A%2F%2Fexample.com%2Fst", "view")).unwrap();
    let geometries = v["geometries"].as_array().unwrap();
    assert_eq!(geometries.len(), 2, "{v}");
    let page = get("http%3A%2F%2Fexample.com%2Fst", "html");
    assert!(page.contains("<svg class=\"map\""), "{page}");
    assert!(
        page.contains("<circle") && page.contains("class=\"area\""),
        "{page}"
    );
    assert!(page.contains("openstreetmap.org/?mlat=50.85"), "{page}");
    // A projected CRS is drawn too, without the OpenStreetMap link; a geometry in another CRS
    // than the first is left out of the drawing.
    let page = get("http%3A%2F%2Fexample.com%2Fother", "html");
    assert!(page.contains("class=\"line\""), "{page}");
    assert_eq!(page.matches("<circle").count(), 1, "{page}");
    assert!(!page.contains("openstreetmap"), "{page}");
}

#[test]
fn big_and_many_geometries_stay_on_a_bounded_map() {
    // A circle of 200,000 points, 25 point geometries, and one geometry a shape hides.
    let mut ring = String::new();
    for i in 0..200_000 {
        let a = std::f64::consts::TAU * f64::from(i) / 200_000.0;
        ring.push_str(&format!("{:.6} {:.6}, ", 5.0 + a.cos(), 50.0 + a.sin()));
    }
    ring.push_str("6.000000 50.000000");
    let mut ttl = format!(
        "@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix ex: <http://example.com/> .\n\
         ex:round ex:shape \"POLYGON(({ring}))\"^^geo:wktLiteral .\n\
         ex:round ex:shape \"POLYGON((5 50, 5.01 50, 5.01 50.01, 5 50.01, 5 50))\"^^geo:wktLiteral .\n\
         ex:many a ex:Cloud ; ex:secretPlace \"POINT(80 80)\"^^geo:wktLiteral .\n"
    );
    for i in 0..25 {
        ttl.push_str(&format!(
            "ex:many ex:at \"POINT({} 10)\"^^geo:wktLiteral .\n",
            f64::from(i) / 10.0
        ));
    }
    let dir = tripletrail_testdir::tempdir("describe-map");
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
    let get = |iri: &str, format: &str| {
        ureq::get(&format!(
            "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2F{iri}&format={format}"
        ))
        .call()
        .unwrap()
        .into_string()
        .unwrap()
    };
    // The circle is thinned, not cut: its path goes all the way round.
    let page = get("round", "html");
    let d = page
        .split("class=\"area\" fill-rule=\"evenodd\" d=\"")
        .nth(1)
        .unwrap();
    let d = &d[..d.find('"').unwrap()];
    let points = d.matches('L').count() + 1;
    assert!((1_000..=50_001).contains(&points), "{points}");
    let xs: Vec<f64> = d
        .split(['M', 'L', 'Z'])
        .filter_map(|p| p.split(',').next()?.parse().ok())
        .collect();
    let (min, max) = xs
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(*x), b.max(*x)));
    // A circle 2 degrees across at 50°N: about 300 px tall and 300 cos(50°) wide, not an arc.
    assert!(max - min > 180.0, "{min} {max}");
    // A small polygon next to it keeps its points, and does not vanish.
    let small = page
        .split("class=\"area\" fill-rule=\"evenodd\" d=\"")
        .nth(2)
        .unwrap();
    let small = &small[..small.find('"').unwrap()];
    assert!(small.matches('L').count() >= 3, "{small}");
    // A country-sized extent does not open on one street.
    assert!(
        page.contains("#map=7/"),
        "{}",
        &page[page.find("openstreetmap").unwrap() - 120..]
    );

    // More than 20: said so; a hidden property is not on the map.
    let shapes = |method: &str, path: &str| {
        ureq::request(method, &format!("{base}/$/datasets/kg/shapes{path}"))
            .set("Authorization", "Bearer t")
    };
    let v: Value = serde_json::from_str(&get("many", "view")).unwrap();
    assert_eq!(v["geometries"].as_array().unwrap().len(), 20);
    assert_eq!(v["geometriesTruncated"], true);
    assert!(get("many", "html").contains("The first 20 geometries are drawn."));
    shapes("POST", "/customise").call().unwrap();
    shapes("PUT", "/files/Cloud.ttl")
        .send_string(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             @prefix dash: <http://datashapes.org/dash#> .\n\
             @prefix ex: <http://example.com/> .\n\
             ex:CloudShape a sh:NodeShape ; sh:targetClass ex:Cloud ;\n\
               sh:property [ sh:path ex:secretPlace ; dash:hidden true ] .\n",
        )
        .unwrap();
    let v = get("many", "view");
    assert!(!v.contains("POINT(80 80)"), "{v}");
}

#[test]
fn any_shacl_path_gives_a_row() {
    let dir = tripletrail_testdir::tempdir("describe-paths");
    let base = start(Some(&dir));
    let shapes = |method: &str, path: &str| {
        ureq::request(method, &format!("{base}/$/datasets/kg/shapes{path}"))
            .set("Authorization", "Bearer t")
    };
    shapes("POST", "/customise").call().unwrap();
    let place = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix ex: <http://example.com/> .
ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;
    sh:property [ sh:path ( ex:line rdfs:label ) ; sh:name "Line name" ; sh:order 0 ] ;
    sh:property [ sh:path [ sh:alternativePath ( ex:uopid [ sh:inversePath ex:stopsAt ] ) ] ;
        sh:order 1 ] ;
    sh:property [ sh:path [ sh:zeroOrMorePath ex:line ] ; sh:name "Self and lines" ; sh:order 2 ] ;
    sh:property [ sh:path [ sh:inversePath ( ex:at [ sh:oneOrMorePath ex:at ] ) ] ;
        sh:name "Nothing" ; sh:order 3 ] ;
    sh:property [ sh:path [ sh:oneOrMorePath [ sh:inversePath ex:at ] ] ; sh:name "Trains" ;
        sh:order 4 ] ;
    sh:property [ sh:path ( ex:line ) ; sh:name "Malformed" ] .
"#;
    shapes("PUT", "/files/Place.ttl")
        .send_string(place)
        .unwrap();
    let v = view(&base);
    let rows: Vec<&Value> = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["rows"].as_array().unwrap())
        .collect();
    let by_path = |path: &str| {
        rows.iter()
            .find(|r| r["path"] == path)
            .unwrap_or_else(|| panic!("no row {path} in {v}"))
    };
    let values = |r: &Value| -> Vec<String> {
        r["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["value"].as_str().unwrap().to_owned())
            .collect()
    };
    let line =
        by_path("(<http://example.com/line> / <http://www.w3.org/2000/01/rdf-schema#label>)");
    assert_eq!(line["label"], "Line name");
    assert_eq!(values(line), ["Line 50"]);
    let alt = by_path("(<http://example.com/uopid> | ^<http://example.com/stopsAt>)");
    assert_eq!(alt["label"], "(uopid | ^stopsAt)");
    assert_eq!(values(alt), ["BE00001", "http://example.com/t2"]);
    let star = by_path("<http://example.com/line>*");
    assert_eq!(
        values(star),
        ["http://example.com/BE1", "http://example.com/L50"]
    );
    let trains = by_path("^<http://example.com/at>+");
    assert_eq!(values(trains), ["http://example.com/t1"]);
    // A path that reaches nothing, and one that is not a path, give no row.
    assert!(
        rows.iter()
            .all(|r| r["label"] != "Nothing" && r["label"] != "Malformed"),
        "{v}"
    );
    // A path row does not take the place of the property's own row.
    assert!(
        rows.iter()
            .any(|r| r["property"] == "http://example.com/line" && r["path"].is_null())
    );
    let page = ureq::get(&format!("{base}/kg/describe?iri={IRI}&format=html"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        page.contains("title=\"(&lt;http://example.com/line&gt; / "),
        "{page}"
    );
}

/// rinf-describe's shapes load as they are (#140): with `RINF_DESCRIBE_DIR` set to a checkout
/// of rinf-describe whose `tests/fixtures/*.nt.gz` are unpacked, its Luxembourg sample gets
/// the groups and rows of its shapes. Skipped without it (the sample is not in this repository).
#[test]
fn rinf_describe_shapes_load() {
    let Ok(root) = std::env::var("RINF_DESCRIBE_DIR") else {
        return;
    };
    let root = std::path::Path::new(&root);
    let engine = Engine::new_tripletrail();
    for f in ["lu-sample.nt", "era-ontology.nt"] {
        let data = std::fs::read(root.join("tests/fixtures").join(f)).unwrap();
        engine.load(RdfFormat::NTriples, data.as_slice()).unwrap();
    }
    let dir = tripletrail_testdir::tempdir("describe-rinf");
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
    shapes("POST", "/customise").call().unwrap();
    for f in std::fs::read_dir(root.join("shapes")).unwrap() {
        let f = f.unwrap().path();
        let name = f.file_name().unwrap().to_str().unwrap().to_owned();
        if let Err(ureq::Error::Status(code, r)) = shapes("PUT", &format!("/files/{name}"))
            .send_string(&std::fs::read_to_string(&f).unwrap())
        {
            panic!("{name}: {code} {}", r.into_string().unwrap());
        }
    }
    let get = |iri: &str| -> Value {
        let started = std::time::Instant::now();
        let r = ureq::get(&format!(
            "{base}/kg/describe?iri={}&format=view",
            iri.replace(':', "%3A").replace('/', "%2F")
        ))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
        eprintln!("{iri}: {:?}", started.elapsed());
        serde_json::from_str(&r).unwrap()
    };
    let groups = |v: &Value| -> Vec<(Option<String>, String)> {
        v["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                (
                    g["tab"].as_str().map(str::to_owned),
                    g["label"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let badge = |v: &Value, label: &str| -> String {
        v["badges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["label"] == label)
            .unwrap_or_else(|| panic!("no badge {label} in {v}"))["values"][0]["value"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let overview = |v: &Value| -> Vec<String> {
        groups(v)
            .into_iter()
            .filter(|(t, _)| t.is_none())
            .map(|(_, l)| l)
            .collect()
    };

    // An operational point: its own shape over the base page of infrastructure elements,
    // whose "parts" row it turns off.
    let op = get("http://data.europa.eu/949/operationalPoint/1f2dd3bd6f");
    assert_eq!(op["label"], "Wecker");
    assert_eq!(badge(&op, "OP ID"), "LUWk");
    assert_eq!(badge(&op, "Number of tracks"), "3");
    assert_eq!(
        overview(&op),
        [
            "",
            "Connections",
            "Operational point",
            "Layout",
            "Identification",
            // What the data marks not applicable or not yet available, last.
            "Data status"
        ]
    );
    let tabs: Vec<(Option<String>, String)> = groups(&op);
    assert!(tabs.contains(&(Some("Tracks and platforms".into()), "Platforms".into())));
    assert!(tabs.contains(&(
        Some("Route compatibility".into()),
        "Over the tracks of the operational point".into()
    )));
    assert!(!tabs.iter().any(|(_, l)| l == "Parts"), "{tabs:?}");
    // The tracks as a table, by the columns of its sh:node (without the hidden geometry).
    let tracks = op["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["rows"].as_array().unwrap())
        .find(|r| r["label"] == "Tracks")
        .unwrap();
    let columns: Vec<&str> = tracks["columns"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["hidden"] != true)
        .map(|c| c["label"].as_str().unwrap())
        .collect();
    assert_eq!(columns[0], "Track", "{columns:?}");
    assert!(!columns.contains(&"Geometry"));
    assert_eq!(tracks["cells"].as_array().unwrap().len(), 3);
    assert!(
        !tracks["cells"][0][0].as_array().unwrap().is_empty(),
        "{tracks}"
    );
    // The validity widget reads its begin and end paths (shared named paths of common.ttl).
    let validity = op["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["rows"].as_array().unwrap())
        .find(|r| r["widget"] == "ValidityViewer")
        .unwrap_or_else(|| panic!("no validity in {op}"));
    assert_eq!(
        validity["params"]["begin"][0]["value"], "2025-12-31",
        "{validity}"
    );

    // A section of line.
    let sol = get("http://data.europa.eu/949/sectionOfLine/14fd15444d");
    assert_eq!(sol["label"], "Michelau <-> Goebelsmuhle");
    assert_eq!(badge(&sol, "Number of tracks"), "2");
    assert!(!groups(&sol).iter().any(|(_, l)| l == "Parts"));
    assert_eq!(badge(&sol, "Length"), "4.506");
}

/// A server over `ttl`, with an admin token for the shapes, and its folder.
fn serve(ttl: &str, name: &str) -> (String, tripletrail_testdir::TestDir) {
    let dir = tripletrail_testdir::tempdir(&format!("describe-{name}"));
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
    ureq::post(&format!("{base}/$/datasets/kg/shapes/customise"))
        .set("Authorization", "Bearer t")
        .call()
        .unwrap();
    (base, dir)
}

fn put_shapes(base: &str, ttl: &str) {
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Place.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(ttl)
        .unwrap();
}

fn view_of(base: &str, local: &str) -> Value {
    let r = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2F{local}&format=view"
    ))
    .call()
    .unwrap();
    serde_json::from_str(&r.into_string().unwrap()).unwrap()
}

fn rows_of(v: &Value) -> Vec<Value> {
    v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["rows"].as_array().unwrap().clone())
        .collect()
}

#[test]
fn shared_paths_that_fan_out_give_no_row() {
    // 8 levels of a named path that is 64 alternatives of the next: 64^8 if unfolded.
    let mut shapes = String::from(
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
           sh:property [ sh:path ex:n0 ; sh:name \"Fan\" ] ;\n\
           sh:property [ sh:path ( ex:line ex:name ) ; sh:name \"Line name\" ] .\n",
    );
    for i in 0..8 {
        let next = format!("ex:n{} ", i + 1).repeat(64);
        shapes.push_str(&format!("ex:n{i} sh:alternativePath ( {next}) .\n"));
    }
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n\
         ex:BE1 a ex:Place ; ex:n8 \"deep\" ; ex:line ex:L1 . ex:L1 ex:name \"L1\" .\n",
        "fan",
    );
    put_shapes(&base, &shapes);
    let started = std::time::Instant::now();
    let v = view_of(&base, "BE1");
    // Generous for a debug build on a busy runner: unbounded, it never answers.
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    let rows = rows_of(&v);
    assert!(rows.iter().all(|r| r["label"] != "Fan"), "{v}");
    // The other rows of the shape are there.
    assert!(rows.iter().any(|r| r["label"] == "Line name"));
}

#[test]
fn summaries_over_many_linked_resources_are_whole_or_say_so() {
    let summed = |n: u32| {
        let mut ttl = String::from("@prefix ex: <http://example.com/> .\nex:BE1 a ex:Place .\n");
        for i in 0..n {
            ttl.push_str(&format!(
                "ex:BE1 ex:part ex:k{i} . ex:k{i} ex:len \"{i}\" .\n"
            ));
        }
        ttl
    };
    let shapes = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
        @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
        ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
         sh:property [ sh:path ex:len ; sh:name \"Lens\" ; rd:over ex:part ; rd:aggregate \"distinct-count\" ] ;\n\
         sh:property [ sh:path ex:len ; sh:name \"Sum\" ; rd:over ex:part ; rd:aggregate \"sum\" ] ;\n\
         sh:property [ sh:path ex:len ; sh:name \"Range\" ; rd:over ex:part ; rd:aggregate \"range\" ] .\n";
    // 60,000 parts: every one is read, and the three summaries are three rows.
    let (base, dir) = serve(&summed(60_000), "sum");
    put_shapes(&base, shapes);
    let started = std::time::Instant::now();
    let v = view_of(&base, "BE1");
    assert!(
        // Generous for a debug build on a busy runner (0.8 s in a release build).
        started.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    let rows = rows_of(&v);
    let row = |label: &str| {
        rows.iter()
            .find(|r| r["label"] == label)
            .unwrap_or_else(|| panic!("no {label}"))
            .clone()
    };
    assert_eq!(row("Sum")["values"][0]["value"], "1799970000");
    assert_eq!(row("Sum")["truncated"], false);
    assert_eq!(row("Range")["values"][0]["value"], "0\u{2013}59999");
    assert_eq!(row("Lens")["aggregate"], "distinct-count");
    let _ = std::fs::remove_dir_all(&dir);

    // 150,000 parts: more than are read, and the summary says it is of the first ones.
    let (base, _dir) = serve(&summed(150_000), "sum-big");
    put_shapes(&base, shapes);
    let v = view_of(&base, "BE1");
    let rows = rows_of(&v);
    let sum = rows.iter().find(|r| r["label"] == "Sum").unwrap();
    assert_eq!(sum["truncated"], true, "{sum}");
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FBE1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(page.contains("Not every linked resource was read"));
}

#[test]
fn a_node_shape_gives_the_columns_of_a_table() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n\
         ex:BE1 a ex:Place ; ex:track ex:T1 , ex:T2 .\n\
         ex:T1 ex:id \"1\" ; ex:speed 120 ; ex:wkt \"secret\" .\n\
         ex:T2 ex:id \"2\" ; ex:speed 80 .\n",
        "table",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
           sh:property [ sh:path ex:track ; sh:name \"Tracks\" ; dash:viewer dash:ValueTableViewer ;\n\
             sh:node ex:TrackRow ] .\n\
         ex:TrackRow a sh:NodeShape ;\n\
           sh:property [ sh:path ex:speed ; sh:name \"Speed\" ; rd:unit \"km/h\" ; sh:order 2 ] ;\n\
           sh:property [ sh:path ex:id ; sh:name \"Track\" ; sh:order 1 ] ;\n\
           sh:property [ sh:path ex:wkt ; rd:hidden \"true\" ] .\n",
    );
    let v = view_of(&base, "BE1");
    let rows = rows_of(&v);
    let tracks = rows.iter().find(|r| r["label"] == "Tracks").unwrap();
    assert_eq!(tracks["columns"][0]["label"], "Track");
    assert_eq!(tracks["columns"][1]["unit"], "km/h");
    // A hidden column is read for widgets but marked hidden, and the page leaves it out.
    assert_eq!(tracks["columns"].as_array().unwrap().len(), 3);
    assert_eq!(tracks["columns"][2]["hidden"], true);
    assert_eq!(tracks["columns"][0]["hidden"], false);
    assert_eq!(tracks["cells"][0][0][0]["value"], "1");
    assert_eq!(tracks["cells"][1][1][0]["value"], "80");
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FBE1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains("<th>Speed <span class=\"unit\">(km/h)</span></th>"),
        "{page}"
    );
    assert!(!page.contains("secret"));
}

#[test]
fn a_table_says_when_its_cells_were_not_all_read() {
    // 100 tracks of 2,000 values each: more than one table reads.
    let mut ttl = String::from("@prefix ex: <http://example.com/> .\nex:BE1 a ex:Place .\n");
    for t in 0..100 {
        ttl.push_str(&format!("ex:BE1 ex:track ex:T{t} .\n"));
        for v in 0..2_000 {
            ttl.push_str(&format!("ex:T{t} ex:x {v} .\n"));
        }
    }
    let (base, _dir) = serve(&ttl, "table-budget");
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
           sh:property [ sh:path ex:track ; sh:name \"Tracks\" ; sh:node ex:TrackRow ] .\n\
         ex:TrackRow a sh:NodeShape ;\n\
           sh:property [ sh:name \"No path\" ; sh:deactivated true ] ;\n\
           sh:property [ sh:path ex:x ; sh:name \"X\" ] .\n",
    );
    let v = view_of(&base, "BE1");
    let rows = rows_of(&v);
    let tracks = rows.iter().find(|r| r["label"] == "Tracks").unwrap();
    assert_eq!(tracks["tableTruncated"], true, "{tracks}");
    assert_eq!(tracks["columns"].as_array().unwrap().len(), 1);
    let cells = tracks["cells"].as_array().unwrap();
    assert_eq!(cells[0][0].as_array().unwrap().len(), 10);
    assert!(cells[99][0].as_array().unwrap().is_empty());
}

#[test]
fn a_widget_row_reads_its_named_paths() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         ex:BE1 a ex:Place ; ex:validity [ ex:begin \"2020-01-01\"^^xsd:date ; ex:end \"2030-12-31\"^^xsd:date ] .\n",
        "widget",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
           sh:property [ sh:name \"Validity\" ; dash:viewer rd:ValidityViewer ;\n\
             rd:begin ( ex:validity ex:begin ) ; rd:end ( ex:validity ex:end ) ] ;\n\
           sh:property [ sh:name \"Nothing\" ; dash:viewer rd:ValidityViewer ; rd:begin ex:none ] .\n",
    );
    let v = view_of(&base, "BE1");
    let rows = rows_of(&v);
    let validity = rows.iter().find(|r| r["label"] == "Validity").unwrap();
    assert_eq!(validity["widget"], "ValidityViewer");
    assert_eq!(validity["params"]["begin"][0]["value"], "2020-01-01");
    assert_eq!(validity["params"]["end"][0]["value"], "2030-12-31");
    // A widget whose paths reach nothing gives no row.
    assert!(rows.iter().all(|r| r["label"] != "Nothing"));
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FBE1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(page.contains("class=\"validity\">from "), "{page}");
    assert!(page.contains(" until "));
}

#[test]
fn rows_of_equal_order_come_in_the_same_order_on_every_server() {
    let mut data = String::from("@prefix ex: <http://example.com/> .\nex:BE1 a ex:Place");
    let mut shapes = String::from(
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place",
    );
    for i in 0..12 {
        data.push_str(&format!(" ; ex:p{i} \"{i}\""));
        shapes.push_str(&format!(
            " ;\n  sh:property [ sh:path ex:p{i} ; sh:name \"P{i}\" ]"
        ));
    }
    data.push_str(" .\n");
    shapes.push_str(" .\n");
    let order = |name: &str| {
        let (base, dir) = serve(&data, name);
        put_shapes(&base, &shapes);
        let labels: Vec<String> = rows_of(&view_of(&base, "BE1"))
            .iter()
            .map(|r| r["label"].as_str().unwrap().to_owned())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        labels
    };
    let first = order("order-a");
    for n in 0..3 {
        assert_eq!(order(&format!("order-{n}")), first);
    }
}

#[test]
fn documents_scale_bars_and_concept_trees_are_drawn() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n\
         ex:T1 a ex:Tunnel ; ex:doc ex:D1 ; ex:length 620 .\n\
         ex:D1 ex:url \"https://example.org/files/tunnel.pdf?v=2\" ; ex:name \"Tunnel description\" .\n\
         ex:C a skos:Concept ; skos:notation \"1\" .\n\
         ex:C1 a skos:Concept ; skos:broader ex:C ; skos:notation \"1.1\" .\n\
         ex:C2 a skos:Concept ; skos:broader ex:C ; skos:notation \"1.2\" .\n\
         ex:C11 a skos:Concept ; skos:broader ex:C1 ; skos:notation \"1.1.1\" .\n\
         ex:X a skos:Concept ; skos:broader ex:Y . ex:Y skos:broader ex:X .\n",
        "widgets",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         @prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n\
         ex:TunnelShape a sh:NodeShape ; sh:targetClass ex:Tunnel ;\n\
           sh:property [ sh:path ex:doc ; sh:name \"Documents\" ; dash:viewer rd:DocumentViewer ; sh:node ex:Card ] ;\n\
           sh:property [ sh:path ex:length ; sh:name \"Length\" ; rd:unit \"m\" ; dash:viewer rd:ScaleBarViewer ;\n\
             rd:marks \"400 Passenger train; 740 Freight train; oops\" ] .\n\
         ex:Card a sh:NodeShape ;\n\
           sh:property [ sh:path ex:url ; rd:key \"url\" ] ; sh:property [ sh:path ex:name ; rd:key \"name\" ] .\n\
         ex:ConceptShape a sh:NodeShape ; sh:targetClass skos:Concept ;\n\
           sh:property [ sh:path [ sh:alternativePath ( [ sh:zeroOrMorePath skos:broader ]\n\
             ( [ sh:zeroOrMorePath skos:broader ] [ sh:inversePath skos:broader ] ) ) ] ;\n\
             sh:name \"Branch\" ; rd:key \"tree\" ; dash:viewer rd:ConceptTreeViewer ; sh:node ex:TreeRow ] .\n\
         ex:TreeRow a sh:NodeShape ;\n\
           sh:property [ sh:path skos:broader ; rd:key \"broader\" ; sh:order 1 ] ;\n\
           sh:property [ sh:path skos:notation ; rd:key \"notation\" ; sh:order 2 ] .\n",
    );
    let html = |local: &str| {
        ureq::get(&format!(
            "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2F{local}&format=html"
        ))
        .call()
        .unwrap()
        .into_string()
        .unwrap()
    };
    let rows = rows_of(&view_of(&base, "T1"));
    let length = rows.iter().find(|r| r["label"] == "Length").unwrap();
    // A mark that does not start with a number is left out.
    assert_eq!(length["marks"].as_array().unwrap().len(), 2);
    assert_eq!(length["marks"][1]["label"], "Freight train");
    let page = html("T1");
    assert!(
        page.contains("<span class=\"ftype\">PDF</span><a href=\"https://example.org/files/tunnel.pdf?v=2\" rel=\"noopener\">Tunnel description</a>"),
        "{page}"
    );
    assert!(page.contains("<strong>620</strong> m"), "{page}");
    assert!(page.contains("mark-label reached\">Passenger train 400 m"));
    assert!(page.contains("<span class=\"mark-label\">Freight train 740 m"));
    // The branch of 1.1: its top concept, then 1.1 and 1.2 under it, and 1.1.1 under 1.1.
    let page = html("C1");
    let tree = &page[page.find("<ul class=\"tree\">").expect(&page)..];
    let at = |s: &str| tree.find(s).unwrap_or_else(|| panic!("{s} in {tree}"));
    assert!(at(">1<") < at(">1.1<") && at(">1.1<") < at(">1.1.1<") && at(">1.1.1<") < at(">1.2<"));
    assert!(tree.contains("<strong><a"), "{tree}");
    // Concepts that are each other's broader concept are drawn, once each.
    let page = html("X");
    let tree = &page[page.find("<ul class=\"tree\">").expect(&page)..];
    let tree = &tree[..tree.find("</td>").unwrap()];
    assert_eq!(tree.matches("<li>").count(), 2, "{tree}");
}

#[test]
fn facilities_and_key_figures_are_tiles() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:hasWalkway rdfs:label \"Walkway\" .\n\
         ex:T1 a ex:Tunnel ; ex:hasWalkway true ; ex:hasEmergencyPlan false ; ex:compliance ex:Partly ; ex:length 1e308 ;\n\
           ex:tree \"loose\" , ex:T1 .\n\
         ex:S1 a ex:Section ; ex:tunnel ex:T1 . ex:S2 a ex:Section ; ex:tunnel ex:T1 . ex:S3 a ex:Section ; ex:tunnel ex:T1 .\n",
        "tiles",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n\
         ex:TunnelShape a sh:NodeShape ; sh:targetClass ex:Tunnel ;\n\
           sh:property [ sh:name \"Safety\" ; rd:key \"safety\" ; dash:viewer rd:FacilitiesViewer ;\n\
             rd:facility ex:hasWalkway, ex:hasEmergencyPlan, ex:compliance, ex:hasLighting ] ;\n\
           sh:property [ sh:name \"Key figures\" ; rd:key \"figures\" ; dash:viewer rd:KeyFiguresViewer ;\n\
             rd:figures \"sections nothing\" ] ;\n\
           sh:property [ sh:path rdf:type ; sh:name \"Sections\" ; rd:key \"sections\" ; rd:hidden \"true\" ;\n\
             rd:over [ sh:inversePath ex:tunnel ] ; rd:aggregate \"count\" ] ;\n\
           sh:property [ sh:path ex:length ; sh:name \"Length\" ; dash:viewer rd:ScaleBarViewer ] ;\n\
           sh:property [ sh:path ex:tree ; sh:name \"Tree\" ; dash:viewer rd:ConceptTreeViewer ; sh:node ex:Row ] .\n\
         ex:Row a sh:NodeShape ; sh:property [ sh:path ex:broader ; rd:key \"broader\" ] .\n",
    );
    let rows = rows_of(&view_of(&base, "T1"));
    let safety = rows.iter().find(|r| r["label"] == "Safety").unwrap();
    // A facility is named by its property's label.
    assert_eq!(safety["params"]["Walkway"][0]["value"], "true");
    let figures = rows
        .iter()
        .find(|r| r["label"] == "Key figures")
        .unwrap_or_else(|| panic!("{rows:#?}"));
    let figures = figures["figures"].as_array().unwrap();
    assert_eq!(figures.len(), 1, "{figures:?}");
    assert_eq!(figures[0]["label"], "Sections");
    assert_eq!(figures[0]["values"][0]["value"], "3");
    // A hidden summary row stays hidden outside the widget, and a facility has no row of
    // its own.
    assert!(
        rows.iter()
            .all(|r| r["label"] != "Sections" && r["label"] != "Walkway")
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FT1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains(
            "<span class=\"facility yes\"><span class=\"flag\">\u{2713}</span> Walkway</span>"
        ),
        "{page}"
    );
    assert!(page.contains("<span class=\"facility no\">"));
    assert!(page.contains("<span class=\"facility other\">"));
    assert!(page.contains(
        "<span class=\"facility unknown\"><span class=\"flag\">?</span> hasLighting</span>"
    ));
    assert!(
        page.contains(
            "<div class=\"figure-value\"><span class=\"lit\">3</span></div><div class=\"figure-label\">Sections</div>"
        ),
        "{page}"
    );
    // A huge number stays short.
    assert!(page.contains("<strong>1e308</strong>"), "{page}");
    // A tree says what it left out.
    assert!(page.contains("1 value is not a concept"), "{page}");
}

#[test]
fn the_map_is_interactive_with_the_web_ui() {
    let ttl = r#"
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix ex: <http://example.com/> .
ex:st geo:hasGeometry ex:g1 , ex:g2 .
ex:g1 geo:asWKT "POINT(4.35 50.85)"^^geo:wktLiteral .
ex:g2 geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/25832> MULTIPOLYGON(((499000 6599000, 501000 6599000, 501000 6601000, 499000 6599000)))"^^geo:wktLiteral .
ex:far ex:at "POINT(500 900)"^^geo:wktLiteral .
"#;
    let page_with = |ui: bool, tiles: Option<&str>| {
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
        let server = Server::bind(
            "127.0.0.1:0",
            vec![("kg".to_owned(), engine)],
            Options {
                ui,
                map_tiles: tiles.map(str::to_owned),
                map_attribution: tiles
                    .map(|_| "&copy; <a href=\"https://example.org\">Tiles</a>".to_owned()),
                ..Options::default()
            },
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        let get = |local: &str| {
            ureq::get(&format!(
                "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2F{local}&format=html"
            ))
            .call()
            .unwrap()
            .into_string()
            .unwrap()
        };
        (base.clone(), get("st"), get("far"))
    };
    let (base, page, far) = page_with(true, None);
    assert!(
        page.contains("<div class=\"live-map\" id=\"mg-live-map\" hidden></div>"),
        "{page}"
    );
    assert!(page.contains("<script type=\"module\" src=\"/$/ui/describe-map.js\">"));
    // Both geometries, the projected one in longitude/latitude, with their box.
    let start = page.find("id=\"mg-map-data\">").unwrap() + "id=\"mg-map-data\">".len();
    let end = start + page[start..].find("</script>").unwrap();
    let data: Value = serde_json::from_str(&page[start..end]).unwrap();
    assert_eq!(data["features"].as_array().unwrap().len(), 2, "{data}");
    assert_eq!(data["features"][1]["geometry"]["type"], "MultiPolygon");
    let bbox: Vec<f64> = data["bbox"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert!(
        bbox[0] > 4.0 && bbox[2] < 10.0 && bbox[1] > 50.0 && bbox[3] < 60.0,
        "{bbox:?}"
    );
    // Coordinates that are not longitude/latitude keep the drawing only.
    assert!(
        far.contains("<svg class=\"map\"") && !far.contains("mg-live-map"),
        "{far}"
    );
    // The script and MapLibre are served by the web UI.
    let js = ureq::get(&format!("{base}/$/ui/describe-map.js"))
        .call()
        .unwrap();
    assert_eq!(js.content_type(), "text/javascript");
    for f in [
        "maplibre-gl.mjs",
        "maplibre-gl-worker.mjs",
        "maplibre-gl.css",
        "LICENSE.txt",
    ] {
        let r = ureq::get(&format!("{base}/$/ui/vendor/maplibre/{f}"))
            .call()
            .unwrap();
        assert_eq!(r.status(), 200, "{f}");
    }
    // Without the web UI, the page draws the map only.
    let (_, page, _) = page_with(false, None);
    assert!(
        page.contains("<svg class=\"map\"") && !page.contains("mg-live-map"),
        "{page}"
    );
    // With --map-tiles, the map names its tiles and attribution; the page sends no Referer.
    let (_, page, _) = page_with(true, Some("https://tiles.example.org/{z}/{x}/{y}.png"));
    assert!(
        page.contains(
            "id=\"mg-live-map\" data-tiles=\"https://tiles.example.org/{z}/{x}/{y}.png\" \
             data-attribution=\"&amp;copy; &lt;a href=&quot;https://example.org&quot;&gt;Tiles&lt;/a&gt;\" hidden>"
        ),
        "{page}"
    );
    assert!(page.contains("<meta name=\"referrer\" content=\"strict-origin\">"));
}

#[test]
fn railway_locations_and_step_charts_are_drawn() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n\
         ex:S1 a ex:Section ; ex:from ex:OpA ; ex:start [ ex:km 53 ; ex:offset 240 ] ;\n\
           ex:to ex:OpB ; ex:end [ ex:km 54.9995 ] ;\n\
           ex:gradient \"2.5(300),0(150),-4(550)\" ; ex:bad \"1(2),x(3)\" .\n",
        "line",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:SectionShape a sh:NodeShape ; sh:targetClass ex:Section ;\n\
           sh:property [ sh:path ex:from ; sh:name \"From\" ; dash:viewer rd:LrsLocationViewer ;\n\
             rd:km ( ex:start ex:km ) ; rd:offset ( ex:start ex:offset ) ] ;\n\
           sh:property [ sh:path ex:to ; sh:name \"To\" ; dash:viewer rd:LrsLocationViewer ; rd:km ( ex:end ex:km ) ] ;\n\
           sh:property [ sh:path ex:gradient ; sh:name \"Gradient\" ; dash:viewer rd:StepChartViewer ;\n\
             rd:unit \"\u{2030}\" ; rd:lengthUnit \"m\" ] ;\n\
           sh:property [ sh:path ex:bad ; sh:name \"Bad\" ; dash:viewer rd:StepChartViewer ] .\n",
    );
    let rows = rows_of(&view_of(&base, "S1"));
    let gradient = rows.iter().find(|r| r["label"] == "Gradient").unwrap();
    assert_eq!(gradient["settings"]["lengthUnit"], "m");
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FS1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains("<span class=\"km\">km 53+240</span>"),
        "{page}"
    );
    // Metres that round up to the next kilometre carry over.
    assert!(
        page.contains("<span class=\"km\">km 55+000</span>"),
        "{page}"
    );
    assert!(
        page.contains("aria-label=\"Profile over 1000 m, from -4 to 4 \u{2030}\""),
        "{page}"
    );
    assert_eq!(page.matches("<rect class=\"bar ").count(), 3);
    assert!(page.contains("<title>+2.5 \u{2030} over 300 m, from 0 m</title>"));
    assert!(page.contains("<rect class=\"bar flat\""));
    // A profile that does not parse is shown as it is.
    assert!(page.contains("1(2),x(3)"));
}

#[test]
fn a_line_lists_its_stops_in_km_order() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:L1 a ex:Line ; ex:section ex:S2 , ex:S1 .\n\
         ex:S1 ex:from ex:A ; ex:to ex:B ; ex:startKm 0 ; ex:endKm 5 ; ex:endOffset 120 .\n\
         ex:S2 ex:from ex:B ; ex:to ex:C ; ex:startKm 5 ; ex:startOffset 120 ; ex:endKm 12 .\n\
         ex:A rdfs:label \"Alpha\" . ex:B rdfs:label \"Bravo\" . ex:C rdfs:label \"Charlie\" .\n\
         ex:L2 a ex:Line .\n",
        "stops",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:LineShape a sh:NodeShape ; sh:targetClass ex:Line ;\n\
           sh:property [ sh:path ex:section ; sh:name \"Sections\" ; rd:key \"sections\" ; sh:node ex:Row ] ;\n\
           sh:property [ sh:name \"Stops\" ; rd:key \"stops\" ; dash:viewer rd:LineStopsViewer ; rd:table \"sections\" ] .\n\
         ex:Row a sh:NodeShape ;\n\
           sh:property [ sh:path ex:from ; rd:key \"from\" ; sh:order 1 ] ;\n\
           sh:property [ sh:path ex:to ; rd:key \"to\" ; sh:order 2 ] ;\n\
           sh:property [ sh:path ex:startKm ; rd:key \"start-km\" ; sh:order 3 ] ;\n\
           sh:property [ sh:path ex:startOffset ; rd:key \"start-offset\" ; rd:hidden \"true\" ; sh:order 4 ] ;\n\
           sh:property [ sh:path ex:endKm ; rd:key \"end-km\" ; rd:hidden \"true\" ; sh:order 5 ] ;\n\
           sh:property [ sh:path ex:endOffset ; rd:key \"end-offset\" ; rd:hidden \"true\" ; sh:order 6 ] .\n",
    );
    let rows = rows_of(&view_of(&base, "L1"));
    let stops = rows.iter().find(|r| r["label"] == "Stops").unwrap();
    // The sections row is on the page too: the widget names it by key.
    assert_eq!(
        stops["rows"]["sections"],
        serde_json::json!({"key": "sections", "shown": true})
    );
    let sections = rows.iter().find(|r| r["key"] == "sections").unwrap();
    assert_eq!(sections["label"], "Sections");
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FL1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    let list = &page[page.find("<ol class=\"stops\">").expect(&page)..];
    let at = |s: &str| list.find(s).unwrap_or_else(|| panic!("{s} in {list}"));
    assert!(at("km 0+000") < at(">Alpha<") && at(">Alpha<") < at("km 5+120"));
    assert!(
        at("km 5+120") < at(">Bravo<")
            && at(">Bravo<") < at("km 12+000")
            && at("km 12+000") < at(">Charlie<")
    );
    assert_eq!(
        list[..list.find("</ol>").unwrap()].matches("<li>").count(),
        3
    );
    // Hidden columns stay off the sections table.
    assert!(!page.contains("end-km"));
    // A line without sections has no stops row.
    assert!(
        rows_of(&view_of(&base, "L2"))
            .iter()
            .all(|r| r["label"] != "Stops")
    );
}

#[test]
fn a_section_draws_its_route_and_what_lies_along_it() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:S1 a ex:Section ; ex:from ex:A ; ex:to ex:B ; ex:length 4.5 ; ex:startKm 10 ; ex:endKm 14 ; ex:endOffset 500 ;\n\
           ex:tunnel ex:T1 , ex:T2 ; ex:post ex:P1 , ex:P2 , ex:P3 .\n\
         ex:A rdfs:label \"Alpha\" . ex:B rdfs:label \"Bravo\" .\n\
         ex:T1 rdfs:label \"Long tunnel\" ; ex:sKm 11 ; ex:eKm 12 ; ex:eOff 250 .\n\
         ex:T2 ex:sKm 30 ; ex:eKm 31 .\n\
         ex:P1 ex:km 12 . ex:P2 ex:km 13 . ex:P3 ex:km 99 .\n",
        "along",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:SectionShape a sh:NodeShape ; sh:targetClass ex:Section ;\n\
           sh:property [ sh:name \"Route\" ; rd:key \"route\" ; sh:order 1 ; dash:viewer rd:RouteDiagramViewer ;\n\
             rd:start ex:from ; rd:end ex:to ; rd:length ex:length ; rd:unit \"km\" ;\n\
             rd:startKm ex:startKm ; rd:endKm ex:endKm ; rd:endOffset ex:endOffset ] ;\n\
           sh:property [ sh:name \"Along the line\" ; rd:key \"along\" ; sh:order 2 ; dash:viewer rd:AlongLineViewer ;\n\
             rd:table \"route\" ; rd:markers \"tunnels posts\" ] ;\n\
           sh:property [ sh:path ex:tunnel ; sh:name \"Tunnels\" ; rd:key \"tunnels\" ; rd:hidden \"true\" ; sh:node ex:Stretch ] ;\n\
           sh:property [ sh:path ex:post ; sh:name \"Posts\" ; rd:key \"posts\" ; rd:hidden \"true\" ; sh:node ex:Point ] .\n\
         ex:Stretch a sh:NodeShape ;\n\
           sh:property [ sh:path ex:sKm ; rd:key \"start-km\" ] ; sh:property [ sh:path ex:eKm ; rd:key \"end-km\" ] ;\n\
           sh:property [ sh:path ex:eOff ; rd:key \"end-offset\" ] .\n\
         ex:Point a sh:NodeShape ; sh:property [ sh:path ex:km ; rd:key \"km\" ] .\n",
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FS1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(page.contains(">Alpha</a> <span class=\"km\">km 10+000</span></span><span class=\"route-line\">4.5 km</span>"), "{page}");
    assert!(page.contains(">Bravo</a> <span class=\"km\">km 14+500</span>"));
    let strip = &page[page.find("<svg class=\"along\"").expect(&page)..];
    let strip = &strip[..strip.find("</div>").unwrap()];
    // The tunnel inside the section is a band, the one outside is left out; two of the three
    // posts are ticks.
    assert_eq!(
        strip.matches("<rect class=\"mark lane-0\"").count(),
        1,
        "{strip}"
    );
    assert!(
        strip.contains("<title>Tunnels: Long tunnel at km 11+000 to km 12+250</title>"),
        "{strip}"
    );
    assert_eq!(
        strip.matches("<line class=\"mark lane-1\"").count(),
        2,
        "{strip}"
    );
    assert!(strip.contains("Tunnels (1)") && strip.contains("Posts (2)"));
    // The hidden marker rows stay off the page.
    assert!(!page.contains("<th title=\"http://example.com/tunnel\">"));
}

#[test]
fn a_line_profile_has_a_lane_per_column() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:L1 a ex:Line ; ex:section ex:S1 , ex:S2 , ex:S3 , ex:S4 .\n\
         ex:S1 ex:from ex:A ; ex:to ex:B ; ex:startKm 0 ; ex:length 10 ; ex:speed 120 , 160 ; ex:energy ex:AC .\n\
         ex:S2 ex:from ex:B ; ex:to ex:C ; ex:startKm 10 ; ex:endKm 25 ; ex:speed 80 ; ex:energy ex:DC .\n\
         ex:S3 ex:from ex:C ; ex:to ex:D ; ex:speed 60 .\n\
         ex:S4 ex:from ex:D ; ex:to ex:E ; ex:startKm 1e308 ; ex:endKm -1e308 .\n\
         ex:AC rdfs:label \"25 kV AC\" . ex:DC rdfs:label \"3 kV DC\" .\n",
        "profile",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:LineShape a sh:NodeShape ; sh:targetClass ex:Line ;\n\
           sh:property [ sh:path ex:section ; sh:name \"Sections\" ; rd:key \"sections\" ; sh:node ex:Row ] ;\n\
           sh:property [ sh:name \"Line profile\" ; rd:key \"profile\" ; dash:viewer rd:LineProfileViewer ;\n\
             rd:table \"sections\" ; rd:lanes \"speed energy gradient\" ; rd:profiles \"gradient\" ] .\n\
         ex:Row a sh:NodeShape ;\n\
           sh:property [ sh:path ex:from ; rd:key \"from\" ; sh:order 1 ] ;\n\
           sh:property [ sh:path ex:to ; rd:key \"to\" ; sh:order 2 ] ;\n\
           sh:property [ sh:path ex:startKm ; rd:key \"start-km\" ; sh:order 3 ] ;\n\
           sh:property [ sh:path ex:endKm ; rd:key \"end-km\" ; rd:hidden \"true\" ; sh:order 4 ] ;\n\
           sh:property [ sh:path ex:length ; rd:key \"length\" ; sh:order 5 ] ;\n\
           sh:property [ sh:path ex:speed ; sh:name \"Max speed\" ; rd:key \"speed\" ; rd:unit \"km/h\" ; sh:order 6 ] ;\n\
           sh:property [ sh:path ex:energy ; sh:name \"Energy\" ; rd:key \"energy\" ; sh:order 7 ] .\n",
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FL1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    let svg = &page[page.find("<svg class=\"profile\"").expect(&page)..];
    let svg = &svg[..svg.find("</svg>").unwrap()];
    assert!(
        svg.contains("aria-label=\"The line from km 0+000 to km 25+000\""),
        "{svg}"
    );
    // Numbers as steps (the highest of a section), other values as bands.
    assert_eq!(svg.matches("<rect class=\"step\"").count(), 2, "{svg}");
    assert!(svg.contains("<title>Max speed: 160 km/h</title>"));
    assert!(
        svg.contains("<title>Energy: 25 kV AC</title>")
            && svg.contains("<title>Energy: 3 kV DC</title>")
    );
    assert_eq!(svg.matches("class=\"op\"").count(), 3, "{svg}");
    assert!(
        page.contains("2 sections of line along 25 km; sections without a km"),
        "{page}"
    );
}

#[test]
fn a_post_shows_where_it_is_on_its_line() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:K3 a ex:Post ; ex:line ex:L ; ex:km 3.2 .\n\
         ex:K1 ex:line ex:L ; ex:km 1 . ex:K9 ex:line ex:L ; ex:km 9 .\n\
         ex:S1 ex:onLine ex:L ; ex:from ex:A ; ex:to ex:B ; ex:startKm 0 ; ex:endKm 2 ; ex:endOffset 500 .\n\
         ex:S2 ex:onLine ex:L ; ex:from ex:B ; ex:to ex:C ; ex:startKm 2 ; ex:startOffset 500 ; ex:endKm 8 .\n\
         ex:A rdfs:label \"Alpha\" . ex:B rdfs:label \"Bravo\" . ex:C rdfs:label \"Charlie\" .\n",
        "position",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PostShape a sh:NodeShape ; sh:targetClass ex:Post ;\n\
           sh:property [ sh:name \"Position on the line\" ; rd:key \"position\" ; dash:viewer rd:LinePositionViewer ;\n\
             rd:table \"sections\" ; rd:posts \"posts\" ] ;\n\
           sh:property [ sh:path ( ex:line [ sh:inversePath ex:onLine ] ) ; rd:key \"sections\" ; rd:hidden \"true\" ; sh:node ex:Row ] ;\n\
           sh:property [ sh:path ( ex:line [ sh:inversePath ex:line ] ) ; rd:key \"posts\" ; rd:hidden \"true\" ;\n\
             sh:node [ sh:property [ sh:path ex:km ; rd:key \"km\" ] ] ] .\n\
         ex:Row a sh:NodeShape ;\n\
           sh:property [ sh:path ex:from ; rd:key \"from\" ] ; sh:property [ sh:path ex:to ; rd:key \"to\" ] ;\n\
           sh:property [ sh:path ex:startKm ; rd:key \"start-km\" ] ; sh:property [ sh:path ex:startOffset ; rd:key \"start-offset\" ] ;\n\
           sh:property [ sh:path ex:endKm ; rd:key \"end-km\" ] ; sh:property [ sh:path ex:endOffset ; rd:key \"end-offset\" ] .\n",
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FK3&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains("aria-label=\"This post at km 3+200 on a line from km 0+000 to km 9+000\""),
        "{page}"
    );
    // The two other posts as ticks, the three stops once each.
    assert_eq!(page.matches("<line class=\"post\"").count(), 2, "{page}");
    assert_eq!(page.matches("<circle class=\"stop\"").count(), 3);
    assert!(
        page.contains("After <a")
            && page.contains(">Bravo</a> <span class=\"km\">km 2+500</span> (0.7 km away)"),
        "{page}"
    );
    assert!(page.contains(">Charlie</a> <span class=\"km\">km 8+000</span> (4.8 km away)"));
}

#[test]
fn platforms_and_cross_sections_are_sketched() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:P1 a ex:Platform ; ex:height ex:H550 ; ex:length 220 ; ex:curved true .\n\
         ex:H550 rdfs:label \"550\" .\n\
         ex:P2 a ex:Platform ; ex:height ex:Hrange ; ex:length 150 .\n ex:Hrange rdfs:label \"between 300 and 380\" .\n\
         ex:T1 a ex:Track ; ex:gauge ex:G1435 ; ex:profile ex:GC ; ex:cls ex:C1 ; ex:inclination \"1:20\" .\n\
         ex:G1435 rdfs:label \"1435 mm\" . ex:GC rdfs:label \"GC\" .\n\
         ex:C1 ex:wireMin 5.0 ; ex:wireMax 6.2 .\n\
         ex:T2 a ex:Track .\n",
        "sketches",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlatformShape a sh:NodeShape ; sh:targetClass ex:Platform ;\n\
           sh:property [ sh:name \"Platform\" ; rd:key \"sketch\" ; dash:viewer rd:PlatformViewer ;\n\
             rd:height ex:height ; rd:length ex:length ; rd:curved ex:curved ] .\n\
         ex:TrackShape a sh:NodeShape ; sh:targetClass ex:Track ;\n\
           sh:property [ sh:name \"Cross-section\" ; rd:key \"xs\" ; dash:viewer rd:CrossSectionViewer ;\n\
             rd:gauge ex:gauge ; rd:loadingGauge ex:profile ; rd:inclination ex:inclination ;\n\
             rd:wireMin ( ex:cls ex:wireMin ) ; rd:wireMax ( ex:cls ex:wireMax ) ] .\n",
    );
    let get = |local: &str| {
        ureq::get(&format!(
            "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2F{local}&format=html"
        ))
        .call()
        .unwrap()
        .into_string()
        .unwrap()
    };
    let p1 = get("P1");
    assert!(
        p1.contains("aria-label=\"Platform: 550 mm \u{b7} 220 m usable \u{b7} curved\""),
        "{p1}"
    );
    // The 550 mm reference line is the platform's own; the edge is curved.
    assert!(
        p1.contains("<path class=\"ref hit\" d=\"M20 86.0H620\"/>"),
        "{p1}"
    );
    assert!(
        p1.contains("<path class=\"block\" d=\"M70 196L70 86.0Q"),
        "{p1}"
    );
    // A height that is not a number of mm is shown as it is, and drawn as unknown.
    let p2 = get("P2");
    assert!(
        p2.contains("aria-label=\"Platform: between 300 and 380 \u{b7} 150 m usable\""),
        "{p2}"
    );
    assert!(p2.contains("class=\"block unknown\""), "{p2}");
    let t1 = get("T1");
    assert!(t1.contains("<figure class=\"cross-section\">"), "{t1}");
    assert!(
        t1.contains(">1435 mm</text>") && t1.contains(">GC</text>"),
        "{t1}"
    );
    assert!(t1.contains(">5\u{2013}6.2 m</text>"), "{t1}");
    assert!(t1.contains(">rails 1:20</text>"), "{t1}");
    // A track with none of the paths gets no sketch.
    assert!(!get("T2").contains("cross-section"));
}

#[test]
fn a_station_draws_its_tracks_and_connections() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:OP a ex:Station ; ex:wkt \"POINT(4 50)\"^^geo:wktLiteral ; ex:part ex:T10 , ex:T2 , ex:Sd1 , ex:Sd2 .\n\
         ex:T10 a ex:Track ; ex:id \"Leuven - Voie 10\" . ex:T2 a ex:Track ; ex:id \"Leuven - Voie 2\" .\n\
         ex:Sd1 a ex:Siding ; ex:length 300 . ex:Sd2 a ex:Siding ; ex:length 600 .\n\
         ex:PE1 a ex:Edge ; ex:on ex:T2 ; ex:length 400 ; ex:height ex:H760 . ex:H760 rdfs:label \"760\" .\n\
         ex:T2 ex:edge ex:PE1 .\n\
         ex:S1 ex:start ex:OP ; ex:end ex:East ; ex:line ex:L1 ; ex:length 12.5 ;\n\
           ex:wkt \"LINESTRING(4 50, 4.5 50)\"^^geo:wktLiteral .\n\
         ex:S2 ex:start ex:North ; ex:end ex:OP ; ex:line ex:L2 ; ex:wkt \"LINESTRING(4 50.5, 4 50)\"^^geo:wktLiteral .\n\
         ex:East rdfs:label \"Eastham\" . ex:North rdfs:label \"Northby\" . ex:L1 rdfs:label \"L1\" . ex:L2 rdfs:label \"L2\" .\n",
        "station",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:StationShape a sh:NodeShape ; sh:targetClass ex:Station ;\n\
           sh:property [ sh:path [ sh:alternativePath ( [ sh:inversePath ex:start ] [ sh:inversePath ex:end ] ) ] ;\n\
             sh:name \"Connections\" ; rd:key \"connections\" ; dash:viewer rd:ConnectionsViewer ;\n\
             sh:node [ sh:property [ sh:path ex:start ; rd:key \"start\" ] ; sh:property [ sh:path ex:end ; rd:key \"end\" ] ;\n\
               sh:property [ sh:path ex:line ; rd:key \"line\" ] ; sh:property [ sh:path ex:length ; rd:key \"length\" ] ;\n\
               sh:property [ sh:path ex:wkt ; rd:key \"wkt\" ] ] ] ;\n\
           sh:property [ sh:path ex:part ; rd:class ex:Track ; sh:name \"Track layout\" ; rd:key \"layout\" ;\n\
             dash:viewer rd:TrackLayoutViewer ; rd:platforms \"platforms\" ; rd:sidings \"sidings\" ;\n\
             sh:node [ sh:property [ sh:path ex:id ; rd:key \"id\" ] ] ] ;\n\
           sh:property [ sh:path ( ex:part ex:edge ) ; sh:name \"Platforms\" ; rd:key \"platforms\" ;\n\
             sh:node [ sh:property [ sh:path ex:on ; rd:key \"track\" ] ; sh:property [ sh:path ex:length ; rd:key \"length\" ] ;\n\
               sh:property [ sh:path ex:height ; rd:key \"height\" ] ] ] ;\n\
           sh:property [ sh:path ex:part ; rd:class ex:Siding ; sh:name \"Sidings\" ; rd:key \"sidings\" ; rd:hidden \"true\" ;\n\
             sh:node [ sh:property [ sh:path ex:length ; rd:key \"length\" ] ] ] .\n",
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FOP&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    // Tracks by their id without the shared "Leuven - ", 2 before 10.
    let two = page.find(">Voie 2</a>").expect(&page);
    let ten = page.find(">Voie 10</a>").expect(&page);
    assert!(two < ten, "{page}");
    // The platform on track 2, to its length against the longest (the 600 m siding).
    assert!(
        page.contains("<a class=\"tl-platform\" style=\"width:66.7%\"")
            && page.contains(">400 m \u{b7} 760 mm</a>"),
        "{page}"
    );
    // The hidden sidings row is drawn by the layout, longest first.
    assert!(page.contains("2 sidings"), "{page}");
    assert!(page.find(">600 m</span>").unwrap() < page.find(">300 m</span>").unwrap());
    assert!(page.contains("2 tracks, 1 platform edge."), "{page}");
    // Spokes to the far ends, pointing where the sections go: east and north.
    assert!(
        page.contains("aria-label=\"2 connected sections\""),
        "{page}"
    );
    assert!(
        page.contains(">Eastham</text>") && page.contains(">Northby</text>"),
        "{page}"
    );
    assert!(
        page.contains("x1=\"320\" y1=\"190\" x2=\"450.0\" y2=\"190.0\""),
        "{page}"
    );
    assert!(page.contains("x2=\"320.0\" y2=\"44.4\""), "{page}");
    assert!(page.contains(">L1 \u{b7} 12.5 km</text>"), "{page}");
    assert!(
        page.contains("each spoke points to where its section goes"),
        "{page}"
    );
}

#[test]
fn an_element_draws_its_topology_and_what_is_on_it() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:E1 a ex:Element ; ex:length 1500 ; ex:wkt \"LINESTRING(4 50, 4.01 50)\"^^geo:wktLiteral .\n\
         ex:E2 ex:length 800 ; ex:wkt \"LINESTRING(4.01 50, 4.02 50)\"^^geo:wktLiteral .\n\
         ex:E3 ex:wkt \"LINESTRING(4.02 50, 4.02 50.01)\"^^geo:wktLiteral .\n\
         ex:R1 ex:a ex:E1 ; ex:b ex:E2 ; ex:nav ex:Both ; ex:originA false ; ex:originB true .\n\
         ex:R2 ex:a ex:E2 ; ex:b ex:E3 ; ex:nav ex:None .\n\
         ex:S1 a ex:Signal ; ex:on ex:E1 ; ex:offset 200 . ex:S2 a ex:Signal ; ex:on ex:E1 ; ex:offset 900 .\n\
         ex:K1 a ex:Post ; ex:on ex:E1 ; ex:offset 1000 . ex:T1 a ex:Line ; ex:over ex:E1 .\n\
         ex:S1 ex:ref ex:E1 . ex:S2 ex:ref ex:E1 . ex:K1 ex:ref ex:E1 . ex:T1 ex:ref ex:E1 .\n\
         ex:Signal rdfs:label \"Signal\" . ex:Post rdfs:label \"Kilometric post\" . ex:Line rdfs:label \"Line\" .\n",
        "topology",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:ElementShape a sh:NodeShape ; sh:targetClass ex:Element ;\n\
           sh:property [ sh:path ex:length ; sh:name \"Length\" ; rd:key \"length\" ] ;\n\
           sh:property [ sh:path [ sh:alternativePath ( [ sh:inversePath ex:a ] [ sh:inversePath ex:b ]\n\
               ( [ sh:inversePath ex:a ] ex:b [ sh:inversePath ex:a ] ) ) ] ;\n\
             sh:name \"Topology\" ; rd:key \"topology\" ; dash:viewer rd:TopologyViewer ; rd:elements \"network\" ;\n\
             sh:node [ sh:property [ sh:path ex:a ; rd:key \"a\" ] ; sh:property [ sh:path ex:b ; rd:key \"b\" ] ;\n\
               sh:property [ sh:path ex:nav ; rd:key \"navigability\" ] ;\n\
               sh:property [ sh:path ex:originA ; rd:key \"origin-a\" ] ; sh:property [ sh:path ex:originB ; rd:key \"origin-b\" ] ] ] ;\n\
           sh:property [ sh:path [ sh:alternativePath ( ( [ sh:inversePath ex:a ] ex:b )\n\
               ( [ sh:inversePath ex:a ] ex:b [ sh:inversePath ex:a ] ex:b ) ) ] ;\n\
             sh:name \"Nearby\" ; rd:key \"network\" ; rd:hidden \"true\" ;\n\
             sh:node [ sh:property [ sh:path ex:length ; rd:key \"length\" ] ; sh:property [ sh:path ex:wkt ; rd:key \"wkt\" ] ] ] ;\n\
           sh:property [ sh:path [ sh:inversePath ex:ref ] ; sh:name \"Along the element\" ; rd:key \"positioned\" ;\n\
             dash:viewer rd:PositionedViewer ;\n\
             sh:node [ sh:property [ sh:path <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ; rd:key \"type\" ] ;\n\
               sh:property [ sh:path ex:on ; rd:key \"on\" ] ; sh:property [ sh:path ex:offset ; rd:key \"offset\" ] ] ] .\n",
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FE1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains("aria-label=\"Topology around the element: 3 elements, 2 net relations\""),
        "{page}"
    );
    // Both ways: arrows at both ends; not navigable: dashed, no arrows.
    assert!(
        page.contains("<path class=\"edge both\"")
            && page.contains("marker-end=\"url(#mg-arrow)\" marker-start=\"url(#mg-arrow)\""),
        "{page}"
    );
    assert!(
        page.contains(">Navigable both ways: end of A meets start of B (E1, E2)</title>"),
        "{page}"
    );
    assert!(page.contains("<path class=\"edge none\" d=\"M"), "{page}");
    assert!(page.contains("<g class=\"node self\">") && page.contains("<g class=\"node far\">"));
    assert!(
        page.contains(">1.5 km</text>") && page.contains(">800 m</text>"),
        "{page}"
    );
    assert!(
        page.contains("where the middle of them lies on the map"),
        "{page}"
    );
    // Positioned: three points over the 1500 m element, a lane per type; the line runs over it.
    assert!(
        page.contains("aria-label=\"3 objects along the element, 0 to 1.5 km\""),
        "{page}"
    );
    assert_eq!(page.matches("<circle class=\"dot\"").count(), 3, "{page}");
    assert!(page.contains("<span class=\"legend\">Signal</span> <span class=\"more\">2</span>"));
    assert!(page.find(">200 m</span>").unwrap() < page.find(">900 m</span>").unwrap());
    assert!(page.contains("runs over this element"), "{page}");
    assert!(page.contains("3 points placed at their offset"), "{page}");
}

#[test]
fn changes_over_time_and_symbols_are_drawn() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         ex:PL a ex:Location ; ex:applies ex:C1 , ex:C2 , ex:C3 ;\n\
           ex:country <http://publications.europa.eu/resource/authority/country/BEL> , ex:Atlantis ;\n\
           ex:supply ex:S25 ; ex:dir ex:N .\n\
         ex:S25 rdfs:label \"AC 25kV-50Hz\" . ex:N rdfs:label \"N\" .\n\
         ex:C1 ex:of ex:Freight ; ex:value false ; ex:begin \"2000-01-01\"^^xsd:date ; ex:end \"2001-06-30\"^^xsd:date .\n\
         ex:C2 ex:of ex:Freight ; ex:value true ; ex:begin \"2001-07-01\"^^xsd:date .\n\
         ex:C3 ex:of ex:Container ; ex:value true ; ex:begin \"2100-01-01\"^^xsd:date .\n\
         ex:Freight rdfs:label \"Freight\" . ex:Container rdfs:label \"Container handling\" .\n\
         ex:Later a ex:Location ; ex:applies ex:C4 , ex:C5 .\n\
         ex:C4 ex:of ex:Freight ; ex:begin \"2100-01-01T00:00:00Z\"^^xsd:dateTime .\n\
         ex:C5 ex:of ex:Container ; ex:begin \"2101-01-01\"^^xsd:date ; ex:end \"2100-06-01\"^^xsd:date .\n",
        "changes",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:LocationShape a sh:NodeShape ; sh:targetClass ex:Location ;\n\
           sh:property [ sh:path ex:applies ; sh:name \"Changes over time\" ; dash:viewer rd:ChangesViewer ;\n\
             sh:node [ sh:property [ sh:path ex:of ; rd:key \"parameter\" ] ; sh:property [ sh:path ex:value ; rd:key \"value\" ] ;\n\
               sh:property [ sh:path ex:begin ; rd:key \"begin\" ] ; sh:property [ sh:path ex:end ; rd:key \"end\" ] ] ] ;\n\
           sh:property [ sh:path ex:country ; sh:name \"Country\" ; dash:viewer rd:SymbolViewer ; rd:symbol \"country\" ] ;\n\
           sh:property [ sh:path ex:supply ; sh:name \"Energy\" ; dash:viewer rd:SymbolViewer ; rd:symbol \"energy-supply\" ] ;\n\
           sh:property [ sh:path ex:dir ; sh:name \"Direction\" ; dash:viewer rd:SymbolViewer ; rd:symbol \"direction\" ] .\n",
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FPL&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    // By name, then begin: Container handling (future), Freight past, Freight now.
    let container = page.find("<li class=\"change future\">").expect(&page);
    let past = page.find("<li class=\"change past\">").expect(&page);
    let now = page.find("<li class=\"change now\">").expect(&page);
    assert!(container < past && past < now, "{page}");
    assert!(
        page.contains(">from 2000-01-01 until 2001-06-30</span>"),
        "{page}"
    );
    assert!(page.contains(">from 2100-01-01</span>"), "{page}");
    // The axis starts at the first begin.
    assert!(
        page.contains("<span class=\"change-bar\" style=\"left:0.00%;"),
        "{page}"
    );
    // A change that begins after a year on, without an end, is on the axis; so is one that
    // ends before it begins.
    let later = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FLater&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(later.contains(">from 2100-01-01</span>"), "{later}");
    for bar in later.split("class=\"change-bar\" style=\"left:").skip(1) {
        let (left, rest) = bar.split_once("%;width:").unwrap();
        let width = rest.split_once('%').unwrap().0;
        let (left, width): (f64, f64) = (left.parse().unwrap(), width.parse().unwrap());
        assert!(
            left >= 0.0 && width > 0.0 && left + width <= 100.0 + 1e-9,
            "{later}"
        );
    }
    // Belgium's flag; an unknown country gets a white flag.
    assert!(
        page.contains("aria-hidden=\"true\">\u{1f1e7}\u{1f1ea}</span>"),
        "{page}"
    );
    assert!(
        page.contains("aria-hidden=\"true\">\u{1f3f3}</span>"),
        "{page}"
    );
    assert!(page.contains("style=\"fill:#c4262e\""), "{page}");
    // Normal direction: one arrow head, along the kilometres.
    assert!(
        page.contains("<path class=\"outline\" d=\"M8 36H56M56 36l-10 -8M56 36l-10 8\"/>"),
        "{page}"
    );
}

#[test]
fn drawings_leave_out_numbers_they_cannot_scale() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         ex:E1 a ex:Element ; ex:length 1e308 ; ex:wkt \"LINESTRING(1e308 1e308, -1e308 -1e308)\"^^geo:wktLiteral .\n\
         ex:E2 ex:wkt \"LINESTRING(1e308 1e308, -1e308 -1e308)\"^^geo:wktLiteral .\n\
         ex:R1 ex:a ex:E1 ; ex:b ex:E2 ; ex:nav ex:Both .\n\
         ex:X1 a ex:Signal ; ex:on ex:E1 ; ex:offset 1e308 ; ex:ref ex:E1 .\n\
         ex:X2 a ex:Signal ; ex:on ex:E1 ; ex:offset \"INF\"^^xsd:double ; ex:ref ex:E1 .\n\
         ex:X3 a ex:Signal ; ex:on ex:E1 ; ex:offset 50 ; ex:ref ex:E1 .\n",
        "unscalable",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:ElementShape a sh:NodeShape ; sh:targetClass ex:Element ;\n\
           sh:property [ sh:path ex:length ; rd:key \"length\" ] ;\n\
           sh:property [ sh:path [ sh:alternativePath ( [ sh:inversePath ex:a ] [ sh:inversePath ex:b ] ) ] ;\n\
             sh:name \"Topology\" ; dash:viewer rd:TopologyViewer ; rd:elements \"network\" ;\n\
             sh:node [ sh:property [ sh:path ex:a ; rd:key \"a\" ] ; sh:property [ sh:path ex:b ; rd:key \"b\" ] ;\n\
               sh:property [ sh:path ex:nav ; rd:key \"navigability\" ] ] ] ;\n\
           sh:property [ sh:path ( [ sh:inversePath ex:a ] ex:b ) ; rd:key \"network\" ; rd:hidden \"true\" ;\n\
             sh:node [ sh:property [ sh:path ex:wkt ; rd:key \"wkt\" ] ] ] ;\n\
           sh:property [ sh:path [ sh:inversePath ex:ref ] ; sh:name \"Along\" ; dash:viewer rd:PositionedViewer ;\n\
             sh:node [ sh:property [ sh:path ex:on ; rd:key \"on\" ] ; sh:property [ sh:path ex:offset ; rd:key \"offset\" ] ] ] .\n",
    );
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FE1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(page.contains("<figure class=\"topology\">"), "{page}");
    assert!(!page.contains("NaN") && !page.contains("inf\""), "{page}");
    // Only the signal at 50 m is placed.
    assert!(
        page.contains("aria-label=\"1 object along the element, 0 to 50 m\""),
        "{page}"
    );
}

#[test]
fn data_status_lists_what_does_not_apply_or_is_not_yet_available() {
    let many: String = (0..150)
        .map(|i| format!("ex:T3 era:notApplicable era:p{i} .\n"))
        .collect();
    let (base, _dir) = serve(
        &format!(
            "@prefix era: <http://data.europa.eu/949/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             @prefix ex: <http://example.com/> .\n\
             ex:T1 a era:RunningTrack ; era:trackId \"1\" ;\n\
               era:notApplicable era:wheelSetGauge , era:cantDeficiency ; era:notYetAvailable era:gaugingProfile .\n\
             era:wheelSetGauge rdfs:label \"nominal track gauge\" . era:cantDeficiency rdfs:label \"cant deficiency\" .\n\
             ex:T2 a era:RunningTrack ; era:trackId \"2\" .\n\
             ex:T3 a era:RunningTrack .\n{many}"
        ),
        "data-status",
    );
    let page = |local: &str| {
        ureq::get(&format!(
            "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2F{local}&format=html"
        ))
        .call()
        .unwrap()
        .into_string()
        .unwrap()
    };
    let t1 = page("T1");
    assert!(
        t1.contains("<summary>Not applicable <span class=\"more\">(2)</span></summary>"),
        "{t1}"
    );
    assert!(
        t1.contains("<summary>Not yet available <span class=\"more\">(1)</span></summary>"),
        "{t1}"
    );
    // By name, and no separate rows for the two properties.
    let at = |x: &str| t1.find(x).unwrap_or_else(|| panic!("{x} in {t1}"));
    assert!(
        at(">cant deficiency</a>") < at(">nominal track gauge</a>"),
        "{t1}"
    );
    assert!(
        !t1.contains(">notApplicable</th>") && !t1.contains(">notYetAvailable</th>"),
        "{t1}"
    );
    let rows = rows_of(&view_of(&base, "T1"));
    let status = rows
        .iter()
        .find(|r| r["widget"] == "DataStatusViewer")
        .unwrap();
    assert_eq!(
        status["params"]["notApplicable"].as_array().unwrap().len(),
        2
    );
    assert!(!page("T2").contains("Data status"));
    // The count is of all of them, also past those listed.
    let t3 = page("T3");
    assert!(
        t3.contains("<summary>Not applicable <span class=\"more\">(150)</span></summary>"),
        "{t3}"
    );
}

/// What rinf-describe shows, the page shows (#140): summaries count the linked resources and
/// say which have no value, a subclass's row replaces the superclass's row of its name, rows
/// of one property with their own keys each stay, and a widget with nothing to draw is left
/// out of the page.
#[test]
fn rows_read_as_rinf_describe_reads_them() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n\
         ex:Station rdfs:subClassOf ex:Place .\n\
         ex:BE1 a ex:Station ; ex:gauge ex:Standard ; ex:within ex:Region ;\n\
           ex:part ex:t1 , ex:t2 , ex:t3 .\n\
         ex:t1 ex:speed 80 ; ex:gauge ex:Standard . ex:t2 ex:speed 120 . ex:t3 ex:note \"x\" .\n"
            .replace(
                "@prefix ex:",
                "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n@prefix ex:",
            )
            .as_str(),
        "rd-rows",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
          sh:property [ sh:path ex:within ; sh:name \"Part of\" ; rd:key \"part-of\" ; sh:order 1 ] .\n\
         ex:StationShape a sh:NodeShape ; sh:targetClass ex:Station ;\n\
          sh:property [ sh:path [ sh:alternativePath ( ex:within ex:in ) ] ; sh:name \"Part of\" ; sh:order 9 ] ;\n\
          sh:property [ sh:path ex:gauge ; sh:name \"Gauge\" ; sh:order 2 ] ;\n\
          sh:property [ sh:path ex:gauge ; sh:name \"Gauge again\" ; rd:key \"gauge-2\" ; sh:order 3 ] ;\n\
          sh:property [ sh:path ex:part ; sh:name \"Tracks\" ; rd:key \"tracks\" ; rd:aggregate \"count\" ; rd:over ex:part ; sh:order 4 ] ;\n\
          sh:property [ sh:path ex:speed ; sh:name \"Speed\" ; rd:over ex:part ; rd:aggregate \"range\" ; sh:order 5 ] ;\n\
          sh:property [ sh:path ex:gauge ; sh:name \"Gauges\" ; rd:over ex:part ; sh:order 6 ] ;\n\
          sh:property [ sh:path ex:missing ; sh:name \"Nothing\" ; rd:over ex:part ; sh:order 7 ] ;\n\
          sh:property [ sh:path ex:missing ; sh:name \"No range\" ; rd:over ex:part ; rd:aggregate \"range\" ; sh:order 8 ] ;\n\
          sh:property [ sh:name \"Along\" ; rd:key \"along\" ; dash:viewer rd:AlongLineViewer ; rd:startKm ex:within ; sh:order 10 ] .\n",
    );
    let v = view_of(&base, "BE1");
    let rows = rows_of(&v);
    let value = |label: &str| {
        let r = rows
            .iter()
            .find(|r| r["label"] == label)
            .unwrap_or_else(|| panic!("no {label} in {v}"));
        r["values"][0]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    // The superclass's "Part of" (key part-of) gives way to the station's, keyed by its name.
    assert_eq!(
        rows.iter().filter(|r| r["label"] == "Part of").count(),
        1,
        "{v}"
    );
    assert!(
        rows.iter()
            .any(|r| r["label"] == "Part of" && r["path"].is_string()),
        "{v}"
    );
    // Two rows of one property, each with its own key.
    value("Gauge");
    value("Gauge again");
    assert_eq!(value("Tracks"), "3");
    assert_eq!(value("Speed"), "80\u{2013}120 (2 of 3)");
    assert_eq!(value("Gauges"), "Standard (1) \u{b7} none (2)");
    assert_eq!(value("Nothing"), "none (3)");
    assert!(!rows.iter().any(|r| r["label"] == "No range"), "{v}");
    // The strip has no kilometres to draw: in the view, not on the page.
    assert!(rows.iter().any(|r| r["label"] == "Along"), "{v}");
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FBE1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(page.contains(">Gauges</th>"), "{page}");
    assert!(!page.contains(">Along</th>"), "{page}");
}

/// rinf-describe's RINF parameters (`rd:ParameterIndexViewer`): every parameter of the
/// resource with an index in the ontology, also on what it links to, in index order under its
/// chapters, with units and the parameters the data marks not applicable.
#[test]
fn rinf_parameters_are_listed_by_index() {
    let (base, _dir) = serve(
        "@prefix era: <http://data.europa.eu/949/> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         @prefix ex: <http://example.com/> .\n\
         era:trackParameter era:rinfIndex \"1.1.1.1\" ; rdfs:label \"track parameters\"@en .\n\
         era:speedGroup era:rinfIndex \"1.1.1.1.2\" ; rdfs:label \"Performance parameters\"@en .\n\
         era:maximumPermittedSpeed era:rinfIndex \"1.1.1.1.2.1\" , \"1.2.1.0.2.1\" ;\n\
           rdfs:label \"maximum permitted speed\"@en ;\n\
           era:unitOfMeasure <http://qudt.org/vocab/unit/KiloM-PER-HR> .\n\
         era:trackId era:rinfIndex \"1.1.1.1.1.1\" ; rdfs:label \"track identification\"@en .\n\
         era:contactLineSystemType era:rinfIndex \"1.1.1.2.2.1.1\" ; rdfs:label \"type of contact line\"@en .\n\
         era:etcsLevel era:rinfIndex \"1.1.1.3.2.1\" ; rdfs:label \"ETCS level\"@en .\n\
         ex:T1 a ex:Place ; era:trackId \"1\" ; era:maximumPermittedSpeed 160 ;\n\
           era:contactLineSystem ex:CLS ; era:notApplicable era:etcsLevel ;\n\
           ex:self ex:T1 .\n\
         ex:CLS era:contactLineSystemType ex:Overhead .\n",
        "params",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
          sh:property [ sh:name \"RINF parameters\" ; rd:key \"rinf-parameters\" ; dash:viewer rd:ParameterIndexViewer ] .\n",
    );
    let v = view_of(&base, "T1");
    let row = rows_of(&v)
        .into_iter()
        .find(|r| r["widget"] == "ParameterIndexViewer")
        .unwrap_or_else(|| panic!("no parameters in {v}"));
    let names: Vec<&str> = row["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "Track identification",
            "Maximum permitted speed",
            "Type of contact line",
            "ETCS level"
        ]
    );
    let cell = |i: usize, column: &str| {
        let at = row["columns"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["key"] == column)
            .unwrap();
        row["cells"][i][at].clone()
    };
    // The index under the track's root (1.1.1), not the operational point's (1.2.1).
    assert_eq!(cell(1, "index")[0]["value"], "1.1.1.1.2.1");
    assert_eq!(cell(1, "value")[0]["value"], "160");
    assert_eq!(cell(1, "unit")[0]["value"], "km/h");
    assert_eq!(cell(1, "chapter")[1]["value"], "Track parameters");
    assert_eq!(cell(1, "section")[1]["value"], "Performance parameters");
    // On the contact line system the track links to.
    assert_eq!(cell(2, "on")[0]["value"], "http://example.com/CLS");
    assert_eq!(cell(3, "status")[0]["value"], "Not applicable");
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FT1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains("3 parameters given, 1 marked not applicable"),
        "{page}"
    );
    assert!(page.contains("Performance parameters"), "{page}");
}

/// The RINF parameters table lists at most 1,000 parameters, the first by index, and says so.
#[test]
fn rinf_parameters_are_capped() {
    let mut data = String::from(
        "@prefix era: <http://data.europa.eu/949/> .\n@prefix ex: <http://example.com/> .\n",
    );
    for i in 0..1005 {
        data.push_str(&format!(
            "ex:p{i} era:rinfIndex \"1.1.1.9.{i}\" .\nex:T1 ex:p{i} {i} .\n"
        ));
    }
    data.push_str("ex:T1 a ex:Place .\n");
    let (base, _dir) = serve(&data, "params-cap");
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
          sh:property [ sh:name \"RINF parameters\" ; rd:key \"rinf-parameters\" ; dash:viewer rd:ParameterIndexViewer ] .\n",
    );
    let v = view_of(&base, "T1");
    let row = rows_of(&v)
        .into_iter()
        .find(|r| r["widget"] == "ParameterIndexViewer")
        .unwrap_or_else(|| panic!("no parameters in {v}"));
    assert_eq!(row["cells"].as_array().unwrap().len(), 1000);
    assert_eq!(row["tableTruncated"], true);
    assert_eq!(row["cells"][0][0][0]["value"], "1.1.1.9.0");
}

/// A map row's context (`rd:context`, the section a track is part of) is on the page's map,
/// muted, also when the resource has no geometry of its own.
#[test]
fn the_map_shows_what_is_around_the_resource() {
    let (base, _dir) = serve(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         ex:T1 a ex:Place ; ex:partOf ex:S1 .\n\
         ex:S1 geo:hasGeometry ex:G1 . ex:G1 geo:asWKT \"LINESTRING(4 50, 5 51)\"^^geo:wktLiteral .\n",
        "context",
    );
    put_shapes(
        &base,
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix dash: <http://datashapes.org/dash#> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix rd: <https://rinf-describe.matdata.eu/ns#> .\n@prefix ex: <http://example.com/> .\n\
         ex:PlaceShape a sh:NodeShape ; sh:targetClass ex:Place ;\n\
          sh:property [ sh:path ( geo:hasGeometry geo:asWKT ) ; sh:name \"Map\" ; rd:key \"map\" ;\n\
            dash:viewer rd:MapViewer ; rd:context ( ex:partOf geo:hasGeometry geo:asWKT ) ] .\n",
    );
    let v = view_of(&base, "T1");
    assert_eq!(v["geometries"], serde_json::json!([]), "{v}");
    assert_eq!(v["context"][0]["value"], "LINESTRING(4 50, 5 51)", "{v}");
    let page = ureq::get(&format!(
        "{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2FT1&format=html"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(
        page.contains("<g class=\"context\"><path class=\"line\""),
        "{page}"
    );
    // The map row is the map, not a row of WKT.
    assert!(!page.contains(">Map</th>"), "{page}");
}
