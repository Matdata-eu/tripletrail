//! The access admin API (#205): from a server without policies, an admin makes RINF public
//! and the KG readable by one group, through `/$/access`, and the policy files keep their
//! comments and other policies.

use serde_json::{Value, json};
use std::path::Path;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::auth::{sha256, to_hex};
use tripletrail_server::{Options, Server};

fn hash(key: &str) -> String {
    to_hex(&sha256(key.as_bytes()))
}

fn start(dir: &Path) -> String {
    let config = format!(
        r#"{{"auth": {{"anonymous": true, "apiKeys": [
            {{"sha256": "{}", "agent": "https://example.org/people/admin", "groups": ["tripletrail:admin"], "label": "ops"}},
            {{"sha256": "{}", "agent": "https://example.org/people/contractor", "groups": ["asset-readers"], "expires": "2099-01-01"}}
        ]}}}}"#,
        hash("admin"),
        hash("contractor"),
    );
    std::fs::write(dir.join("guard.json"), config).unwrap();
    let rinf = Engine::new_tripletrail();
    rinf.load(RdfFormat::NTriples, &b"<urn:s> <urn:p> \"o\" .\n"[..])
        .unwrap();
    let assets = Engine::new_tripletrail();
    assets
        .load(
            RdfFormat::TriG,
            &br#"<urn:assets:release> { <urn:a> <urn:p> 1 . <urn:b> <urn:p> 2 }
             <urn:assets:wip> { <urn:c> <urn:p> 3 }
             <urn:meta> { <urn:assets:release> <http://purl.org/dc/terms/title> "Release" }"#[..],
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("rinf".to_owned(), rinf), ("assets".to_owned(), assets)],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn call(request: ureq::Request, body: Option<&Value>) -> (u16, Value) {
    let result = match body {
        Some(b) => request
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => request.call(),
    };
    match result {
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let status = r.status();
            let text = r.into_string().unwrap();
            (
                status,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            )
        }
        Err(e) => panic!("{e}"),
    }
}

fn as_key(request: ureq::Request, key: &str) -> ureq::Request {
    request.set("Authorization", &format!("Bearer {key}"))
}

