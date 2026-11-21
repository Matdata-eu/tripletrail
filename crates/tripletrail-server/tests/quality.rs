//! A stored SHACL validation report as the map's data-quality facet (#92).

use serde_json::Value;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

fn get(url: &str) -> Value {
    serde_json::from_str(&ureq::get(url).call().unwrap().into_string().unwrap()).unwrap()
}

#[test]
fn a_stored_report_is_a_facet_of_every_class() {
    let dir = tripletrail_testdir::tempdir("quality");
    let trig = "@prefix ex: <http://example.com/> .\n\
                @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
                @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                ex:s1 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
                ex:s2 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(2 2)\"^^geo:wktLiteral ] .\n\
                ex:s3 a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT(3 3)\"^^geo:wktLiteral ] .\n\
                ex:l1 a ex:Line ; geo:hasGeometry [ geo:asWKT \"LINESTRING(1 1, 2 2)\"^^geo:wktLiteral ] .\n\
                <urn:report> {\n\
                  [] a sh:ValidationReport ; sh:conforms false ;\n\
                    sh:result [ sh:focusNode ex:s1 ; sh:resultSeverity sh:Warning ] ,\n\
                      [ sh:focusNode ex:s1 ; sh:resultSeverity sh:Violation ] ,\n\
                      [ sh:focusNode ex:s2 ; sh:resultSeverity sh:Warning ] ,\n\
                      [ sh:focusNode ex:l1 ; sh:resultSeverity sh:Info ] .\n\
                }\n";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, trig.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 2,
            admin_token: Some("t".to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let quality = |url: &str| -> Option<Value> {
        get(url)["facets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "quality")
            .cloned()
    };
    let counts = |facet: &Value| -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = facet["values"]
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
        out.sort();
        out
    };
    // No facet until the shapes name the report.
    assert!(quality(&format!("{base}/kg/tiles/facets/Station")).is_none());
    ureq::put(&format!("{base}/$/datasets/kg/shapes/files/_set.ttl"))
        .set("Authorization", "Bearer t")
        .send_string(
            "@prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
             <urn:my:set> a tts:ShapeSet ; tts:qualityReport <urn:report> .\n",
        )
        .unwrap();
    // Each station by its worst result: s1 has a warning and a violation.
    let station = quality(&format!("{base}/kg/tiles/facets/Station")).unwrap();
    assert_eq!(station["title"], "Data quality");
    assert_eq!(
        counts(&station),
        [
            ("ok".to_owned(), 1),
            ("violation".to_owned(), 1),
            ("warning".to_owned(), 1)
        ]
    );
    // A filter, as on any facet.
    let violating = get(&format!("{base}/kg/tiles/facets/Station?quality=violation"));
    assert_eq!(violating["features"], 1, "{violating}");
    // Shared by the collections of one panel.
    let both = get(&format!(
        "{base}/kg/tiles/facets?collection=Station&collection=Line"
    ));
    let shared = both["shared"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "quality")
        .cloned()
        .unwrap();
    assert_eq!(
        counts(&shared),
        [
            ("info".to_owned(), 1),
            ("ok".to_owned(), 1),
            ("violation".to_owned(), 1),
            ("warning".to_owned(), 1)
        ]
    );
    // A tile coloured by it carries each feature's severity.
    let r = ureq::get(&format!("{base}/kg/tiles/Station/0/0/0.mvt?color=quality"))
        .call()
        .unwrap();
    let mut tile = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut tile).unwrap();
    let holds = |text: &str| tile.windows(text.len()).any(|w| w == text.as_bytes());
    assert!(holds("violation") && holds("warning") && holds("ok"));
    // The OGC API has no such property.
    let items = get(&format!("{base}/kg/ogc/collections/Station/items"));
    assert!(
        items["features"][0]["properties"].get("quality").is_none(),
        "{items}"
    );
}
