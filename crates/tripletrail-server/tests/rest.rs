//! The JSON-LD REST API generated from the shapes (#143).

use serde_json::{Value, json};
use tripletrail::Engine;
use tripletrail::oxigraph::io::{JsonLdProfileSet, RdfFormat};
use tripletrail::oxigraph::sparql::QueryResults;
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

const DATA: &str = "@prefix ex: <http://example.com/> .\n\
    @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
    @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
    @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
    ex:o1 a ex:Operator ; rdfs:label \"Infrabel\" .\n\
    ex:o2 a ex:Operator ; rdfs:label \"SNCF Réseau\" .\n\
    ex:o3 a ex:Operator ; rdfs:label \"A<B->C\" .\n\
    ex:o4 a ex:Operator ; rdfs:label \"=1+1\", \"-2.5\" .\n";

fn data() -> String {
    let mut ttl = DATA.to_owned();
    // A depot with more values of one key than a record gives.
    for i in 0..1500 {
        ttl.push_str(&format!("ex:d1 a ex:Depot ; ex:holds ex:w{i:04} .\n"));
    }
    for i in 0..12 {
        let (country, operator) = if i < 8 { ("BE", "o1") } else { ("FR", "o2") };
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; rdfs:label \"Station {i}\"@en, \"Gare {i}\"@fr ; \
             ex:country \"{country}\" ; ex:tracks {} ; ex:open {} ; ex:opened \"2001-01-0{}\"^^xsd:date ; \
             ex:hasOperator ex:{operator} ; ex:platform ex:p{i}a, ex:p{i}b ; \
             geo:hasGeometry [ geo:asWKT \"POINT({i} 50)\"^^geo:wktLiteral ] .\n\
             ex:p{i}a a ex:Platform . ex:p{i}b a ex:Platform .\n",
            i % 3 + 1,
            i % 2 == 0,
            i % 9 + 1,
        ));
    }
    ttl
}

