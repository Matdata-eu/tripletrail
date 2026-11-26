//! SHACL validation and SHACL-AF inference over HTTP (#401): on a workspace's scratch store,
//! with provenance, and reading only on a dataset.

use serde_json::{Value, json};
use tripletrail::Engine;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail_server::{Options, Server, WorkspaceOptions};

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("reason-{name}"))
}

const DATA: &str = r#"@prefix ex: <http://example.com/> .
ex:a1 a ex:A ; ex:name "one" .
ex:a2 a ex:A ; ex:name "two" .
ex:a3 a ex:A .
ex:b1 a ex:B .
"#;

/// Rules: a triple rule types every ex:A as ex:Thing; a SPARQL rule copies ex:name to
/// ex:label.
const RULES: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
ex:Rules a sh:NodeShape ; sh:targetClass ex:A ;
  sh:rule ex:typeRule , ex:labelRule .
ex:typeRule a sh:TripleRule ; sh:subject sh:this ; sh:predicate <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ; sh:object ex:Thing .
ex:labelRule a sh:SPARQLRule ;
  sh:construct "CONSTRUCT { $this <http://example.com/label> ?n } WHERE { $this <http://example.com/name> ?n }" .
"#;

/// Every ex:A needs an ex:label; a shape for a class with no instances.
const SHAPES: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
ex:AShape a sh:NodeShape ; sh:targetClass ex:A ;
  sh:property [ sh:path ex:label ; sh:minCount 1 ; sh:message "needs a label" ] .
ex:ZShape a sh:NodeShape ; sh:targetClass ex:Z .
"#;

