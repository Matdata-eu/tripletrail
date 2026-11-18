//! The GraphQL API generated from the shapes (#144).

use serde_json::{Value, json};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

const DATA: &str = "@prefix ex: <http://example.com/> .\n\
    @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
    @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
    ex:o1 a ex:Operator ; rdfs:label \"Infrabel\" ; ex:seat ex:t1 .\n\
    ex:o2 a ex:Operator ; rdfs:label \"SNCF Réseau\" ; ex:seat ex:t2 .\n\
    ex:t1 a ex:Town ; rdfs:label \"Brussels\" ; ex:population 1200000 .\n\
    ex:t2 a ex:Town ; rdfs:label \"Saint-Denis\" ; ex:population 110000 .\n";

fn data() -> String {
    let mut ttl = DATA.to_owned();
    for i in 0..12 {
        let (country, operator) = if i < 8 { ("BE", "o1") } else { ("FR", "o2") };
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; rdfs:label \"Station {i}\"@en, \"Gare {i}\"@fr ; \
             ex:country \"{country}\" ; ex:tracks {} ; ex:open {} ; ex:hasOperator ex:{operator} ; \
             ex:address [ a ex:Address ; ex:city \"City {i}\" ; ex:near ex:t1 ] ; \
             geo:hasGeometry [ geo:asWKT \"POINT({i} 50)\"^^geo:wktLiteral ] .\n",
            i % 3 + 1,
            i % 2 == 0,
        ));
    }
    ttl
}

fn start() -> String {
    serve(&data())
}

fn serve(ttl: &str) -> String {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
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

/// Posts `query` (with `variables`); the answer's JSON.
fn post(base: &str, query: &str, variables: Value) -> Value {
    let r = ureq::post(&format!("{base}/kg/graphql"))
        .set("Content-Type", "application/json")
        .send_string(&json!({ "query": query, "variables": variables }).to_string())
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.content_type(), "application/json");
    serde_json::from_str(&r.into_string().unwrap()).unwrap()
}

