//! The workbench's workspaces (#400): files, typed data, samples of a dataset, the scratch
//! store's SPARQL endpoint, caps, the TTL, and that no workspace operation changes a dataset.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server, WorkspaceOptions};

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("ws-{name}"))
}

/// A dataset `kg` of `n` instances of ex:A; each points to an ex:B node, which points to an
/// ex:C node, which points to an ex:D node.
fn dataset(n: usize) -> Engine {
    let mut ttl = String::from("@prefix ex: <http://example.com/> .\n");
    for i in 0..n {
        ttl.push_str(&format!(
            "ex:a{i} a ex:A ; ex:label \"a{i}\" ; ex:next ex:b{i} .\n\
             ex:b{i} a ex:B ; ex:next ex:c{i} .\n\
             ex:c{i} a ex:C ; ex:next ex:d{i} .\n\
             ex:d{i} a ex:D .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    engine
}

/// ureq without its json feature.
#[allow(clippy::result_large_err)]
trait SendJson {
    fn send_json(self, v: Value) -> Result<ureq::Response, ureq::Error>;
}

impl SendJson for ureq::Request {
    fn send_json(self, v: Value) -> Result<ureq::Response, ureq::Error> {
        self.set("Content-Type", "application/json")
            .send_string(&v.to_string())
    }
}

struct Running {
    base: String,
}

fn start(dir: &Path, ws: WorkspaceOptions, token: Option<&str>) -> Running {
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), dataset(50))],
        Options {
            threads: 4,
            update_token: token.map(str::to_owned),
            workspaces: WorkspaceOptions {
                dir: Some(dir.to_owned()),
                ..ws
            },
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    Running { base }
}

fn status(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

fn error_text(r: Result<ureq::Response, ureq::Error>) -> (u16, String) {
    match r {
        Ok(r) => panic!("expected an error, got {}", r.status()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

fn json_of(r: ureq::Response) -> Value {
    serde_json::from_str(&r.into_string().unwrap()).unwrap()
}

impl Running {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn create(&self, body: Value) -> String {
        let r = ureq::post(&self.url("/$/workspaces"))
            .send_json(body)
            .unwrap();
        assert_eq!(r.status(), 201);
        json_of(r)["id"].as_str().unwrap().to_owned()
    }

    /// The bindings of `?n` of a one-number SELECT on the workspace's scratch store.
    fn count(&self, id: &str, query: &str) -> u64 {
        let r = ureq::get(&self.url(&format!("/$/workspaces/{id}/sparql")))
            .query("query", query)
            .call()
            .unwrap();
        json_of(r)["results"]["bindings"][0]["n"]["value"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// The dataset, as sorted N-Quads.
    fn dump(&self) -> Vec<String> {
        let r = ureq::get(&self.url("/kg/data"))
            .set("Accept", "application/n-quads")
            .call()
            .unwrap();
        let mut lines: Vec<String> = r
            .into_string()
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }
}

const SHAPES: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
ex:AShape a sh:NodeShape ; sh:targetClass ex:A ;
  sh:property [ sh:path ex:label ; sh:minCount 1 ] .
"#;

#[test]
fn typed_data_files_and_query() {
    let dir = temp_dir("typed");
    let s = start(&dir, WorkspaceOptions::default(), None);
    let id = s.create(json!({
        "name": "first",
        "files": {
            "data/a.ttl": "<urn:a> a <urn:T> . <urn:b> a <urn:T> .",
            "shapes/a.ttl": SHAPES,
            "queries/all.rq": "SELECT * { ?s ?p ?o }",
        }
    }));
    assert_eq!(id.len(), 32);
    // Data in the default graph, shapes in their own graph.
    assert_eq!(s.count(&id, "SELECT (COUNT(*) AS ?n) { ?s a <urn:T> }"), 2);
    assert_eq!(
        s.count(
            &id,
            "SELECT (COUNT(*) AS ?n) { GRAPH <urn:tripletrail:workspace:shapes> { ?s a ?t } }"
        ),
        1
    );
    let info = json_of(
        ureq::get(&s.url(&format!("/$/workspaces/{id}")))
            .call()
            .unwrap(),
    );
    assert_eq!(info["name"], "first");
    assert_eq!(info["lifetime"], "scratch");
    // Two data triples, five of the shape.
    assert_eq!(info["triples"], 7);
    let paths: Vec<&str> = info["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["data/a.ttl", "queries/all.rq", "shapes/a.ttl"]);

    // Files: read, replace, add, delete; the scratch store follows.
    let file = s.url(&format!("/$/workspaces/{id}/files/queries/all.rq"));
    let r = ureq::get(&file).call().unwrap();
    assert_eq!(r.content_type(), "application/sparql-query");
    assert_eq!(r.into_string().unwrap(), "SELECT * { ?s ?p ?o }");
    let data = s.url(&format!("/$/workspaces/{id}/files/data/a.ttl"));
    assert_eq!(
        status(ureq::put(&data).send_string("<urn:c> a <urn:T> .")),
        204
    );
    assert_eq!(s.count(&id, "SELECT (COUNT(*) AS ?n) { ?s a <urn:T> }"), 1);
    let more = s.url(&format!("/$/workspaces/{id}/files/data/sub/more.nt"));
    assert_eq!(
        status(
            ureq::put(&more).send_string(
                "<urn:d> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <urn:T> .\n"
            )
        ),
        201
    );
    assert_eq!(s.count(&id, "SELECT (COUNT(*) AS ?n) { ?s a <urn:T> }"), 2);
    assert_eq!(status(ureq::delete(&more).call()), 204);
    assert_eq!(s.count(&id, "SELECT (COUNT(*) AS ?n) { ?s a <urn:T> }"), 1);
    assert!(!dir.join(&id).join("files/data/sub").exists());
    assert_eq!(status(ureq::get(&more).call()), 404);

    // RDF that does not parse is refused, naming the file, and nothing changes.
    let (code, text) = error_text(ureq::put(&data).send_string("<urn:c> a ."));
    assert_eq!(code, 400);
    assert!(text.starts_with("data/a.ttl:"), "{text}");
    assert_eq!(s.count(&id, "SELECT (COUNT(*) AS ?n) { ?s a <urn:T> }"), 1);
    // Paths outside the layout are refused.
    for bad in [
        "/files/data/%2E%2E/x.ttl",
        "/files/elsewhere/x.ttl",
        "/files/data/.x.ttl",
        "/files/data/x.csv",
    ] {
        let r = ureq::put(&s.url(&format!("/$/workspaces/{id}{bad}"))).send_string("x");
        assert_eq!(status(r), 400, "{bad}");
    }

    // The scratch store follows the files: an update is refused.
    let r = ureq::post(&s.url(&format!("/$/workspaces/{id}/update")))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <urn:x> a <urn:T> }");
    assert_eq!(status(r), 405);

    // Rename and save; listed; deleted.
    let r = ureq::post(&s.url(&format!("/$/workspaces/{id}")))
        .send_json(json!({"name": "kept", "lifetime": "saved"}))
        .unwrap();
    let v = json_of(r);
    assert_eq!(v["lifetime"], "saved");
    assert!(v["expires"].is_null());
    let list = json_of(ureq::get(&s.url("/$/workspaces")).call().unwrap());
    assert_eq!(list["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(
        status(ureq::delete(&s.url(&format!("/$/workspaces/{id}"))).call()),
        204
    );
    assert!(!dir.join(&id).exists());
    assert_eq!(
        status(ureq::get(&s.url(&format!("/$/workspaces/{id}"))).call()),
        404
    );
}

#[test]
fn sample_of_a_dataset_leaves_it_unchanged() {
    let dir = temp_dir("sample");
    let s = start(&dir, WorkspaceOptions::default(), None);
    let before = s.dump();
    let id = s.create(json!({ "files": { "shapes/a.ttl": SHAPES } }));
    let sample = s.url(&format!("/$/workspaces/{id}/sample"));
    let r = ureq::post(&sample)
        .send_json(json!({
            "dataset": "kg",
            "classes": ["http://example.com/A"],
            "instances": 10,
            "hops": 2,
        }))
        .unwrap();
    let v = json_of(r);
    assert_eq!(v["file"], "data/sample-kg.nt");
    assert_eq!(v["instances"], 10);
    // Hop 1: 3 triples per ex:A; hop 2: the ex:B nodes' 2 and ex:A's class (none).
    assert_eq!(v["triples"], 50);
    assert_eq!(v["truncated"], false);
    assert_eq!(
        s.count(
            &id,
            "SELECT (COUNT(*) AS ?n) { ?s a <http://example.com/A> }"
        ),
        10
    );
    assert_eq!(
        s.count(
            &id,
            "SELECT (COUNT(*) AS ?n) { ?s a <http://example.com/B> }"
        ),
        10
    );
    // Three hops away is not in a two-hop sample.
    assert_eq!(
        s.count(
            &id,
            "SELECT (COUNT(*) AS ?n) { ?s a <http://example.com/C> }"
        ),
        0
    );
    // A sample again replaces the file.
    let r = ureq::post(&sample)
        .send_json(json!({
            "dataset": "kg",
            "classes": ["http://example.com/A"],
            "instances": 5,
            "hops": 3,
        }))
        .unwrap();
    assert_eq!(json_of(r)["triples"], 5 * 3 + 5 * 2 + 5 * 2);
    assert_eq!(
        s.count(
            &id,
            "SELECT (COUNT(*) AS ?n) { ?s a <http://example.com/C> }"
        ),
        5
    );

    // Unknown datasets and bad requests.
    let r = ureq::post(&sample).send_json(json!({"dataset": "nope", "classes": ["urn:x"]}));
    assert_eq!(status(r), 404);
    let r = ureq::post(&sample).send_json(json!({"dataset": "kg"}));
    assert_eq!(status(r), 400);
    for file in ["shapes/x.nt", "data/x.nt.gz", "data/x.ttl"] {
        let r = ureq::post(&sample)
            .send_json(json!({"dataset": "kg", "classes": ["urn:x"], "file": file}));
        assert_eq!(status(r), 400, "{file}");
    }

    // Nothing a workspace does changes the dataset.
    let other = s.url(&format!("/$/workspaces/{id}/files/data/sample-kg.nt"));
    ureq::put(&other)
        .send_string("<http://example.com/a0> <urn:p> <urn:o> .")
        .unwrap();
    ureq::delete(&s.url(&format!("/$/workspaces/{id}")))
        .call()
        .unwrap();
    assert_eq!(s.dump(), before);
}

#[test]
fn caps() {
    let dir = temp_dir("caps");
    let s = start(
        &dir,
        WorkspaceOptions {
            max_workspaces: 3,
            max_per_owner: 2,
            max_triples: 20,
            max_files: 2,
            max_file: 4000,
            max_bytes: 6000,
            ..WorkspaceOptions::default()
        },
        None,
    );
    let id = s.create(json!({}));
    let file = |p: &str| s.url(&format!("/$/workspaces/{id}/files/{p}"));
    let triples = |n: usize| -> String {
        (0..n)
            .map(|i| format!("<urn:s{i}> <urn:p> <urn:o> .\n"))
            .collect()
    };
    assert_eq!(
        status(ureq::put(&file("data/a.nt")).send_string(&triples(15))),
        201
    );
    // 15 + 10 is over 20; replacing the 15 with 20 is not.
    let (code, _) = error_text(ureq::put(&file("data/b.nt")).send_string(&triples(10)));
    assert_eq!(code, 413);
    assert_eq!(
        status(ureq::put(&file("data/a.nt")).send_string(&triples(20))),
        204
    );
    // Files: at most two, at most 4000 bytes.
    assert_eq!(
        status(ureq::put(&file("queries/q.rq")).send_string("ASK {}")),
        201
    );
    assert_eq!(
        status(ureq::put(&file("queries/r.rq")).send_string("ASK {}")),
        413
    );
    assert_eq!(
        status(ureq::put(&file("queries/q.rq")).send_string(&"#".repeat(4001))),
        413
    );
    // A .gz file that unpacks past the file limit is refused, not unpacked whole.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    std::io::Write::write_all(&mut gz, &"#".repeat(1 << 20).into_bytes()).unwrap();
    let bomb = gz.finish().unwrap();
    assert!(bomb.len() < 4000);
    assert_eq!(
        status(ureq::put(&file("data/bomb.nt.gz")).send_bytes(&bomb)),
        413
    );
    // At most 6000 bytes of files: 3500 + 3000 is over.
    assert_eq!(
        status(ureq::put(&file("queries/q.rq")).send_string(&"#".repeat(3500))),
        204
    );
    let (code, text) = error_text(ureq::put(&file("queries/r.rq")).send_string(&"#".repeat(3000)));
    assert_eq!(code, 413, "{text}");
    assert_eq!(
        status(ureq::put(&file("queries/q.rq")).send_string("ASK {}")),
        204
    );
    // A sample stops at the room left, and says so.
    assert_eq!(status(ureq::delete(&file("data/a.nt")).call()), 204);
    let r = ureq::post(&s.url(&format!("/$/workspaces/{id}/sample")))
        .send_json(json!({"dataset": "kg", "classes": ["http://example.com/A"]}))
        .unwrap();
    let v = json_of(r);
    assert_eq!(v["triples"], 20);
    assert_eq!(v["truncated"], true);
    // Workspaces: two per caller (everyone is one caller here), then 429.
    s.create(json!({}));
    let r = ureq::post(&s.url("/$/workspaces")).send_json(json!({}));
    assert_eq!(status(r), 429);
    // Created with too many triples: refused before anything is written.
    ureq::delete(&s.url(&format!("/$/workspaces/{id}")))
        .call()
        .unwrap();
    let r =
        ureq::post(&s.url("/$/workspaces")).send_json(json!({"files": {"data/a.nt": triples(21)}}));
    assert_eq!(status(r), 413);
}

#[test]
fn scratch_workspaces_expire_and_saved_ones_survive_a_restart() {
    let dir = temp_dir("ttl");
    let options = WorkspaceOptions {
        ttl: Duration::from_millis(1500),
        ..WorkspaceOptions::default()
    };
    let s = start(&dir, options.clone(), None);
    let scratch = s.create(json!({ "files": { "data/a.ttl": "<urn:a> a <urn:T> ." } }));
    let saved = s.create(json!({
        "lifetime": "saved",
        "files": { "data/a.ttl": "<urn:a> a <urn:T> ." }
    }));
    assert!(dir.join(&scratch).exists());
    std::thread::sleep(Duration::from_millis(2500));
    let list = json_of(ureq::get(&s.url("/$/workspaces")).call().unwrap());
    let ids: Vec<&str> = list["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [saved.as_str()]);
    assert!(!dir.join(&scratch).exists());

    // Another server on the same folder has the saved workspace and builds its store.
    let s2 = start(&dir, options, None);
    assert_eq!(
        s2.count(&saved, "SELECT (COUNT(*) AS ?n) { ?s a <urn:T> }"),
        1
    );
}

#[test]
fn changes_need_the_token_when_the_server_has_one() {
    let dir = temp_dir("token");
    let s = start(&dir, WorkspaceOptions::default(), Some("tok"));
    let r = ureq::post(&s.url("/$/workspaces")).send_json(json!({}));
    assert_eq!(status(r), 401);
    let r = ureq::post(&s.url("/$/workspaces"))
        .set("Authorization", "Bearer tok")
        .send_json(json!({ "files": { "data/a.ttl": "<urn:a> a <urn:T> ." } }))
        .unwrap();
    let id = json_of(r)["id"].as_str().unwrap().to_owned();
    // Reading and querying need none.
    assert_eq!(s.count(&id, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }"), 1);
    let r = ureq::delete(&s.url(&format!("/$/workspaces/{id}"))).call();
    assert_eq!(status(r), 401);
}

#[test]
fn turned_off() {
    let dir = temp_dir("off");
    let s = start(
        &dir,
        WorkspaceOptions {
            enabled: false,
            ..WorkspaceOptions::default()
        },
        None,
    );
    assert_eq!(status(ureq::get(&s.url("/$/workspaces")).call()), 404);
}

#[test]
fn with_authentication_callers_see_their_own_workspaces() {
    use tripletrail_server::auth::{sha256, to_hex};
    let dir = temp_dir("auth");
    let hash = |k: &str| to_hex(&sha256(k.as_bytes()));
    let guard = dir.join("guard.json");
    std::fs::write(
        &guard,
        format!(
            r#"{{"auth": {{"apiKeys": [
                {{"sha256": "{}", "agent": "https://example.org/people/ann"}},
                {{"sha256": "{}", "agent": "https://example.org/people/bob", "datasets": ["other"]}}
            ], "anonymous": true}}}}"#,
            hash("ann-key"),
            hash("bob-key"),
        ),
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [
            ("kg".to_owned(), dataset(5)),
            ("other".to_owned(), dataset(1)),
        ],
        Options {
            threads: 2,
            auth_config: Some(guard),
            workspaces: WorkspaceOptions {
                dir: Some(dir.join("ws")),
                ..WorkspaceOptions::default()
            },
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let url = |p: &str| format!("{base}{p}");
    let as_ = |key: &str, r: ureq::Request| r.set("Authorization", &format!("Bearer {key}"));

    // Anonymous callers may not make workspaces.
    assert_eq!(
        status(ureq::post(&url("/$/workspaces")).send_json(json!({}))),
        401
    );
    let ann = json_of(
        as_("ann-key", ureq::post(&url("/$/workspaces")))
            .send_json(json!({}))
            .unwrap(),
    );
    assert_eq!(ann["owner"], "https://example.org/people/ann");
    let id = ann["id"].as_str().unwrap();
    let bob = json_of(
        as_("bob-key", ureq::post(&url("/$/workspaces")))
            .send_json(json!({}))
            .unwrap(),
    );
    let bob_id = bob["id"].as_str().unwrap();
    // Each sees their own only.
    let list = json_of(
        as_("ann-key", ureq::get(&url("/$/workspaces")))
            .call()
            .unwrap(),
    );
    assert_eq!(list["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(
        status(as_("bob-key", ureq::get(&url(&format!("/$/workspaces/{id}")))).call()),
        404
    );
    // A sample only of what the caller may read.
    let sample = |key: &str, ws: &str, ds: &str| {
        status(
            as_(key, ureq::post(&url(&format!("/$/workspaces/{ws}/sample"))))
                .send_json(json!({"dataset": ds, "classes": ["http://example.com/A"]})),
        )
    };
    assert_eq!(sample("bob-key", bob_id, "kg"), 404);
    assert_eq!(sample("bob-key", bob_id, "other"), 200);
    assert_eq!(sample("ann-key", id, "kg"), 200);

    // An overlay takes its owner's updates, also as a form to …/sparql (#406), not others'.
    as_(
        "ann-key",
        ureq::post(&url(&format!("/$/workspaces/{id}/overlay"))),
    )
    .send_json(json!({"dataset": "kg"}))
    .unwrap();
    let form = |key: Option<&str>| {
        let r = ureq::post(&url(&format!("/$/workspaces/{id}/sparql")));
        let r = match key {
            Some(k) => as_(k, r),
            None => r,
        };
        status(r.send_form(&[("update", "INSERT DATA { <urn:x> a <urn:T> }")]))
    };
    assert_eq!(form(Some("ann-key")), 200);
    assert_eq!(form(Some("bob-key")), 404);
    assert_ne!(form(None), 200);
}

/// #400's target: 500 instances with 2 hops of a 1M-triple dataset in under 2 s. Run with
/// `cargo test --release -p tripletrail-server --test workspace -- --ignored`.
#[test]
#[ignore]
fn sample_speed() {
    // 100,000 track sections with 6 triples each, pointing to 100,000 nodes with 4.
    let engine = Engine::new_tripletrail();
    let mut nt = String::new();
    let ex = "http://example.com/";
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    for i in 0..100_000 {
        nt.push_str(&format!(
            "<{ex}s{i}> <{rdf_type}> <{ex}Section> .\n<{ex}s{i}> <{ex}label> \"s{i}\" .\n\
             <{ex}s{i}> <{ex}length> \"{i}\" .\n<{ex}s{i}> <{ex}from> <{ex}n{i}> .\n\
             <{ex}s{i}> <{ex}to> <{ex}n{}> .\n<{ex}s{i}> <{ex}line> <{ex}l{}> .\n\
             <{ex}n{i}> <{rdf_type}> <{ex}Node> .\n<{ex}n{i}> <{ex}label> \"n{i}\" .\n\
             <{ex}n{i}> <{ex}km> \"{i}\" .\n<{ex}n{i}> <{ex}in> <{ex}l{}> .\n",
            (i + 1) % 100_000,
            i % 100,
            i % 100,
        ));
    }
    engine.load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    assert_eq!(engine.store().len().unwrap(), 1_000_000);
    let dir = temp_dir("speed");
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            workspaces: WorkspaceOptions {
                dir: Some(dir.to_path_buf()),
                ..WorkspaceOptions::default()
            },
            ..Options::default()
        },
    )
    .unwrap();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    let id = s.create(json!({}));
    let started = std::time::Instant::now();
    let v = json_of(
        ureq::post(&s.url(&format!("/$/workspaces/{id}/sample")))
            .send_json(json!({"dataset": "kg", "classes": [format!("{ex}Section")],
                              "instances": 500, "hops": 2}))
            .unwrap(),
    );
    let elapsed = started.elapsed();
    eprintln!("sample: {v} in {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn tests_of_a_workspace() {
    let dir = temp_dir("tests");
    let s = start(&dir, WorkspaceOptions::default(), Some("tok"));
    let rules = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .\n\
        ex:S a sh:NodeShape ; sh:targetClass ex:A ;\n\
        sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:ok ; sh:object true ] .";
    let r = ureq::post(&s.url("/$/workspaces"))
        .set("Authorization", "Bearer tok")
        .send_json(json!({ "files": {
            "data/a.ttl": "<http://example.com/x> a <http://example.com/A> .",
            "rules/r.ttl": rules,
            "tests/rules/input.ttl": "<http://example.com/y> a <http://example.com/A> .",
            "tests/rules/test.yaml": "rules: ../../rules\n",
            "tests/rules/expected.nt": "<http://example.com/y> <http://example.com/ok> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> .\n",
            "tests/count/query.rq": "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }",
            "tests/escape/test.yaml": "input: ../../../../../etc/passwd\n",
            "tests/escape/expected.nt": "",
            // A chain whose closure has 4.5 million pairs, each step a store lookup.
            "tests/slow/input.nt": (0..3000)
                .map(|i| format!("<http://e/n{i}> <http://e/next> <http://e/n{}> .\n", i + 1))
                .collect::<String>(),
            "tests/slow/query.rq": "SELECT (COUNT(*) AS ?n) { ?a <http://e/next>* ?b }",
            "tests/slow/expected.srj": r#"{"head":{"vars":["n"]},"results":{"bindings":[]}}"#,
        } }))
        .unwrap();
    let id = json_of(r)["id"].as_str().unwrap().to_owned();
    let tests = s.url(&format!("/$/workspaces/{id}/tests"));
    // Listing and running need no token, as queries do.
    let list = json_of(ureq::get(&tests).call().unwrap());
    assert_eq!(
        list,
        json!({ "tests": ["count", "escape", "rules", "slow"] })
    );
    let three = json!({ "tests": ["count", "escape", "rules"] });
    let run = json_of(ureq::post(&tests).send_json(three.clone()).unwrap());
    assert_eq!(
        (run["passed"].as_u64(), run["failed"].as_u64()),
        (Some(1), Some(2))
    );
    let by = |name: &str| {
        run["tests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(by("rules")["passed"], true);
    assert_eq!(by("rules")["kind"], "rules");
    assert!(
        by("count")["error"]
            .as_str()
            .unwrap()
            .starts_with("no expected output")
    );
    assert_eq!(
        by("escape")["error"],
        "../../../../../etc/passwd is outside the workspace"
    );
    // One test, by name.
    let one = json_of(
        ureq::post(&tests)
            .send_json(json!({ "tests": ["rules"] }))
            .unwrap(),
    );
    assert_eq!(one["tests"].as_array().unwrap().len(), 1);
    let (code, _) = error_text(ureq::post(&tests).query("test", "nope").call());
    assert_eq!(code, 404);
    // Recording writes files: it needs the token, and the tests named.
    let r = ureq::post(&tests)
        .query("record", "true")
        .query("test", "count")
        .call();
    assert_eq!(status(r), 401);
    let r = ureq::post(&tests)
        .set("Authorization", "Bearer tok")
        .query("record", "true")
        .call();
    assert_eq!(status(r), 400);
    let r = ureq::post(&tests)
        .set("Authorization", "Bearer tok")
        .query("record", "true")
        .query("test", "count")
        .call()
        .unwrap();
    assert_eq!(json_of(r)["passed"], 1);
    let srj = ureq::get(&s.url(&format!(
        "/$/workspaces/{id}/files/tests/count/expected.srj"
    )))
    .call()
    .unwrap()
    .into_string()
    .unwrap();
    assert!(srj.contains(r#""vars":["n"]"#), "{srj}");
    let run = json_of(ureq::post(&tests).send_json(three.clone()).unwrap());
    assert_eq!(run["passed"], 2);
    // Tests are not data: the scratch store has the data and rules files only.
    assert_eq!(s.count(&id, "SELECT (COUNT(*) AS ?n) { ?s a ?t }"), 1);
    // The query timeout stops a test (#472 made such a query cancellable).
    let started = std::time::Instant::now();
    let run = json_of(
        ureq::post(&tests)
            .query("test", "slow")
            .query("timeout", "0.3")
            .call()
            .unwrap(),
    );
    assert_eq!(
        run["tests"][0]["error"],
        "the test took longer than its timeout"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn tests_of_a_workspace_stay_within_its_limits() {
    use std::io::Write;
    let dir = temp_dir("test-limits");
    let s = start(
        &dir,
        WorkspaceOptions {
            max_bytes: 64 << 10,
            ..WorkspaceOptions::default()
        },
        None,
    );
    let values: Vec<String> = (0..350).map(|i| i.to_string()).collect();
    let query = format!("SELECT ?i {{ VALUES ?i {{ {} }} }}", values.join(" "));
    let id = s.create(json!({ "files": {
        "tests/a/query.rq": query,
        "tests/b/query.rq": query,
        "tests/bomb/test.yaml": "input: input.nt.gz\n",
        "tests/bomb/query.rq": "ASK {}",
        "tests/bomb/expected.srj": r#"{"head":{},"boolean":true}"#,
    } }));
    // A gzip bomb: 16 MB of blank lines in a few KB, more than the workspace holds.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&vec![b'\n'; 16 << 20]).unwrap();
    let bomb = gz.finish().unwrap();
    assert!(bomb.len() < 32 << 10);
    let r = ureq::put(&s.url(&format!("/$/workspaces/{id}/files/tests/bomb/input.nt.gz")))
        .send_bytes(&bomb);
    assert_eq!(status(r), 201);
    let tests = s.url(&format!("/$/workspaces/{id}/tests"));
    let run = json_of(
        ureq::post(&tests)
            .send_json(json!({ "tests": ["bomb"] }))
            .unwrap(),
    );
    let error = |name: &str| {
        run["tests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(error("bomb").contains("(unpacked)"), "{}", error("bomb"));
    // Recording: the room left is counted again for each test.
    let r = ureq::post(&tests)
        .query("record", "true")
        .send_json(json!({ "tests": ["a", "b"] }))
        .unwrap();
    let run = json_of(r);
    let passed: Vec<bool> = run["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["passed"].as_bool().unwrap())
        .collect();
    assert_eq!(passed, [true, false], "{run}");
    assert!(
        run["tests"][1]["error"]
            .as_str()
            .unwrap()
            .contains("room for")
    );
}

#[test]
fn pipeline_of_a_workspace_runs_step_by_step() {
    let dir = temp_dir("pipeline");
    let s = start(&dir, WorkspaceOptions::default(), Some("tok"));
    let rules = "@prefix sh: <http://www.w3.org/ns/shacl#> .
        <http://e/S> a sh:NodeShape ; sh:targetClass <http://e/A> ;
          sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate <http://e/ok> ; sh:object true ] .";
    let r = ureq::post(&s.url("/$/workspaces"))
        .set("Authorization", "Bearer tok")
        .send_json(json!({ "files": {
            "data/a.ttl": "<http://e/x> a <http://e/A> . <http://e/y> a <http://e/B> .",
            "updates/01-tag.ru": "INSERT { ?s <http://e/tag> ?_tag } WHERE { ?s a ?c }",
            "updates/02-drop-b.ru": "DELETE WHERE { ?s a <http://e/B> }",
            "rules/r.ttl": rules,
            "pipeline.yaml": "parameters: {tag: one}
steps:
  - load: data/a.ttl
  - name: tag
    update: updates/
  - infer: rules/r.ttl
  - export: queries/out.nt
",
        } }))
        .unwrap();
    let id = json_of(r)["id"].as_str().unwrap().to_owned();
    let url = s.url(&format!("/$/workspaces/{id}/pipeline"));
    let before = json_of(ureq::get(&url).call().unwrap());
    let names: Vec<&str> = before["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["1 load", "tag", "3 infer", "4 export"]);
    assert!(before["steps"][0]["report"].is_null());
    // Running writes files: it needs the token.
    assert_eq!(status(ureq::post(&url).send_json(json!({}))), 401);
    let post = |body: Value| {
        json_of(
            ureq::post(&url)
                .set("Authorization", "Bearer tok")
                .send_json(body)
                .unwrap(),
        )
    };
    let all = post(json!({}));
    let steps = all["steps"].as_array().unwrap().clone();
    assert!(
        steps
            .iter()
            .all(|s| s["report"]["status"] == "ok" && s["snapshot"] == true),
        "{all:#}"
    );
    // What each step changed: the updates add two tags and remove y's class.
    let tag = &steps[1]["diff"];
    assert_eq!(
        (tag["added"].as_u64(), tag["removed"].as_u64()),
        (Some(2), Some(1))
    );
    let by_predicate: Vec<(&str, u64, u64)> = tag["predicates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["predicate"].as_str().unwrap(),
                p["added"].as_u64().unwrap(),
                p["removed"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by_predicate,
        [
            ("http://e/tag", 2, 0),
            ("http://www.w3.org/1999/02/22-rdf-syntax-ns#type", 0, 1)
        ]
    );
    assert!(
        tag["classes"]
            .as_array()
            .unwrap()
            .contains(&json!({"class": "http://e/B", "added": 0, "removed": 1})),
        "{tag:#}"
    );
    assert_eq!(steps[2]["diff"]["added"], 1);
    assert_eq!(steps[3]["report"]["triples"], 4);
    // The export is a file of the workspace now.
    let out = ureq::get(&s.url(&format!("/$/workspaces/{id}/files/queries/out.nt")))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        out.contains("<http://e/x> <http://e/tag> \"one\" ."),
        "{out}"
    );
    // The store after each step, to query (no token needed, as for queries).
    let ask = |step: usize, query: &str| {
        let r = ureq::post(&s.url(&format!("/$/workspaces/{id}/pipeline/{step}/sparql")))
            .set("Content-Type", "application/sparql-query")
            .set("Accept", "application/sparql-results+json")
            .send_string(query)
            .unwrap();
        json_of(r)["boolean"].as_bool().unwrap()
    };
    assert!(!ask(1, "ASK { ?s <http://e/tag> ?o }"));
    assert!(ask(2, "ASK { ?s <http://e/tag> \"one\" }"));
    assert!(ask(1, "ASK { ?s a <http://e/B> }"));
    assert!(!ask(2, "ASK { ?s a <http://e/B> }"));
    // Read only.
    let r = ureq::post(&s.url(&format!("/$/workspaces/{id}/pipeline/1/sparql")))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <urn:a> <urn:b> <urn:c> }");
    assert_ne!(status(r), 200);
    assert_eq!(
        status(
            ureq::get(&s.url(&format!("/$/workspaces/{id}/pipeline/9/sparql")))
                .query("query", "ASK {}")
                .call()
        ),
        404
    );
    // Edit an update: its step is changed, those after it out of date.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let file = s.url(&format!("/$/workspaces/{id}/files/updates/02-drop-b.ru"));
    ureq::put(&file)
        .set("Authorization", "Bearer tok")
        .send_string("DELETE { ?s <http://e/tag> ?t } WHERE { ?s <http://e/tag> ?t FILTER(?s = <http://e/y>) }")
        .unwrap();
    let state = json_of(ureq::get(&url).call().unwrap());
    let flags: Vec<(bool, bool)> = state["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["changed"] == true, s["outdated"] == true))
        .collect();
    assert_eq!(
        flags,
        [(false, false), (true, true), (false, true), (false, true)]
    );
    // Run again: from the changed step, on the snapshot before it (step 1 is not run again).
    let again = post(json!({}));
    assert_eq!(again["steps"][0]["report"], steps[0]["report"]);
    let tag = &again["steps"][1]["diff"];
    assert_eq!(
        (tag["added"].as_u64(), tag["removed"].as_u64()),
        (Some(1), Some(0))
    );
    assert!(again["steps"][3]["report"]["status"] == "ok");
    // One step from its input: what follows it is cleared, to run again.
    let one = post(json!({"from": "3 infer", "to": 3}));
    assert_eq!(one["steps"][2]["report"]["status"], "ok");
    assert!(one["steps"][3]["report"].is_null());
    assert_eq!(one["steps"][1]["report"], again["steps"][1]["report"]);
    // A parameter for this run.
    let tagged = post(json!({"from": 1, "values": {"tag": "two"}}));
    assert_eq!(tagged["steps"][3]["report"]["status"], "ok");
    assert!(ask(2, "ASK { ?s <http://e/tag> \"two\" }"));
    // A broken pipeline.yaml says why.
    ureq::put(&s.url(&format!("/$/workspaces/{id}/files/pipeline.yaml")))
        .set("Authorization", "Bearer tok")
        .send_string("steps:\n  - lod: x.ttl\n")
        .unwrap();
    let broken = json_of(ureq::get(&url).call().unwrap());
    assert!(
        broken["error"].as_str().unwrap().contains("no step kind"),
        "{broken}"
    );
    let (code, text) = error_text(
        ureq::post(&url)
            .set("Authorization", "Bearer tok")
            .send_json(json!({})),
    );
    assert_eq!(code, 400);
    assert!(text.contains("no step kind"), "{text}");
}

#[test]
fn pipeline_of_a_workspace_stays_within_its_limits() {
    let dir = temp_dir("pipeline-limits");
    let s = start(
        &dir,
        WorkspaceOptions {
            max_triples: 100,
            max_bytes: 64 << 10,
            ..WorkspaceOptions::default()
        },
        None,
    );
    let id = s.create(json!({ "files": {
        "updates/grow.ru": "INSERT { ?s <http://e/p> ?b } WHERE { VALUES ?a { 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 } VALUES ?b { 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 } BIND(IRI(CONCAT(\"http://e/\", STR(?a))) AS ?s) }",
        "updates/small.ru": "INSERT DATA { <http://e/a> <http://e/b> <http://e/c> }",
        "pipeline.yaml": "steps:\n  - update: updates/small.ru\n  - export: ../escape.nt\n",
    } }));
    let url = s.url(&format!("/$/workspaces/{id}/pipeline"));
    let run = json_of(ureq::post(&url).send_json(json!({})).unwrap());
    assert_eq!(run["steps"][1]["report"]["status"], "failed");
    assert!(
        run["steps"][1]["report"]["error"]
            .as_str()
            .unwrap()
            .contains("outside the workspace")
    );
    // More quads than four times the workspace's triples: the step fails.
    ureq::put(&s.url(&format!("/$/workspaces/{id}/files/pipeline.yaml")))
        .send_string("steps:\n  - update: updates/grow.ru\n")
        .unwrap();
    let run = json_of(ureq::post(&url).send_json(json!({})).unwrap());
    let report = &run["steps"][0]["report"];
    assert_eq!(report["status"], "failed");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("more than the 400"),
        "{report}"
    );
    // An export bigger than the room left.
    ureq::put(&s.url(&format!("/$/workspaces/{id}/files/pipeline.yaml")))
        .send_string("steps:\n  - update: updates/grow.ru\n  - export: queries/big.nt\n")
        .unwrap();
    // 300 triples of more than 300 bytes each: more than the 64 KB the workspace has.
    let grow = format!(
        "INSERT {{ ?s <http://e/p> ?o }} WHERE {{ VALUES ?a {{ {} }} VALUES ?b {{ {} }} \
         BIND(IRI(CONCAT(\"http://e/\", STR(?a), \"-\", STR(?b))) AS ?s) \
         BIND(CONCAT(STR(?b), \"{}\") AS ?o) }}",
        (1..=20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        (1..=15)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        "x".repeat(300)
    );
    ureq::put(&s.url(&format!("/$/workspaces/{id}/files/updates/grow.ru")))
        .send_string(&grow)
        .unwrap();
    let run = json_of(ureq::post(&url).send_json(json!({})).unwrap());
    let report = &run["steps"][1]["report"];
    assert_eq!(report["status"], "failed", "{run:#}");
    assert!(
        report["error"].as_str().unwrap().contains("room for"),
        "{report}"
    );
    assert_eq!(
        status(ureq::get(&s.url(&format!("/$/workspaces/{id}/files/queries/big.nt"))).call()),
        404
    );
}

#[test]
fn pipeline_blame_names_the_step_and_file() {
    let dir = temp_dir("pipeline-blame");
    let s = start(&dir, WorkspaceOptions::default(), None);
    let rules = "@prefix sh: <http://www.w3.org/ns/shacl#> .
        <http://e/S> a sh:NodeShape ; sh:targetClass <http://e/A> ;
          sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate <http://e/ok> ; sh:object true ] .";
    let id = s.create(json!({ "files": {
        "data/a.ttl": "<http://e/x> a <http://e/A> . <http://e/x> <http://e/old> 1 .",
        "data/b.ttl": "<http://e/y> a <http://e/A> .",
        "updates/01-tag.ru": "INSERT { ?s <http://e/tag> \"t\" } WHERE { ?s a <http://e/A> }",
        "updates/02-drop.ru": "DELETE WHERE { ?s <http://e/old> ?o }",
        "rules/r.ttl": rules,
        "pipeline.yaml": "steps:\n  - load: data/\n  - update: updates/\n  - infer: rules/r.ttl\n",
    } }));
    let url = s.url(&format!("/$/workspaces/{id}/pipeline"));
    ureq::post(&url).send_json(json!({})).unwrap();
    let blame = |s_: &str, p: &str, o: &str| {
        json_of(
            ureq::get(&format!("{url}/blame"))
                .query("subject", s_)
                .query("predicate", p)
                .query("object", o)
                .call()
                .unwrap(),
        )
    };
    let events = |b: &Value| -> Vec<(u64, String, Value)> {
        b["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["step"].as_u64().unwrap(),
                    e["change"].as_str().unwrap().to_owned(),
                    e["file"].clone(),
                )
            })
            .collect()
    };
    let b = blame(
        "<http://e/y>",
        "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>",
        "<http://e/A>",
    );
    assert_eq!(events(&b), [(1, "added".to_owned(), json!("data/b.ttl"))]);
    assert_eq!(b["present"], true);
    let b = blame("<http://e/x>", "<http://e/tag>", "\"t\"");
    assert_eq!(
        events(&b),
        [(2, "added".to_owned(), json!("updates/01-tag.ru"))]
    );
    // Added, then removed: and by which file.
    let b = blame(
        "<http://e/x>",
        "<http://e/old>",
        "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>",
    );
    assert_eq!(
        events(&b),
        [
            (1, "added".to_owned(), json!("data/a.ttl")),
            (2, "removed".to_owned(), json!("updates/02-drop.ru"))
        ]
    );
    assert_eq!(b["present"], false);
    // Inferred: by the rules of a step.
    let b = blame(
        "<http://e/x>",
        "<http://e/ok>",
        "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
    );
    assert_eq!(events(&b), [(3, "added".to_owned(), Value::Null)]);
    assert_eq!(b["events"][0]["files"], json!(["rules/r.ttl"]));
    assert_eq!(b["checkedSteps"], 3);
    // Never there.
    assert_eq!(
        events(&blame("<http://e/z>", "<http://e/p>", "<http://e/o>")),
        []
    );
    let (code, text) = error_text(
        ureq::get(&format!("{url}/blame"))
            .query("subject", "nope")
            .query("predicate", "<http://e/p>")
            .query("object", "<http://e/o>")
            .call(),
    );
    assert_eq!(code, 400);
    assert!(text.contains("subject"), "{text}");
}

/// `path` relative to the working directory (`..` up to the root, then down).
fn relative(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap();
    let mut out = PathBuf::from(".");
    for _ in cwd.components().skip(1) {
        out.push("..");
    }
    out.join(path.strip_prefix("/").unwrap())
}

#[test]
fn pipeline_of_a_workspace_in_a_relative_folder() {
    let tmp = temp_dir("pipeline-relative");
    let dir = relative(&tmp);
    assert!(dir.is_relative());
    let s = start(&dir, WorkspaceOptions::default(), None);
    let mut values = String::new();
    for i in 0..300 {
        values.push_str(&format!("{i} "));
    }
    let id = s.create(json!({ "files": {
        "data/a.ttl": "<http://e/a> <http://e/b> <http://e/c> .",
        "updates/grow.ru": format!("INSERT {{ ?s <http://e/p> ?b }} WHERE {{ VALUES ?a {{ {values} }} VALUES ?b {{ {values} }} BIND(IRI(CONCAT(\"http://e/\", STR(?a))) AS ?s) }}"),
        "pipeline.yaml": "steps:\n  - load: data/\n  - update: updates/grow.ru\n  - export: out/kg.nt\n    query: queries/one.rq\n",
        "queries/one.rq": "CONSTRUCT WHERE { <http://e/a> ?p ?o }",
    } }));
    let url = s.url(&format!("/$/workspaces/{id}/pipeline"));
    let post = {
        let url = url.clone();
        std::thread::spawn(move || json_of(ureq::post(&url).send_json(json!({})).unwrap()))
    };
    // While it runs, reading the pipeline answers at once.
    let mut seen = false;
    while !post.is_finished() {
        let started = std::time::Instant::now();
        let view = json_of(ureq::get(&url).call().unwrap());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        if view["running"] == true {
            let blame = ureq::get(&format!(
                "{url}/blame?subject=%3Chttp://e/a%3E&predicate=%3Chttp://e/b%3E&object=%3Chttp://e/c%3E"
            ))
            .call();
            assert!(matches!(blame, Err(ureq::Error::Status(409, _))));
            seen = true;
            break;
        }
    }
    let run = post.join().unwrap();
    // The update of 90,000 triples takes about a second: long enough to be seen running.
    assert!(seen);
    for step in run["steps"].as_array().unwrap() {
        assert_eq!(step["report"]["status"], "ok", "{step}");
    }
    assert!(run.get("running").is_none());
    let file = dir.join(&id).join("files/out/kg.nt");
    assert!(
        std::fs::read_to_string(file)
            .unwrap()
            .contains("<http://e/c>")
    );
    // The pipeline as code to run elsewhere.
    let dag = ureq::get(&format!("{url}/export?to=airflow&location=/data/kg"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        dag.contains("t1 = BashOperator(task_id=\"load_1\"") && dag.contains("t1 >> t2 >> t3"),
        "{dag}"
    );
    assert!(dag.contains("LOCATION = \"/data/kg\""));
    let script = ureq::get(&format!("{url}/export?to=python"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(script.contains("e = tripletrail.Engine()"), "{script}");
    assert!(matches!(
        ureq::get(&format!("{url}/export?to=cobol")).call(),
        Err(ureq::Error::Status(400, _))
    ));
}

#[test]
fn a_python_step_waits_for_its_code_from_a_client() {
    let dir = temp_dir("pipeline-python");
    let s = start(&dir, WorkspaceOptions::default(), None);
    let id = s.create(json!({ "files": {
        "data/a.ttl": "<http://e/a> a <http://e/Station> .",
        "updates/tag.ru": "INSERT { ?s <http://e/tag> 1 } WHERE { GRAPH <http://e/py> { ?s <http://e/seen> true } }",
        "pipeline.yaml": "union_default_graph: false\nsteps:\n  - load: data/\n  - python: steps:run\n    graph: http://e/py\n    clear: true\n  - update: updates/tag.ru\n",
    } }));
    let url = s.url(&format!("/$/workspaces/{id}/pipeline"));
    let run = json_of(ureq::post(&url).send_json(json!({})).unwrap());
    let steps = run["steps"].as_array().unwrap();
    assert_eq!(steps[0]["report"]["status"], "ok");
    assert_eq!(steps[1]["report"]["status"], "waiting", "{run}");
    assert!(steps[2]["report"].is_null());
    // Its store, served as a Fuseki dataset named "2" under …/pipeline, takes updates and
    // the Graph Store Protocol; the step before it stays read-only.
    ureq::post(&format!("{url}/2/update"))
        .send_form(&[("update", "INSERT { GRAPH <http://e/py> { ?s <http://e/seen> true } } WHERE { ?s a <http://e/Station> }")])
        .unwrap();
    ureq::post(&format!("{url}/2/data?graph=http://e/extra"))
        .set("Content-Type", "text/turtle")
        .send_string("<http://e/b> <http://e/p> 1 .")
        .unwrap();
    let refused = ureq::post(&format!("{url}/1/update")).send_form(&[("update", "CLEAR ALL")]);
    assert!(matches!(refused, Err(ureq::Error::Status(405, _))));
    let completed = ureq::post(&format!("{url}/2/complete")).call().unwrap();
    let view = json_of(completed);
    assert_eq!(view["steps"][1]["report"]["status"], "ok", "{view}");
    assert_eq!(view["steps"][1]["report"]["quadsAfter"], 3);
    assert!(matches!(
        ureq::post(&format!("{url}/2/complete")).call(),
        Err(ureq::Error::Status(400, _))
    ));
    // The next run goes on from the step after it.
    let run = json_of(ureq::post(&url).send_json(json!({})).unwrap());
    assert_eq!(run["steps"][2]["report"]["status"], "ok", "{run}");
    let ask = ureq::get(&format!("{url}/3/sparql"))
        .query("query", "ASK { <http://e/a> <http://e/tag> 1 }")
        .set("Accept", "application/sparql-results+json")
        .call()
        .unwrap();
    assert_eq!(json_of(ask)["boolean"], true);
}

#[test]
fn a_waiting_python_step_keeps_its_result_and_its_bound() {
    let dir = temp_dir("pipeline-python-bound");
    // The store may hold 4 × 2 quads; a snapshot is kept up to 2.
    let s = start(
        &dir,
        WorkspaceOptions {
            max_triples: 2,
            ..WorkspaceOptions::default()
        },
        None,
    );
    let id = s.create(json!({ "files": {
        "data/a.ttl": "<http://e/a> a <http://e/Station> .",
        "updates/tag.ru": "INSERT DATA { <http://e/z> <http://e/tag> 1 }",
        "pipeline.yaml": "steps:\n  - load: data/\n  - python: steps:run\n  - update: updates/tag.ru\n",
    } }));
    let url = s.url(&format!("/$/workspaces/{id}/pipeline"));
    ureq::post(&url).send_json(json!({})).unwrap();
    let update =
        |text: &str| status(ureq::post(&format!("{url}/2/update")).send_form(&[("update", text)]));
    assert_eq!(
        update("INSERT DATA { <http://e/b> <http://e/p> 1, 2, 3, 4 }"),
        200
    );
    // 5 quads: more than a snapshot keeps, but the step's result is kept all the same, and the
    // next run goes on after it.
    let view = json_of(ureq::post(&format!("{url}/2/complete")).call().unwrap());
    assert_eq!(view["steps"][1]["snapshot"], true);
    let run = json_of(ureq::post(&url).send_json(json!({})).unwrap());
    assert_eq!(run["steps"][1]["report"]["status"], "ok", "{run}");
    assert_eq!(run["steps"][2]["report"]["status"], "ok", "{run}");

    // Waiting again: past 8 quads only removing a graph is taken.
    ureq::post(&url).send_json(json!({"from": 2})).unwrap();
    assert_eq!(
        update(
            "INSERT DATA { GRAPH <http://e/g> { <http://e/c> <http://e/p> 1, 2, 3, 4, 5, 6, 7, 8 } }"
        ),
        200
    );
    assert_eq!(update("INSERT DATA { <http://e/d> <http://e/p> 1 }"), 413);
    assert_eq!(
        status(
            ureq::post(&format!("{url}/2/data?graph=http://e/h"))
                .set("Content-Type", "text/turtle")
                .send_string("<http://e/x> <http://e/p> 1 .")
        ),
        413
    );
    assert_eq!(status(ureq::post(&format!("{url}/2/complete")).call()), 400);
    ureq::delete(&format!("{url}/2/data?graph=http://e/g"))
        .call()
        .unwrap();
    assert_eq!(update("INSERT DATA { <http://e/d> <http://e/p> 1 }"), 200);
    ureq::post(&format!("{url}/2/complete")).call().unwrap();
}

#[test]
fn with_authentication_the_owner_writes_a_waiting_python_step() {
    use tripletrail_server::auth::{sha256, to_hex};
    let dir = temp_dir("pipeline-python-auth");
    let hash = |k: &str| to_hex(&sha256(k.as_bytes()));
    let guard = dir.join("guard.json");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        &guard,
        format!(
            r#"{{"auth": {{"apiKeys": [
                {{"sha256": "{}", "agent": "https://example.org/people/ann"}},
                {{"sha256": "{}", "agent": "https://example.org/people/bob"}}
            ], "anonymous": true}}}}"#,
            hash("ann-key"),
            hash("bob-key"),
        ),
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), dataset(5))],
        Options {
            threads: 2,
            auth_config: Some(guard),
            workspaces: WorkspaceOptions {
                dir: Some(dir.join("ws")),
                ..WorkspaceOptions::default()
            },
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let as_ = |key: &str, r: ureq::Request| r.set("Authorization", &format!("Bearer {key}"));
    let made = json_of(
        as_("ann-key", ureq::post(&format!("{base}/$/workspaces")))
            .send_json(json!({ "files": {
                "data/a.ttl": "<http://e/a> a <http://e/Station> .",
                "pipeline.yaml": "steps:\n  - load: data/\n  - python: steps:run\n",
            } }))
            .unwrap(),
    );
    let url = format!(
        "{base}/$/workspaces/{}/pipeline",
        made["id"].as_str().unwrap()
    );
    as_("ann-key", ureq::post(&url))
        .send_json(json!({}))
        .unwrap();
    let form = |key: Option<&str>, service: &str, text: &str| {
        let r = ureq::post(&format!("{url}/2/{service}"));
        let r = match key {
            Some(k) => as_(k, r),
            None => r,
        };
        status(r.send_form(&[("update", text)]))
    };
    // The owner writes with an update, to /update or to /sparql, and with the GSP.
    assert_eq!(
        form(
            Some("ann-key"),
            "update",
            "INSERT DATA { <http://e/b> <http://e/p> 1 }"
        ),
        200
    );
    assert_eq!(
        form(
            Some("ann-key"),
            "sparql",
            "INSERT DATA { <http://e/c> <http://e/p> 1 }"
        ),
        200
    );
    let gsp = as_(
        "ann-key",
        ureq::post(&format!("{url}/2/data?graph=http://e/g")),
    )
    .set("Content-Type", "text/turtle")
    .send_string("<http://e/d> <http://e/p> 1 .");
    assert!(matches!(status(gsp), 200 | 201 | 204));
    // Others, anonymous callers too, do not see it.
    assert_eq!(form(Some("bob-key"), "update", "CLEAR ALL"), 404);
    assert_eq!(form(None, "sparql", "CLEAR ALL"), 404);
    let view = json_of(
        as_("ann-key", ureq::post(&format!("{url}/2/complete")))
            .call()
            .unwrap(),
    );
    assert_eq!(view["steps"][1]["report"]["quadsAfter"], 4, "{view}");
}

#[test]
fn runs_of_other_clients_appear_in_the_history() {
    let dir = temp_dir("history");
    let s = start(&dir, WorkspaceOptions::default(), None);
    let id = s.create(json!({ "files": {
        "data/a.ttl": "@prefix ex: <http://example.com/> . ex:a a ex:A ; ex:parent ex:b . ex:b ex:parent ex:c .",
        "shapes/s.ttl": SHAPES,
        "rules/r.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .\nex:R a sh:NodeShape ; sh:targetSubjectsOf ex:parent ;\n  sh:rule [ a sh:SPARQLRule ; sh:construct \"CONSTRUCT { $this <http://example.com/grand> ?g } WHERE { $this <http://example.com/parent>/<http://example.com/parent> ?g }\" ] .",
        "pipeline.yaml": "steps:\n  - load: data/\n",
    } }));
    let ws = s.url(&format!("/$/workspaces/{id}"));
    // The workbench's own requests are not recorded.
    ureq::post(&format!("{ws}/query"))
        .set("X-TripleTrail-Client", "workbench")
        .send_form(&[("query", "ASK {}")])
        .unwrap();
    // A client's are: a query (its text), a validation and an inference answered as JSON (kept),
    // an inference as triples (counted), a failed query and a pipeline run.
    ureq::post(&format!("{ws}/sparql"))
        .set("X-TripleTrail-Client", "python")
        .set("Content-Type", "application/sparql-query")
        .send_string("SELECT * { ?s ?p ?o }")
        .unwrap();
    let v = ureq::post(&format!("{ws}/validate"))
        .set("Accept", "application/json")
        .send_form(&[("limit", "1000")]);
    if let Err(ureq::Error::Status(_, r)) = v {
        panic!("{}", r.into_string().unwrap());
    }
    ureq::post(&format!("{ws}/infer"))
        .set("Accept", "application/json")
        .send_form(&[("provenance", "true")])
        .unwrap();
    ureq::post(&format!("{ws}/infer"))
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(
        status(
            ureq::get(&format!("{ws}/query"))
                .query("query", "SELECT nonsense")
                .call()
        ),
        400
    );
    ureq::post(&format!("{ws}/pipeline"))
        .set("User-Agent", "curl/8.5.0")
        .send_json(json!({}))
        .unwrap();
    let history = json_of(ureq::get(&format!("{ws}/history")).call().unwrap());
    let runs = history["runs"].as_array().unwrap();
    let ops: Vec<&str> = runs.iter().map(|r| r["op"].as_str().unwrap()).collect();
    assert_eq!(
        ops,
        ["pipeline", "query", "infer", "infer", "validate", "query"],
        "{history}"
    );
    assert_eq!(runs[0]["client"], "curl");
    assert_eq!(runs[1]["status"], 400);
    assert!(!runs[1]["summary"].as_str().unwrap().is_empty());
    assert_eq!(runs[2]["summary"], "1 triple inferred", "{}", runs[2]);
    assert_eq!(runs[2]["result"], false);
    assert_eq!(runs[3]["summary"], "1 triple inferred");
    assert_eq!(runs[3]["params"]["provenance"], "true");
    assert_eq!(runs[4]["summary"], "1 result", "{}", runs[4]);
    assert_eq!(runs[5]["client"], "python");
    assert_eq!(runs[5]["text"], "SELECT * { ?s ?p ?o }");
    // A kept answer: the inference with its provenance, for the why? panel.
    let inferred = json_of(
        ureq::get(&format!("{ws}/history/{}", runs[3]["id"]))
            .call()
            .unwrap(),
    );
    assert_eq!(inferred["result"]["count"], 1);
    assert!(inferred["result"]["provenance"].is_array(), "{inferred}");
    // Only what is newer than a run, then cleared.
    let last = history["last"].as_u64().unwrap();
    let newer = json_of(
        ureq::get(&format!("{ws}/history"))
            .query("since", &last.to_string())
            .call()
            .unwrap(),
    );
    assert_eq!(newer["runs"].as_array().unwrap().len(), 0);
    assert_eq!(status(ureq::delete(&format!("{ws}/history")).call()), 204);
    let cleared = json_of(ureq::get(&format!("{ws}/history")).call().unwrap());
    assert_eq!(cleared["runs"].as_array().unwrap().len(), 0);
    assert_eq!(
        status(ureq::get(&format!("{ws}/history/{}", runs[3]["id"])).call()),
        404
    );
}

#[test]
fn a_sampled_file_is_marked_until_replaced_and_after_a_restart() {
    let dir = temp_dir("sampled");
    let id = {
        let s = start(&dir, WorkspaceOptions::default(), None);
        let id = s.create(json!({ "lifetime": "saved", "files": { "data/own.ttl": "<http://e/a> <http://e/p> 1 ." } }));
        for file in ["data/one.nt", "data/two.nt"] {
            ureq::post(&s.url(&format!("/$/workspaces/{id}/sample")))
                .send_json(json!({ "dataset": "kg", "classes": ["http://example.com/A"], "instances": 2, "hops": 1, "file": file }))
                .unwrap();
        }
        // An edited sample is still a sample; a deleted one is gone from the list.
        ureq::put(&s.url(&format!("/$/workspaces/{id}/files/data/one.nt")))
            .set("Content-Type", "application/n-triples")
            .send_string("<http://e/b> <http://e/p> \"2\" .\n")
            .unwrap();
        id
    };
    let s = start(&dir, WorkspaceOptions::default(), None);
    let info = json_of(
        ureq::get(&s.url(&format!("/$/workspaces/{id}")))
            .call()
            .unwrap(),
    );
    let sampled: Vec<&str> = info["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["sampled"] == true)
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(sampled, ["data/one.nt", "data/two.nt"], "{info}");
    ureq::delete(&s.url(&format!("/$/workspaces/{id}/files/data/two.nt")))
        .call()
        .unwrap();
    ureq::put(&s.url(&format!("/$/workspaces/{id}/files/data/two.nt")))
        .set("Content-Type", "application/n-triples")
        .send_string("<http://e/c> <http://e/p> \"3\" .\n")
        .unwrap();
    let info = json_of(
        ureq::get(&s.url(&format!("/$/workspaces/{id}")))
            .call()
            .unwrap(),
    );
    // A file written where a deleted sample was is the caller's.
    let sampled: Vec<&str> = info["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["sampled"] == true)
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(sampled, ["data/one.nt"], "{info}");
}

#[cfg(feature = "git")]
fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[cfg(feature = "git")]
#[test]
fn a_workspace_is_pulled_from_a_folder_of_a_git_repository() {
    let dir = temp_dir("pull");
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("dev/shapes")).unwrap();
    std::fs::create_dir_all(repo.join("dev/other")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("dev/shapes/s.ttl"), SHAPES).unwrap();
    std::fs::write(repo.join("dev/pipeline.yaml"), "steps:\n  - load: data/\n").unwrap();
    std::fs::write(repo.join("dev/other/x.txt"), "not a workspace file").unwrap();
    std::fs::write(repo.join("top.ttl"), "<http://e/a> <http://e/p> 1 .").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let first = git(&repo, &["rev-parse", "HEAD"]);

    let s = start(&dir.join("ws"), WorkspaceOptions::default(), None);
    let id = s.create(json!({ "files": { "data/own.ttl": "<http://e/a> <http://e/p> 1 ." } }));
    let pull = s.url(&format!("/$/workspaces/{id}/pull"));
    let url = repo.to_string_lossy().to_string();
    let r = json_of(
        ureq::post(&pull)
            .send_json(json!({ "url": url, "ref": "main", "path": "dev" }))
            .unwrap(),
    );
    assert_eq!(r["git"]["commit"], first.as_str());
    assert_eq!(
        r["changed"],
        json!(["pipeline.yaml", "shapes/s.ttl"]),
        "{r}"
    );
    let info = json_of(
        ureq::get(&s.url(&format!("/$/workspaces/{id}")))
            .call()
            .unwrap(),
    );
    let paths: Vec<&str> = info["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    // Files only in the workspace stay; the rest of the repository is not read.
    assert_eq!(paths, ["data/own.ttl", "pipeline.yaml", "shapes/s.ttl"]);
    assert_eq!(info["git"]["path"], "dev");

    // Again, as last time: only what changed is written; an RDF file that does not parse
    // refuses the pull and changes nothing.
    std::fs::write(
        repo.join("dev/pipeline.yaml"),
        "steps:\n  - load: data/own.ttl\n",
    )
    .unwrap();
    git(&repo, &["commit", "-qam", "two"]);
    let r = json_of(ureq::post(&pull).send_json(json!({})).unwrap());
    assert_eq!(r["changed"], json!(["pipeline.yaml"]), "{r}");
    std::fs::write(repo.join("dev/shapes/s.ttl"), "not turtle").unwrap();
    git(&repo, &["commit", "-qam", "three"]);
    assert_eq!(status(ureq::post(&pull).send_json(json!({}))), 400);
    let text = ureq::get(&s.url(&format!("/$/workspaces/{id}/files/shapes/s.ttl")))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(text, SHAPES);
}

#[cfg(feature = "git")]
#[test]
fn only_an_admin_pulls_a_workspace_from_git() {
    let dir = temp_dir("pull-admin");
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), dataset(5))],
        Options {
            threads: 2,
            update_token: Some("w".to_owned()),
            admin_token: Some("a".to_owned()),
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
    let id = json_of(
        ureq::post(&format!("{base}/$/workspaces"))
            .set("Authorization", "Bearer w")
            .send_json(json!({}))
            .unwrap(),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pull = format!("{base}/$/workspaces/{id}/pull");
    let body = json!({ "url": dir.join("nothing").to_string_lossy() });
    let (code, text) = error_text(
        ureq::post(&pull)
            .set("Authorization", "Bearer w")
            .send_json(body.clone()),
    );
    assert_eq!(code, 403, "{text}");
    // An admin gets as far as the fetch, which fails: there is no repository.
    let (code, text) = error_text(
        ureq::post(&pull)
            .set("Authorization", "Bearer a")
            .send_json(body),
    );
    assert_eq!(code, 502, "{text}");
}

#[test]
fn a_workspaces_shapes_are_promoted_as_a_datasets_next_version() {
    let dir = temp_dir("promote");
    // A dataset's own shapes are kept under the server's location.
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), dataset(50))],
        Options {
            threads: 2,
            location: Some(dir.join("store")),
            workspaces: WorkspaceOptions {
                dir: Some(dir.join("ws")),
                ..WorkspaceOptions::default()
            },
            ..Options::default()
        },
    )
    .unwrap();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    let before = s.dump();
    let files = s.url("/$/datasets/kg/shapes/files");
    // The answer, or the status of a refusal.
    let put = |body: Value| match ureq::put(&files).send_json(body) {
        Ok(r) => Ok(json_of(r)),
        Err(ureq::Error::Status(code, _)) => Err(code),
        Err(e) => panic!("{e}"),
    };
    let source = json!({ "workspace": "w1", "name": "dev" });
    let r = put(json!({ "files": { "a.ttl": SHAPES }, "source": source })).unwrap();
    assert_eq!(r["version"], 1);
    let r = put(json!({ "files": { "a.ttl": SHAPES, "b.ttl": "" } })).unwrap();
    assert_eq!(r["version"], 2);
    let listed = json_of(ureq::get(&files).call().unwrap());
    assert_eq!(listed["files"].as_array().unwrap().len(), 2);
    // Checked as one set first: Turtle that does not parse, a bad name, or nothing.
    assert_eq!(put(json!({ "files": { "a.ttl": "not turtle" } })), Err(422));
    assert_eq!(put(json!({ "files": { "../a.ttl": SHAPES } })), Err(400));
    assert_eq!(put(json!({ "files": {} })), Err(400));
    assert_eq!(json_of(ureq::get(&files).call().unwrap())["version"], 2);
    // The data is not touched.
    assert_eq!(s.dump(), before);
}

#[test]
fn an_overlay_changes_apart_from_its_dataset() {
    let dir = temp_dir("overlay");
    let s = start(
        &dir,
        WorkspaceOptions {
            max_overlay_changes: 100,
            ..WorkspaceOptions::default()
        },
        Some("tok"),
    );
    let before = s.dump();
    let r = ureq::post(&s.url("/$/workspaces"))
        .set("Authorization", "Bearer tok")
        .send_json(
            json!({ "files": { "shapes/s.ttl": SHAPES, "pipeline.yaml": "steps:
  - update: updates/
", "updates/01-drop.ru": "DELETE WHERE { ?s a <http://example.com/D> }" } }),
        )
        .unwrap();
    let id = json_of(r)["id"].as_str().unwrap().to_owned();
    let overlay = s.url(&format!("/$/workspaces/{id}/overlay"));
    let update = s.url(&format!("/$/workspaces/{id}/update"));
    #[allow(clippy::result_large_err)]
    let send = |text: &str| {
        ureq::post(&update)
            .set("Authorization", "Bearer tok")
            .set("Content-Type", "application/sparql-update")
            .send_string(text)
    };
    // A plain workspace takes no update.
    assert_eq!(status(send("INSERT DATA { <urn:x> a <urn:T> }")), 405);
    // Not a dataset; no token.
    let r = ureq::post(&overlay)
        .set("Authorization", "Bearer tok")
        .send_json(json!({"dataset": "nope"}));
    assert_eq!(status(r), 404);
    assert_eq!(
        status(ureq::post(&overlay).send_json(json!({"dataset": "kg"}))),
        401
    );
    let taken = json_of(
        ureq::post(&overlay)
            .set("Authorization", "Bearer tok")
            .send_json(json!({"dataset": "kg"}))
            .unwrap(),
    );
    assert_eq!(taken["dataset"], "kg");
    assert_eq!(taken["quads"], 50 * 8);
    assert_eq!(taken["lost"], false);
    let count = |q: &str| s.count(&id, q);
    let all = "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }";
    assert_eq!(count(all), 50 * 8);
    // The shapes are on top, in their graph.
    assert!(count("SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }") > 0);

    // Updates write to the overlay only; they need the token.
    assert_eq!(
        status(
            ureq::post(&update)
                .set("Content-Type", "application/sparql-update")
                .send_string("INSERT DATA { <urn:x> a <urn:T> }")
        ),
        401
    );
    send("DELETE WHERE { ?s a <http://example.com/C> }").unwrap();
    send("INSERT DATA { <urn:x> a <urn:T> . <urn:x> <urn:p> \"one\" }").unwrap();
    assert_eq!(count(all), 50 * 8 - 50 + 2);
    assert_eq!(s.dump(), before, "the dataset is unchanged");
    // Validation reads the overlay: the shapes see the instances of ex:A.
    let v = json_of(
        ureq::get(&s.url(&format!("/$/workspaces/{id}/validate")))
            .set("Accept", "application/json")
            .call()
            .unwrap(),
    );
    assert_eq!(v["conforms"], true, "{v}");
    // A change of the shapes keeps the overlay's changes.
    ureq::put(&s.url(&format!("/$/workspaces/{id}/files/shapes/s.ttl")))
        .set("Authorization", "Bearer tok")
        .set("Content-Type", "text/turtle")
        .send_string(&SHAPES.replace("sh:minCount 1", "sh:minCount 2"))
        .unwrap();
    assert_eq!(count(all), 50 * 8 - 50 + 2);
    let v = json_of(
        ureq::get(&s.url(&format!("/$/workspaces/{id}/validate")))
            .set("Accept", "application/json")
            .call()
            .unwrap(),
    );
    assert_eq!(v["conforms"], false);

    // The changes as a patch, and as added and removed quads.
    let patch = ureq::get(&format!("{overlay}/patch"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    let lines: Vec<&str> = patch.lines().collect();
    assert_eq!(lines.first(), Some(&"TX ."));
    assert_eq!(lines.last(), Some(&"TC ."));
    assert_eq!(lines.iter().filter(|l| l.starts_with("D ")).count(), 50);
    assert_eq!(lines.iter().filter(|l| l.starts_with("A ")).count(), 2);
    assert!(patch.contains("A <urn:x> <urn:p> \"one\" ."), "{patch}");
    assert!(!patch.contains("shacl"), "the shapes are left out");
    let added = ureq::get(&format!("{overlay}/patch"))
        .query("format", "added")
        .call()
        .unwrap();
    assert_eq!(added.content_type(), "application/n-quads");
    assert_eq!(added.into_string().unwrap().lines().count(), 2);
    let described = json_of(ureq::get(&overlay).call().unwrap());
    assert!(described["changes"].as_u64().unwrap() >= 52);

    // An update sent as a form to …/sparql is checked the same way.
    #[allow(clippy::result_large_err)]
    let form = |key: Option<&str>| {
        let r = ureq::post(&s.url(&format!("/$/workspaces/{id}/sparql")));
        let r = match key {
            Some(k) => r.set("Authorization", &format!("Bearer {k}")),
            None => r,
        };
        r.send_form(&[("update", "INSERT DATA { <urn:f> a <urn:T> }")])
    };
    assert_eq!(status(form(None)), 401);
    assert_eq!(status(form(Some("tok"))), 200);
    send("DELETE DATA { <urn:f> a <urn:T> }").unwrap();

    // Past its cap, it takes no more updates; queries go on.
    let many: String = (0..200)
        .map(|i| format!("<urn:n{i}> a <urn:T> . "))
        .collect();
    send(&format!("INSERT DATA {{ {many} }}")).unwrap();
    let (code, text) = error_text(send("INSERT DATA { <urn:y> a <urn:T> }"));
    assert_eq!(code, 413, "{text}");
    assert_eq!(status(form(Some("tok"))), 413);
    assert_eq!(count(all), 50 * 8 - 50 + 2 + 200);

    // The pipeline starts from the dataset too.
    let pipeline = s.url(&format!("/$/workspaces/{id}/pipeline"));
    let run = json_of(
        ureq::post(&pipeline)
            .set("Authorization", "Bearer tok")
            .send_json(json!({}))
            .unwrap(),
    );
    let step = &run["steps"][0]["report"];
    assert_eq!(step["status"], "ok", "{run}");
    assert_eq!(step["quadsBefore"], 50 * 8);
    assert_eq!(step["quadsAfter"], 50 * 8 - 50);

    // The changes go, back to the dataset as taken; the dataset changes apart from it.
    ureq::delete(&format!("{overlay}/changes"))
        .set("Authorization", "Bearer tok")
        .call()
        .unwrap();
    assert_eq!(count(all), 50 * 8);
    ureq::post(&s.url("/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .set("Authorization", "Bearer tok")
        .send_string("INSERT DATA { <urn:new> a <urn:T> }")
        .unwrap();
    assert_eq!(count(all), 50 * 8);
    // Taken again, it sees the change.
    ureq::post(&overlay)
        .set("Authorization", "Bearer tok")
        .send_json(json!({"dataset": "kg"}))
        .unwrap();
    assert_eq!(count(all), 50 * 8 + 1);
    // A plain workspace again.
    ureq::delete(&overlay)
        .set("Authorization", "Bearer tok")
        .call()
        .unwrap();
    assert_eq!(count(all), 0);
    assert_eq!(status(send("INSERT DATA { <urn:x> a <urn:T> }")), 405);
}

/// `POST /$/workspaces/{id}/check` checks a file as it would be written, and writes nothing
/// (#409): what the AI assistant proposes is checked before the user applies it.
#[test]
fn a_file_is_checked_without_writing_it() {
    let dir = temp_dir("check");
    let s = start(&dir, WorkspaceOptions::default(), None);
    let id = s.create(json!({ "name": "check", "files": { "shapes/a.ttl": SHAPES } }));
    let check = |path: &str, text: &str| {
        json_of(
            ureq::post(&s.url(&format!("/$/workspaces/{id}/check")))
                .send_json(json!({ "path": path, "text": text }))
                .unwrap(),
        )
    };
    let errors = |path: &str, text: &str| -> Vec<String> {
        let r = check(path, text);
        assert_eq!(
            r["valid"],
            r["errors"].as_array().unwrap().is_empty(),
            "{r}"
        );
        serde_json::from_value(r["errors"].clone()).unwrap()
    };
    // Fine as they are.
    for (path, text) in [
        ("shapes/b.ttl", SHAPES),
        (
            "rules/r.ttl",
            r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .
            ex:R a sh:NodeShape ; sh:targetClass ex:A ;
              sh:rule [ a sh:SPARQLRule ; sh:construct "CONSTRUCT { $this a <http://example.com/B> } WHERE { }" ] ."#,
        ),
        ("queries/q.rq", "SELECT * { ?s ?p ?o }"),
        ("updates/u.ru", "INSERT DATA { <urn:a> <urn:p> 1 }"),
        (
            "tests/t/test.yaml",
            "query: ../../queries/q.rq\nexpected: expected.srj\n",
        ),
        ("tests/t/input.ttl", "<urn:a> <urn:p> 1 ."),
        (
            "tests/t/expected.srj",
            r#"{"head":{"vars":[]},"results":{"bindings":[]}}"#,
        ),
        ("README.md", "# Notes"),
    ] {
        assert_eq!(errors(path, text), Vec::<String>::new(), "{path}");
    }
    // What the server would refuse, or could not run.
    for (path, text, says) in [
        ("shapes/b.ttl", "ex:A a", "shapes/b.ttl"),
        (
            "shapes/b.ttl",
            "@prefix sh: <http://www.w3.org/ns/shacl#> . <urn:S> a sh:NodeShape ; sh:minCount \"x\" .",
            "minCount",
        ),
        (
            "rules/r.ttl",
            r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
            <urn:R> a sh:NodeShape ; sh:targetNode <urn:a> ; sh:rule [ a sh:SPARQLRule ; sh:construct "SELECT * {}" ] ."#,
            "CONSTRUCT",
        ),
        ("queries/q.rq", "SELECT * { ?s ?p }", "queries/q.rq"),
        (
            "updates/u.ru",
            "INSERT { <urn:a> <urn:p> 1 }",
            "updates/u.ru",
        ),
        ("tests/t/test.yaml", "query: [unclosed", "tests/t/test.yaml"),
        ("tests/t/expected.srj", "{", "tests/t/expected.srj"),
    ] {
        let found = errors(path, text);
        assert!(found.iter().any(|e| e.contains(says)), "{path}: {found:?}");
    }
    // A path the workspace can't hold is refused; nothing was written.
    assert_eq!(
        status(
            ureq::post(&s.url(&format!("/$/workspaces/{id}/check")))
                .send_json(json!({ "path": "../x.ttl", "text": "" }))
        ),
        400
    );
    let files = json_of(
        ureq::get(&s.url(&format!("/$/workspaces/{id}")))
            .call()
            .unwrap(),
    );
    let paths: Vec<&str> = files["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["shapes/a.ttl"]);
}