#[test]
fn an_admin_opens_rinf_and_gives_assets_to_a_group() {
    let dir = tripletrail_testdir::tempdir("access");
    let base = start(&dir);
    let get = |path: &str, key: &str| call(as_key(ureq::get(&format!("{base}{path}")), key), None);
    let put = |ds: &str, policy: Value| {
        call(
            as_key(
                ureq::put(&format!("{base}/$/access/datasets/{ds}/policies")),
                "admin",
            ),
            Some(&policy),
        )
    };
    let ask = |ds: &str, key: Option<&str>, query: &str| {
        let request = ureq::get(&format!("{base}/{ds}/query")).query("query", query);
        let request = match key {
            Some(k) => as_key(request, k),
            None => request,
        };
        call(request, None)
    };

    // Only server admins see the access pages.
    assert_eq!(get("/$/access", "contractor").0, 403);
    let (status, overview) = get("/$/access", "admin");
    assert_eq!(status, 200, "{overview}");
    assert_eq!(overview["policiesOn"], false);
    let names: Vec<&str> = overview["datasets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["assets", "rinf"]);
    let keys = overview["keys"].as_array().unwrap();
    assert_eq!(keys[0]["label"], "ops");
    assert_eq!(keys[1]["expires"], "2099-01-01");
    assert!(
        !overview.to_string().contains(&hash("admin")),
        "no key hashes"
    );
    let groups: Vec<&str> = overview["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert!(groups.contains(&"asset-readers") && groups.contains(&"tripletrail:admin"));

    // The graph matrix rows: graphs, counts and labels.
    let (status, graphs) = get("/$/access/datasets/assets/graphs", "admin");
    assert_eq!(status, 200, "{graphs}");
    assert_eq!(
        graphs["graphs"][0],
        json!({"graph": "urn:assets:release", "triples": 2, "label": "Release"})
    );
    assert_eq!(graphs["graphs"].as_array().unwrap().len(), 3);

    // A name that is neither attached nor configured is refused: a typo must not turn
    // policies on (closing every dataset) or leave a file behind.
    let (status, _) = put(
        "nosuch",
        json!({"id": "urn:p:x", "groups": ["g"], "readAllGraphs": true}),
    );
    assert_eq!(status, 404);
    assert!(!dir.join("policies").exists());

    // RINF public: the dataset gets its own policy file, named in the configuration.
    let (status, reply) = put(
        "rinf",
        json!({"id": "https://example.org/policies/RinfPublic",
               "agents": ["https://matdata.eu/ns/sparql-guard#Anyone"], "readAllGraphs": true}),
    );
    assert_eq!(status, 200, "{reply}");
    assert!(dir.join("policies/rinf.ttl").exists());
    let config = std::fs::read_to_string(dir.join("guard.json")).unwrap();
    assert!(config.contains("policies/rinf.ttl"), "{config}");
    assert_eq!(ask("rinf", None, "ASK { ?s ?p ?o }").0, 200);
    // Policies are on: the KG, without any, is closed.
    assert_eq!(ask("assets", Some("contractor"), "ASK {}").0, 403);

    // The KG: the contractors' group reads the release graph only.
    let contractors = json!({"id": "https://example.org/policies/Contractors",
        "groups": ["asset-readers"], "readGraphs": ["urn:assets:release"]});
    assert_eq!(put("assets", contractors.clone()).0, 200);
    let (status, answer) = ask(
        "assets",
        Some("contractor"),
        "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }",
    );
    assert_eq!(status, 200);
    assert_eq!(answer["results"]["bindings"][0]["n"]["value"], "2");

    // A comment and a policy written by hand survive a change made through the API.
    let file = dir.join("policies/assets.ttl");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        format!(
            "# Kept by the asset team.\n{text}\n# Writers, by hand.\n<https://example.org/policies/Writers> a sg:Policy ; sg:group \"asset-team\" ; sg:writeAllGraphs true .\n"
        ),
    )
    .unwrap();
    let mut wider = contractors.clone();
    wider["readGraphs"] = json!(["urn:assets:release", "urn:assets:wip"]);
    assert_eq!(put("assets", wider).0, 200);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("# Kept by the asset team.") && text.contains("# Writers, by hand."));
    assert!(text.contains("sg:readGraph <urn:assets:wip>"), "{text}");
    let (_, overview) = get("/$/access", "admin");
    let assets = &overview["datasets"][0];
    assert_eq!(assets["ownPolicies"], true);
    assert_eq!(assets["policies"].as_array().unwrap().len(), 2);
    assert_eq!(
        ask(
            "assets",
            Some("contractor"),
            "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }"
        )
        .1["results"]["bindings"][0]["n"]["value"],
        "3"
    );

    // A policy that would not load changes nothing.
    let (status, _) = put(
        "assets",
        json!({"id": "https://example.org/policies/Nobody"}),
    );
    assert_eq!(status, 422);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);

    // Removing a policy.
    let (status, _) = call(
        as_key(
            ureq::delete(&format!("{base}/$/access/datasets/assets/policies"))
                .query("id", "https://example.org/policies/Contractors"),
            "admin",
        ),
        None,
    );
    assert_eq!(status, 204);
    assert_eq!(ask("assets", Some("contractor"), "ASK {}").0, 403);
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("# Writers, by hand.")
    );
}