#[test]
fn records_are_queried_with_graphql() {
    let base = start();

    // A page of a list, filtered, with references followed two levels deep.
    let answer = post(
        &base,
        "query($n: Int) { stations(first: $n, filter: {country: \"FR\"}) {
            edges { cursor node { id tracks open label
                hasOperator { id label seat { label population } } } }
            pageInfo { hasNextPage endCursor } } }",
        json!({ "n": 3 }),
    );
    assert!(answer["errors"].is_null(), "{answer}");
    let page = &answer["data"]["stations"];
    let edges = page["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 3, "{answer}");
    assert_eq!(page["pageInfo"]["hasNextPage"], true, "{answer}");
    let node = &edges[0]["node"];
    assert!(
        node["id"]
            .as_str()
            .unwrap()
            .starts_with("http://example.com/s")
    );
    assert!(node["tracks"].is_i64(), "{node}");
    assert!(node["open"].is_boolean(), "{node}");
    assert_eq!(node["label"]["fr"].as_str().unwrap()[..4], *"Gare");
    let operator = &node["hasOperator"];
    assert_eq!(operator["id"], "http://example.com/o2", "{node}");
    assert_eq!(operator["label"], "SNCF Réseau");
    assert_eq!(
        operator["seat"],
        json!({ "label": "Saint-Denis", "population": 110000 })
    );

    // The next page starts after the end cursor.
    let end = page["pageInfo"]["endCursor"].as_str().unwrap().to_owned();
    let next = post(
        &base,
        "query($after: String) { stations(first: 3, after: $after, filter: {country: \"FR\"}) {
            nodes { id } pageInfo { hasNextPage } } }",
        json!({ "after": end }),
    );
    assert_eq!(
        next["data"]["stations"]["nodes"].as_array().unwrap().len(),
        1,
        "{next}"
    );
    assert_eq!(next["data"]["stations"]["pageInfo"]["hasNextPage"], false);

    // One record, with an embedded object whose reference is followed too.
    let one = post(
        &base,
        "{ station(id: \"http://example.com/s1\") { label address { city near { label population } } } }",
        json!({}),
    );
    assert!(one["errors"].is_null(), "{one}");
    let station = &one["data"]["station"];
    assert_eq!(
        station["label"],
        json!({ "en": "Station 1", "fr": "Gare 1" })
    );
    assert_eq!(
        station["address"],
        json!({ "city": "City 1", "near": { "label": "Brussels", "population": 1200000 } }),
        "{one}"
    );
    let missing = post(
        &base,
        "{ station(id: \"http://example.com/o1\") { id } }",
        json!({}),
    );
    assert_eq!(missing["data"]["station"], Value::Null, "{missing}");

    // q and bbox, as in the REST API.
    let ids = |answer: &Value| -> Vec<String> {
        let mut ids: Vec<String> = answer["data"]["stations"]["nodes"]
            .as_array()
            .unwrap_or_else(|| panic!("{answer}"))
            .iter()
            .map(|n| {
                n["id"]
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
        ids(&post(
            &base,
            "{ stations(q: \"gare 11\") { nodes { id } } }",
            json!({})
        )),
        ["s11"]
    );
    assert_eq!(
        ids(&post(
            &base,
            "{ stations(bbox: [2.5, 49, 4.5, 51]) { nodes { id } } }",
            json!({})
        )),
        ["s3", "s4"]
    );

    // GET works for queries, and the schema can be introspected.
    let schema: Value = serde_json::from_str(
        &ureq::get(&format!("{base}/kg/graphql"))
            .query("query", "{ __schema { queryType { fields { name } } } }")
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    let roots: Vec<&str> = schema["data"]["__schema"]["queryType"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    for root in [
        "station",
        "stations",
        "operator",
        "operators",
        "town",
        "towns",
    ] {
        assert!(roots.contains(&root), "{root}: {roots:?}");
    }

    // Errors come as GraphQL errors.
    let wrong = post(&base, "{ stations { nodes { colour } } }", json!({}));
    assert!(
        wrong["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("colour"),
        "{wrong}"
    );
    let too_many = post(
        &base,
        "{ stations(first: 5000) { nodes { id } } }",
        json!({}),
    );
    assert!(too_many["errors"].is_array(), "{too_many}");
    let r = ureq::get(&format!("{base}/kg/graphql")).call();
    assert!(matches!(r, Err(ureq::Error::Status(400, _))));

    // The REST API's index links the endpoint and the playground; a browser that opens the
    // endpoint lands on the playground.
    let index: Value = serde_json::from_str(
        &ureq::get(&format!("{base}/kg/api"))
            .set("Accept", "application/json")
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(index["graphql"], format!("{base}/kg/graphql"), "{index}");
    assert_eq!(index["playground"], format!("{base}/$/ui/graphql/kg"));
    let page = ureq::get(&format!("{base}/kg/graphql"))
        .set("Accept", "text/html,*/*;q=0.8")
        .call()
        .unwrap();
    assert_eq!(page.get_url(), format!("{base}/$/ui/graphql/kg"));
    assert!(
        page.header("Content-Security-Policy")
            .unwrap()
            .contains("default-src 'self'")
    );
    let html = page.into_string().unwrap();
    assert!(html.contains("data-dataset=\"kg\""), "{html}");
    for file in ["graphql.js", "vendor/codemirror/codemirror.min.js"] {
        assert!(html.contains(&format!("../{file}")), "{file}");
        let r = ureq::get(&format!("{base}/$/ui/{file}")).call().unwrap();
        assert_eq!(r.status(), 200, "{file}");
    }
}

#[test]
fn class_names_never_clash_with_graphql_types() {
    // Classes named like built-in scalars, fixed types and another class's generated types.
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n",
    );
    for class in [
        "Int",
        "String",
        "ID",
        "Boolean",
        "Float",
        "Query",
        "PageInfo",
        "LangString",
        "Line",
        "LineEdge",
        "LineConnection",
        "LineFilter",
    ] {
        ttl.push_str(&format!(
            "ex:{class}1 a ex:{class} ; rdfs:label \"{class} one\" ; ex:count 3 .\n"
        ));
    }
    let base = serve(&ttl);
    let answer = post(
        &base,
        "{ __schema { types { name } } ints { nodes { id label count } } lines { nodes { id } }
           lineEdges { nodes { label } } queries { nodes { label } } }",
        json!({}),
    );
    assert!(answer["errors"].is_null(), "{answer}");
    assert_eq!(
        answer["data"]["ints"]["nodes"][0],
        json!({ "id": "http://example.com/Int1", "label": "Int one", "count": 3 })
    );
    assert_eq!(
        answer["data"]["lineEdges"]["nodes"][0]["label"],
        "LineEdge one"
    );
    assert_eq!(answer["data"]["queries"]["nodes"][0]["label"], "Query one");
}

#[test]
fn an_answer_has_a_bounded_size() {
    // 20 lines with 300 stations each, every station on every line: following the references
    // back and forth multiplies the answer by 6,000 per two levels.
    let mut ttl = String::from("@prefix ex: <http://example.com/> .\n");
    for l in 0..20 {
        for s in 0..300 {
            ttl.push_str(&format!(
                "ex:l{l} a ex:Line ; ex:hasStation ex:s{s} . ex:s{s} a ex:Station ; ex:onLine ex:l{l} .\n"
            ));
        }
    }
    let base = serve(&ttl);
    let answer = post(
        &base,
        "{ stations(first: 5) { nodes { id onLine { id hasStation { id onLine { id hasStation { id } } } } } } }",
        json!({}),
    );
    let message = answer["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(message.contains("more than 500000 values"), "{answer:.300}");
    // Many root fields share the one budget too.
    let aliases: String = (0..200)
        .map(|i| format!("a{i}: lines(first: 1000) {{ nodes {{ id hasStation {{ id }} }} }} "))
        .collect();
    let answer = post(&base, &format!("{{ {aliases} }}"), json!({}));
    assert!(answer["errors"].is_array(), "{answer:.300}");
    // The server still answers.
    let small = post(&base, "{ lines(first: 2) { nodes { id } } }", json!({}));
    assert_eq!(small["data"]["lines"]["nodes"].as_array().unwrap().len(), 2);
}
