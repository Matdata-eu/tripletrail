//! Finding resources by their labels (#142): `/{ds}/search/labels`.

use serde_json::{Value, json};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

fn start() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:o1 a ex:Operator ; rdfs:label \"Paris Transport\" .\n\
         ex:pn a ex:Station ; rdfs:label \"Paris Nord\" .\n\
         ex:pn3 a ex:Station ; rdfs:label \"Paris Nord Poste 3\" .\n\
         ex:ge a ex:Station ; rdfs:label \"Gent-Sint-Pieters\"@nl, \"Gand-Saint-Pierre\"@fr .\n\
         ex:koln a ex:Station ; rdfs:label \"Köln Hbf\" .\n",
    );
    for i in 0..12 {
        ttl.push_str(&format!(
            "ex:sig{i} a ex:Signal ; rdfs:label \"Paris signal {i}\" .\n"
        ));
    }
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
    base
}

fn search(base: &str, query: &str) -> Value {
    let r = ureq::get(&format!("{base}/kg/search/labels?{query}"))
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/json");
    serde_json::from_str(&r.into_string().unwrap()).unwrap()
}

fn labels(answer: &Value) -> Vec<&str> {
    answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap())
        .collect()
}

#[test]
fn resources_are_found_by_their_labels() {
    let base = start();

    // Every word, the last as a prefix (so it works while typing); the closest labels first.
    let found = search(&base, "q=paris+no");
    assert_eq!(
        labels(&found),
        ["Paris Nord", "Paris Nord Poste 3"],
        "{found}"
    );
    let first = &found["results"][0];
    assert_eq!(first["iri"], "http://example.com/pn");
    assert_eq!(first["class"], "Station");
    assert_eq!(
        first["describe"],
        format!("{base}/kg/describe?iri=http%3A%2F%2Fexample.com%2Fpn")
    );
    assert_eq!(
        first["record"],
        format!("{base}/kg/api/v1/Station/http%3A%2F%2Fexample.com%2Fpn")
    );
    // The classes to search in come along, for a picker.
    let classes: Vec<&str> = found["classes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    for class in ["Operator", "Signal", "Station"] {
        assert!(classes.contains(&class), "{classes:?}");
    }

    // Case and accents aside, any language.
    assert_eq!(labels(&search(&base, "q=KOLN")), ["Köln Hbf"]);
    assert_eq!(labels(&search(&base, "q=gand")), ["Gand-Saint-Pierre"]);

    // At most 5 of a class unless a class is asked for.
    let paris = search(&base, "q=paris");
    let of = |answer: &Value, class: &str| {
        answer["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["class"] == class)
            .count()
    };
    assert_eq!(of(&paris, "Signal"), 5, "{paris}");
    assert_eq!(of(&paris, "Station"), 2);
    assert_eq!(of(&paris, "Operator"), 1);
    let signals = search(&base, "q=paris&class=Signal&limit=8");
    assert_eq!(of(&signals, "Signal"), 8, "{signals}");
    assert_eq!(signals["results"].as_array().unwrap().len(), 8);
    let by_iri = search(
        &base,
        "q=paris&class=http%3A%2F%2Fexample.com%2FOperator,Station",
    );
    assert_eq!(by_iri["results"].as_array().unwrap().len(), 3, "{by_iri}");

    // Nothing for words no label has, or no words at all.
    assert!(labels(&search(&base, "q=zurich")).is_empty());
    assert!(labels(&search(&base, "q=%22%7D+")).is_empty());

    // Wrong requests.
    let status = |query: &str| match ureq::get(&format!("{base}/kg/search/labels?{query}")).call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(status(""), 400);
    assert_eq!(status("q=paris&limit=0"), 400);
    assert_eq!(status("q=paris&limit=101"), 400);
    assert_eq!(status("q=paris&class=Nothing"), 404);
    assert!(matches!(
        ureq::get(&format!("{base}/kg/search/other?q=x")).call(),
        Err(ureq::Error::Status(404, _))
    ));
    assert!(matches!(
        ureq::post(&format!("{base}/kg/search/labels?q=x")).call(),
        Err(ureq::Error::Status(405, _))
    ));
}

// The search builder (#142): `/{ds}/search/classes`, `/search/results` and `/search/sparql`,
// on the user guide's sample network.

fn network() -> String {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            include_bytes!("../../../docs/guide/data/network.ttl").as_slice(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("net".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn find(base: &str, model: &Value, limit: Option<&str>) -> (u16, Value, String) {
    let mut request =
        ureq::get(&format!("{base}/net/search/results")).query("m", &model.to_string());
    if let Some(limit) = limit {
        request = request.query("limit", limit);
    }
    match request.call() {
        Ok(r) => {
            let body = r.into_string().unwrap();
            (200, serde_json::from_str(&body).unwrap(), body)
        }
        Err(ureq::Error::Status(code, r)) => (code, Value::Null, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

fn ids(answer: &Value) -> Vec<String> {
    answer["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r["iri"]
                .as_str()
                .unwrap()
                .rsplit('#')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[test]
fn the_builder_lists_classes_and_their_properties() {
    let base = network();
    let r = ureq::get(&format!("{base}/net/search/classes"))
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/json");
    let answer: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    let classes = answer["classes"].as_array().unwrap();
    let section = classes.iter().find(|c| c["id"] == "Section").unwrap();
    assert_eq!(section["class"], "https://example.org/rail#Section");
    assert_eq!(section["count"], 12);
    let kinds: Vec<(&str, &str)> = section["properties"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["key"].as_str().unwrap(), p["kind"].as_str().unwrap()))
        .collect();
    for want in [
        ("electrified", "boolean"),
        ("from", "reference"),
        ("lengthKm", "number"),
        ("line", "text"),
        ("maxSpeed", "number"),
    ] {
        assert!(kinds.contains(&want), "{want:?} in {kinds:?}");
    }
    let from = section["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "from")
        .unwrap();
    assert_eq!(from["target"], "Station");
    let station = classes.iter().find(|c| c["id"] == "Station").unwrap();
    assert_eq!(station["count"], 10);
}

#[test]
fn builder_searches_give_the_expected_resources() {
    let base = network();
    let cases: Vec<(Value, Vec<&str>)> = vec![
        // Numbers, and a reference picked from the list (an IRI).
        (
            json!({"class": "Section", "where": [
                {"key": "maxSpeed", "op": "gt", "value": "120"},
                {"key": "from", "op": "is", "value": "https://example.org/rail#BXS"}]}),
            vec!["S08", "S09"],
        ),
        (
            json!({"class": "Section", "where": [{"key": "maxSpeed", "op": "between", "value": "100..150"}]}),
            vec!["S06", "S11", "S12"],
        ),
        (
            json!({"class": "Section", "where": [{"key": "maxSpeed", "op": "eq", "value": 300}]}),
            vec!["S07"],
        ),
        (
            json!({"class": "Section", "where": [{"key": "lengthKm", "op": "le", "value": "2"}]}),
            vec!["S01", "S02"],
        ),
        (
            json!({"class": "Section", "where": [
                {"key": "maxSpeed", "op": "ge", "value": "160"},
                {"key": "maxSpeed", "op": "lt", "value": "300"},
                {"key": "lengthKm", "op": "ne", "value": "30"}]}),
            vec!["S03", "S04", "S08", "S10"],
        ),
        // "any": either condition may hold.
        (
            json!({"class": "Section", "where": [
                {"any": [{"key": "maxSpeed", "op": "ge", "value": 300},
                         {"key": "electrified", "op": "false"}]},
                {"key": "lengthKm", "op": "gt", "value": 50}]}),
            vec!["S07", "S12"],
        ),
        (
            json!({"class": "Section", "where": [
                {"any": [{"key": "from", "op": "is", "value": "https://example.org/rail#BXS"},
                         {"key": "to", "op": "is", "value": "https://example.org/rail#BXS"}]}]}),
            vec!["S01", "S08", "S09"],
        ),
        (
            json!({"class": "Station", "where": [
                {"any": [{"key": "code", "op": "missing"}, {"key": "label", "op": "starts", "value": "nam"}]}]}),
            vec!["NAM"],
        ),
        // Booleans and text.
        (
            json!({"class": "Section", "where": [{"key": "electrified", "op": "false"}]}),
            vec!["S12"],
        ),
        (
            json!({"class": "Section", "where": [{"key": "line", "op": "starts", "value": "16"}]}),
            vec!["S09", "S10"],
        ),
        (
            json!({"class": "Section", "where": [{"key": "line", "op": "is", "value": "25"}]}),
            vec!["S03", "S04"],
        ),
        // Labels ignore case; resources come in label order.
        (
            json!({"class": "Station", "where": [{"key": "label", "op": "contains", "value": "BRUS"}]}),
            vec!["BXC", "BXN", "BXS"],
        ),
        (
            json!({"class": "Station", "where": [{"key": "code", "op": "missing"}]}),
            vec![],
        ),
        (
            json!({"class": "Station", "where": [{"key": "code", "op": "exists"}]}),
            // By label: Leuven before Liège.
            vec![
                "ANT", "BXC", "BXN", "BXS", "GNT", "LV", "LG", "MEC", "NAM", "OTN",
            ],
        ),
    ];
    for (model, want) in cases {
        let (code, answer, body) = find(&base, &model, None);
        assert_eq!(code, 200, "{model}: {body}");
        assert_eq!(ids(&answer), want, "{model}");
        assert_eq!(answer["count"], want.len(), "{model}");
        assert_eq!(answer["more"], false, "{model}");
    }
}

#[test]
fn builder_results_carry_columns_values_and_the_sparql() {
    let base = network();
    let model = json!({"class": "Section", "where": [
        {"key": "maxSpeed", "op": "gt", "value": "120"},
        {"key": "from", "op": "is", "value": "https://example.org/rail#BXS"}],
        "show": ["lengthKm", "electrified"]});
    let (_, answer, _) = find(&base, &model, None);
    let columns: Vec<&str> = answer["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key"].as_str().unwrap())
        .collect();
    assert_eq!(columns, ["maxSpeed", "from", "lengthKm", "electrified"]);
    let first = &answer["rows"][0];
    assert_eq!(first["iri"], "https://example.org/rail#S08");
    assert_eq!(
        first["describe"],
        format!("{base}/net/describe?iri=https%3A%2F%2Fexample.org%2Frail%23S08")
    );
    assert_eq!(first["values"]["maxSpeed"], 160);
    assert_eq!(first["values"]["lengthKm"], 55);
    assert_eq!(first["values"]["electrified"], true);
    assert_eq!(
        first["values"]["from"]["iri"],
        "https://example.org/rail#BXS"
    );
    assert_eq!(first["values"]["from"]["label"], "Brussels-South");

    // The same search as SPARQL, which gives the same resources.
    let r = ureq::get(&format!("{base}/net/search/sparql"))
        .query("m", &model.to_string())
        .call()
        .unwrap();
    assert!(r.content_type().starts_with("application/sparql-query"));
    let sparql = r.into_string().unwrap();
    assert_eq!(sparql, answer["sparql"].as_str().unwrap());
    let r = ureq::post(&format!("{base}/net/query"))
        .set("Content-Type", "application/sparql-query")
        .set("Accept", "application/sparql-results+json")
        .send_string(&sparql)
        .unwrap();
    let results: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    let found: Vec<&str> = results["results"]["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["s"]["value"].as_str().unwrap())
        .collect();
    assert_eq!(
        found,
        [
            "https://example.org/rail#S08",
            "https://example.org/rail#S09"
        ]
    );
}

#[test]
fn builder_results_are_limited_and_counted() {
    let base = network();
    let model = json!({"class": "Section"});
    let (_, answer, _) = find(&base, &model, Some("5"));
    assert_eq!(ids(&answer), ["S01", "S02", "S03", "S04", "S05"]);
    assert_eq!(answer["count"], 12);
    assert_eq!(answer["more"], true);
    let (_, answer, _) = find(&base, &model, Some("12"));
    assert_eq!(answer["rows"].as_array().unwrap().len(), 12);
    assert_eq!(answer["more"], false);
}

#[test]
fn bad_builder_searches_are_refused() {
    let base = network();
    for (model, code, says) in [
        (json!({"class": "Nope"}), 404, "No class Nope"),
        (
            json!({"class": "Section", "where": [{"key": "nope", "op": "eq", "value": "1"}]}),
            422,
            "no key nope",
        ),
        (
            json!({"class": "Section", "where": [{"key": "maxSpeed", "op": "ge", "value": "abc"}]}),
            422,
            "needs a number",
        ),
        (
            json!({"class": "Section", "where": [{"key": "to", "op": "is", "value": "Liège"}]}),
            422,
            "not an IRI",
        ),
        (
            json!({"class": "Section", "where": [{"key": "maxSpeed", "op": "nearby", "value": "1"}]}),
            422,
            "",
        ),
        (
            json!({"class": "Section", "where": vec![json!({"key": "line", "op": "exists"}); 21]}),
            422,
            "at most 20",
        ),
        (json!({"where": []}), 400, "no class"),
        (
            json!({"class": "Section", "where": [{"any": []}]}),
            422,
            "1 to 10",
        ),
        (
            json!({"class": "Section", "where": [{"any": [{"any": [{"key": "line", "op": "exists"}]}]}]}),
            422,
            "do not nest",
        ),
    ] {
        let (got, _, body) = find(&base, &model, None);
        assert_eq!(got, code, "{model}: {body}");
        assert!(body.contains(says), "{model}: {body}");
    }
    let err = ureq::get(&format!("{base}/net/search/results"))
        .call()
        .unwrap_err();
    assert!(matches!(err, ureq::Error::Status(400, _)));
    let err = ureq::get(&format!("{base}/net/search/results?m=not+json"))
        .call()
        .unwrap_err();
    assert!(matches!(err, ureq::Error::Status(400, _)));
    let err = ureq::get(&format!("{base}/net/search/nothing"))
        .call()
        .unwrap_err();
    assert!(matches!(err, ureq::Error::Status(404, _)));
}

fn suggest(base: &str, model: &Value, params: &[(&str, &str)]) -> Value {
    let mut request =
        ureq::get(&format!("{base}/net/search/values")).query("m", &model.to_string());
    for (k, v) in params {
        request = request.query(k, v);
    }
    serde_json::from_str(&request.call().unwrap().into_string().unwrap()).unwrap()
}

fn pairs(answer: &Value) -> Vec<(String, u64)> {
    answer["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            let name = v["label"].as_str().unwrap_or(v["value"].as_str().unwrap());
            (name.to_owned(), v["count"].as_u64().unwrap())
        })
        .collect()
}

#[test]
fn values_are_suggested_with_their_counts() {
    let base = network();
    let all = json!({"class": "Section"});
    // The most frequent first, then by value.
    let speeds = suggest(&base, &all, &[("key", "maxSpeed")]);
    assert_eq!(speeds["kind"], "number");
    assert_eq!(
        pairs(&speeds),
        [
            ("160".into(), 6),
            ("120".into(), 3),
            ("50".into(), 2),
            ("300".into(), 1)
        ]
    );
    // A reference comes with its label; `q` matches the label.
    let from = suggest(&base, &all, &[("key", "from"), ("q", "brus")]);
    assert_eq!(
        pairs(&from),
        [
            ("Brussels-South".into(), 3),
            ("Brussels-North".into(), 2),
            ("Brussels-Central".into(), 1)
        ]
    );
    assert_eq!(from["values"][0]["value"], "https://example.org/rail#BXS");
    assert_eq!(from["values"][0]["iri"], true);
    // The other conditions narrow the values; the one at `at` does not.
    let fast = json!({"class": "Section", "where": [
        {"key": "maxSpeed", "op": "ge", "value": 160},
        {"key": "line", "op": "is", "value": "nothing like it"}]});
    let lines = suggest(&base, &fast, &[("key", "line"), ("at", "1")]);
    assert_eq!(
        pairs(&lines),
        [
            ("161".into(), 2),
            ("25".into(), 2),
            ("2".into(), 1),
            ("36".into(), 1),
            ("50A".into(), 1)
        ]
    );
    // limit, and whether there are more.
    let two = suggest(&base, &all, &[("key", "maxSpeed"), ("limit", "2")]);
    assert_eq!(two["values"].as_array().unwrap().len(), 2);
    assert_eq!(two["more"], true);
    assert_eq!(speeds["more"], false);
    // Refusals.
    for (params, code) in [
        (vec![("key", "nope")], 422),
        (vec![("key", "hasGeometry")], 422),
        (vec![("key", "line"), ("limit", "0")], 400),
        (vec![("key", "line"), ("at", "x")], 400),
        (vec![], 400),
    ] {
        let mut request =
            ureq::get(&format!("{base}/net/search/values")).query("m", &all.to_string());
        for (k, v) in &params {
            request = request.query(k, v);
        }
        match request.call() {
            Err(ureq::Error::Status(got, _)) => assert_eq!(got, code, "{params:?}"),
            other => panic!("{params:?}: {:?}", other.map(|r| r.status())),
        }
    }
}

#[test]
fn exists_and_missing_work_inside_an_or_group() {
    let ttl = "@prefix ex: <http://example.com/> .\n\
               ex:a a ex:Thing ; ex:n 1 ; ex:note \"x\" .\n\
               ex:b a ex:Thing ; ex:n 2 .\n\
               ex:c a ex:Thing ; ex:n 3 .\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("net".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let ids = |answer: &Value| -> Vec<String> {
        answer["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r["iri"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    for (any, want) in [
        // Only a has a note: the exists branch adds a, not every thing.
        (
            json!([{"key": "note", "op": "exists"}, {"key": "n", "op": "ge", "value": 3}]),
            vec!["a", "c"],
        ),
        // b and c have none: the missing branch adds them.
        (
            json!([{"key": "note", "op": "missing"}, {"key": "n", "op": "eq", "value": 1}]),
            vec!["a", "b", "c"],
        ),
        (
            json!([{"key": "note", "op": "missing"}, {"key": "n", "op": "gt", "value": 5}]),
            vec!["b", "c"],
        ),
    ] {
        let model = json!({"class": "Thing", "where": [{"any": any}]});
        let (code, answer, body) = find(&base, &model, None);
        assert_eq!(code, 200, "{model}: {body}");
        assert_eq!(ids(&answer), want, "{model}");
    }
}

#[test]
fn a_search_keeps_to_a_map_area() {
    let base = network();
    let brussels = json!([4.30, 50.82, 4.40, 50.87]);
    // Stations in the area, alone or with conditions; the box may be given as text too.
    let (status, found, body) = find(&base, &json!({"class": "Station", "area": brussels}), None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&found), ["BXC", "BXN", "BXS"]);
    assert_eq!(found["count"], 3);
    assert!(found["sparql"].as_str().unwrap().contains("sfIntersects"));
    let (_, found, _) = find(
        &base,
        &json!({"class": "https://example.org/rail#Station", "area": "4.30,50.82,4.40,50.87",
            "where": [{"key": "code", "op": "starts", "value": "bxn"}]}),
        None,
    );
    assert_eq!(ids(&found), ["BXN"]);
    // Lines that cross it, and the values suggested within it.
    let (_, found, _) = find(&base, &json!({"class": "Section", "area": brussels}), None);
    let sections = ids(&found);
    assert!(
        sections.starts_with(&["S01".to_owned(), "S02".to_owned()]),
        "{sections:?}"
    );
    assert!(!sections.contains(&"S07".to_owned()), "{sections:?}");
    let speeds = suggest(
        &base,
        &json!({"class": "Section", "where": [], "area": brussels}),
        &[("key", "maxSpeed")],
    );
    let counted: u64 = pairs(&speeds).iter().map(|(_, n)| n).sum();
    assert_eq!(counted, sections.len() as u64);
    // Outside everything, nothing; a bad box, or a class with no geometry, is refused.
    let (_, found, _) = find(
        &base,
        &json!({"class": "Station", "area": [10, 10, 11, 11]}),
        None,
    );
    assert_eq!(found["count"], 0);
    let (status, _, _) = find(
        &base,
        &json!({"class": "Station", "area": [4, 51, 5]}),
        None,
    );
    assert_eq!(status, 400);
    let kg = start();
    let r = ureq::get(&format!("{kg}/kg/search/results"))
        .query(
            "m",
            &json!({"class": "Station", "area": brussels}).to_string(),
        )
        .call();
    match r {
        Err(ureq::Error::Status(code, r)) => {
            assert_eq!(code, 422);
            assert!(r.into_string().unwrap().contains("no geometry"));
        }
        other => panic!("{:?}", other.map(|r| r.status())),
    }
}

#[test]
fn columns_show_every_value_of_a_property() {
    // Lines run through several stations, and carry several names; the columns do not
    // multiply each other's rows.
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:a a ex:Station ; rdfs:label \"Zottegem\" .\n\
         ex:b a ex:Station ; rdfs:label \"Brugge\" .\n\
         ex:c a ex:Station ; rdfs:label \"Charleroi\" .\n\
         ex:l1 a ex:Line ; rdfs:label \"L1\" ; ex:via ex:c, ex:a, ex:b ; ex:alias \"one\", \"first\" .\n\
         ex:l2 a ex:Line ; rdfs:label \"L2\" ; ex:via ex:b ; ex:alias \"two\" .\n\
         ex:l3 a ex:Line ; rdfs:label \"L3\" .\n",
    );
    for i in 0..12 {
        ttl.push_str(&format!("ex:l1 ex:code \"c{i:02}\" .\n"));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("net".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let (status, found, body) = find(
        &base,
        &json!({"class": "Line", "show": ["via", "alias", "code"]}),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let rows = found["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let via = |row: &Value| -> Vec<String> {
        row["values"]["via"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["label"].as_str().unwrap().to_owned())
            .collect()
    };
    // In label order, not IRI order.
    assert_eq!(via(&rows[0]), ["Brugge", "Charleroi", "Zottegem"]);
    assert_eq!(rows[0]["values"]["alias"], json!(["first", "one"]));
    let codes = rows[0]["values"]["code"].as_array().unwrap();
    assert_eq!(codes.len(), 10);
    assert_eq!(codes[0], "c00");
    assert_eq!(rows[0]["moreValues"], json!({"code": 2}));
    assert_eq!(via(&rows[1]), ["Brugge"]);
    assert_eq!(rows[1].get("moreValues"), None);
    assert_eq!(rows[2]["values"]["via"], json!([]));
    assert_eq!(found["count"], 3);
    // The search's SPARQL leaves them to a query of their own; the Query page's has them all.
    assert!(
        !found["sparql"].as_str().unwrap().contains("ex:via")
            && !found["sparql"]
                .as_str()
                .unwrap()
                .contains("example.com/via")
    );
    let whole = ureq::get(&format!("{base}/net/search/sparql"))
        .query("m", &json!({"class": "Line", "show": ["via"]}).to_string())
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(whole.contains("GROUP_CONCAT(DISTINCT STR(?c0)"), "{whole}");
}

/// `--text-properties` (#188) reaches every dataset that `*` names: those served at start and
/// one created over the admin API while the server runs.
#[test]
fn text_properties_for_every_dataset_reach_one_created_later() {
    let code = tripletrail::oxigraph::model::NamedNode::new("http://example.com/code").unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "<http://example.com/a> <http://example.com/code> \"startcode\" .".as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            text_properties: vec![("*".to_owned(), code)],
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let found = |ds: &str, word: &str| -> String {
        let q = format!(
            "PREFIX text: <http://jena.apache.org/text#> SELECT ?s WHERE {{ ?s text:query (<http://example.com/code> \"{word}\") }}"
        );
        ureq::post(&format!("{base}/{ds}/query"))
            .set("Content-Type", "application/sparql-query")
            .set("Accept", "text/csv")
            .send_string(&q)
            .unwrap_or_else(|e| panic!("{}", e.into_response().unwrap().into_string().unwrap()))
            .into_string()
            .unwrap()
    };
    assert!(found("kg", "startcode").contains("http://example.com/a"));
    let created = ureq::post(&format!("{base}/$/datasets?dbName=fresh&dbType=mem"))
        .call()
        .unwrap();
    assert!(created.status() < 300);
    ureq::post(&format!("{base}/fresh/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(
            r#"INSERT DATA { <http://example.com/f> <http://example.com/code> "freshcode" }"#,
        )
        .unwrap();
    assert!(found("fresh", "freshcode").contains("http://example.com/f"));
}