/// Edits keep a 0600 configuration 0600, give a new policy file its mode, and write through a
/// configuration that is a symbolic link.
#[cfg(unix)]
#[test]
fn edits_keep_file_modes_and_links() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tripletrail_testdir::tempdir("access-mode");
    std::fs::create_dir_all(dir.join("mounted")).unwrap();
    let base = start(&dir);
    // The configuration moves elsewhere and a link takes its place, as when it is mounted.
    let real = dir.join("mounted/guard.json");
    std::fs::rename(dir.join("guard.json"), &real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&real, dir.join("guard.json")).unwrap();

    let (status, reply) = call(
        as_key(
            ureq::put(&format!("{base}/$/access/datasets/rinf/policies")),
            "admin",
        ),
        Some(&json!({"id": "https://example.org/policies/RinfPublic",
                     "agents": ["https://matdata.eu/ns/sparql-guard#Anyone"], "readAllGraphs": true})),
    );
    assert_eq!(status, 200, "{reply}");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert!(
        std::fs::symlink_metadata(dir.join("guard.json"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link stays a link"
    );
    assert!(
        std::fs::read_to_string(&real)
            .unwrap()
            .contains("policies/rinf.ttl")
    );
    assert_eq!(mode(&real), 0o600);
    assert_eq!(mode(&dir.join("policies/rinf.ttl")), 0o600);
}

#[test]
fn an_admin_creates_and_revokes_a_key() {
    let dir = tripletrail_testdir::tempdir("access-keys");
    let base = start(&dir);
    let post = |body: Value, key: &str| {
        call(
            as_key(ureq::post(&format!("{base}/$/access/keys")), key),
            Some(&body),
        )
    };
    let count = |key: &str| {
        call(
            as_key(
                ureq::get(&format!("{base}/assets/query"))
                    .query("query", "SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }"),
                key,
            ),
            None,
        )
    };

    // Only admins create keys, and a key needs an agent IRI.
    assert_eq!(
        post(
            json!({"agent": "https://example.org/people/x"}),
            "contractor"
        )
        .0,
        403
    );
    assert_eq!(post(json!({"agent": "not an iri"}), "admin").0, 400);
    assert_eq!(
        post(
            json!({"agent": "https://example.org/people/x", "expires": "soon"}),
            "admin"
        )
        .0,
        400
    );

    let (status, created) = post(
        json!({"label": "asset team", "agent": "https://example.org/people/assets",
               "groups": ["asset-team"], "datasets": ["assets"], "expires": "2099-12-31"}),
        "admin",
    );
    assert_eq!(status, 201, "{created}");
    let key = created["key"].as_str().unwrap().to_owned();
    let id = created["id"].as_str().unwrap().to_owned();
    // The file holds the hash, never the key.
    let config = std::fs::read_to_string(dir.join("guard.json")).unwrap();
    assert!(
        !config.contains(&key) && config.contains(&hash(&key)),
        "{config}"
    );

    // The key works at once, for its datasets only, and shows when it was last used.
    let (status, answer) = count(&key);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["results"]["bindings"][0]["n"]["value"], "4");
    let (status, _) = call(
        as_key(
            ureq::get(&format!("{base}/rinf/query")).query("query", "ASK {}"),
            &key,
        ),
        None,
    );
    assert_eq!(status, 403);
    let (_, overview) = call(
        as_key(ureq::get(&format!("{base}/$/access")), "admin"),
        None,
    );
    let shown = overview["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(shown["label"], "asset team");
    assert_eq!(shown["groups"], json!(["asset-team"]));
    assert!(shown["lastUsed"].is_string(), "{shown}");
    assert!(overview["groups"].to_string().contains("asset-team"));

    // Revoking: unknown ids are 404, and the key stops working at once.
    let delete = |id: &str| {
        call(
            as_key(ureq::delete(&format!("{base}/$/access/keys/{id}")), "admin"),
            None,
        )
        .0
    };
    assert_eq!(delete("00000000"), 404);
    assert_eq!(delete(&id), 204);
    assert_eq!(count(&key).0, 401);
    let config = std::fs::read_to_string(dir.join("guard.json")).unwrap();
    assert!(!config.contains(&hash(&key)));
    assert!(config.contains(&hash("contractor")), "other keys stay");

    // Anonymous access off: callers without a key get 401.
    let (status, _) = call(
        as_key(ureq::put(&format!("{base}/$/access/anonymous")), "admin"),
        Some(&json!({"anonymous": false})),
    );
    assert_eq!(status, 200);
    let (status, _) = call(
        ureq::get(&format!("{base}/rinf/query")).query("query", "ASK {}"),
        None,
    );
    assert_eq!(status, 401);
}

#[test]
fn an_admin_views_a_dataset_as_someone_else() {
    let dir = tripletrail_testdir::tempdir("access-view");
    let base = start(&dir);
    let (status, _) = call(
        as_key(
            ureq::put(&format!("{base}/$/access/datasets/assets/policies")),
            "admin",
        ),
        Some(&json!({"id": "https://example.org/policies/Contractors",
                     "groups": ["asset-readers"], "readGraphs": ["urn:assets:release"]})),
    );
    assert_eq!(status, 200);
    let count = |key: &str, view: Option<(&str, &str)>| {
        let mut request = as_key(
            ureq::get(&format!("{base}/assets/query"))
                .query("query", "SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }"),
            key,
        );
        if let Some((agent, groups)) = view {
            request = request
                .set("X-TripleTrail-View-As", agent)
                .set("X-TripleTrail-View-As-Groups", groups);
        }
        let (status, answer) = call(request, None);
        (
            status,
            answer["results"]["bindings"][0]["n"]["value"]
                .as_str()
                .map(str::to_owned),
        )
    };
    // The admin sees everything; viewing as a contractor, what the contractor sees.
    assert_eq!(count("admin", None), (200, Some("4".to_owned())));
    let contractor = ("https://example.org/people/someone", "asset-readers");
    assert_eq!(
        count("admin", Some(contractor)),
        (200, Some("2".to_owned()))
    );
    assert_eq!(
        count("contractor", None),
        count("admin", Some(contractor)),
        "the same view as the contractor's own key"
    );
    // Another group sees nothing of the rail KG; anonymous callers are asked to sign in.
    assert_eq!(
        count("admin", Some(("https://example.org/people/x", "others"))).0,
        403
    );
    let request = as_key(
        ureq::get(&format!("{base}/assets/query")).query("query", "ASK {}"),
        "admin",
    )
    .set("X-TripleTrail-View-As", "anonymous");
    assert_eq!(call(request, None).0, 401);
    // The view drops the admin's own rights, and only admins may use it.
    let request = as_key(ureq::get(&format!("{base}/$/access")), "admin")
        .set("X-TripleTrail-View-As", contractor.0)
        .set("X-TripleTrail-View-As-Groups", contractor.1);
    assert_eq!(call(request, None).0, 403);
    assert_eq!(count("contractor", Some(contractor)).0, 403);
}

#[test]
fn every_change_is_kept_and_can_be_restored() {
    let dir = tripletrail_testdir::tempdir("access-history");
    let base = start(&dir);
    let admin = |request: ureq::Request| as_key(request, "admin");
    let put = |ds: &str, policy: Value| {
        call(
            admin(ureq::put(&format!(
                "{base}/$/access/datasets/{ds}/policies"
            ))),
            Some(&policy),
        )
        .0
    };
    let found = std::fs::read_to_string(dir.join("guard.json")).unwrap();
    assert_eq!(
        put(
            "rinf",
            json!({"id": "urn:p:public", "agents": ["https://matdata.eu/ns/sparql-guard#Anyone"], "readAllGraphs": true})
        ),
        200
    );
    assert_eq!(
        put(
            "assets",
            json!({"id": "urn:p:contractors", "groups": ["asset-readers"], "readGraphs": ["urn:assets:release"]})
        ),
        200
    );
    let (status, history) = call(admin(ureq::get(&format!("{base}/$/access/history"))), None);
    assert_eq!(status, 200, "{history}");
    let changes: Vec<&str> = history["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["change"].as_str().unwrap())
        .collect();
    assert_eq!(
        changes,
        [
            "Policy urn:p:contractors of assets written",
            "Policy urn:p:public of rinf written",
            "As found"
        ]
    );
    assert_eq!(
        history["versions"][0]["by"],
        "https://example.org/people/admin"
    );
    // A version comes with the one before it, to show what changed.
    let (_, shown) = call(
        admin(ureq::get(&format!("{base}/$/access/history/2"))),
        None,
    );
    assert_eq!(shown["before"]["version"], 1);
    assert!(shown["files"].to_string().contains("urn:p:public"));
    assert!(
        !shown["before"]["files"]
            .to_string()
            .contains("urn:p:public")
    );
    assert_eq!(
        call(
            admin(ureq::get(&format!("{base}/$/access/history/99"))),
            None
        )
        .0,
        404
    );

    // A key made after the version survives its restore: a restore never brings keys back
    // or takes them away.
    let (_, created) = call(
        admin(ureq::post(&format!("{base}/$/access/keys"))),
        Some(&json!({"agent": "https://example.org/people/new"})),
    );
    let key = created["key"].as_str().unwrap().to_owned();
    // An operator edits a policy file by hand; the restore below undoes that too, so the
    // edited files are kept first.
    let assets_file = dir.join("policies/assets.ttl");
    let edited = std::fs::read_to_string(&assets_file)
        .unwrap()
        .replace("urn:assets:release", "urn:assets:wip");
    std::fs::write(&assets_file, &edited).unwrap();
    // What a restore of version 1 would change now, and no full key hashes.
    let (_, shown) = call(
        admin(ureq::get(&format!("{base}/$/access/history/1"))),
        None,
    );
    assert!(shown["current"].to_string().contains("urn:assets:wip"));
    assert!(!shown.to_string().contains(&hash(&key)), "hashes are cut");
    assert!(shown.to_string().contains(&hash(&key)[..8]));
    let (status, reply) = call(
        admin(ureq::post(&format!("{base}/$/access/history/1/restore"))),
        None,
    );
    assert_eq!(status, 200, "{reply}");
    let config = std::fs::read_to_string(dir.join("guard.json")).unwrap();
    assert!(!config.contains("policies/rinf.ttl"), "{config}");
    assert!(config.contains(&hash(&key)), "the new key stays");
    let as_found: Value = serde_json::from_str(&found).unwrap();
    let mut now: Value = serde_json::from_str(&config).unwrap();
    now["auth"]["apiKeys"].as_array_mut().unwrap().pop();
    assert_eq!(now, as_found);
    // Without policies, the contractor reads every KG graph again.
    let (status, answer) = call(
        as_key(
            ureq::get(&format!("{base}/assets/query"))
                .query("query", "SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }"),
            "contractor",
        ),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(answer["results"]["bindings"][0]["n"]["value"], "4");
    let (_, history) = call(admin(ureq::get(&format!("{base}/$/access/history"))), None);
    assert_eq!(history["versions"][0]["change"], "Restored version 1");
    let hand = &history["versions"][1];
    assert_eq!(hand["change"], "Changed outside the access pages");
    assert!(hand["by"].is_null());
    let (_, kept) = call(
        admin(ureq::get(&format!(
            "{base}/$/access/history/{}",
            hand["version"]
        ))),
        None,
    );
    assert!(
        kept["files"].to_string().contains("urn:assets:wip"),
        "the hand edit is kept"
    );
    // The files made after version 1 are gone, so the same changes can be made again.
    assert!(!dir.join("policies/rinf.ttl").exists());
    assert_eq!(
        put(
            "rinf",
            json!({"id": "urn:p:public", "agents": ["https://matdata.eu/ns/sparql-guard#Anyone"], "readAllGraphs": true})
        ),
        200
    );
}

/// Policies swapped while updates run (#208): each update is taken whole under the policies of
/// one moment, never half under the old and half under the new, so an update writing two
/// graphs is applied to both or to neither, as its answer says.
#[test]
fn updates_during_a_policy_swap_apply_whole_or_not_at_all() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tripletrail_testdir::tempdir("access-swap");
    let base = start(&dir);
    let policy = |graphs: &[&str]| {
        let (status, reply) = call(
            as_key(
                ureq::put(&format!("{base}/$/access/datasets/assets/policies")),
                "admin",
            ),
            Some(&json!({"id": "https://example.org/policies/Editors",
                         "groups": ["asset-readers"], "readAllGraphs": true, "writeGraphs": graphs})),
        );
        assert_eq!(status, 200, "{reply}");
    };
    let narrow = ["urn:assets:wip"];
    let wide = ["urn:assets:wip", "urn:assets:release"];
    policy(&narrow);
    let swapping = AtomicBool::new(true);
    let started = std::time::Instant::now();
    let answers: Vec<(String, u16, bool)> = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..2)
            .map(|t| {
                let (base, swapping) = (&base, &swapping);
                scope.spawn(move || {
                    let mut answers = Vec::new();
                    let (mut applied, mut refused, mut after) = (false, false, 0);
                    for i in 0.. {
                        let settled = !swapping.load(Ordering::Relaxed);
                        let done = settled && applied && refused && after >= 3;
                        if done || started.elapsed().as_secs() > 60 {
                            break;
                        }
                        let s = format!("urn:w{t}-{i}");
                        // One operation over both graphs, or one per graph.
                        let update = if i % 2 == 0 {
                            format!(
                                "INSERT DATA {{ GRAPH <urn:assets:wip> {{ <{s}> <urn:p> 1 }} \
                                 GRAPH <urn:assets:release> {{ <{s}> <urn:p> 1 }} }}"
                            )
                        } else {
                            format!(
                                "INSERT DATA {{ GRAPH <urn:assets:wip> {{ <{s}> <urn:p> 1 }} }} ; \
                                 INSERT DATA {{ GRAPH <urn:assets:release> {{ <{s}> <urn:p> 1 }} }}"
                            )
                        };
                        let request =
                            as_key(ureq::post(&format!("{base}/assets/update")), "contractor")
                                .set("Content-Type", "application/sparql-update");
                        let status = match request.send_string(&update) {
                            Ok(r) => r.status(),
                            Err(ureq::Error::Status(code, _)) => code,
                            Err(e) => panic!("{e}"),
                        };
                        applied |= status == 200;
                        refused |= status == 403;
                        after += usize::from(settled);
                        answers.push((s, status, settled));
                    }
                    answers
                })
            })
            .collect();
        for round in 0..20 {
            policy(if round % 2 == 0 { &wide } else { &narrow });
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        swapping.store(false, Ordering::Relaxed);
        writers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    });
    let (_, reply) = call(
        as_key(
            ureq::get(&format!("{base}/assets/query")).query(
                "query",
                "SELECT ?g ?s { GRAPH ?g { ?s <urn:p> 1 } FILTER(STRSTARTS(STR(?s), \"urn:w\")) }",
            ),
            "admin",
        ),
        None,
    );
    let mut stored = std::collections::BTreeMap::<String, Vec<String>>::new();
    for row in reply["results"]["bindings"].as_array().unwrap() {
        let s = row["s"]["value"].as_str().unwrap().to_owned();
        stored
            .entry(s)
            .or_default()
            .push(row["g"]["value"].as_str().unwrap().to_owned());
    }
    let (mut applied, mut refused) = (0, 0);
    for (s, status, settled) in &answers {
        // Sent once the narrow policy was the last one: refused.
        assert!(!settled || *status == 403, "{s}: {status}");
        let mut graphs = stored.remove(s).unwrap_or_default();
        graphs.sort();
        match status {
            200 => {
                applied += 1;
                assert_eq!(graphs, ["urn:assets:release", "urn:assets:wip"], "{s}");
            }
            403 => {
                refused += 1;
                assert!(graphs.is_empty(), "{s}: {graphs:?}");
            }
            other => panic!("{s}: {other}"),
        }
    }
    assert!(stored.is_empty(), "{stored:?}");
    assert!(
        applied > 0 && refused > 0,
        "{applied} applied, {refused} refused"
    );
}

