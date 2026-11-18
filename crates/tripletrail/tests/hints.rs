//! Query hints (#238): `#pragma tt.…` lines switch off the optimizer, a rewrite or the result
//! cache for one query or update, and the answers stay the same.

use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::{CacheStatus, Engine, Error, QueryOptions};

const P: &str = "PREFIX ex: <http://example.org/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
";

fn engine() -> Engine {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..400 {
        ttl.push_str(&format!(
            "ex:s{i} a ex:{} ; ex:km {} ; geo:hasGeometry ex:g{i} . ex:g{i} geo:asWKT \"POINT({} {})\"^^geo:wktLiteral .\n",
            if i % 20 == 0 { "Station" } else { "Signal" },
            i % 100,
            i % 20,
            i / 20,
        ));
    }
    ttl.push_str("ex:area geo:asWKT \"POLYGON((0 0, 5 0, 5 5, 0 5, 0 0))\"^^geo:wktLiteral .\n");
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, q: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(&format!("{P}{q}")).unwrap() else {
        panic!("a SELECT")
    };
    let mut v: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    v.sort();
    v
}

fn explain(e: &Engine, q: &str) -> serde_json::Value {
    serde_json::from_str(&e.explain(&format!("{P}{q}")).unwrap()).unwrap()
}

#[test]
fn each_hint_turns_its_step_off_and_keeps_the_answers() {
    let e = engine();
    let cases = [
        (
            "tt.spatial-rewrite off",
            "SELECT ?s WHERE { ex:area geo:asWKT ?a . ?s geo:hasGeometry/geo:asWKT ?w . FILTER(geof:sfWithin(?w, ?a)) }",
        ),
        (
            "tt.range-rewrite off",
            "SELECT ?s WHERE { ?s ex:km ?km . FILTER(?km >= 10 && ?km <= 10.5) }",
        ),
        (
            "tt.id-rewrites off",
            "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
        ),
        (
            "tt.optimizer off",
            "SELECT ?s ?km WHERE { ?s ex:km ?km . ?s a ex:Station }",
        ),
    ];
    for (hint, q) in cases {
        let hinted = format!("#pragma {hint}\n{q}");
        assert_eq!(rows(&e, &hinted), rows(&e, q), "{hint}");
        let (plain, with) = (explain(&e, q), explain(&e, &hinted));
        assert_ne!(plain, with, "{hint} changes nothing");
        assert_eq!(with["hints"][0], format!("#pragma {hint}"), "{with}");
        assert!(plain.get("hints").is_none(), "{plain}");
        match hint {
            "tt.spatial-rewrite off" => {
                assert!(plain["spatial"]["joins"].as_u64().unwrap() > 0, "{plain}");
                assert_eq!(with["spatial"]["joins"], 0, "{with}");
                assert_eq!(with["spatial"]["filters"], 0, "{with}");
            }
            "tt.range-rewrite off" => {
                assert!(plain.to_string().contains("range ?km"), "{plain}");
                assert!(!with.to_string().contains("range ?km"), "{with}");
            }
            "tt.id-rewrites off" => {
                assert!(
                    !plain["plan"].to_string().contains("QuadPattern"),
                    "{plain}"
                );
                assert!(with["plan"].to_string().contains("QuadPattern"), "{with}");
            }
            _ => {}
        }
    }
    // Hints hold for the request only.
    let q = "SELECT ?s WHERE { ?s ex:km ?km . FILTER(?km >= 10 && ?km <= 10.5) }";
    e.query(&format!("{P}#pragma tt.range-rewrite off\n{q}"))
        .unwrap();
    assert!(explain(&e, q).to_string().contains("range ?km"));
    // The written join order: the scan of every ex:km triple comes first.
    let fixed = explain(
        &e,
        "#pragma tt.join-order fixed\nSELECT ?s ?km WHERE { ?s ex:km ?km . ?s a ex:Station }",
    );
    let plan = fixed["plan"].to_string();
    let (km, class) = (plan.find("km").unwrap(), plan.find("Station").unwrap());
    assert!(km < class, "{plan}");
}