#[test]
fn records_from_the_shapes() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let base = start(engine);
    let api = format!("{base}/kg/api");

    let (status, kind, index) = get(&format!("{api}/v1"));
    assert_eq!(status, 200);
    assert_eq!(kind, "application/json");
    let ids: Vec<&str> = index["datasets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["Depot", "Operator", "Platform", "Station"], "{index}");
    let station = &index["datasets"][3];
    assert_eq!(station["count"], 12);
    assert_eq!(station["href"], format!("{api}/v1/Station"));

    // A page of records, JSON-LD by default, with a next link.
    let (status, kind, page) = get(&format!("{api}/v1/Station?limit=5"));
    assert_eq!(status, 200, "{page}");
    assert_eq!(kind, "application/ld+json");
    let records = page["@graph"].as_array().unwrap();
    assert_eq!(records.len(), 5);
    let next = page["next"].as_str().unwrap();
    assert!(next.ends_with("/v1/Station?limit=5&offset=5"), "{next}");
    let (_, _, last) = get(&format!("{api}/v1/Station?limit=5&offset=10"));
    assert_eq!(last["@graph"].as_array().unwrap().len(), 2);
    assert!(last["next"].is_null());
    assert!(last["prev"].is_string());

    // One record: typed values, a list where many are allowed, a reference with its label,
    // a language map.
    let one = format!("{api}/v1/Station/{}", "http%3A%2F%2Fexample.com%2Fs1");
    let (status, _, record) = get(&one);
    assert_eq!(status, 200, "{record}");
    assert_eq!(record["id"], "http://example.com/s1");
    assert_eq!(record["type"], "http://example.com/Station");
    assert_eq!(record["tracks"], 2);
    assert_eq!(record["open"], false);
    assert_eq!(record["country"], "BE");
    assert_eq!(record["opened"], "2001-01-02");
    assert_eq!(
        record["label"],
        json!({ "en": "Station 1", "fr": "Gare 1" })
    );
    assert_eq!(
        record["hasOperator"],
        json!({ "id": "http://example.com/o1", "type": "http://example.com/Operator", "label": "Infrabel" })
    );
    assert_eq!(
        record["platform"],
        json!([
            { "id": "http://example.com/p1a", "type": "http://example.com/Platform" },
            { "id": "http://example.com/p1b", "type": "http://example.com/Platform" },
        ])
    );
    let context = &record["@context"];
    assert_eq!(context["tracks"]["@id"], "http://example.com/tracks");
    assert_eq!(
        context["opened"]["@type"],
        "http://www.w3.org/2001/XMLSchema#date"
    );
    assert_eq!(context["label"]["@container"], "@language");
    assert_eq!(context["hasOperator"]["@type"], "@id");

    // The record as JSON-LD gives the triples it came from (but the geometry's blank node,
    // and the references' labels, which aren't statements of the record).
    let back = Engine::new_tripletrail();
    back.load(
        RdfFormat::JsonLd {
            profile: JsonLdProfileSet::empty(),
        },
        record.to_string().as_bytes(),
    )
    .unwrap();
    let triples = |engine: &Engine| -> Vec<String> {
        let QueryResults::Solutions(rows) = engine
            .query(
                "SELECT ?p ?o WHERE { <http://example.com/s1> ?p ?o FILTER(!isBlank(?o)) } ORDER BY ?p ?o",
            )
            .unwrap()
        else {
            panic!()
        };
        rows.map(|r| {
            let r = r.unwrap();
            format!("{} {}", r.get("p").unwrap(), r.get("o").unwrap())
        })
        .collect()
    };
    let original = Engine::new_tripletrail();
    original.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    assert_eq!(triples(&back), triples(&original));
    let QueryResults::Boolean(labelled) = back.query("ASK { ?r ?p \"Infrabel\" }").unwrap() else {
        panic!()
    };
    assert!(!labelled);

    // Plain JSON: no context.
    let (_, kind, plain) = get(&format!("{one}?f=json"));
    assert_eq!(kind, "application/json");
    assert!(plain["@context"].is_null());
    assert_eq!(plain["tracks"], 2);

    // Filters: a key, the label's text, a box.
    let ids = |url: &str| -> Vec<String> {
        let (status, _, page) = get(url);
        assert_eq!(status, 200, "{page}");
        let mut ids: Vec<String> = page["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("http://example.com/")
                    .to_owned()
            })
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(
        ids(&format!("{api}/v1/Station?country=FR")),
        ["s10", "s11", "s8", "s9"]
    );
    assert_eq!(
        ids(&format!("{api}/v1/Station?country=FR&tracks=3")),
        ["s11", "s8"]
    );
    assert_eq!(
        ids(&format!(
            "{api}/v1/Station?hasOperator=http://example.com/o2&open=true"
        )),
        ["s10", "s8"]
    );
    assert_eq!(ids(&format!("{api}/v1/Station?q=GARE%2011")), ["s11"]);
    // Regardless of accents, either way.
    assert_eq!(ids(&format!("{api}/v1/Station?q=g%C3%A2re%2011")), ["s11"]);
    assert_eq!(
        ids(&format!("{api}/v1/Station?bbox=2.5,49,4.5,51")),
        ["s3", "s4"]
    );

    // A key with more values than a record gives: the first 1,000, and how many there are.
    let (status, _, depot) = get(&format!("{api}/v1/Depot/http%3A%2F%2Fexample.com%2Fd1"));
    assert_eq!(status, 200);
    let held = depot["holds"].as_array().unwrap();
    assert_eq!(held.len(), 1000);
    assert_eq!(held[0], "http://example.com/w0000");
    assert_eq!(held[999], "http://example.com/w0999");
    assert_eq!(depot["truncated"], json!({ "holds": 1500 }));
    assert_eq!(
        get(&format!("{api}/v1/Depot")).2["@graph"][0]["truncated"]["holds"],
        1500
    );

    // GeoJSON and CSV.
    let (status, kind, geojson) = get(&format!("{api}/v1/Station?limit=2&f=geojson&country=BE"));
    assert_eq!(status, 200, "{geojson}");
    assert_eq!(kind, "application/geo+json");
    let feature = &geojson["features"][0];
    assert_eq!(feature["type"], "Feature");
    assert_eq!(feature["geometry"]["type"], "Point");
    assert_eq!(feature["properties"]["country"], "BE");
    assert!(feature["properties"]["id"].is_null());
    assert!(geojson["next"].as_str().unwrap().contains("offset=2"));
    let (_, _, one_feature) = get(&format!("{one}?f=geojson"));
    assert_eq!(one_feature["id"], "http://example.com/s1");
    assert_eq!(one_feature["geometry"]["coordinates"], json!([1, 50]));
    assert_eq!(get(&format!("{api}/v1/Operator?f=geojson")).0, 400);
    let response = ureq::get(&format!("{api}/v1/Station?limit=1&f=csv&country=BE"))
        .call()
        .unwrap();
    assert_eq!(response.content_type(), "text/csv");
    assert!(response.header("Link").unwrap().contains("rel=\"next\""));
    let text = response.into_string().unwrap();
    let mut lines = text.lines();
    let header = lines.next().unwrap();
    assert!(header.starts_with("id,type,"), "{header}");
    let row = lines.next().unwrap();
    let column = |name: &str| header.split(',').position(|c| c == name).unwrap();
    let cells: Vec<&str> = row.split(',').collect();
    assert_eq!(cells[column("country")], "BE", "{text}");
    assert_eq!(
        cells[column("hasOperator")],
        "http://example.com/o1",
        "{text}"
    );
    assert!(cells[column("platform")].contains(" | "), "{text}");

    // A cell that a spreadsheet would read as a formula is text.
    let text = ureq::get(&format!(
        "{api}/v1/Operator/http%3A%2F%2Fexample.com%2Fo4?f=csv"
    ))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(text.contains(",'-2.5 | =1+1"), "{text}");
    // Accept chooses the format when f doesn't.
    let by_accept = |accept: &str| {
        ureq::get(&format!("{api}/v1/Station?limit=1"))
            .set("Accept", accept)
            .call()
            .unwrap()
            .content_type()
            .to_owned()
    };
    assert_eq!(by_accept("application/geo+json"), "application/geo+json");
    assert_eq!(by_accept("text/csv, application/json;q=0.5"), "text/csv");
    assert_eq!(by_accept("application/json"), "application/json");
    assert_eq!(by_accept("text/html,*/*;q=0.8"), "application/ld+json");

    // The docs page (Swagger UI) and its files.
    let docs = index["docs"].as_str().unwrap();
    assert!(docs.ends_with("/$/ui/api/kg"), "{docs}");
    let page = ureq::get(docs).call().unwrap();
    assert!(
        page.header("Content-Security-Policy")
            .unwrap()
            .contains("default-src 'self'")
    );
    let html = page.into_string().unwrap();
    assert!(html.contains("data-dataset=\"kg\""), "{html}");
    for file in [
        "swagger-ui/swagger-ui-bundle.js",
        "swagger-ui/swagger-ui.css",
        "swagger-ui/LICENSE",
    ] {
        let r = ureq::get(&format!("{base}/$/ui/vendor/{file}"))
            .call()
            .unwrap();
        assert_eq!(r.status(), 200, "{file}");
    }

    // Errors.
    let operators = get(&format!("{api}/v1/Operator?q=reseau&f=json")).2;
    assert_eq!(
        operators["records"][0]["id"], "http://example.com/o2",
        "{operators}"
    );
    let operators = get(&format!("{api}/v1/Operator?q=a%3Cb-%3Ec&f=json")).2;
    assert_eq!(
        operators["records"][0]["id"], "http://example.com/o3",
        "{operators}"
    );
    assert_eq!(get(&format!("{api}/v1/Nothing")).0, 404);
    assert_eq!(get(&format!("{api}/v1/Station?colour=red")).0, 400);
    assert_eq!(get(&format!("{api}/v1/Operator?bbox=0,0,1,1")).0, 400);
    assert_eq!(
        get(&format!("{api}/v1/Station/http%3A%2F%2Fexample.com%2Fo1")).0,
        404
    );
    assert_eq!(get(&format!("{api}/v1/Station?f=xml")).0, 406);

    // The OpenAPI definition.
    let (status, kind, openapi) = get(&format!("{api}/openapi.json"));
    assert_eq!(status, 200);
    assert!(
        kind.starts_with("application/vnd.oai.openapi+json"),
        "{kind}"
    );
    assert_eq!(openapi["openapi"], "3.1.0");
    let list = &openapi["paths"]["/v1/Station"]["get"];
    let names: Vec<&str> = list["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for name in ["limit", "offset", "q", "bbox", "country", "hasOperator"] {
        assert!(names.contains(&name), "{name}: {names:?}");
    }
    let schema = &openapi["components"]["schemas"]["Station"]["properties"];
    assert_eq!(schema["tracks"]["type"], "integer");
    assert_eq!(
        schema["hasOperator"]["$ref"],
        "#/components/schemas/Reference"
    );
    assert_eq!(schema["platform"]["type"], "array");
    assert!(openapi["paths"]["/v1/Station/{id}"]["get"].is_object());

    // What the Spectral OpenAPI rules ask: a contact, and every operation with a unique
    // operationId, a description and tags that are defined.
    assert!(openapi["info"]["contact"].is_object());
    let defined: Vec<&str> = openapi["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let mut ids = std::collections::HashSet::new();
    for (path, item) in openapi["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            let at = format!("{method} {path}");
            assert!(
                ids.insert(op["operationId"].as_str().unwrap().to_owned()),
                "{at}"
            );
            assert!(!op["description"].as_str().unwrap().is_empty(), "{at}");
            for tag in op["tags"].as_array().unwrap() {
                assert!(defined.contains(&tag.as_str().unwrap()), "{at}: {tag}");
            }
        }
    }

    // The same definition as YAML.
    let response = ureq::get(&format!("{api}/openapi.yaml")).call().unwrap();
    assert!(
        response
            .content_type()
            .starts_with("application/vnd.oai.openapi"),
        "{}",
        response.content_type()
    );
    let text = response.into_string().unwrap();
    let docs = yaml_rust2::YamlLoader::load_from_str(&text).unwrap();
    let yaml = &docs[0];
    assert_eq!(yaml["openapi"].as_str(), Some("3.1.0"), "{text}");
    let get = &yaml["paths"]["/v1/Station"]["get"];
    assert_eq!(
        get["parameters"].as_vec().unwrap().len(),
        list["parameters"].as_array().unwrap().len()
    );
    assert!(!get["responses"]["200"].is_badvalue(), "{text}");
    assert_eq!(
        yaml["components"]["schemas"]["Station"]["properties"]["tracks"]["type"].as_str(),
        Some("integer")
    );
}