/// A server over `datasets`, and the folder of its workspaces (kept while the test runs).
fn start(datasets: Vec<(String, Engine)>) -> (String, tripletrail_testdir::TestDir) {
    let dir = temp_dir("workspaces");
    let server = Server::bind(
        "127.0.0.1:0",
        datasets,
        Options {
            threads: 2,
            workspaces: WorkspaceOptions {
                dir: Some(dir.to_path_buf()),
                ..WorkspaceOptions::default()
            },
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    (base, dir)
}

fn json_of(r: ureq::Response) -> Value {
    serde_json::from_str(&r.into_string().unwrap()).unwrap()
}

fn status(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

fn workspace(base: &str, files: Value) -> String {
    let r = ureq::post(&format!("{base}/$/workspaces"))
        .set("Content-Type", "application/json")
        .send_string(&json!({ "files": files }).to_string())
        .unwrap();
    json_of(r)["id"].as_str().unwrap().to_owned()
}

#[test]
fn infer_with_provenance_on_a_workspace() {
    let (base, _dir) = start(Vec::new());
    let id = workspace(&base, json!({ "data/d.ttl": DATA, "rules/r.ttl": RULES }));
    let url = format!("{base}/$/workspaces/{id}/infer");
    let v = json_of(ureq::post(&url).query("provenance", "true").call().unwrap());
    // ex:Thing for a1..a3, ex:label for a1, a2.
    assert_eq!(v["count"], 5);
    let triples = v["triples"].as_array().unwrap();
    let why = v["provenance"].as_array().unwrap();
    assert_eq!(why.len(), 5);
    for (t, w) in triples.iter().zip(why) {
        assert_eq!(w["kind"], "rule");
        assert_eq!(w["shape"]["value"], "http://example.com/Rules");
        // The focus node is the subject in both rules.
        assert_eq!(w["focus"], t["subject"]);
        if t["predicate"]["value"] == "http://example.com/label" {
            assert_eq!(w["rule"], "<http://example.com/labelRule>");
            assert_eq!(w["bindings"]["n"], t["object"]);
            assert_eq!(w["bindings"]["this"], t["subject"]);
        } else {
            assert_eq!(w["rule"], "<http://example.com/typeRule>");
            assert_eq!(w["bindings"], json!({}));
        }
    }
    let label_rule = &v["rules"]["<http://example.com/labelRule>"];
    assert!(
        label_rule["construct"]
            .as_str()
            .unwrap()
            .starts_with("CONSTRUCT")
    );
    assert_eq!(
        label_rule["type"],
        "<http://www.w3.org/ns/shacl#SPARQLRule>"
    );
    assert!(!v["runs"].as_array().unwrap().is_empty());

    // Without provenance: the same triples, no provenance.
    let plain = json_of(ureq::post(&url).call().unwrap());
    assert_eq!(plain["count"], 5);
    assert!(plain.get("provenance").is_none());
    // As RDF by Accept.
    let r = ureq::post(&url)
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(r.content_type(), "application/n-triples");
    let text = r.into_string().unwrap();
    let n = RdfParser::from_format(RdfFormat::NTriples)
        .for_slice(text.as_bytes())
        .count();
    assert_eq!(n, 5);
    // One pass, and existing triples too.
    let v = json_of(
        ureq::post(&url)
            .query("single_pass", "true")
            .query("include_existing", "true")
            .call()
            .unwrap(),
    );
    assert_eq!(v["count"], 5);
}

#[test]
fn validate_a_workspace_after_its_rules() {
    let (base, _dir) = start(Vec::new());
    let id = workspace(
        &base,
        json!({ "data/d.ttl": DATA, "rules/r.ttl": RULES, "shapes/s.ttl": SHAPES }),
    );
    let url = format!("{base}/$/workspaces/{id}/validate");
    // With the rules: a1 and a2 get labels, a3 does not.
    let v = json_of(ureq::post(&url).call().unwrap());
    assert_eq!(v["conforms"], false);
    assert_eq!(v["resultCount"], 1);
    let r = &v["results"][0];
    assert_eq!(r["focusNode"]["value"], "http://example.com/a3");
    assert_eq!(r["resultPath"], "<http://example.com/label>");
    assert_eq!(r["messages"], json!(["needs a label"]));
    // The finding key: 32 hexadecimal digits, the same in the next validation.
    let key = r["key"].as_str().unwrap().to_owned();
    assert!(
        key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "{key}"
    );
    assert!(r["shapeIri"].as_str().unwrap().starts_with("http"), "{r}");
    let again = json_of(ureq::post(&url).call().unwrap());
    assert_eq!(again["results"][0]["key"], json!(key));
    // The summary: one cell, whose sample is that result.
    let s = &v["summary"];
    assert_eq!(
        (s["results"].as_u64(), s["failingFocusNodes"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(s["cells"].as_array().unwrap().len(), 1);
    assert_eq!(s["cells"][0]["shapeIri"], r["shapeIri"]);
    assert_eq!(s["cells"][0]["samples"][0]["key"], json!(key));
    assert_eq!(s["bySeverity"][0]["results"], 1);
    assert_eq!(
        r["sourceConstraintComponent"]["value"],
        "http://www.w3.org/ns/shacl#MinCountConstraintComponent"
    );
    // Coverage: ex:B and ex:Thing have instances no shape targets; ex:ZShape matches nothing.
    let classes: Vec<&str> = v["coverage"]["untargetedClasses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["class"]["value"].as_str().unwrap())
        .collect();
    assert!(classes.contains(&"http://example.com/B"), "{classes:?}");
    assert!(classes.contains(&"http://example.com/Thing"), "{classes:?}");
    assert!(!classes.contains(&"http://example.com/A"), "{classes:?}");
    assert_eq!(
        v["coverage"]["emptyShapes"],
        json!([{"type": "uri", "value": "http://example.com/ZShape"}])
    );
    let a_shape = v["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["shape"]["value"] == "http://example.com/AShape")
        .unwrap();
    assert_eq!(a_shape["focusNodes"], 3);

    // Without the rules every ex:A fails.
    let v = json_of(ureq::post(&url).query("rules", "false").call().unwrap());
    assert_eq!(v["resultCount"], 3);
    // The W3C report by Accept.
    let r = ureq::get(&url).set("Accept", "text/turtle").call().unwrap();
    assert_eq!(r.content_type(), "text/turtle");
    let text = r.into_string().unwrap();
    assert!(text.contains("conforms"), "{text}");

    // Shapes that do not compile: 422 with the reason.
    let bad = format!("{base}/$/workspaces/{id}/files/shapes/bad.ttl");
    ureq::put(&bad)
        .send_string(
            "@prefix sh: <http://www.w3.org/ns/shacl#> . <urn:s> a sh:NodeShape ; \
             sh:targetClass <urn:A> ; sh:minCount \"x\" .",
        )
        .unwrap();
    let r = ureq::post(&url).query("rules", "false").call();
    assert_eq!(status(r), 422);
}

#[test]
fn validate_and_infer_on_a_dataset_read_only() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    engine
        .load_into(
            tripletrail::oxigraph::model::NamedNode::new("urn:shapes").unwrap(),
            RdfFormat::Turtle,
            RULES.as_bytes(),
        )
        .unwrap();
    let (base, _dir) = start(vec![("kg".to_owned(), engine)]);
    let count = || -> String {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query(
                "query",
                "SELECT (COUNT(*) AS ?n) { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }",
            )
            .call()
            .unwrap();
        json_of(r)["results"]["bindings"][0]["n"]["value"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let before = count();
    let infer = format!("{base}/$/datasets/kg/infer");
    let v = json_of(
        ureq::post(&infer)
            .query("shapes", "urn:shapes")
            .call()
            .unwrap(),
    );
    assert_eq!(v["count"], 5);
    // Provenance is for workspaces.
    let r = ureq::post(&infer)
        .query("shapes", "urn:shapes")
        .query("provenance", "true")
        .call();
    assert_eq!(status(r), 400);
    // The shapes graph is required.
    assert_eq!(status(ureq::post(&infer).call()), 400);
    let v = json_of(
        ureq::get(&format!("{base}/$/datasets/kg/validate"))
            .query("shapes", "urn:shapes")
            .call()
            .unwrap(),
    );
    assert_eq!(v["conforms"], true);
    assert_eq!(count(), before);
    assert_eq!(
        status(
            ureq::post(&format!("{base}/$/datasets/nope/validate"))
                .query("shapes", "urn:shapes")
                .call()
        ),
        404
    );
}

/// A chain of `n` nodes and a SPARQL rule that closes it transitively: far more work than a
/// short timeout allows.
fn chain(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                "<http://example.com/s{i}> <http://example.com/p> <http://example.com/s{}> .\n",
                i + 1
            )
        })
        .collect()
}

const CLOSURE: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
ex:Closure a sh:NodeShape ; sh:targetSubjectsOf ex:p ;
  sh:rule [ a sh:SPARQLRule ;
    sh:construct "CONSTRUCT { $this <http://example.com/p> ?c } WHERE { $this <http://example.com/p>/<http://example.com/p> ?c }" ] .
"#;

#[test]
fn validate_with_rules_keeps_to_the_timeout() {
    let (base, _dir) = start(Vec::new());
    let id = workspace(
        &base,
        json!({
            "data/chain.nt": chain(1500),
            "rules/closure.ttl": CLOSURE,
            "shapes/shapes.ttl": SHAPES,
        }),
    );
    let started = std::time::Instant::now();
    let r = ureq::post(&format!("{base}/$/workspaces/{id}/validate"))
        .query("timeout", "0.3")
        .call();
    assert_eq!(status(r), 503);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn dataset_validate_and_infer_take_a_turn() {
    let dir = temp_dir("limits");
    let file = dir.join("limits.json");
    std::fs::write(
        &file,
        r#"{ "default": { "queueTimeout": 0.2 },
             "datasets": { "kg": { "maxConcurrent": 1, "maxQueued": 0, "maxTimeout": 1 } } }"#,
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(RdfFormat::NTriples, chain(1500).as_bytes())
        .unwrap();
    engine
        .load_into(
            tripletrail::oxigraph::model::NamedNode::new("urn:shapes").unwrap(),
            RdfFormat::Turtle,
            CLOSURE.as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 4,
            limits: Some(file),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    // The inference asks for 100 s; the dataset caps it at 1 s, and it holds the one turn.
    let started = std::time::Instant::now();
    let holder = {
        let url = format!("{base}/$/datasets/kg/infer");
        std::thread::spawn(move || {
            status(
                ureq::post(&url)
                    .query("shapes", "urn:shapes")
                    .query("timeout", "100")
                    .call(),
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    // With the turn taken, a query and a validation are both refused.
    let query = ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK {}")
        .call();
    assert_eq!(status(query), 429);
    let validate = ureq::get(&format!("{base}/$/datasets/kg/validate"))
        .query("shapes", "urn:shapes")
        .call();
    assert_eq!(status(validate), 429);
    assert_eq!(holder.join().unwrap(), 503);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// Rules for the strata: `seed` starts `ex:reach`, `step` extends it (a group that repeats),
/// and `lonely` reads it only under a negation (after both, once).
const STRATA_RULES: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
ex:Rules a sh:NodeShape ; sh:targetClass ex:N ;
  sh:rule ex:lonely , ex:step , ex:seed .
ex:seed a sh:SPARQLRule ; sh:order 1 ;
  sh:construct "CONSTRUCT { $this <http://example.com/reach> ?y } WHERE { $this <http://example.com/next> ?y }" .
ex:step a sh:SPARQLRule ; sh:order 2 ;
  sh:construct "CONSTRUCT { $this <http://example.com/reach> ?z } WHERE { $this <http://example.com/reach> ?y . ?y <http://example.com/next> ?z }" .
ex:lonely a sh:SPARQLRule ; sh:order 0 ;
  sh:construct "CONSTRUCT { $this <http://example.com/isolated> true } WHERE { FILTER NOT EXISTS { $this <http://example.com/reach> ?x } }" .
"#;

#[test]
fn infer_explains_its_strata() {
    let (base, _dir) = start(Vec::new());
    let data = "@prefix ex: <http://example.com/> .\n\
                ex:n1 a ex:N ; ex:next ex:n2 . ex:n2 a ex:N ; ex:next ex:n3 . ex:n3 a ex:N .\n";
    let id = workspace(
        &base,
        json!({ "data/d.ttl": data, "rules/r.ttl": STRATA_RULES }),
    );
    let v = json_of(
        ureq::post(&format!("{base}/$/workspaces/{id}/infer"))
            .query("plan", "true")
            .call()
            .unwrap(),
    );
    let plan = &v["plan"];
    let rules: Vec<&str> = plan["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["rule"].as_str().unwrap())
        .collect();
    let index = |name: &str| {
        rules
            .iter()
            .position(|r| *r == format!("<http://example.com/{name}>"))
            .unwrap()
    };
    let (seed, step, lonely) = (index("seed"), index("step"), index("lonely"));
    // Groups in run order: seed once, step to a fixpoint, lonely once at the end, although
    // its sh:order is the lowest.
    let groups: Vec<(Vec<usize>, bool)> = plan["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                serde_json::from_value(g["rules"].clone()).unwrap(),
                g["recursive"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            (vec![seed], false),
            (vec![step], true),
            (vec![lonely], false)
        ],
        "{plan}"
    );
    let edge = |from: usize, to: usize| {
        plan["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["from"] == from && e["to"] == to)
            .cloned()
    };
    let reach = json!(["http://example.com/reach"]);
    assert_eq!(edge(seed, step).unwrap()["through"], reach);
    assert_eq!(edge(step, step).unwrap()["positive"], true);
    let negated = edge(step, lonely).unwrap();
    assert_eq!(negated["positive"], false, "{negated}");
    assert_eq!(negated["through"], reach);
    assert!(edge(lonely, seed).is_none());
    // The runs' group numbers are the plan's.
    for run in v["runs"].as_array().unwrap() {
        let i = index(
            run["rule"]
                .as_str()
                .unwrap()
                .trim_start_matches("<http://example.com/")
                .trim_end_matches('>'),
        );
        let group = groups.iter().position(|(r, _)| r.contains(&i)).unwrap() + 1;
        assert_eq!(run["group"], group);
    }
}

/// Rules whose reads and writes are unknown get no edges, shared keys are capped (#742),
/// and the plan is only there when asked for.
#[test]
fn infer_plan_stays_bounded() {
    let (base, _dir) = start(Vec::new());
    let mut rules = String::from(
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
         ex:Rules a sh:NodeShape ; sh:targetClass ex:N .\n",
    );
    // A variable predicate: TripleTrail cannot tell what these read or write.
    for i in 0..300 {
        rules.push_str(&format!(
            "ex:Rules sh:rule [ a sh:SPARQLRule ; sh:construct \"CONSTRUCT {{ $this ?p true }} \
             WHERE {{ $this ?p ?o . FILTER(?o = {i}) }}\" ] .\n"
        ));
    }
    // All read and write ex:p: an edge between each pair, 120 x 120 > 10,000.
    for i in 0..120 {
        rules.push_str(&format!(
            "ex:Rules sh:rule [ a sh:SPARQLRule ; sh:construct \"CONSTRUCT {{ $this \
             <http://example.com/p> {i} }} WHERE {{ $this <http://example.com/p> ?o }}\" ] .\n"
        ));
    }
    let data = "<http://example.com/n> a <http://example.com/N> ; <http://example.com/p> 0 .\n";
    let id = workspace(&base, json!({ "data/d.ttl": data, "rules/r.ttl": rules }));
    let url = format!("{base}/$/workspaces/{id}/infer");
    let plain = json_of(ureq::post(&url).call().unwrap());
    assert!(plain.get("plan").is_none());
    let v = json_of(ureq::post(&url).query("plan", "true").call().unwrap());
    let plan = &v["plan"];
    let rules = plan["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 420);
    let unknown: Vec<usize> = (0..rules.len())
        .filter(|&i| rules[i]["reads"].is_null())
        .collect();
    assert_eq!(unknown.len(), 300);
    let writes_any = rules.iter().filter(|r| r["writes"].is_null()).count();
    assert_eq!(writes_any, 300, "{}", rules[0]);
    let edges = plan["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 10_000);
    assert_eq!(plan["truncated"], true);
    assert!(edges.iter().all(
        |e| !unknown.contains(&(e["from"].as_u64().unwrap() as usize))
            && !unknown.contains(&(e["to"].as_u64().unwrap() as usize))
    ));
}

#[test]
fn reason_entails_and_explains_on_a_workspace() {
    let (base, _dir) = start(Vec::new());
    let data = "@prefix ex: <http://example.com/> .\n\
                @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                ex:Dog rdfs:subClassOf ex:Animal . ex:Animal rdfs:subClassOf ex:Being .\n\
                ex:owns rdfs:range ex:Dog . ex:ann ex:owns ex:rex .\n\
                ex:Dog owl:disjointWith ex:Cat . ex:rex a ex:Cat .\n";
    // A shape whose sub class triple must not be entailed from: shapes and rules are left out.
    let shapes = "@prefix ex: <http://example.com/> .\n\
                  @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                  ex:Being rdfs:subClassOf ex:Ghost .\n";
    let id = workspace(&base, json!({ "data/d.ttl": data, "shapes/s.ttl": shapes }));
    let url = format!("{base}/$/workspaces/{id}/reason");
    let v = json_of(ureq::post(&url).call().unwrap());
    assert_eq!(v["regime"], "rdfs");
    let triples = v["triples"].as_array().unwrap();
    let has = |s: &str, o: &str| {
        triples.iter().any(|t| {
            t["subject"]["value"] == format!("http://example.com/{s}")
                && t["object"]["value"] == format!("http://example.com/{o}")
        })
    };
    // rex a Dog (range), Animal, Being; Dog subClassOf Being.
    assert!(
        has("rex", "Dog") && has("rex", "Being") && has("Dog", "Being"),
        "{v}"
    );
    assert!(!has("rex", "Ghost"), "the shapes graph was read: {v}");
    assert_eq!(v["count"], 4, "{v}");
    let classes = v["classes"].as_array().unwrap();
    assert_eq!(classes.len(), 3, "{v}");
    assert!(v.get("inconsistencies").is_none());
    // Its derivation, down to asserted triples.
    let why = json_of(
        ureq::post(&format!("{url}/why"))
            .query("subject", "<http://example.com/rex>")
            .query(
                "predicate",
                "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>",
            )
            .query("object", "<http://example.com/Animal>")
            .call()
            .unwrap(),
    );
    assert_eq!(why["entailed"], true);
    let d = &why["derivation"];
    assert_eq!(d["rule"], "rdfs9", "{why}");
    let premises = d["premises"].as_array().unwrap();
    assert_eq!(premises.len(), 2);
    let ranged = premises
        .iter()
        .find(|p| p["rule"] == "rdfs3")
        .expect("rdfs3");
    assert!(
        ranged["premises"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["rule"].is_null())
    );
    // Not entailed: no derivation.
    let none = json_of(
        ureq::post(&format!("{url}/why"))
            .query("subject", "<http://example.com/rex>")
            .query(
                "predicate",
                "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>",
            )
            .query("object", "<http://example.com/Ghost>")
            .call()
            .unwrap(),
    );
    assert!(none["derivation"].is_null());
    assert_eq!(none["entailed"], false);
    // OWL 2 RL: rex is a Dog and a Cat, which are disjoint.
    let v = json_of(ureq::post(&url).query("regime", "owl-rl").call().unwrap());
    let bad = &v["inconsistencies"];
    assert_eq!(bad["count"], 1, "{v}");
    assert_eq!(bad["first"][0]["rule"], "cax-dw");
    // RDF by Accept.
    let ttl = ureq::post(&url)
        .set("Accept", "text/turtle")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(ttl.contains("Being"), "{ttl}");
    assert!(ureq::post(&url).query("regime", "owl").call().is_err());
}

#[test]
fn why_keeps_to_the_timeout() {
    let (base, _dir) = start(Vec::new());
    // A chain of owl:sameAs: its closure, and the search for a derivation, grow quadratically.
    let mut data = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix owl: <http://www.w3.org/2002/07/owl#> .\n",
    );
    for i in 0..150 {
        data.push_str(&format!("ex:n{i} owl:sameAs ex:n{} .\n", i + 1));
    }
    data.push_str("ex:n0 ex:p ex:v .\n");
    let id = workspace(&base, json!({ "data/d.ttl": data }));
    let why = |timeout: Option<&str>| {
        let mut request = ureq::post(&format!("{base}/$/workspaces/{id}/reason/why"))
            .query("regime", "owl-rl")
            .query("subject", "<http://example.com/n150>")
            .query("predicate", "<http://example.com/p>")
            .query("object", "<http://example.com/v>");
        if let Some(t) = timeout {
            request = request.query("timeout", t);
        }
        let started = std::time::Instant::now();
        let status = match request.call() {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("{e}"),
        };
        (status, started.elapsed())
    };
    let (status, _) = why(Some("0.000001"));
    assert_eq!(status, 503);
    // Without a timeout of its own, the search still comes back.
    let (status, took) = why(None);
    assert_eq!(status, 200);
    assert!(took < std::time::Duration::from_secs(30), "{took:?}");
}

/// Fuseki's SHACL service (#829): `POST /{ds}/shacl?graph=&target=` with the shapes in the body.
#[test]
fn the_shacl_service_answers_as_fuseki_does() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    engine
        .load(
            RdfFormat::TriG,
            b"<http://example.com/g> { <http://example.com/a4> a <http://example.com/A> . }"
                .as_slice(),
        )
        .unwrap();
    let (base, _dir) = start(vec![("kg".to_owned(), engine)]);
    let post = |query: &str, kind: &str, body: &str| {
        ureq::post(&format!("{base}/kg/shacl{query}"))
            .set("Content-Type", kind)
            .send_string(body)
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => code,
                e => panic!("{e}"),
            })
    };
    // The focus nodes of the report's results, and whether it says the data conforms.
    let report = |r: ureq::Response| -> (bool, Vec<String>) {
        assert!(r.content_type().starts_with("text/turtle"));
        let text = r.into_string().unwrap();
        let mut conforms = None;
        let mut foci = Vec::new();
        for t in RdfParser::from_format(RdfFormat::Turtle).for_slice(text.as_bytes()) {
            let t = t.unwrap();
            match t.predicate.as_str() {
                "http://www.w3.org/ns/shacl#conforms" => {
                    conforms = Some(t.object.to_string().contains("true"))
                }
                "http://www.w3.org/ns/shacl#focusNode" => foci.push(t.object.to_string()),
                _ => {}
            }
        }
        foci.sort();
        (conforms.expect("sh:conforms"), foci)
    };
    let a = |n: &str| format!("<http://example.com/{n}>");
    // The default graph, a named graph, and the named graphs as one.
    let (ok, foci) = report(post("", "text/turtle", SHAPES).unwrap());
    assert!(!ok);
    assert_eq!(foci, [a("a1"), a("a2"), a("a3")]);
    let (_, foci) = report(post("?graph=http://example.com/g", "text/turtle", SHAPES).unwrap());
    assert_eq!(foci, [a("a4")]);
    // The union is the named graphs, as Jena's union graph: not the default graph.
    let (_, foci) = report(post("?graph=union", "text/turtle", SHAPES).unwrap());
    assert_eq!(foci, [a("a4")]);
    // One node: the shapes that target it.
    let (_, foci) = report(post("?target=http://example.com/a1", "text/turtle", SHAPES).unwrap());
    assert_eq!(foci, [a("a1")]);
    assert_eq!(
        report(post("?target=http://example.com/b1", "text/turtle", SHAPES).unwrap()),
        (true, vec![])
    );
    // The shapes' rules run first; curl's form data is Turtle; other formats in and out.
    let (_, foci) = report(
        post(
            "",
            "application/x-www-form-urlencoded",
            &format!("{RULES}\n{SHAPES}"),
        )
        .unwrap(),
    );
    assert_eq!(foci, [a("a3")]);
    let nt = ureq::post(&format!("{base}/kg/shacl"))
        .set("Content-Type", "text/turtle")
        .set("Accept", "application/n-triples")
        .send_string(SHAPES)
        .unwrap();
    assert_eq!(nt.content_type(), "application/n-triples");
    assert_eq!(nt.into_string().unwrap().matches("#focusNode>").count(), 3);
    // Errors: no such graph, bad shapes, a GET.
    assert_eq!(
        post("?graph=http://example.com/none", "text/turtle", SHAPES).unwrap_err(),
        404
    );
    assert_eq!(post("", "text/turtle", "not turtle").unwrap_err(), 400);
    assert_eq!(post("", "application/x-unknown", SHAPES).unwrap_err(), 415);
    assert_eq!(status(ureq::get(&format!("{base}/kg/shacl")).call()), 405);
}