/// Reviewed access changes (#207): with `auth.review`, a change waits for a second admin,
/// who makes it; nobody approves their own; a key for an admin is reviewed too, others and
/// revoking are not.
#[cfg(feature = "enterprise")]
#[test]
fn with_review_a_second_admin_approves_each_change() {
    let dir = tripletrail_testdir::tempdir("access-review");
    let config = format!(
        r#"{{"auth": {{"review": true, "apiKeys": [
            {{"sha256": "{}", "agent": "https://example.org/people/ann", "groups": ["tripletrail:admin"]}},
            {{"sha256": "{}", "agent": "https://example.org/people/bob", "groups": ["tripletrail:admin"]}},
            {{"sha256": "{}", "agent": "https://example.org/people/eve", "groups": ["asset-readers"]}}
        ]}}}}"#,
        hash("ann"),
        hash("bob"),
        hash("eve"),
    );
    std::fs::write(dir.join("guard.json"), config).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let policy = json!({"id": "urn:p:readers", "groups": ["asset-readers"], "readAllGraphs": true});
    // The policy file the change writes.
    let written = || dir.join("policies").join("assets.ttl").exists();
    assert!(!written());

    // Ann proposes: nothing changes yet.
    let (status, proposal) = call(
        as_key(
            ureq::put(&format!("{base}/$/access/datasets/assets/policies")),
            "ann",
        ),
        Some(&policy),
    );
    assert_eq!(status, 202, "{proposal}");
    assert_eq!(proposal["status"], "pending");
    assert_eq!(proposal["by"], "https://example.org/people/ann");
    assert_eq!(
        proposal["description"],
        "Policy urn:p:readers of assets written"
    );
    let n = proposal["proposal"].as_u64().unwrap();
    assert!(!written());
    // A policy that cannot be written is refused at once, not proposed.
    assert_eq!(
        call(
            as_key(
                ureq::put(&format!("{base}/$/access/datasets/assets/policies")),
                "ann"
            ),
            Some(&json!({"groups": ["x"]})),
        )
        .0,
        400
    );

    // Not by Ann herself.
    let approve = |n: u64, key: &str| {
        call(
            as_key(
                ureq::post(&format!("{base}/$/access/proposals/{n}/approve")),
                key,
            ),
            None,
        )
    };
    let (status, mine) = approve(n, "ann");
    assert_eq!(status, 403, "{mine}");
    // Not by someone who is no admin.
    assert_eq!(approve(n, "eve").0, 403);
    // Bob approves: it is made, and the history says who did what.
    let (status, approved) = approve(n, "bob");
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved["proposal"]["status"], "approved");
    assert_eq!(
        approved["proposal"]["decidedBy"],
        "https://example.org/people/bob"
    );
    assert_eq!(approved["policy"]["id"], "urn:p:readers");
    assert!(written());
    assert_eq!(approve(n, "bob").0, 409);
    let (_, history) = call(
        as_key(ureq::get(&format!("{base}/$/access/history")), "bob"),
        None,
    );
    assert_eq!(
        history["versions"][0]["change"],
        format!(
            "Policy urn:p:readers of assets written (proposal {n} by https://example.org/people/ann, approved)"
        )
    );
    assert_eq!(
        history["versions"][0]["by"],
        "https://example.org/people/bob"
    );

    // A key for an admin waits; the approver gets the key, once. Other keys do not wait.
    let (status, proposed) = call(
        as_key(ureq::post(&format!("{base}/$/access/keys")), "ann"),
        Some(&json!({"agent": "https://example.org/people/sock", "groups": ["tripletrail:admin"]})),
    );
    assert_eq!(status, 202, "{proposed}");
    let (status, made) = approve(proposed["proposal"].as_u64().unwrap(), "bob");
    assert_eq!(status, 200, "{made}");
    assert!(made["key"].as_str().unwrap().starts_with("mg_"));
    let (status, plain) = call(
        as_key(ureq::post(&format!("{base}/$/access/keys")), "ann"),
        Some(&json!({"agent": "https://example.org/people/reader", "groups": ["asset-readers"]})),
    );
    assert_eq!(status, 201, "{plain}");

    // A rejected proposal is never made; anonymous access stays off.
    let (status, anonymous) = call(
        as_key(ureq::put(&format!("{base}/$/access/anonymous")), "ann"),
        Some(&json!({"anonymous": true})),
    );
    assert_eq!(status, 202);
    let a = anonymous["proposal"].as_u64().unwrap();
    let (status, rejected) = call(
        as_key(
            ureq::post(&format!("{base}/$/access/proposals/{a}/reject")),
            "ann",
        ),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(rejected["proposal"]["status"], "rejected");
    assert_eq!(approve(a, "bob").0, 409);
    let (_, overview) = call(as_key(ureq::get(&format!("{base}/$/access")), "bob"), None);
    assert_eq!(overview["anonymous"], false);
    assert_eq!(overview["review"], true);
    let (_, listed) = call(
        as_key(ureq::get(&format!("{base}/$/access/proposals")), "bob"),
        None,
    );
    let statuses: Vec<&str> = listed["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["rejected", "approved", "approved"]);
}

/// Reviewed access changes (#207) cannot be got round: an admin viewing as someone else
/// makes, proposes and decides nothing (the audit log names the admin), the built-in admin
/// token is no second admin, and an unknown dataset is refused before it is proposed.
#[cfg(feature = "enterprise")]
#[test]
fn review_is_not_got_round_by_view_as_or_the_admin_token() {
    let dir = tripletrail_testdir::tempdir("access-review-round");
    let config = format!(
        r#"{{"auth": {{"review": true, "apiKeys": [
            {{"sha256": "{}", "agent": "https://example.org/people/ann", "groups": ["tripletrail:admin"]}}
        ]}}}}"#,
        hash("ann"),
    );
    std::fs::write(dir.join("guard.json"), config).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            admin_token: Some("root".to_owned()),
            audit_log: Some(dir.join("audit.log")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let carol = |request: ureq::Request| {
        as_key(request, "ann")
            .set("X-TripleTrail-View-As", "urn:carol")
            .set("X-TripleTrail-View-As-Groups", "tripletrail:admin")
    };
    let anonymous = || ureq::put(&format!("{base}/$/access/anonymous"));
    let on = json!({"anonymous": true});

    // Not proposed under another name.
    let (status, body) = call(carol(anonymous()), Some(&on));
    assert_eq!(status, 403, "{body}");
    // Ann proposes as herself, and cannot approve as Carol.
    let (status, proposal) = call(as_key(anonymous(), "ann"), Some(&on));
    assert_eq!(status, 202, "{proposal}");
    let n = proposal["proposal"].as_u64().unwrap();
    let approve = || ureq::post(&format!("{base}/$/access/proposals/{n}/approve"));
    let (status, body) = call(carol(approve()), None);
    assert_eq!(status, 403, "{body}");
    // Nor with the built-in admin token, which can still reject.
    let (status, body) = call(as_key(approve(), "root"), None);
    assert_eq!(status, 403, "{body}");
    let (status, body) = call(as_key(anonymous(), "root"), Some(&on));
    assert_eq!(status, 403, "{body}");
    let (status, body) = call(
        as_key(
            ureq::post(&format!("{base}/$/access/proposals/{n}/reject")),
            "root",
        ),
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["proposal"]["status"], "rejected");
    // Looking as Carol still works.
    let (status, body) = call(
        carol(ureq::get(&format!("{base}/$/access/proposals"))),
        None,
    );
    assert_eq!(status, 200, "{body}");

    // A dataset that is not there is refused at once.
    let (status, body) = call(
        as_key(
            ureq::put(&format!("{base}/$/access/datasets/nope/policies")),
            "ann",
        ),
        Some(&json!({"id": "urn:p:x", "groups": ["x"], "readAllGraphs": true})),
    );
    assert_eq!(status, 404, "{body}");
    let (_, list) = call(
        as_key(ureq::get(&format!("{base}/$/access/proposals")), "ann"),
        None,
    );
    assert_eq!(list["proposals"].as_array().unwrap().len(), 1, "{list}");

    // The audit log names Ann, viewing as Carol.
    let log = std::fs::read_to_string(dir.join("audit.log")).unwrap();
    let viewed: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|l| !l["viewAs"].is_null())
        .collect();
    assert!(!viewed.is_empty(), "{log}");
    for line in viewed {
        assert_eq!(line["agent"], "https://example.org/people/ann", "{line}");
        assert_eq!(line["viewAs"]["agent"], "urn:carol", "{line}");
    }
}