#[test]
fn the_result_cache_hint_bypasses_the_cache() {
    let e = engine();
    e.set_result_cache(1 << 20);
    let status = |q: &str| {
        let (r, s) = e
            .query_with_cache_status(&format!("{P}{q}"), QueryOptions::default())
            .unwrap();
        if let QueryResults::Solutions(s) = r {
            s.for_each(drop);
        }
        s
    };
    let q = "SELECT ?s WHERE { ?s a ex:Station }";
    assert_eq!(status(q), CacheStatus::Miss);
    assert_eq!(status(q), CacheStatus::Hit);
    let hinted = format!("#pragma tt.result-cache off\n{q}");
    assert_eq!(status(&hinted), CacheStatus::Bypass);
    // A hint that changes the work is not answered from the run without it, nor kept.
    let plan = format!("#pragma tt.optimizer off\n{q}");
    assert_eq!(status(&plan), CacheStatus::Bypass);
    assert_eq!(status(&plan), CacheStatus::Bypass);
    assert_eq!(status(q), CacheStatus::Hit);
}

#[test]
fn pragma_lines_inside_literals_are_data() {
    let e = engine();
    e.update(&format!(
        "{P}INSERT DATA {{ ex:doc ex:text \"\"\"line one\n#pragma tt.nope off\nline three\"\"\" , '''a\n#pragma tt.optimizer off\nb''' }}"
    ))
    .unwrap();
    assert_eq!(rows(&e, "SELECT ?t WHERE { ex:doc ex:text ?t }").len(), 2);
    let q = "SELECT ?x WHERE { BIND(\"\"\"x\n#pragma tt.optimizer off\n\"\"\" AS ?x) }";
    assert_eq!(rows(&e, q).len(), 1);
    assert!(explain(&e, q).get("hints").is_none());
}

#[test]
fn hints_apply_to_updates_and_unknown_ones_are_refused() {
    let e = engine();
    e.update(&format!(
        "{P}#pragma tt.optimizer off\nINSERT {{ ?s ex:near true }} WHERE {{ ?s ex:km 3 . ?s a ex:Signal }}"
    ))
    .unwrap();
    assert_eq!(rows(&e, "SELECT ?s WHERE { ?s ex:near true }").len(), 4);
    let Err(err) = e.query(&format!(
        "{P}#pragma tt.optimiser off\nSELECT * {{ ?s ?p ?o }}"
    )) else {
        panic!("an unknown hint is refused")
    };
    assert!(
        matches!(&err, Error::Request(m) if m.contains("tt.optimiser")),
        "{err}"
    );
    assert!(e.update("#pragma tt.timeout never\nCLEAR DEFAULT").is_err());
    // Another engine's pragma is a comment.
    assert_eq!(
        rows(
            &e,
            "#pragma join.order.fixed\nSELECT ?s WHERE { ?s a ex:Station }"
        )
        .len(),
        20
    );
}