const TOKEN: &str = "admin-secret";

/// Sends `body` (JSON) with `method`; the status and the answer as JSON (or text).
fn send(method: &str, url: &str, body: Option<&Value>) -> (u16, Value) {
    let request = ureq::request(method, url);
    let result = match body {
        Some(b) => request
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => request.call(),
    };
    let (code, r) = match result {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(code, r)) => (code, r),
        Err(e) => panic!("{e}"),
    };
    let text = r.into_string().unwrap();
    (
        code,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[test]
fn records_are_written_and_checked_against_the_shapes() {
    let dir = tripletrail_testdir::tempdir("rest-write");
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "@prefix ex: <http://example.com/> .\n\
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             ex:o1 a ex:Operator ; rdfs:label \"Infrabel\" .\n\
             ex:s1 a ex:Station ; rdfs:label \"Brussels\"@en ; ex:tracks 4 ; ex:country \"BE\" ; ex:hasOperator ex:o1 .\n"
                .as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 8,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let api = format!("{base}/kg/api/v1");
    let id = |iri: &str| iri.replace(':', "%3A").replace('/', "%2F");
    let ask = |query: &str| -> bool {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", query)
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["boolean"].as_bool().unwrap()
    };

    // With only the generated shapes, a write is not held to what the data has now.
    let (status, made) = send(
        "POST",
        &format!("{api}/Operator"),
        Some(&json!({ "id": "http://example.com/o2", "label": "SNCF Réseau" })),
    );
    assert_eq!(status, 201, "{made}");
    assert_eq!(made["label"], "SNCF Réseau");

    // The dataset's own shape of a station.
    let shape = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
        @prefix ex: <http://example.com/> .\n\
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n\
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
        @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
        ex:StationShape a sh:NodeShape ; sh:targetClass ex:Station ; tts:key \"Station\" ;\n\
          sh:property [ sh:path rdfs:label ; sh:datatype rdf:langString ; sh:minCount 1 ] ,\n\
            [ sh:path ex:tracks ; sh:datatype xsd:integer ; sh:maxCount 1 ; sh:minInclusive 1 ;\n\
              sh:message \"A station has at least one track\" ] ,\n\
            [ sh:path ex:country ; sh:in ( \"BE\" \"FR\" \"NL\" ) ; sh:maxCount 1 ] ,\n\
            [ sh:path ex:hasOperator ; sh:class ex:Operator ; sh:maxCount 1 ] .\n";
    let saved = ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Station.ttl"))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_string(shape);
    assert!(saved.is_ok(), "{saved:?}");

    // A new record: 201, its URL, and the record as it reads back.
    let new = json!({
        "id": "http://example.com/s2",
        "label": { "en": "Ghent", "nl": "Gent" },
        "tracks": 12,
        "country": "BE",
        "operator": { "id": "http://example.com/o1" },
    });
    let (status, made) = send("POST", &format!("{api}/Station"), Some(&new));
    assert_eq!(status, 201, "{made}");
    assert_eq!(made["tracks"], 12);
    assert_eq!(made["operator"]["label"], "Infrabel");
    let one = format!("{api}/Station/{}", id("http://example.com/s2"));
    let (_, _, read) = get(&one);
    assert_eq!(read["label"], json!({ "en": "Ghent", "nl": "Gent" }));
    assert_eq!(send("POST", &format!("{api}/Station"), Some(&new)).0, 409);

    // What the shape refuses, with the key and the constraint; nothing is written.
    let (status, refused) = send("PATCH", &one, Some(&json!({ "tracks": 0 })));
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["errors"][0]["key"], "tracks", "{refused}");
    assert_eq!(refused["errors"][0]["constraint"], "sh:minInclusive");
    assert_eq!(
        refused["errors"][0]["message"],
        "A station has at least one track"
    );
    let (status, refused) = send(
        "PATCH",
        &one,
        Some(&json!({ "operator": "http://example.com/s1", "country": "DE" })),
    );
    assert_eq!(status, 422);
    let constraints: Vec<&str> = refused["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["constraint"].as_str().unwrap())
        .collect();
    assert_eq!(constraints.len(), 2, "{refused}");
    assert!(constraints.contains(&"sh:class") && constraints.contains(&"sh:in"));
    let (status, refused) = send("PATCH", &one, Some(&json!({ "tracks": "many" })));
    assert_eq!(status, 422);
    assert!(
        refused["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("not a valid"),
        "{refused}"
    );
    let (status, refused) = send("PATCH", &one, Some(&json!({ "label": null })));
    assert_eq!(status, 422);
    assert_eq!(refused["errors"][0]["constraint"], "sh:minCount");
    assert_eq!(send("PATCH", &one, Some(&json!({ "platforms": 3 }))).0, 422);
    assert!(ask(
        "ASK { <http://example.com/s2> <http://example.com/tracks> 12 }"
    ));

    // PATCH changes the keys it names; PUT replaces them all.
    let (status, changed) = send("PATCH", &one, Some(&json!({ "tracks": 13 })));
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["tracks"], 13);
    assert_eq!(changed["country"], "BE");
    let (status, replaced) = send(
        "PUT",
        &one,
        Some(&json!({ "label": { "nl": "Gent-Sint-Pieters" }, "tracks": 12 })),
    );
    assert_eq!(status, 200, "{replaced}");
    assert!(replaced.get("country").is_none(), "{replaced}");
    assert!(!ask("ASK { <http://example.com/s2> ?p \"Ghent\"@en }"));
    // A record read, changed and written back stays as it was.
    let (_, _, before) = get(&format!("{one}?f=json"));
    let (status, after) = send("PUT", &one, Some(&before));
    assert_eq!(status, 200, "{after}");
    assert_eq!(after["label"], before["label"]);

    // If-Match: a write with the ETag of an older read is refused.
    let tag = ureq::get(&one)
        .call()
        .unwrap()
        .header("ETag")
        .unwrap()
        .to_owned();
    let patch = |tag: &str, tracks: u64| match ureq::request("PATCH", &one)
        .set("If-Match", tag)
        .send_string(&json!({ "tracks": tracks }).to_string())
    {
        Ok(r) => (r.status(), r.header("ETag").map(str::to_owned)),
        Err(ureq::Error::Status(code, r)) => (code, r.header("ETag").map(str::to_owned)),
        Err(e) => panic!("{e}"),
    };
    let (status, newer) = patch(&tag, 14);
    assert_eq!(status, 200);
    let newer = newer.unwrap();
    assert_ne!(newer, tag);
    assert_eq!(patch(&tag, 15).0, 412);
    assert_eq!(patch(&newer, 15).0, 200);
    assert_eq!(patch("*", 12).0, 200);
    // Plain JSON has a tag of its own, and a write takes either.
    let tags = |url: &str| {
        ureq::get(url)
            .call()
            .unwrap()
            .header("ETag")
            .unwrap()
            .to_owned()
    };
    let (linked, plain) = (tags(&one), tags(&format!("{one}?f=json")));
    assert_ne!(linked, plain);
    assert_eq!(patch(&plain, 13).0, 200);

    // Concurrent writes take turns: each is checked against the record as the one before
    // left it, so all pass and one value of a maxCount-1 key remains.
    let codes: Vec<u16> = std::thread::scope(|scope| {
        let writes: Vec<_> = (0..40)
            .map(|i| {
                let one = &one;
                scope.spawn(move || send("PATCH", one, Some(&json!({ "tracks": i + 1 }))).0)
            })
            .collect();
        writes.into_iter().map(|w| w.join().unwrap()).collect()
    });
    assert!(codes.iter().all(|&c| c == 200), "{codes:?}");
    let (_, _, raced) = get(&one);
    assert!(raced["tracks"].is_u64(), "{raced}");

    // DELETE takes the record's type and keys.
    assert_eq!(send("DELETE", &one, None).0, 204);
    assert_eq!(get(&one).0, 404);
    assert!(!ask("ASK { <http://example.com/s2> ?p ?o }"));
    assert_eq!(send("DELETE", &one, None).0, 404);
}

#[test]
fn records_in_named_graphs_are_written_where_they_are() {
    let engine = Engine::new_tripletrail();
    engine.set_union_default_graph(true);
    engine
        .load(
            RdfFormat::TriG,
            "@prefix ex: <http://example.com/> .\n\
             ex:g1 { ex:s1 a ex:Station ; ex:tracks 4 ; ex:name \"A\" ; ex:address _:a . _:a a ex:Address ; ex:city \"Gent\" . }\n\
             ex:g2 { ex:s1 ex:name \"B\" . _:a ex:zip 9000 . }\n"
                .as_bytes(),
        )
        .unwrap();
    let base = start(engine);
    let api = format!("{base}/kg/api/v1");
    let one = format!("{api}/Station/http%3A%2F%2Fexample.com%2Fs1");
    let count = |query: &str| -> usize {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", query)
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["results"]["bindings"].as_array().unwrap().len()
    };

    // A key's values go from every graph, the new ones to the graph of the type triple.
    let (status, changed) = send("PATCH", &one, Some(&json!({ "name": "C", "tracks": 5 })));
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["name"], "C");
    let where_ = "SELECT ?g ?v WHERE { GRAPH ?g { <http://example.com/s1> ?p ?v } FILTER(?p != <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> && !isBlank(?v)) }";
    assert_eq!(count(where_), 2);

    // An embedded blank node's triples go from every graph with the blank node.
    let (status, changed) = send(
        "PATCH",
        &one,
        Some(&json!({ "address": { "city": "Gent", "zip": 9000 } })),
    );
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["address"]["zip"], 9000, "{changed}");
    assert_eq!(
        count("SELECT * WHERE { GRAPH ?g { ?a <http://example.com/zip> ?z } }"),
        1
    );
    assert_eq!(
        count(
            "SELECT * WHERE { GRAPH <http://example.com/g1> { <http://example.com/s1> <http://example.com/address> ?a . ?a <http://example.com/zip> 9000 ; <http://example.com/city> \"Gent\" ; a <http://example.com/Address> } }"
        ),
        1
    );
    assert_eq!(
        count(
            "SELECT * WHERE { GRAPH <http://example.com/g1> { <http://example.com/s1> <http://example.com/name> \"C\" ; <http://example.com/tracks> 5 } }"
        ),
        1
    );

    // A new record goes to `graph`; PUT makes a record that is missing.
    let (status, _) = send(
        "POST",
        &format!("{api}/Station?graph=http://example.com/g3"),
        Some(&json!({ "id": "http://example.com/s2", "name": "D" })),
    );
    assert_eq!(status, 201);
    assert_eq!(
        count(
            "SELECT * WHERE { GRAPH <http://example.com/g3> { <http://example.com/s2> a <http://example.com/Station> ; <http://example.com/name> \"D\" } }"
        ),
        1
    );
    let (status, made) = send(
        "PUT",
        &format!("{api}/Station/http%3A%2F%2Fexample.com%2Fs3"),
        Some(&json!({ "name": "E" })),
    );
    assert_eq!(status, 201, "{made}");
    assert_eq!(made["id"], "http://example.com/s3");
    // Errors: a wrong id, a POST to a record, an unreadable body.
    assert_eq!(
        send(
            "PATCH",
            &one,
            Some(&json!({ "id": "http://example.com/x" }))
        )
        .0,
        422
    );
    assert_eq!(send("POST", &one, Some(&json!({}))).0, 405);
    assert_eq!(send("PATCH", &one, Some(&json!([1]))).0, 400);
}