#[test]
fn blazegraph_hint_triples_are_taken_out_and_mapped() {
    let e = engine();
    let plain = "SELECT ?s ?km WHERE { ?s ex:km ?km . ?s a ex:Station }";
    let hinted = "PREFIX hint: <http://www.bigdata.com/queryHints#>
        SELECT ?s ?km WHERE { hint:Query hint:optimizer \"None\" .
            ?s ex:km ?km . hint:Prior hint:runFirst true . ?s a ex:Station
            OPTIONAL { ?s ex:km ?k2 . hint:Group hint:analytic true } }";
    // The hint triples match no data: the answers are those without them.
    let expected: Vec<String> = rows(&e, plain);
    let got: Vec<String> = rows(&e, hinted)
        .into_iter()
        .map(|r| r.split(" k2=").next().unwrap().to_owned())
        .collect();
    assert_eq!(got, expected);
    assert_eq!(expected.len(), 20);
    // hint:optimizer "None" is tt.optimizer off: the written join order.
    let with = explain(&e, hinted);
    assert_eq!(with["hints"][0], "#pragma tt.optimizer off", "{with}");
    let plan = with["plan"].to_string();
    assert!(!plan.contains("queryHints"), "{plan}");
    let (km, class) = (plan.find("km").unwrap(), plan.find("Station").unwrap());
    assert!(km < class, "{plan}");
    // Other hints are taken out but change nothing.
    let other = "PREFIX hint: <http://www.bigdata.com/queryHints#>
        SELECT ?s ?km WHERE { ?s ex:km ?km . ?s a ex:Station . hint:Prior hint:runFirst true }";
    assert_eq!(rows(&e, other), expected);
    assert!(explain(&e, other).get("hints").is_none());
    // In an update's WHERE too.
    e.update(&format!(
        "{P}PREFIX hint: <http://www.bigdata.com/queryHints#>
        INSERT {{ ?s ex:seen true }} WHERE {{ hint:Query hint:optimizer \"None\" . ?s a ex:Station }}"
    ))
    .unwrap();
    assert_eq!(rows(&e, "SELECT ?s WHERE { ?s ex:seen true }").len(), 20);
}

#[test]
fn virtuoso_defines_are_read_and_taken_out() {
    let e = engine();
    let plain = "SELECT ?s ?km WHERE { ?s ex:km ?km . ?s a ex:Station }";
    let QueryResults::Solutions(s) = e
        .query(&format!("DEFINE sql:select-option \"order\"\n{P}{plain}"))
        .unwrap()
    else {
        panic!("a SELECT")
    };
    assert_eq!(s.count(), 20);
    let with: serde_json::Value = serde_json::from_str(
        &e.explain(&format!("define sql:select-option \"order\" {P}{plain}"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(with["hints"][0], "#pragma tt.optimizer off", "{with}");
    // Inference or other default graphs would change the answers: refused.
    assert!(matches!(
        e.query(&format!("DEFINE input:inference \"rules\"\n{P}{plain}")),
        Err(Error::Request(_))
    ));
}

#[test]
fn blazegraph_hints_inside_exists_are_taken_out() {
    let e = engine();
    let h = "PREFIX hint: <http://www.bigdata.com/queryHints#>\n";
    let n = |q: &str| rows(&e, &format!("{h}{q}")).len();
    assert_eq!(
        n(
            "SELECT * { ?s a ex:Station FILTER EXISTS { hint:Query hint:analytic true . ?s ex:km ?k } }"
        ),
        20
    );
    assert_eq!(
        n(
            "SELECT * { ?s a ex:Station FILTER NOT EXISTS { hint:Query hint:analytic true . ?s ex:km ?k } }"
        ),
        0
    );
    let QueryResults::Solutions(mut s) = e
        .query(&format!(
            "{P}{h}SELECT (EXISTS {{ hint:Query hint:x 1 }} AS ?e) {{}}"
        ))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        s.next().unwrap().unwrap().get("e").unwrap().to_string(),
        "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
    );
    assert_eq!(
        n(
            "SELECT ?s ?e { ?s a ex:Station BIND(EXISTS { hint:Query hint:analytic true . ?s ex:km ?k } AS ?e) FILTER(?e) }"
        ),
        20
    );
    assert_eq!(
        n(
            "SELECT ?s (COUNT(*) AS ?c) { ?s a ex:Station } GROUP BY ?s HAVING (EXISTS { hint:Prior hint:x 1 . ?s ex:km ?k })"
        ),
        20
    );
    assert_eq!(
        n(
            "SELECT ?s { ?s a ex:Station OPTIONAL { ?s ex:km ?k FILTER EXISTS { hint:Query hint:x 1 } } FILTER(BOUND(?k)) }"
        ),
        20
    );
    let with = explain(
        &e,
        &format!(
            "{h}SELECT * {{ ?s a ex:Station FILTER EXISTS {{ hint:Query hint:optimizer \"None\" . ?s ex:km ?k }} }}"
        ),
    );
    assert_eq!(with["hints"][0], "#pragma tt.optimizer off", "{with}");
}