#[test]
fn blank_nodes_are_embedded_objects() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
        ex:s1 a ex:Station ; ex:name \"One\" ;\n\
          ex:address [ a ex:Address ; ex:city \"Gent\" ; ex:zip 9000 ;\n\
            ex:region [ a ex:Region ; ex:title \"Vlaanderen\" ; ex:part [ a ex:Part ; ex:code \"x\" ] ] ] .\n\
        ex:s2 a ex:Station ; ex:name \"Two\" ;\n\
          ex:address [ a ex:Address ; ex:city \"Brugge\" ; ex:zip 8000 ;\n\
            ex:region [ a ex:Region ; ex:title \"Vlaanderen\" ; ex:part [ a ex:Part ; ex:code \"y\" ] ] ] .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let base = start(engine);
    let api = format!("{base}/kg/api");
    let (status, _, record) = get(&format!("{api}/v1/Station/http%3A%2F%2Fexample.com%2Fs1"));
    assert_eq!(status, 200, "{record}");
    assert_eq!(
        record["address"],
        json!({
            "type": "http://example.com/Address",
            "city": "Gent",
            "zip": 9000,
            "region": {
                "type": "http://example.com/Region",
                // Below the embedding depth, blank nodes are left out.
                "title": "Vlaanderen",
            },
        }),
        "{record}"
    );
    // As JSON-LD, the record gives the blank nodes' triples back.
    let back = Engine::new_tripletrail();
    back.load(
        RdfFormat::JsonLd {
            profile: JsonLdProfileSet::empty(),
        },
        record.to_string().as_bytes(),
    )
    .unwrap();
    let QueryResults::Boolean(found) = back
        .query(
            "PREFIX ex: <http://example.com/> ASK { ex:s1 ex:address ?a . ?a a ex:Address ; \
             ex:city \"Gent\" ; ex:zip 9000 ; ex:region ?r . ?r a ex:Region ; ex:title \"Vlaanderen\" \
             FILTER(isBlank(?a) && isBlank(?r)) }",
        )
        .unwrap()
    else {
        panic!()
    };
    assert!(found, "{record}");
    // In a list, each record has its own.
    let (status, _, page) = get(&format!("{api}/v1/Station?f=json"));
    assert_eq!(status, 200, "{page}");
    let cities: Vec<&str> = page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["address"]["city"].as_str().unwrap())
        .collect();
    assert_eq!(cities, ["Gent", "Brugge"], "{page}");
    // CSV gives an embedded object as JSON.
    let text = ureq::get(&format!("{api}/v1/Station?f=csv"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(text.contains("\"\"city\"\":\"\"Gent\"\""), "{text}");
    // The OpenAPI schema describes the object's keys.
    let (_, _, openapi) = get(&format!("{api}/openapi.json"));
    let object = |schema: &Value| -> Value {
        schema["oneOf"]
            .as_array()
            .and_then(|o| o.iter().find(|s| s["type"] == "object"))
            .unwrap_or(schema)
            .clone()
    };
    let address = object(&openapi["components"]["schemas"]["Station"]["properties"]["address"]);
    assert_eq!(address["properties"]["zip"]["type"], "integer", "{address}");
    let region = object(&address["properties"]["region"]);
    assert_eq!(region["properties"]["title"]["type"], "string", "{address}");
}

#[test]
fn embedded_objects_are_written_with_their_record() {
    let dir = tripletrail_testdir::tempdir("rest-embed");
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "@prefix ex: <http://example.com/> .\n\
             ex:s1 a ex:Station ; ex:name \"One\" ; ex:contact ex:c1 ;\n\
               ex:address [ a ex:Address ; ex:city \"Gent\" ; ex:region [ a ex:Region ; ex:title \"Vlaanderen\" ] ] .\n\
             ex:s2 a ex:Station ; ex:name \"Two\" ;\n\
               ex:address [ a ex:Address ; ex:city \"Brugge\" ; ex:region [ a ex:Region ; ex:title \"Vlaanderen\" ] ] .\n\
             ex:c1 ex:phone \"+32 9 000\" .\n\
             ex:s3 a ex:Station ; ex:address _:shared .\n\
             ex:s4 a ex:Station ; ex:address _:shared .\n\
             ex:other ex:uses _:shared .\n\
             _:shared a ex:Address ; ex:city \"Leuven\" ; ex:region _:loop .\n\
             _:loop a ex:Region ; ex:title \"VB\" ; ex:part _:shared .\n\
             ex:s5 a ex:Station ; ex:address _:x .\n\
             _:x a ex:Address ; ex:city \"Mons\" ; ex:region _:y .\n\
             _:y a ex:Region ; ex:title \"Hainaut\" ; ex:part _:x .\n"
                .as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let api = format!("{base}/kg/api/v1");
    let one = format!("{api}/Station/http%3A%2F%2Fexample.com%2Fs1");
    let ask = |query: &str| -> bool {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query(
                "query",
                &format!("PREFIX ex: <http://example.com/> ASK {{ {query} }}"),
            )
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["boolean"].as_bool().unwrap()
    };
    let count = || -> u64 {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }")
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["results"]["bindings"][0]["n"]["value"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };

    // Own shapes: an address is a node shape with no target, a contact a resource that is
    // no dataset's record.
    let shape = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
        @prefix ex: <http://example.com/> .\n\
        @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
        ex:StationShape a sh:NodeShape ; sh:targetClass ex:Station ; tts:key \"Station\" ;\n\
          sh:property [ sh:path ex:name ; sh:maxCount 1 ] ,\n\
            [ sh:path ex:address ; sh:class ex:Address ; sh:node ex:AddressShape ; sh:maxCount 1 ] ,\n\
            [ sh:path ex:contact ; sh:nodeKind sh:IRI ; sh:node ex:ContactShape ; sh:maxCount 1 ] .\n\
        ex:AddressShape a sh:NodeShape ;\n\
          sh:property [ sh:path ex:city ; sh:minCount 1 ; sh:maxCount 1 ; sh:message \"An address has a city\" ] ,\n\
            [ sh:path ex:region ; sh:node ex:RegionShape ; sh:maxCount 1 ] .\n\
        ex:RegionShape a sh:NodeShape ; sh:property [ sh:path ex:title ; sh:maxCount 1 ] .\n\
        ex:ContactShape a sh:NodeShape ; sh:property [ sh:path ex:phone ; sh:maxCount 1 ] .\n";
    let saved = ureq::put(&format!("{base}/$/datasets/kg/shapes/files/Station.ttl"))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_string(shape);
    assert!(saved.is_ok(), "{saved:?}");

    let (status, record) = send("GET", &one, None);
    assert_eq!(status, 200, "{record}");
    assert_eq!(
        record["address"],
        json!({
            "type": "http://example.com/Address",
            "city": "Gent",
            "region": { "type": "http://example.com/Region", "title": "Vlaanderen" },
        }),
        "{record}"
    );
    assert_eq!(
        record["contact"],
        json!({ "id": "http://example.com/c1", "phone": "+32 9 000" }),
        "{record}"
    );

    // Reading a record and writing it back leaves the data as it was.
    let write = |method: &str, body: Value| -> (u16, Value) {
        let r = ureq::request(method, &one)
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let (code, r) = match r {
            Ok(r) => (r.status(), r),
            Err(ureq::Error::Status(code, r)) => (code, r),
            Err(e) => panic!("{e}"),
        };
        let text = r.into_string().unwrap();
        (
            code,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    };
    let before = count();
    let (status, written) = write("PUT", record.clone());
    assert_eq!(status, 200, "{written}");
    assert_eq!(count(), before);
    assert_eq!(written["address"], record["address"]);

    // A new address replaces the old one, with the blank nodes below it.
    let patch = |body: Value| write("PATCH", body);
    let (status, changed) = patch(json!({ "address": { "city": "Antwerpen" } }));
    assert_eq!(status, 200, "{changed}");
    assert_eq!(
        changed["address"],
        json!({ "type": "http://example.com/Address", "city": "Antwerpen" })
    );
    assert!(!ask("?a ex:city \"Gent\""));
    assert_eq!(count(), before - 3, "the old region went too");
    assert!(ask("ex:s2 ex:address/ex:region/ex:title \"Vlaanderen\""));

    // The embedded object is checked against its node shape.
    let (status, refused) = patch(json!({ "address": { "region": { "title": "Wallonie" } } }));
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["errors"][0]["key"], "address.city", "{refused}");
    assert_eq!(
        refused["errors"][0]["message"], "An address has a city",
        "{refused}"
    );
    let (status, refused) = patch(json!({ "address": { "town": "Gent" } }));
    assert_eq!(status, 422, "{refused}");
    assert!(ask("ex:s1 ex:address/ex:city \"Antwerpen\""));

    // An embedded object with an id is a link: only the link is written.
    let (status, changed) =
        patch(json!({ "contact": { "id": "http://example.com/c2", "phone": "0" } }));
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["contact"], json!({ "id": "http://example.com/c2" }));
    assert!(!ask("?c ex:phone \"0\""));

    // A blank node that something else links to stays: only the record's link goes.
    let at = |s: &str| format!("{api}/Station/http%3A%2F%2Fexample.com%2F{s}");
    let send_to = |method: &str, url: &str, body: Value| -> (u16, Value) {
        let r = ureq::request(method, url)
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let (code, r) = match r {
            Ok(r) => (r.status(), r),
            Err(ureq::Error::Status(code, r)) => (code, r),
            Err(e) => panic!("{e}"),
        };
        let text = r.into_string().unwrap();
        (
            code,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    };
    let (status, changed) = send_to(
        "PATCH",
        &at("s3"),
        json!({ "address": { "city": "Brugge" } }),
    );
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["address"]["city"], "Brugge", "{changed}");
    let (_, s4) = send("GET", &at("s4"), None);
    assert_eq!(s4["address"]["city"], "Leuven", "{s4}");
    assert_eq!(s4["address"]["region"]["title"], "VB", "{s4}");
    assert!(ask(
        "ex:other ex:uses ?a . ?a ex:city \"Leuven\" ; ex:region/ex:title \"VB\""
    ));
    // Once the last link goes, so does the blank node, with the one it is in a cycle with.
    let (status, _) = send_to("PATCH", &at("s4"), json!({ "address": null }));
    assert_eq!(status, 200);
    assert!(ask("?a ex:city \"Leuven\""), "ex:other still links to it");
    let before = count();
    let (status, changed) = send_to(
        "PATCH",
        &at("s5"),
        json!({ "address": { "city": "Namur" } }),
    );
    assert_eq!(status, 200, "{changed}");
    assert!(!ask("?a ex:city \"Mons\""));
    assert!(!ask("?r ex:title \"Hainaut\""));
    // Six triples went (address and region, with their types and the cycle), two came.
    assert_eq!(count(), before - 6 + 2);
    // What was written reads back the same, for every record.
    let (_, s5) = send("GET", &at("s5"), None);
    assert_eq!(
        s5["address"],
        json!({ "type": "http://example.com/Address", "city": "Namur" }),
        "{s5}"
    );
    let (_, s3) = send("GET", &at("s3"), None);
    assert_eq!(s3["address"]["city"], "Brugge", "{s3}");

    // Deleting the record takes its embedded blank nodes with it.
    let (status, _) = ureq::delete(&one)
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .map(|r| (r.status(), ()))
        .unwrap();
    assert_eq!(status, 204);
    assert!(!ask("?a ex:city \"Antwerpen\""));
    assert!(ask("?a ex:city \"Brugge\""));
}
