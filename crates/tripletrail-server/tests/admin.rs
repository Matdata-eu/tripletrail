//! The admin API: attach, create, detach, alias swap, read-only datasets, snapshots, backups,
//! statistics and readiness (issues #86 to #90).

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::{Options, Server};

const TOKEN: &str = "s3cret";

const TTL: &str = r#"@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:a geo:hasGeometry ex:ga . ex:ga geo:asWKT "POINT(1 1)"^^geo:wktLiteral .
ex:b geo:hasGeometry ex:gb . ex:gb geo:asWKT "POINT(50 50)"^^geo:wktLiteral .
"#;

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("admin-{name}"))
}

struct Running {
    base: String,
}

fn start(dir: &Path, datasets: Vec<(String, Engine)>) -> Running {
    let server = Server::bind(
        "127.0.0.1:0",
        datasets,
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            backup_dir: Some(dir.join("backups")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    Running { base }
}

impl Running {
    fn admin(&self, method: &str, path: &str) -> ureq::Request {
        ureq::request(method, &format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {TOKEN}"))
    }

    fn get_json(&self, path: &str) -> Value {
        let r = ureq::get(&format!("{}{path}", self.base)).call().unwrap();
        serde_json::from_str(&r.into_string().unwrap()).unwrap()
    }

    fn count(&self, ds: &str) -> u64 {
        let r = ureq::get(&format!("{}/{ds}/query", self.base))
            .query("query", "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }")
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["results"]["bindings"][0]["n"]["value"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn update(&self, ds: &str, update: &str) -> u16 {
        status(
            ureq::post(&format!("{}/{ds}/update", self.base))
                .set("Content-Type", "application/sparql-update")
                .send_string(update),
        )
    }
}

fn status(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

fn saved_dataset(dir: &Path, name: &str, extra: &str) -> PathBuf {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
    engine
        .update(&format!("INSERT DATA {{ {extra} }}"))
        .unwrap();
    let target = dir.join(name);
    engine.save(&target).unwrap();
    target
}

#[test]
fn attach_query_swap_and_detach() {
    let dir = temp_dir("swap");
    let v1 = saved_dataset(
        &dir,
        "assets_v1",
        "<http://example.com/v> <http://example.com/is> 1",
    );
    let v2 = saved_dataset(
        &dir,
        "assets_v2",
        "<http://example.com/v> <http://example.com/is> 2 . <http://example.com/w> <http://example.com/is> 2",
    );
    let s = start(&dir, Vec::new());

    // Without the token, nothing changes.
    let r = ureq::post(&format!("{}/$/datasets", s.base))
        .query("name", "assets_v1")
        .query("location", v1.to_str().unwrap())
        .call();
    assert_eq!(status(r), 401);

    for (name, path) in [("assets_v1", &v1), ("assets_v2", &v2)] {
        let r = s
            .admin("POST", "/$/datasets")
            .query("name", name)
            .query("location", path.to_str().unwrap())
            .query("mode", "read-only")
            .call()
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    // Fuseki's form body works too, here for an alias.
    let r = s
        .admin("POST", "/$/datasets")
        .send_form(&[("dbName", "assets"), ("alias-of", "assets_v1")])
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(s.count("assets"), 5);
    assert_eq!(s.count("assets_v2"), 6);

    // Read-only: updates and uploads answer 405.
    assert_eq!(
        s.update("assets", "INSERT DATA { <http://x> <http://y> 1 }"),
        405
    );
    let r = ureq::post(&format!("{}/assets/data?default", s.base))
        .set("Content-Type", "text/turtle")
        .send_string(TTL);
    assert_eq!(status(r), 405);

    let d = s.get_json("/$/datasets/assets");
    assert_eq!(d["target"], "assets_v1");
    assert_eq!(d["mode"], "read-only");

    // Swap: new requests see the new version, and the previous one is remembered.
    let r = s
        .admin("POST", "/$/datasets/assets/swap")
        .query("to", "assets_v2")
        .call()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(s.count("assets"), 6);
    let d = s.get_json("/$/datasets/assets");
    assert_eq!(d["target"], "assets_v2");
    assert_eq!(d["previous"], "assets_v1");
    // An aliased dataset is not detached by accident.
    assert_eq!(
        status(s.admin("DELETE", "/$/datasets/assets_v2").call()),
        409
    );
    // Rollback is a swap back.
    s.admin("POST", "/$/datasets/assets/swap")
        .query("to", "assets_v1")
        .call()
        .unwrap();
    assert_eq!(s.count("assets"), 5);

    // Detach assets_v2 and delete its files.
    let r = s
        .admin("DELETE", "/$/datasets/assets_v2")
        .query("delete-files", "true")
        .call()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!v2.exists());
    assert_eq!(
        status(ureq::get(&format!("{}/assets_v2/query?query=ASK{{}}", s.base)).call()),
        404
    );
    let list = s.get_json("/$/datasets");
    let names: Vec<&str> = list["datasets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["ds.name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["/assets_v1", "/assets"]);
}

#[test]
fn a_long_query_finishes_on_the_version_it_started_on() {
    let dir = temp_dir("drain");
    let v1 = saved_dataset(&dir, "v1", "");
    let v2 = saved_dataset(
        &dir,
        "v2",
        "<http://example.com/new> <http://example.com/is> 1",
    );
    let s = start(&dir, Vec::new());
    for (name, path) in [("v1", &v1), ("v2", &v2)] {
        s.admin("POST", "/$/datasets")
            .query("name", name)
            .query("location", path.to_str().unwrap())
            .query("mode", "read-only")
            .call()
            .unwrap();
    }
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("alias-of", "v1")
        .call()
        .unwrap();
    // A query that takes a while: a cross product.
    let slow =
        "SELECT (COUNT(*) AS ?n) WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i . ?j ?k ?l . ?m ?o ?q }";
    let base = s.base.clone();
    let running = std::thread::spawn(move || {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", slow)
            .call()
            .unwrap();
        r.into_string().unwrap()
    });
    std::thread::sleep(Duration::from_millis(50));
    // Swap and release the old version while the query runs: the release waits for it.
    s.admin("POST", "/$/datasets/kg/swap")
        .query("to", "v2")
        .query("release", "true")
        .call()
        .unwrap();
    assert_eq!(s.count("kg"), 5);
    let answer = running.join().unwrap();
    // 4^5 rows on v1, the version it started on (v2 has 5 triples: 5^5).
    assert!(answer.contains("\"1024\""), "{answer}");
    for _ in 0..200 {
        if !s.get_json("/$/datasets").to_string().contains("\"/v1\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!s.get_json("/$/datasets").to_string().contains("\"/v1\""));
}

#[test]
fn create_backup_restore_snapshot_and_stats() {
    let dir = temp_dir("backup");
    let s = start(&dir, Vec::new());
    // An empty on-disk dataset under the server's location.
    s.admin("POST", "/$/datasets")
        .query("name", "metadata")
        .query("type", "disk")
        .call()
        .unwrap();
    assert!(dir.join("data/metadata/wal-0.log").exists());
    let r = ureq::put(&format!(
        "{}/metadata/data?graph=http://example.com/g",
        s.base
    ))
    .set("Content-Type", "text/turtle")
    .send_string(TTL)
    .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(
        s.update(
            "metadata",
            "INSERT DATA { <http://example.com/x> <http://example.com/y> 1 }"
        ),
        200
    );

    // Backup: a task, then a file in the list.
    let r = s.admin("POST", "/$/backup/metadata").call().unwrap();
    assert_eq!(r.status(), 202);
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    let id = v["taskId"].as_str().unwrap().to_owned();
    // Written after the backup started: not in it.
    s.update(
        "metadata",
        "INSERT DATA { <http://example.com/later> <http://example.com/y> 1 }",
    );
    let mut task = Value::Null;
    for _ in 0..500 {
        task = s.get_json(&format!("/$/tasks/{id}"));
        if task.get("finished").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(task["success"], true, "{task}");
    assert_eq!(task["task"], "Backup");
    assert_eq!(task["quads"], 5);
    let list = s.get_json("/$/backups-list");
    let file = list["backups"][0].as_str().unwrap().to_owned();
    assert!(
        file.starts_with("metadata_") && file.ends_with(".nq.gz"),
        "{file}"
    );

    // Restore into a new in-memory dataset: exactly what was committed when it started.
    s.admin("POST", "/$/datasets")
        .query("name", "restored")
        .query("from", &file)
        .call()
        .unwrap();
    assert_eq!(s.count("restored"), 1);
    let r = ureq::get(&format!("{}/restored/query", s.base))
        .query(
            "query",
            "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }",
        )
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(r.contains("\"4\""), "{r}");
    assert_eq!(
        status(
            s.admin("POST", "/$/datasets")
                .query("name", "bad")
                .query("from", "../etc/passwd")
                .call()
        ),
        400
    );

    // Snapshot of the read-write dataset, then attached read-only.
    let snap = dir.join("metadata_snapshot");
    let r = s
        .admin("POST", "/$/datasets/metadata/snapshot")
        .query("location", snap.to_str().unwrap())
        .call()
        .unwrap();
    assert_eq!(r.status(), 200);
    s.admin("POST", "/$/datasets")
        .query("name", "frozen")
        .query("location", snap.to_str().unwrap())
        .query("mode", "read-only")
        .call()
        .unwrap();
    assert_eq!(s.count("frozen"), 2);

    // Statistics, Fuseki's keys and ours.
    let stats = s.get_json("/$/stats/metadata");
    let m = &stats["datasets"]["/metadata"];
    assert_eq!(m["updates"], 2, "{m}");
    assert!(m["Requests"].as_u64().unwrap() >= 3);
    assert_eq!(m["triples"], 6);
    assert_eq!(m["graphs"], 1);
    assert!(m["sizeOnDisk"].as_u64().unwrap() > 0);
    assert!(m["lastWrite"].is_string());
    let all = s.get_json("/$/stats");
    assert!(all["datasets"]["/frozen"]["mappedBytes"].as_u64().unwrap() > 0);
    let server = s.get_json("/$/server");
    assert_eq!(server["adminToken"], true);
}

impl Running {
    /// Waits for a task of `/$/tasks` to finish, and returns it.
    fn task(&self, reply: ureq::Response) -> Value {
        assert_eq!(reply.status(), 202);
        let v: Value = serde_json::from_str(&reply.into_string().unwrap()).unwrap();
        let id = v["taskId"].as_str().unwrap().to_owned();
        for _ in 0..1000 {
            let task = self.get_json(&format!("/$/tasks/{id}"));
            if task.get("finished").is_some() {
                return task;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("task {id} did not finish");
    }
}

#[test]
fn store_backups_restore_verify_and_retention() {
    let dir = temp_dir("store-backup");
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            backup_dir: Some(dir.join("backups")),
            backup_keep: Some(2),
            ..Options::default()
        },
    )
    .unwrap();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "disk")
        .call()
        .unwrap();
    let r = ureq::post(&format!("{}/kg/data", s.base))
        .set("Content-Type", "text/turtle")
        .send_string(TTL)
        .unwrap();
    assert!(r.status() < 300);
    s.update(
        "kg",
        "INSERT DATA { <http://example.com/x> <http://example.com/y> 1 }",
    );
    let n = s.count("kg");
    assert!(n >= 2, "{n}");

    assert_eq!(
        status(s.admin("POST", "/$/backup/kg").query("kind", "zip").call()),
        400
    );
    // A store backup: a directory in the list, restored without parsing.
    let task = s.task(
        s.admin("POST", "/$/backup/kg")
            .query("kind", "store")
            .call()
            .unwrap(),
    );
    assert_eq!(task["success"], true, "{task}");
    assert_eq!(task["quads"], n);
    let store = task["file"].as_str().unwrap().to_owned();
    assert!(
        store.starts_with("kg_") && store.ends_with(".store"),
        "{store}"
    );
    let list = s.get_json("/$/backups-list");
    let listed = list["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == store.as_str())
        .unwrap()
        .clone();
    assert_eq!(listed["kind"], "store", "{listed}");
    assert_eq!(listed["dataset"], "kg");
    assert!(listed["time"].as_str().unwrap().ends_with('Z'));
    assert!(listed["size"].as_u64().unwrap() > 0);

    // Into a new on-disk dataset: the backup's files, copied.
    s.admin("POST", "/$/datasets")
        .query("name", "copy")
        .query("type", "disk")
        .query("from", &store)
        .call()
        .unwrap();
    assert_eq!(s.count("copy"), n);
    // Writes to the copy leave the backup as it was.
    s.update(
        "copy",
        "INSERT DATA { <http://example.com/new> <http://example.com/y> 2 }",
    );
    assert_eq!(s.count("copy"), n + 1);
    let check = s.task(
        s.admin("POST", &format!("/$/backups/{store}/verify"))
            .call()
            .unwrap(),
    );
    assert_eq!(check["success"], true, "{check}");
    assert_eq!(check["task"], "Verify");
    assert_eq!(check["quads"], n);
    // Into a new in-memory dataset: the quads, loaded.
    s.admin("POST", "/$/datasets")
        .query("name", "mem")
        .query("from", &store)
        .call()
        .unwrap();
    assert_eq!(s.count("mem"), n);

    // An N-Quads backup verifies by reading it through; a damaged store backup fails.
    let task = s.task(s.admin("POST", "/$/backup/kg").call().unwrap());
    let nquads = task["file"].as_str().unwrap().to_owned();
    let check = s.task(
        s.admin("POST", &format!("/$/backups/{nquads}/verify"))
            .call()
            .unwrap(),
    );
    assert_eq!(check["success"], true, "{check}");
    assert_eq!(check["quads"], n);
    assert_eq!(
        status(s.admin("POST", "/$/backups/nothing.nq.gz/verify").call()),
        404
    );

    // Two backups of kg are kept: the oldest goes when a third is written (the first two may
    // share a second).
    std::thread::sleep(Duration::from_millis(1100));
    let task = s.task(
        s.admin("POST", "/$/backup/kg")
            .query("kind", "store")
            .call()
            .unwrap(),
    );
    assert_eq!(task["success"], true, "{task}");
    let names: Vec<String> = s.get_json("/$/backups-list")["backups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap().to_owned())
        .collect();
    let of_kg: Vec<&String> = names.iter().filter(|n| n.starts_with("kg_")).collect();
    assert_eq!(of_kg.len(), 2, "{names:?}");
    assert!(
        names.iter().any(|n| n == task["file"].as_str().unwrap()),
        "{names:?}"
    );

    // Damage the remaining store backup: verify fails.
    let last = task["file"].as_str().unwrap().to_owned();
    let backup = dir.join("backups").join(&last);
    let cp = std::fs::read_dir(&backup)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .unwrap()
        .path();
    let seg = std::fs::read_dir(&cp)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".seg"))
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&seg).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&seg, bytes).unwrap();
    let check = s.task(
        s.admin("POST", &format!("/$/backups/{last}/verify"))
            .call()
            .unwrap(),
    );
    assert_eq!(check["success"], false, "{check}");
}

/// `t` as an RFC 3339 time in UTC, with milliseconds.
fn rfc3339(t: std::time::SystemTime) -> String {
    let ms = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

#[test]
fn point_in_time_restore_from_a_store_backup() {
    let dir = temp_dir("pitr");
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            backup_dir: Some(dir.join("backups")),
            wal_archive: Some(dir.join("archive")),
            ..Options::default()
        },
    )
    .unwrap();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "disk")
        .call()
        .unwrap();
    s.update(
        "kg",
        "INSERT DATA { <http://example.com/a> <http://example.com/p> 1 }",
    );
    let task = s.task(
        s.admin("POST", "/$/backup/kg")
            .query("kind", "store")
            .call()
            .unwrap(),
    );
    assert_eq!(task["success"], true, "{task}");
    let store = task["file"].as_str().unwrap().to_owned();
    assert!(dir.join("backups").join(&store).join("BASE").exists());

    // Two writes after the backup, with a restore point between them.
    s.update(
        "kg",
        "INSERT DATA { <http://example.com/b> <http://example.com/p> 2 }",
    );
    std::thread::sleep(Duration::from_millis(20));
    let between = rfc3339(std::time::SystemTime::now());
    std::thread::sleep(Duration::from_millis(20));
    s.update(
        "kg",
        "INSERT DATA { <http://example.com/c> <http://example.com/p> 3 }",
    );
    assert_eq!(s.count("kg"), 3);

    // Without until=, the backup as it was; with it, the writes up to that time replayed.
    let restore = |name: &str, until: Option<&str>| {
        let mut r = s
            .admin("POST", "/$/datasets")
            .query("name", name)
            .query("type", "disk")
            .query("from", &store);
        if let Some(until) = until {
            r = r.query("until", until);
        }
        // The status, and the answer when it is an error (a flaky 500 once came without it).
        match r.call() {
            Ok(r) => (r.status(), String::new()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    };
    let ok = |(code, body): (u16, String)| assert_eq!(code, 200, "{body}");
    ok(restore("plain", None));
    assert_eq!(s.count("plain"), 1);
    ok(restore("between", Some(&between)));
    assert_eq!(s.count("between"), 2);
    ok(restore("latest", Some("latest")));
    assert_eq!(s.count("latest"), 3);
    // The restored dataset takes writes of its own.
    s.update(
        "latest",
        "INSERT DATA { <http://example.com/d> <http://example.com/p> 4 }",
    );
    assert_eq!(s.count("latest"), 4);
    assert_eq!(s.count("kg"), 3);

    assert_eq!(restore("bad", Some("yesterday")).0, 400);
    assert_eq!(
        status(
            s.admin("POST", "/$/datasets")
                .query("name", "mem")
                .query("until", "latest")
                .call()
        ),
        400
    );
}

#[test]
fn backup_retention_prunes_the_wal_archive() {
    let dir = temp_dir("pitr-prune");
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            backup_dir: Some(dir.join("backups")),
            backup_keep: Some(1),
            wal_archive: Some(dir.join("archive")),
            ..Options::default()
        },
    )
    .unwrap();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "disk")
        .call()
        .unwrap();
    // Logs of an earlier incarnation of kg, which no kept backup can use.
    let stale = dir.join("archive").join("kg").join("0123456789abcdef");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("wal-1.log"), "").unwrap();
    let live = dir.join("archive").join("kg").join(
        std::fs::read_to_string(dir.join("data").join("kg").join("TIMELINE"))
            .unwrap()
            .trim(),
    );
    assert!(live.is_dir());
    let task = s.task(
        s.admin("POST", "/$/backup/kg")
            .query("kind", "store")
            .call()
            .unwrap(),
    );
    assert_eq!(task["success"], true, "{task}");
    // The prune runs after the task reports success.
    let started = std::time::Instant::now();
    while stale.exists() && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!stale.exists());
    // The live timeline's directory stays: a kept backup belongs to it.
    assert!(live.is_dir());
}

#[test]
fn dataset_profile() {
    let dir = temp_dir("profile");
    let engine = Engine::new_tripletrail();
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..6 {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:kind ex:k{} ; geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n",
            i % 2
        ));
    }
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let profile = s.get_json("/$/datasets/kg/profile");
    assert_eq!(profile["dataset"], "kg");
    assert_eq!(profile["classesTotal"], 1);
    let station = &profile["classes"][0];
    assert_eq!(station["class"], "http://example.com/Station");
    assert_eq!(station["instances"], 6);
    let kind = station["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["property"] == "http://example.com/kind")
        .unwrap();
    assert_eq!(kind["minCount"], 1);
    assert_eq!(kind["distinctValues"].as_array().unwrap().len(), 2);
    assert_eq!(kind["distinctValues"][0]["type"], "uri");
    let geometry = &station["geometries"][0];
    assert_eq!(
        geometry["path"],
        serde_json::json!([
            "http://www.opengis.net/ont/geosparql#hasGeometry",
            "http://www.opengis.net/ont/geosparql#asWKT"
        ])
    );
    assert_eq!(geometry["types"][0]["type"], "POINT");
    // Smaller samples on request; bad numbers and unknown datasets refused.
    let small = s.get_json("/$/datasets/kg/profile?sample=2");
    assert_eq!(small["classes"][0]["sampled"], 2);
    assert_eq!(small["options"]["pathSample"], 2);
    assert_eq!(
        status(ureq::get(&format!("{}/$/datasets/kg/profile?sample=0", s.base)).call()),
        400
    );
    assert_eq!(
        status(ureq::get(&format!("{}/$/datasets/nope/profile", s.base)).call()),
        404
    );
    // Under the query timeout: a shorter one may be asked for, a bad one is refused.
    assert_eq!(
        s.get_json("/$/datasets/kg/profile?timeout=30")["classesTotal"],
        1
    );
    assert_eq!(
        status(ureq::get(&format!("{}/$/datasets/kg/profile?timeout=x", s.base)).call()),
        400
    );
}

#[test]
fn generated_shapes_per_reader() {
    let dir = temp_dir("shapes");
    let engine = Engine::new_tripletrail();
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..6 {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:kind ex:k{} ; geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n\
             ex:o{i} a ex:Operator ; ex:name \"O{i}\" .\n",
            i % 2
        ));
    }
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let url = |path: &str| format!("{}{path}", s.base);
    let all = ureq::get(&url("/$/datasets/kg/shapes")).call().unwrap();
    assert_eq!(all.header("TripleTrail-Shapes"), Some("generated"));
    assert!(all.content_type().starts_with("text/turtle"));
    let all = all.into_string().unwrap();
    assert!(
        all.contains("sh:targetClass <http://example.com/Station>"),
        "{all}"
    );
    assert!(all.contains("sh:targetClass <http://example.com/Operator>"));
    assert!(all.contains("tts:Facet-Values"));
    // The map leaves out the operators, which have no geometry.
    let map = ureq::get(&url("/$/datasets/kg/shapes?reader=map"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(map.contains("<http://example.com/Station>"));
    assert!(
        !map.contains("sh:targetClass <http://example.com/Operator>"),
        "{map}"
    );
    let nt = ureq::get(&format!("{}/$/datasets/kg/shapes", s.base))
        .set("Accept", "application/n-triples")
        .call()
        .unwrap();
    assert_eq!(nt.content_type(), "application/n-triples");
    assert_eq!(
        status(ureq::get(&url("/$/datasets/kg/shapes?reader=nope")).call()),
        400
    );
    assert_eq!(
        status(ureq::get(&url("/$/datasets/kg/shapes?profile=not%20an%20iri")).call()),
        400
    );
    assert_eq!(
        status(ureq::get(&url("/$/datasets/nope/shapes")).call()),
        404
    );
}

#[test]
fn a_dataset_keeps_its_own_shapes() {
    let dir = temp_dir("own-shapes");
    let engine = Engine::new_tripletrail();
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..4 {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n\
             ex:o{i} a ex:Operator ; ex:name \"O{i}\" .\n"
        ));
    }
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let url = |path: &str| format!("{}/$/datasets/kg/shapes{path}", s.base);
    let w = |m: &str, path: &str| s.admin(m, &format!("/$/datasets/kg/shapes{path}"));
    let json =
        |r: ureq::Response| -> Value { serde_json::from_str(&r.into_string().unwrap()).unwrap() };

    let list = json(ureq::get(&url("/files")).call().unwrap());
    assert_eq!(list["version"], Value::Null);
    // Customise writes the generated shapes as files, one per class.
    let made = json(w("POST", "/customise").call().unwrap());
    assert_eq!(made["version"], 1);
    let list = json(ureq::get(&url("/files")).call().unwrap());
    let names: Vec<&str> = list["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Operator.ttl", "Station.ttl"]);
    assert_eq!(status(w("POST", "/customise").call()), 409);
    let station = ureq::get(&url("/files/Station.ttl"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(station.contains("tts:color \"#1f77b4\""), "{station}");

    // An edit is checked before it is kept.
    let bad = station.replace("#1f77b4", "red");
    let refused = w("PUT", "/files/Station.ttl").send_string(&bad);
    assert_eq!(status(refused), 422);
    let check = json(
        w("POST", "/validate?file=Station.ttl")
            .send_string(&bad)
            .unwrap(),
    );
    assert_eq!(check["valid"], false);
    assert!(check["errors"][0].as_str().unwrap().contains("#rrggbb"));
    let edited = station.replace("#1f77b4", "#000000");
    let saved = json(w("PUT", "/files/Station.ttl").send_string(&edited).unwrap());
    assert_eq!(saved["version"], 2);
    let read = ureq::get(&url("?reader=map")).call().unwrap();
    assert_eq!(read.header("TripleTrail-Shapes"), Some("local; version=2"));
    let read = read.into_string().unwrap();
    assert!(
        read.contains("\"#000000\"") && !read.contains("#1f77b4"),
        "{read}"
    );
    assert!(read.contains("tts:version 2"));

    // Removing a file brings back the generated shape; a rollback brings back the file.
    json(w("DELETE", "/files/Station.ttl").call().unwrap());
    let read = ureq::get(&url("")).call().unwrap().into_string().unwrap();
    assert!(read.contains("#1f77b4"));
    let back = json(w("POST", "/rollback?version=2").call().unwrap());
    assert_eq!(
        (back["version"].as_u64(), back["from"].as_u64()),
        (Some(4), Some(2))
    );
    let read = ureq::get(&url("")).call().unwrap().into_string().unwrap();
    assert!(read.contains("#000000"));
    assert_eq!(
        json(ureq::get(&url("/files")).call().unwrap())["versions"],
        serde_json::json!([1, 2, 3, 4])
    );
    assert_eq!(status(ureq::get(&url("/files/..%2Fx.ttl")).call()), 400);
    assert_eq!(status(w("POST", "/rollback?version=9").call()), 404);
}

#[test]
fn names_from_before_the_rename_are_warned_about_at_start() {
    // Spelt in pieces, as in the code (#955): the rename script and the export pass them by.
    let old = |rest: &str| format!("urn:{}graph:{rest}", "mat");
    let dir = temp_dir("old-names");
    let engine = Engine::new_tripletrail();
    engine
        .update(&format!(
            "INSERT DATA {{ GRAPH <{}> {{ <http://example.com/a> a <http://example.com/T> }} \
             GRAPH <urn:tripletrail:inferred> {{ <http://example.com/b> a <http://example.com/T> }} }}",
            old("inferred")
        ))
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
    let handle = server.handle();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    let put = |text: &str| {
        s.admin("PUT", "/$/datasets/kg/shapes/files/T.ttl")
            .send_string(text)
            .unwrap()
    };

    // A graph with an old name.
    let warnings = handle.old_names("kg");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let (graph, moved) = (old("inferred"), "urn:tripletrail:inferred");
    assert!(
        warnings[0].starts_with(&format!("graph <{graph}>"))
            && warnings[0].contains(&format!("MOVE <{graph}> TO <{moved}>")),
        "{}",
        warnings[0]
    );

    // A shape file in the old namespace loads, and is warned about with its path.
    let shape = |ns: &str| {
        format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             <http://example.com/TShape> a sh:NodeShape ; sh:targetClass <http://example.com/T> ; \
             <{ns}shapes#color> \"#000000\" .\n"
        )
    };
    put(&shape(&format!("http://w3id.org/{}graph/", "mat")));
    let warnings = handle.old_names("kg");
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].contains("v1") && warnings[0].contains("T.ttl: uses http://w3id.org/"),
        "{}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("s#http://w3id.org/")
            && warnings[0].contains("#http://w3id.org/tripletrail/#g"),
        "{}",
        warnings[0]
    );
    put(&shape("http://w3id.org/tripletrail/"));
    assert_eq!(handle.old_names("kg").len(), 1);
    assert!(handle.old_names("none").is_empty());
}

#[test]
fn the_shapes_editor_changes_reader_settings() {
    let dir = temp_dir("shapes-editor");
    let engine = Engine::new_tripletrail();
    let mut ttl = String::from("@prefix ex: <http://example.com/> .\n");
    for i in 0..3 {
        ttl.push_str(&format!(
            "ex:s{i} a ex:Station ; ex:name \"S{i}\" ; ex:code \"C{i}\" .\nex:o{i} a ex:Operator ; ex:name \"O{i}\" .\n"
        ));
    }
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let url = |path: &str| format!("{}/$/datasets/kg/shapes{path}", s.base);
    let w = |m: &str, path: &str| s.admin(m, &format!("/$/datasets/kg/shapes{path}"));
    let json =
        |r: ureq::Response| -> Value { serde_json::from_str(&r.into_string().unwrap()).unwrap() };

    // The outline: every node shape with its layer and its property shapes' settings.
    let outline = json(ureq::get(&url("/outline")).call().unwrap());
    assert_eq!(outline["editable"], true);
    assert_eq!(outline["version"], Value::Null);
    let station = outline["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["classes"][0] == "http://example.com/Station")
        .unwrap()
        .clone();
    assert_eq!(station["layer"], "generated");
    assert_eq!(station["overrides"], false);
    let shape = station["iri"].as_str().unwrap().to_owned();
    let paths: Vec<&str> = station["properties"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["path"].as_str())
        .collect();
    assert!(paths.contains(&"http://example.com/code"), "{paths:?}");
    // What each property holds, for the diagram.
    let name = station["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == "http://example.com/name")
        .unwrap();
    assert_eq!(name["datatype"], "http://www.w3.org/2001/XMLSchema#string");

    // An edit of a generated shape copies it into the own set, where it overrides it.
    // The answer, or the status of a refusal.
    let edit = |changes: Value, query: &str| -> Result<ureq::Response, u16> {
        let r = w("POST", &format!("/edit{query}"))
            .set("Content-Type", "application/json")
            .send_string(&serde_json::json!({ "changes": changes }).to_string());
        match r {
            Ok(r) => Ok(r),
            Err(e) => Err(status(Err(e))),
        }
    };
    let changes = serde_json::json!([
        { "shape": shape, "path": "http://example.com/code", "setting": "hidden", "value": true },
        { "shape": shape, "path": "http://example.com/code", "setting": "hiddenFrom", "value": ["describe", "search"] },
        { "shape": shape, "path": "http://example.com/code", "setting": "name", "value": "Code", "lang": "en" },
        { "shape": shape, "path": "http://example.com/code", "setting": "order", "value": 3 },
    ]);
    // A `dry` the server does not know is refused rather than made.
    assert_eq!(edit(changes.clone(), "?dry=yes").err(), Some(400));
    let dry = json(edit(changes.clone(), "?dry=true").unwrap());
    assert_eq!(dry["changed"], serde_json::json!(["Station.ttl"]));
    let text = dry["files"]["Station.ttl"].as_str().unwrap();
    assert!(text.contains("hidden"), "{text}");
    assert_eq!(
        json(ureq::get(&url("/files")).call().unwrap())["version"],
        Value::Null
    );
    let done = json(edit(changes, "").unwrap());
    assert_eq!(done["version"], 1);
    let outline = json(ureq::get(&url("/outline")).call().unwrap());
    let station = outline["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["iri"] == shape.as_str())
        .unwrap();
    assert_eq!(
        (station["layer"].as_str(), station["overrides"].as_bool()),
        (Some("own"), Some(true))
    );
    assert_eq!(station["file"], "Station.ttl");
    // The file refines the generated shape: it holds the changed property, and the rest
    // still comes from below.
    assert_eq!(station["refines"], true);
    let own = ureq::get(&url("/files/Station.ttl"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        own.contains("refines") && !own.contains("example.com/name") && !own.contains("instances"),
        "{own}"
    );
    let paths: Vec<&str> = station["properties"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["path"].as_str())
        .collect();
    assert!(paths.contains(&"http://example.com/name"), "{paths:?}");
    let code = station["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == "http://example.com/code")
        .unwrap();
    assert_eq!(code["hidden"], true);
    assert_eq!(
        code["hiddenFrom"],
        serde_json::json!(["describe", "search"])
    );
    assert_eq!(code["nameByLanguage"]["en"], "Code");
    assert_eq!(code["nameByLanguage"][""], "code");
    assert_eq!(code["order"], 3);
    // The describe reader no longer shows the hidden property.
    let read = ureq::get(&url("?reader=describe"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(!read.contains("example.com/code"), "{read}");
    // Back to the default: the setting removed, in the same file.
    let back = serde_json::json!([
        { "shape": shape, "path": "http://example.com/code", "setting": "hidden", "value": false },
        { "shape": shape, "path": "http://example.com/code", "setting": "hiddenFrom", "value": [] },
    ]);
    assert_eq!(json(edit(back, "").unwrap())["version"], 2);
    let text = ureq::get(&url("/files/Station.ttl"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        !text.contains("dash#hidden") && !text.contains("Reader-Describe"),
        "{text}"
    );
    // A dry edit with all=true gives the whole set as it would be, for a preview of the map.
    let operator = "urn:tripletrail:shape:http%3A%2F%2Fexample.com%2FOperator";
    let all = json(
        edit(
            serde_json::json!([{ "shape": operator, "setting": "name", "value": "Company" }]),
            "?dry=true&all=true",
        )
        .unwrap(),
    );
    let mut names: Vec<&str> = all["files"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["Operator.ttl", "Station.ttl"], "{all}");
    assert_eq!(all["changed"], serde_json::json!(["Operator.ttl"]));
    // A profile is created and a shape put in it; the other shapes, in none, are in all.
    let r = w("POST", "/edit")
        .set("Content-Type", "application/json")
        .send_string(
            &serde_json::json!({
                "profiles": [{ "iri": "urn:p:ops", "label": "Operations" }],
                "changes": [{ "shape": operator, "setting": "profiles", "value": ["urn:p:ops"] }],
            })
            .to_string(),
        )
        .unwrap();
    let mut changed: Vec<String> = serde_json::from_value(json(r)["changed"].clone()).unwrap();
    changed.sort();
    assert_eq!(changed, ["Operator.ttl", "profiles.ttl"]);
    let outline = json(ureq::get(&url("/outline")).call().unwrap());
    assert_eq!(
        outline["profiles"],
        serde_json::json!([{ "iri": "urn:p:ops", "label": "Operations", "shapes": 1 }])
    );
    let op = outline["shapes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["iri"] == operator)
        .unwrap();
    assert_eq!(op["profiles"], serde_json::json!(["urn:p:ops"]));
    // Profiles are set on node shapes only.
    assert_eq!(
        edit(
            serde_json::json!([{ "shape": shape, "path": "http://example.com/code", "setting": "profiles", "value": ["urn:p:ops"] }]),
            ""
        )
        .unwrap_err(),
        400
    );
    // The forms come from the editor shapes, and their settings are those of the edit API.
    let form = json(ureq::get(&url("/form")).call().unwrap());
    let field = |kind: &str, setting: &str| -> Value {
        form[kind]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["setting"] == setting)
            .cloned()
            .unwrap_or_else(|| panic!("no {kind} field {setting}: {form}"))
    };
    assert_eq!(field("node", "color")["pattern"], "^#[0-9a-fA-F]{6}$");
    assert_eq!(field("node", "hiddenFrom")["in"][1]["value"], "map");
    assert_eq!(field("property", "facet")["editor"], "EnumSelectEditor");
    assert_eq!(
        field("property", "facet")["in"][2],
        serde_json::json!({ "value": "http://w3id.org/tripletrail/shapes#Facet-Text", "label": "Text" })
    );
    assert_eq!(
        field("property", "group")["class"],
        "http://www.w3.org/ns/shacl#PropertyGroup"
    );
    let colour = |c: &str| {
        edit(
            serde_json::json!([{ "shape": operator, "setting": "color", "value": c }]),
            "?dry=true",
        )
    };
    assert!(
        json(colour("#ff0000").unwrap())["files"]["Operator.ttl"]
            .as_str()
            .unwrap()
            .contains("#ff0000")
    );
    assert_eq!(colour("red").unwrap_err(), 422);
    // Mistakes are named.
    let bad = |c: Value| edit(c, "").unwrap_err();
    assert_eq!(
        bad(serde_json::json!([{ "shape": "urn:none", "setting": "name", "value": "x" }])),
        404
    );
    assert_eq!(
        bad(serde_json::json!([{ "shape": shape, "setting": "colour", "value": "x" }])),
        400
    );
    assert_eq!(
        bad(
            serde_json::json!([{ "shape": shape, "path": "http://example.com/code", "setting": "order", "value": "first" }])
        ),
        400
    );
    assert_eq!(
        bad(serde_json::json!([{ "shape": shape, "setting": "hiddenFrom", "value": ["nowhere"] }])),
        400
    );
    assert_eq!(edit(serde_json::json!([]), "").unwrap_err(), 400);
    // Edits need the admin token.
    assert_eq!(
        status(
            ureq::post(&url("/edit"))
                .set("Content-Type", "application/json")
                .send_string("{\"changes\": []}")
        ),
        401
    );
}

#[test]
fn a_shape_pack_sits_between_the_generated_shapes_and_the_own_set() {
    let dir = temp_dir("shape-pack");
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "@prefix ex: <http://example.com/> .\n\
             @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
             ex:s1 a ex:Station ; ex:name \"S1\" ;\n\
               geo:hasGeometry [ geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral ] .\n\
             ex:o1 a ex:Operator ; ex:name \"O1\" .\n"
                .as_bytes(),
        )
        .unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let url = |path: &str| format!("{}/$/datasets/kg/shapes{path}", s.base);
    let w = |m: &str, path: &str| s.admin(m, &format!("/$/datasets/kg/shapes{path}"));
    let json =
        |r: ureq::Response| -> Value { serde_json::from_str(&r.into_string().unwrap()).unwrap() };
    let station = |colour: &str| {
        format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
             @prefix ex: <http://example.com/> .\n\
             <https://example.org/pack/rail#Station> a sh:NodeShape ;\n\
               sh:targetClass ex:Station ; tts:color \"{colour}\" .\n"
        )
    };
    let pack = |version: &str, colour: &str| {
        serde_json::json!({
            "name": "rail",
            "version": version,
            "files": { "Station.ttl": station(colour) },
            "source": { "repository": "https://example.org/packs/rail.git" },
        })
        .to_string()
    };
    // The status, and the answer when it is 200.
    let put_pack = |body: &str| -> (u16, Value) {
        match w("PUT", "/pack")
            .set("Content-Type", "application/json")
            .send_string(body)
        {
            Ok(r) => (200, json(r)),
            Err(e) => (status(Err(e)), Value::Null),
        }
    };
    // What is read: the source header, and the colour of the station shape.
    let read = || {
        let r = ureq::get(&url("")).call().unwrap();
        let source = r.header("TripleTrail-Shapes").unwrap().to_owned();
        let text = r.into_string().unwrap();
        let colour = ["#ff0000", "#00ff00", "#000000", "#1f77b4"]
            .into_iter()
            .find(|c| text.contains(&format!("\"{c}\"")))
            .unwrap_or("none");
        assert!(
            text.contains("sh:targetClass <http://example.com/Operator>"),
            "{text}"
        );
        (source, colour)
    };

    let none = json(ureq::get(&url("/pack")).call().unwrap());
    assert_eq!(none["installed"], false);
    assert_eq!(status(w("DELETE", "/pack").call()), 404);
    assert_eq!(read(), ("generated".to_owned(), "#1f77b4"));

    // Installing needs the admin token, a name and a version, and shapes that are valid.
    assert_eq!(
        status(
            ureq::put(&url("/pack"))
                .set("Content-Type", "application/json")
                .send_string(&pack("1.0", "#ff0000"))
        ),
        401
    );
    assert_eq!(
        put_pack(&pack("1.0", "#ff0000").replace("\"rail\"", "\"../x\"")).0,
        400
    );
    assert_eq!(put_pack(&pack("1.0", "red")).0, 422);
    assert_eq!(
        json(ureq::get(&url("/pack")).call().unwrap())["installed"],
        false
    );

    let installed = put_pack(&pack("1.0", "#ff0000")).1;
    assert_eq!(installed["replaced"], Value::Null);
    // The pack's shape replaces the generated one for its class; the others stay.
    assert_eq!(read(), ("pack rail@1.0".to_owned(), "#ff0000"));
    let shown = json(ureq::get(&url("/pack")).call().unwrap());
    assert_eq!(
        (shown["name"].as_str(), shown["version"].as_str()),
        (Some("rail"), Some("1.0"))
    );
    assert_eq!(shown["files"][0]["name"], "Station.ttl");
    assert_eq!(
        shown["source"]["repository"],
        "https://example.org/packs/rail.git"
    );
    // The pack is a layer of its own in the shapes' SPARQL dataset.
    let r = ureq::get(&url("/sparql"))
        .query(
            "query",
            "PREFIX tts: <http://w3id.org/tripletrail/shapes#>\n\
             SELECT ?name ?v WHERE { GRAPH <urn:tripletrail:shapes:pack> \
               { <urn:tripletrail:shapes:pack> tts:pack ?name ; tts:packVersion ?v } }",
        )
        .set("Accept", "application/sparql-results+json")
        .call()
        .unwrap();
    let answer = json(r);
    let row = &answer["results"]["bindings"][0];
    assert_eq!(
        (row["name"]["value"].as_str(), row["v"]["value"].as_str()),
        (Some("rail"), Some("1.0"))
    );

    // Customising starts from what is read, the pack included; the own set wins over it.
    json(w("POST", "/customise").call().unwrap());
    let file = ureq::get(&url("/files/Station.ttl"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(file.contains("\"#ff0000\""), "{file}");
    assert!(!file.contains("urn:tripletrail:shapes:pack"), "{file}");
    json(
        w("PUT", "/files/Station.ttl")
            .send_string(&file.replace("#ff0000", "#000000"))
            .unwrap(),
    );
    assert_eq!(
        read(),
        ("pack rail@1.0; local; version=2".to_owned(), "#000000")
    );

    // A new version of the pack keeps the dataset's own overrides.
    let replaced = put_pack(&pack("2.0", "#00ff00")).1;
    assert_eq!(replaced["replaced"], "rail@1.0");
    assert_eq!(
        read(),
        ("pack rail@2.0; local; version=2".to_owned(), "#000000")
    );
    json(w("DELETE", "/files/Station.ttl").call().unwrap());
    assert_eq!(
        read(),
        ("pack rail@2.0; local; version=3".to_owned(), "#00ff00")
    );

    // Removing the pack brings back the generated shape.
    let removed = json(w("DELETE", "/pack").call().unwrap());
    assert_eq!(removed["removed"], "rail@2.0");
    assert_eq!(read(), ("local; version=3".to_owned(), "#1f77b4"));

    // A snapshot of a store carries its pack, even without a set of its own.
    s.admin("POST", "/$/datasets")
        .query("name", "disk")
        .query("type", "disk")
        .call()
        .unwrap();
    ureq::post(&format!("{}/disk/data", s.base))
        .set("Content-Type", "text/turtle")
        .send_string("<http://example.com/s1> a <http://example.com/Station> .")
        .unwrap();
    s.admin("PUT", "/$/datasets/disk/shapes/pack")
        .set("Content-Type", "application/json")
        .send_string(&pack("1.0", "#ff0000"))
        .unwrap();
    let snap = dir.join("snap");
    s.admin("POST", "/$/datasets/disk/snapshot")
        .query("location", snap.to_str().unwrap())
        .call()
        .unwrap();
    s.admin("POST", "/$/datasets")
        .query("name", "ro")
        .query("location", snap.to_str().unwrap())
        .query("mode", "read-only")
        .call()
        .unwrap();
    let ro = ureq::get(&format!("{}/$/datasets/ro/shapes", s.base))
        .call()
        .unwrap();
    assert_eq!(ro.header("TripleTrail-Shapes"), Some("pack rail@1.0"));
    assert_eq!(
        status(s.admin("DELETE", "/$/datasets/ro/shapes/pack").call()),
        409
    );
}

#[test]
fn a_pack_write_that_stopped_halfway_keeps_the_old_pack() {
    let dir = temp_dir("pack-crash");
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "<http://example.com/s1> a <http://example.com/Station> .".as_bytes(),
        )
        .unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let body = serde_json::json!({
        "name": "rail",
        "version": "1.0",
        "files": { "Station.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
            <https://example.org/pack/rail#Station> a sh:NodeShape ; \
            sh:targetClass <http://example.com/Station> ; \
            <http://w3id.org/tripletrail/shapes#color> \"#ff0000\" ." },
    });
    s.admin("PUT", "/$/datasets/kg/shapes/pack")
        .send_string(&body.to_string())
        .unwrap();
    // A write moved the installed pack aside and stopped before the new one was in place.
    let shapes = dir.join("data/.shapes/kg");
    std::fs::rename(shapes.join("pack"), shapes.join(".pack.old")).unwrap();
    std::fs::create_dir_all(shapes.join(".pack.writing")).unwrap();

    let header = || {
        ureq::get(&format!("{}/$/datasets/kg/shapes", s.base))
            .call()
            .unwrap()
            .header("TripleTrail-Shapes")
            .unwrap()
            .to_owned()
    };
    assert_eq!(header(), "pack rail@1.0");
    // Put back by a read, once no write runs (the lock is server-wide, and other tests in
    // this process may hold it).
    for _ in 0..100 {
        if shapes.join("pack/pack.json").is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(header(), "pack rail@1.0");
    }
    assert!(shapes.join("pack/pack.json").is_file());
    assert!(!shapes.join(".pack.old").exists());

    // So too by a write, with the old pack named as the one replaced.
    std::fs::rename(shapes.join("pack"), shapes.join(".pack.old")).unwrap();
    let v: Value = serde_json::from_str(
        &s.admin("PUT", "/$/datasets/kg/shapes/pack")
            .send_string(&body.to_string().replace("\"1.0\"", "\"1.1\""))
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["replaced"], "rail@1.0");
    assert_eq!(header(), "pack rail@1.1");
}

#[test]
fn shapes_go_with_the_store_directory() {
    let dir = temp_dir("disk-shapes");
    let s = start(&dir, Vec::new());
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "disk")
        .call()
        .unwrap();
    ureq::post(&format!("{}/kg/data", s.base))
        .set("Content-Type", "text/turtle")
        .send_string(&format!("{TTL}ex:a a ex:Thing .\n"))
        .unwrap();
    let shapes_of = |ds: &str| {
        ureq::get(&format!("{}/$/datasets/{ds}/shapes", s.base))
            .call()
            .unwrap()
            .header("TripleTrail-Shapes")
            .map(str::to_owned)
    };
    s.admin("POST", "/$/datasets/kg/shapes/customise")
        .call()
        .unwrap();
    assert_eq!(shapes_of("kg").as_deref(), Some("local; version=1"));
    // Kept in the store directory.
    assert!(dir.join("data/kg/shapes/CURRENT").is_file());
    assert!(dir.join("data/kg/shapes/v1/Thing.ttl").is_file());

    // A store backup carries them, and a restore brings them back.
    let task = s.task(
        s.admin("POST", "/$/backup/kg")
            .query("kind", "store")
            .call()
            .unwrap(),
    );
    let store = task["file"].as_str().unwrap().to_owned();
    s.admin("POST", "/$/datasets")
        .query("name", "copy")
        .query("type", "disk")
        .query("from", &store)
        .call()
        .unwrap();
    assert_eq!(shapes_of("copy").as_deref(), Some("local; version=1"));

    // So does a snapshot, opened again as a dataset (read-only: the shapes are read, not
    // changed).
    let snap = dir.join("snap");
    s.admin("POST", "/$/datasets/kg/snapshot")
        .query("location", snap.to_str().unwrap())
        .call()
        .unwrap();
    s.admin("POST", "/$/datasets")
        .query("name", "ro")
        .query("location", snap.to_str().unwrap())
        .query("mode", "read-only")
        .call()
        .unwrap();
    assert_eq!(shapes_of("ro").as_deref(), Some("local; version=1"));
    assert!(snap.join("shapes/v1/Thing.ttl").is_file());
    assert_eq!(
        status(
            s.admin("POST", "/$/datasets/ro/shapes/customise?force=true")
                .call()
        ),
        409
    );
    assert_eq!(
        status(
            s.admin("DELETE", "/$/datasets/ro/shapes/files/Thing.ttl")
                .call()
        ),
        409
    );

    // A swap gives the new target the old one's shapes when it has none.
    s.admin("POST", "/$/datasets")
        .query("name", "fresh")
        .query("type", "disk")
        .call()
        .unwrap();
    s.admin("POST", "/$/datasets/live/swap")
        .query("to", "kg")
        .call()
        .unwrap();
    s.admin("POST", "/$/datasets/live/swap")
        .query("to", "fresh")
        .call()
        .unwrap();
    assert_eq!(shapes_of("fresh").as_deref(), Some("local; version=1"));
}

#[test]
fn an_in_memory_dataset_leaves_no_shapes_behind() {
    let dir = temp_dir("memory-shapes");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    s.admin("POST", "/$/datasets/kg/shapes/customise")
        .call()
        .unwrap();
    assert!(dir.join("data/.shapes/kg/CURRENT").is_file());
    s.admin("DELETE", "/$/datasets/kg").call().unwrap();
    assert!(!dir.join("data/.shapes/kg").exists());
}

#[test]
fn shapes_are_checked_without_a_location() {
    // The AI assistant (#146) checks a proposed file on any dataset: nothing is kept.
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("kg".to_owned(), engine)],
        Options {
            threads: 2,
            admin_token: Some(TOKEN.to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    let check = |text: &str| -> Value {
        let r = s
            .admin("POST", "/$/datasets/kg/shapes/validate?file=a.ttl")
            .set("Content-Type", "text/turtle")
            .send_string(text)
            .unwrap();
        serde_json::from_str(&r.into_string().unwrap()).unwrap()
    };
    let good = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                <urn:S> a sh:NodeShape ; sh:targetClass <urn:C> ; tts:color \"#1f6feb\" .";
    assert_eq!(check(good)["valid"], true);
    let bad = check("<urn:S> a <urn:T>");
    assert_eq!(bad["valid"], false);
    assert!(bad["errors"][0].as_str().unwrap().contains("a.ttl"));
    // Keeping a set still needs somewhere to keep it.
    assert_eq!(
        status(
            s.admin("PUT", "/$/datasets/kg/shapes/files/a.ttl")
                .set("Content-Type", "text/turtle")
                .send_string(good)
        ),
        409
    );
    // The vocabulary the assistant gives its model.
    let vocabulary = ureq::get(&format!("{}/$/ui/tts.ttl", s.base))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(vocabulary.contains("tts:color"));
}

#[test]
fn shapes_answer_sparql() {
    let dir = temp_dir("shapes-sparql");
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            format!("{TTL}<http://example.com/a> a <http://example.com/Thing> .\n").as_bytes(),
        )
        .unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let ask = |query: &str| -> Value {
        let r = ureq::get(&format!("{}/$/datasets/kg/shapes/sparql", s.base))
            .query("query", query)
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap();
        serde_json::from_str(&r.into_string().unwrap()).unwrap()
    };
    let count = |query: &str| -> String {
        ask(query)["results"]["bindings"][0]["n"]["value"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let shapes = "PREFIX sh: <http://www.w3.org/ns/shacl#>\n\
                  SELECT (COUNT(*) AS ?n) WHERE { ?s a sh:NodeShape }";
    assert_eq!(count(shapes), "1");
    // The layers in named graphs, the vocabulary too.
    let local =
        "SELECT (COUNT(*) AS ?n) WHERE { GRAPH <urn:tripletrail:shapes:local> { ?s ?p ?o } }";
    assert_eq!(count(local), "0");
    s.admin("POST", "/$/datasets/kg/shapes/customise")
        .call()
        .unwrap();
    assert_ne!(count(local), "0");
    let version = ask("PREFIX tts: <http://w3id.org/tripletrail/shapes#>\n\
         SELECT ?v WHERE { <urn:tripletrail:shapes:local> tts:version ?v }");
    assert_eq!(version["results"]["bindings"][0]["v"]["value"], "1");
    let label = ask("PREFIX skos: <http://www.w3.org/2004/02/skos/core#>\n\
         SELECT ?l WHERE { GRAPH <http://w3id.org/tripletrail/shapes> \
           { <http://w3id.org/tripletrail/shapes#Reader-Map> skos:prefLabel ?l } }");
    assert_eq!(label["results"]["bindings"][0]["l"]["value"], "Map");
    // A POSTed query needs no admin token: it reads.
    let posted = ureq::post(&format!("{}/$/datasets/kg/shapes/sparql", s.base))
        .set("Content-Type", "application/sparql-query")
        .set("Accept", "application/sparql-results+json")
        .send_string(shapes)
        .unwrap();
    assert_eq!(posted.status(), 200);
    assert_eq!(
        posted.header("TripleTrail-Shapes"),
        Some("local; version=1")
    );
    assert_eq!(
        status(ureq::get(&format!("{}/$/datasets/kg/shapes/sparql", s.base)).call()),
        400
    );
}

#[test]
fn ready_waits_for_start_and_spatial_index() {
    let server = Server::bind("127.0.0.1:0", Vec::new(), Options::default()).unwrap();
    let base = format!("http://{}", server.address());
    let handle = server.handle();
    handle.set_starting(true);
    server.spawn();
    assert_eq!(status(ureq::get(&format!("{base}/$/ping")).call()), 200);
    assert_eq!(status(ureq::get(&format!("{base}/$/ready")).call()), 503);
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
    engine.warm_up();
    handle.attach("kg", engine, None);
    handle.set_starting(false);
    let mut ok = false;
    for _ in 0..500 {
        if status(ureq::get(&format!("{base}/$/ready")).call()) == 200 {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ok);
    // Without a token configured, admin requests are open.
    assert_eq!(
        status(
            ureq::post(&format!("{base}/$/datasets"))
                .query("name", "mem")
                .call()
        ),
        200
    );
}

#[test]
fn graceful_stop_lets_a_running_query_finish_and_checkpoints() {
    let dir = temp_dir("stop");
    let store = tripletrail::store::Store::open(dir.join("kg")).unwrap();
    let engine = Engine::from_store(store);
    engine.load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
    engine
        .update("INSERT DATA { <http://example.com/x> <http://example.com/y> 1 }")
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            shutdown_grace: Duration::from_secs(30),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    let handle = server.handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        handle.stop();
    });
    let query = std::thread::spawn({
        let base = base.clone();
        move || {
            std::thread::sleep(Duration::from_millis(30));
            ureq::get(&format!("{base}/kg/query"))
                .query(
                    "query",
                    "SELECT (COUNT(*) AS ?n) WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i . ?j ?k ?l . ?m ?o ?q . ?r ?t ?u }",
                )
                .call()
                .unwrap()
                .into_string()
                .unwrap()
        }
    });
    server.run_until_stopped();
    stopper.join().unwrap();
    let answer = query.join().unwrap();
    assert!(answer.contains("\"15625\""), "{answer}");
    // The write is in the checkpoint now: the new log is empty (the retired one is kept for
    // read replicas, --keep-logs).
    let mut logs: Vec<(u64, u64)> = std::fs::read_dir(dir.join("kg"))
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = name
                .strip_prefix("wal-")?
                .strip_suffix(".log")?
                .parse()
                .ok()?;
            Some((n, e.metadata().unwrap().len()))
        })
        .collect();
    logs.sort();
    assert_eq!(logs.last().map(|l| l.1), Some(0), "{logs:?}");
}

#[test]
#[allow(clippy::result_large_err)]
fn the_web_ui_and_its_embedded_matgui() {
    let get = |base: &str, path: &str| ureq::get(&format!("{base}{path}")).call();
    let server = Server::bind("127.0.0.1:0", Vec::new(), Options::default()).unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let page = get(&base, "/$/ui/datasets/x")
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        page.contains("/$/ui/vendor/matgui/yasgui.min.js")
            || page.contains("data-matgui=\"/$/ui/vendor/matgui/\"")
    );
    // The tour, About and What's new; the newest release notes are for this version, so a
    // release without an entry in ui/whats-new.json fails here.
    assert!(page.contains("/$/ui/guide.js") && page.contains("id=\"about-btn\""));
    assert!(
        get(&base, "/$/ui/guide.js")
            .unwrap()
            .into_string()
            .unwrap()
            .contains("openTour")
    );
    let notes: serde_json::Value = serde_json::from_str(
        &get(&base, "/$/ui/whats-new.json")
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(notes[0]["version"], env!("CARGO_PKG_VERSION"));
    // ureq asks for gzip and decodes it: the script arrives whole.
    let js = get(&base, "/$/ui/vendor/matgui/yasgui.min.js").unwrap();
    assert_eq!(js.content_type(), "text/javascript");
    assert!(
        js.into_string()
            .unwrap()
            .starts_with("\"use strict\";var Yasgui")
    );
    // Without gzip, it is decompressed by the server.
    let r = ureq::get(&format!("{base}/$/ui/vendor/matgui/yasgui.min.css"))
        .set("Accept-Encoding", "identity")
        .call()
        .unwrap();
    assert!(r.header("Content-Encoding").is_none());
    assert!(r.into_string().unwrap().contains("--yasgui-brand"));
    assert_eq!(status(get(&base, "/$/ui/vendor/matgui/nope.js")), 404);
    // The Map page (#139): its page and script.
    let page = get(&base, "/$/ui/map/kg").unwrap().into_string().unwrap();
    assert!(page.contains("/$/ui/map.js") && page.contains("id=\"page-map\""));
    // A shared link holds the hidden layers and filters in its query.
    let shared = get(&base, "/$/ui/map/kg?hide=Track&Station.kind=a%20b")
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(shared, page);
    // Share links: the top bar's button, and the links it copies (the open feature of the map,
    // the Query page's tab, the Shapes page's shape) open the same page.
    assert!(page.contains("id=\"share-btn\""));
    for link in [
        "/$/ui/map/kg?feature=urn%3Aa",
        "/$/ui/query/kg?query=ASK%20%7B%7D&tabTitle=Ask",
        "/$/ui/shapes/kg?shape=urn%3As",
    ] {
        assert_eq!(get(&base, link).unwrap().into_string().unwrap(), page);
    }
    assert!(
        get(&base, "/$/ui/map.js")
            .unwrap()
            .into_string()
            .unwrap()
            .contains("TripleTrailMap")
    );
    // The map's tools (#33), loaded before the map.
    let index = get(&base, "/$/ui/map/kg").unwrap().into_string().unwrap();
    assert!(index.find("/$/ui/map-tools.js").unwrap() < index.find("/$/ui/map.js").unwrap());
    assert!(
        get(&base, "/$/ui/map-tools.js")
            .unwrap()
            .into_string()
            .unwrap()
            .contains("TripleTrailMapTools")
    );
    // The Explore page (#226): its page, with the start and opened nodes in the query.
    let page = get(&base, "/$/ui/explore/kg?iri=urn%3Aa&open=urn%3Ab")
        .unwrap()
        .into_string()
        .unwrap();
    assert!(page.contains("/$/ui/explore.js") && page.contains("id=\"page-explore\""));
    assert!(
        get(&base, "/$/ui/explore.js")
            .unwrap()
            .into_string()
            .unwrap()
            .contains("TripleTrailExplore")
    );
    // The Workbench (#402): its page, script, and vendored editor and tables.
    let page = get(&base, "/$/ui/workbench/abc/shapes/a.ttl")
        .unwrap()
        .into_string()
        .unwrap();
    assert!(page.contains("/$/ui/workbench.js") && page.contains("id=\"page-workbench\""));
    assert!(
        get(&base, "/$/ui/workbench.js")
            .unwrap()
            .into_string()
            .unwrap()
            .contains("TripleTrailWorkbench")
    );
    for (path, starts) in [
        ("codemirror/codemirror.min.js", "var CM="),
        ("codemirror/LICENSES.txt", "CodeMirror 6"),
        ("tabulator/tabulator.min.js", ""),
        ("tabulator/tabulator.min.css", ""),
        ("tabulator/LICENSE", "The MIT License"),
    ] {
        let body = get(&base, &format!("/$/ui/vendor/{path}"))
            .unwrap()
            .into_string()
            .unwrap();
        assert!(body.len() > 1000 && body.starts_with(starts), "{path}");
    }
    assert_eq!(status(get(&base, "/$/ui/vendor/codemirror/nope.js")), 404);
    // Tabulator inserts a string a formatter returns as HTML: every formatter of the Workbench
    // returns a DOM node for text from the data (textCell, termNode), or fixed or numeric text.
    let script =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/workbench.js")).unwrap();
    for (i, line) in script.lines().enumerate() {
        for part in line.split("formatter:").skip(1) {
            let safe = [
                "textCell(",
                "termNode(",
                "() => \"",
                ".toFixed(",
                ".length)",
                "? \"property\" : \"node\"",
                "\"Try in Query\" : \"\"",
            ];
            assert!(
                safe.iter()
                    .any(|s| part.split("},").next().unwrap_or(part).contains(s)),
                "workbench.js:{}: a formatter that may return data as HTML: {}",
                i + 1,
                line.trim()
            );
        }
    }
    assert_eq!(status(get(&base, "/$/ui/vendor/other/LICENSE")), 404);
    // `/` and `/$/ui` lead to the UI.
    let r = ureq::AgentBuilder::new().redirects(0).build();
    let to = |p: &str| {
        r.get(&format!("{base}{p}"))
            .call()
            .unwrap()
            .header("Location")
            .map(str::to_owned)
    };
    assert_eq!(to("/").as_deref(), Some("/$/ui/"));
    assert_eq!(to("/$/ui").as_deref(), Some("/$/ui/"));

    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            matgui_url: Some("https://cdn.example/matgui@6.15.0/build".to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let cdn = format!("http://{}", server.address());
    server.spawn();
    let page = get(&cdn, "/$/ui/").unwrap().into_string().unwrap();
    assert!(page.contains("https://cdn.example/matgui@6.15.0/build/yasgui.min.css"));

    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            ui: false,
            ..Options::default()
        },
    )
    .unwrap();
    let off = format!("http://{}", server.address());
    server.spawn();
    assert_eq!(status(get(&off, "/$/ui/")), 404);
}

/// MatGUI's Explain view (from 6.15.0, with yasgui-explain-plugin 1.1.0) knows TripleTrail: it
/// recognises the server by its `Server` header, asks `…/explain` beside the tab's `…/sparql`
/// with the query as a form, and draws the plan from `plan.plan`, whose steps carry `name`,
/// `number of results`, `duration in seconds` and `children`. Before, the view said "No engine
/// recognised" and showed no plan.
#[test]
fn the_query_page_explain_view_reads_the_plan_of_this_server() {
    let dir = temp_dir("explain-view");
    let s = start(&dir, Vec::new());
    s.admin("POST", "/$/datasets")
        .query("name", "plan")
        .query("type", "mem")
        .call()
        .unwrap();
    assert_eq!(
        s.update(
            "plan",
            "INSERT DATA { <http://example.com/a> <http://example.com/p> <http://example.com/b> .
               <http://example.com/b> <http://example.com/p> <http://example.com/c> }"
        ),
        200
    );
    // The built-in MatGUI carries the plugin's TripleTrail engine and its detection.
    let matgui = ureq::get(&format!("{}/$/ui/vendor/matgui/yasgui.min.js", s.base))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    for wired in [
        "id:\"tripletrail\"",
        "includes(\"tripletrail\")",
        "\"number of results\"",
        "\"duration in seconds\"",
        "\"planning duration in seconds\"",
    ] {
        assert!(matgui.contains(wired), "MatGUI no longer has {wired}");
    }
    // The request the view sends, and the fields it reads from the answer.
    let r = ureq::post(&format!("{}/plan/explain", s.base))
        .set("Accept", "application/json")
        .send_form(&[(
            "query",
            "SELECT * WHERE { ?a <http://example.com/p> ?b . ?b <http://example.com/p> ?c }",
        )])
        .unwrap();
    assert!(r.content_type().contains("json"));
    // The Server header names TripleTrail, which the Explain plugin recognises (on every
    // answer, so on the query the view reads it from too).
    let server = |r: &ureq::Response| r.header("Server").unwrap_or_default().to_owned();
    assert!(server(&r).starts_with("TripleTrail/"), "{}", server(&r));
    let q = ureq::get(&format!("{}/plan/sparql", s.base))
        .query("query", "ASK {}")
        .call()
        .unwrap();
    assert!(server(&q).starts_with("TripleTrail/"), "{}", server(&q));
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(v["rows"], 1);
    assert!(v["plan"]["planning duration in seconds"].is_number());
    let root = &v["plan"]["plan"];
    fn walk(n: &Value, steps: &mut usize) {
        assert!(n["name"].is_string(), "{n}");
        assert!(n["number of results"].is_number(), "{n}");
        assert!(n["duration in seconds"].is_number(), "{n}");
        *steps += 1;
        for c in n["children"].as_array().unwrap() {
            walk(c, steps);
        }
    }
    let mut steps = 0;
    walk(root, &mut steps);
    assert!(steps >= 3, "{root}");
}

#[test]
fn a_dataset_created_with_a_union_default_graph_reads_every_graph() {
    let dir = temp_dir("union");
    let s = start(&dir, Vec::new());
    s.admin("POST", "/$/datasets")
        .query("name", "build")
        .query("type", "mem")
        .query("unionDefaultGraph", "true")
        .call()
        .unwrap();
    assert_eq!(s.get_json("/$/datasets/build")["unionDefaultGraph"], true);
    assert_eq!(
        s.update(
            "build",
            "INSERT DATA { GRAPH <http://example.com/a> { <http://example.com/x> <http://example.com/p> 1 }
               GRAPH <http://example.com/b> { <http://example.com/y> <http://example.com/p> 2 } }"
        ),
        200
    );
    assert_eq!(s.count("build"), 2);
    // A rule without USING reads both source graphs, and writes to the default graph.
    assert_eq!(
        s.update(
            "build",
            "INSERT { ?s <http://example.com/q> ?o } WHERE { ?s <http://example.com/p> ?o }"
        ),
        200
    );
    assert_eq!(s.count("build"), 4);
    let r = ureq::get(&format!("{}/build/query", s.base))
        .query(
            "query",
            "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s <http://example.com/q> ?o } }",
        )
        .call()
        .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    // Not in a named graph: in the default graph.
    assert_eq!(v["results"]["bindings"][0]["n"]["value"], "0");
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
fn a_shape_pack_installs_from_a_git_tag() {
    let dir = temp_dir("git-pack");
    let repo = dir.join("rail-pack");
    std::fs::create_dir_all(repo.join("shapes")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let station = |colour: &str| {
        format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
             @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
             <https://example.org/pack/rail#Station> a sh:NodeShape ; sh:targetClass ex:Station ; \
             tts:color \"{colour}\" .\n"
        )
    };
    std::fs::write(repo.join("shapes/Station.ttl"), station("#ff0000")).unwrap();
    std::fs::write(repo.join("README.md"), "not a shape").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    git(&repo, &["tag", "v1.2"]);
    let tagged = git(&repo, &["rev-parse", "HEAD"]);
    // The branch moves on after the tag.
    std::fs::write(repo.join("shapes/Station.ttl"), station("#00ff00")).unwrap();
    git(&repo, &["commit", "-q", "-am", "two"]);

    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "<http://example.com/s1> a <http://example.com/Station> ; \
             <http://www.opengis.net/ont/geosparql#hasGeometry> [ \
             <http://www.opengis.net/ont/geosparql#asWKT> \
             \"POINT(1 1)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> ] ."
                .as_bytes(),
        )
        .unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let url = format!("file://{}", repo.display());
    let put = |body: Value| -> (u16, Value) {
        match s
            .admin("PUT", "/$/datasets/kg/shapes/pack")
            .send_string(&body.to_string())
        {
            Ok(r) => (
                200,
                serde_json::from_str(&r.into_string().unwrap()).unwrap(),
            ),
            Err(e) => (status(Err(e)), Value::Null),
        }
    };
    let read = || {
        let r = ureq::get(&format!("{}/$/datasets/kg/shapes", s.base))
            .call()
            .unwrap();
        let source = r.header("TripleTrail-Shapes").unwrap().to_owned();
        let text = r.into_string().unwrap();
        let colour = ["#ff0000", "#00ff00"]
            .into_iter()
            .find(|c| text.contains(&format!("\"{c}\"")))
            .unwrap_or("generated");
        (source, colour)
    };

    // The tag, not the branch: named after the repository, versioned by the tag.
    let (code, installed) = put(serde_json::json!({
        "git": { "url": url, "tag": "v1.2", "path": "shapes" },
    }));
    assert_eq!(code, 200, "{installed}");
    assert_eq!(
        (installed["name"].as_str(), installed["version"].as_str()),
        (Some("rail-pack"), Some("v1.2"))
    );
    assert_eq!(installed["files"], 1);
    assert_eq!(read(), ("pack rail-pack@v1.2".to_owned(), "#ff0000"));
    let shown: Value = s.get_json("/$/datasets/kg/shapes/pack");
    assert_eq!(shown["source"]["commit"], tagged.as_str());
    assert_eq!(shown["source"]["ref"], "refs/tags/v1.2");
    // The fetched objects stay out of a copy of the shapes.
    assert!(dir.join("data/.shapes/kg/.pack-git").is_dir());

    // A branch, with a name and version given.
    let (code, _) = put(serde_json::json!({
        "git": { "url": url, "ref": "main", "path": "shapes" }, "name": "rail", "version": "next",
    }));
    assert_eq!(code, 200);
    assert_eq!(read(), ("pack rail@next".to_owned(), "#00ff00"));

    // Refused, and the pack stays as it was.
    let refused = [
        serde_json::json!({ "git": { "url": url, "tag": "v1.2", "ref": "main" } }),
        serde_json::json!({ "git": { "url": url, "tag": "v1.2" }, "files": {} }),
        serde_json::json!({ "git": { "url": url, "tag": "v1.2" }, "name": "a b" }),
    ];
    for body in refused {
        assert_eq!(put(body.clone()).0, 400, "{body}");
    }
    assert_eq!(
        put(serde_json::json!({ "git": { "url": url, "tag": "v9" } })).0,
        502
    );
    assert_eq!(
        put(serde_json::json!({ "git": { "url": url, "tag": "v1.2", "path": "nope" } })).0,
        502
    );
    assert_eq!(
        put(serde_json::json!({ "git": { "url": url, "tag": "v1.2", "path": "" } })).0,
        200
    );
    assert_eq!(read(), ("pack rail-pack@v1.2".to_owned(), "#ff0000"));

    s.admin("DELETE", "/$/datasets/kg/shapes/pack")
        .call()
        .unwrap();
    assert!(!dir.join("data/.shapes/kg/.pack-git").exists());
    assert_eq!(read(), ("generated".to_owned(), "generated"));
}

#[cfg(feature = "git")]
#[test]
fn shapes_come_from_a_git_repository() {
    let dir = temp_dir("git-shapes");
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("shapes/more")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let shape = |colour: &str| {
        format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
             @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
             ex:ThingShape a sh:NodeShape ; sh:targetClass ex:Thing ; tts:color \"{colour}\" .\n"
        )
    };
    std::fs::write(repo.join("shapes/thing.ttl"), shape("#ff0000")).unwrap();
    std::fs::write(
        repo.join("shapes/more/other.ttl"),
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n<http://example.com/OtherShape> a sh:NodeShape .\n",
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "not a shape").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let first = git(&repo, &["rev-parse", "HEAD"]);
    let secret = dir.join("secret");
    std::fs::write(&secret, "s3cret\n").unwrap();

    let s = start(&dir, Vec::new());
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "mem")
        .call()
        .unwrap();
    let binding = serde_json::json!({
        "url": format!("file://{}", repo.display()),
        "ref": "main",
        "path": "shapes",
        "webhookSecretFile": secret.to_str().unwrap(),
    });
    let r = s
        .admin("PUT", "/$/datasets/kg/shapes/git")
        .send_string(&binding.to_string())
        .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(v["pull"]["commit"], first.as_str());
    assert_eq!(v["pull"]["files"], 2);
    let shapes_of = || {
        ureq::get(&format!("{}/$/datasets/kg/shapes", s.base))
            .call()
            .unwrap()
            .header("TripleTrail-Shapes")
            .unwrap()
            .to_owned()
    };
    assert_eq!(shapes_of(), format!("git; version=1; commit={first}"));
    let info = s.get_json("/$/datasets/kg/shapes/git");
    assert_eq!(info["bound"], true);
    assert_eq!(info["binding"]["webhook"], true);
    assert_eq!(info["last"]["commit"], first.as_str());

    // The repository is the master: the API does not change the set, and only server admins
    // bind.
    assert_eq!(
        status(
            s.admin("PUT", "/$/datasets/kg/shapes/files/x.ttl")
                .send_string("")
        ),
        409
    );
    assert_eq!(
        status(ureq::post(&format!("{}/$/datasets/kg/shapes/pull", s.base)).call()),
        401
    );

    // A commit that fails the check leaves the shapes as they were.
    std::fs::write(repo.join("shapes/thing.ttl"), shape("red")).unwrap();
    git(&repo, &["commit", "-q", "-am", "two"]);
    let push = br#"{"ref":"refs/heads/main"}"#;
    // A wrong GitHub signature or GitLab token is refused (the HMAC itself is checked
    // against RFC 4231 in a unit test).
    assert_eq!(
        status(
            ureq::post(&format!("{}/$/datasets/kg/shapes/pull", s.base))
                .set("X-Hub-Signature-256", "sha256=00")
                .send_bytes(push)
        ),
        401
    );
    assert_eq!(
        status(
            ureq::post(&format!("{}/$/datasets/kg/shapes/pull", s.base))
                .set("X-Gitlab-Token", "wrong")
                .send_bytes(push)
        ),
        401
    );
    let err = ureq::post(&format!("{}/$/datasets/kg/shapes/pull", s.base))
        .set("X-Gitlab-Token", "s3cret")
        .send_bytes(push)
        .unwrap_err();
    let ureq::Error::Status(422, r) = err else {
        panic!("{err}")
    };
    assert!(r.into_string().unwrap().contains("tts:color"));
    assert_eq!(shapes_of(), format!("git; version=1; commit={first}"));
    assert!(
        s.get_json("/$/datasets/kg/shapes/git")["last"]["error"]
            .as_str()
            .unwrap()
            .contains("not a valid shape set")
    );

    // A fixed one comes in; pulling again changes nothing.
    std::fs::write(repo.join("shapes/thing.ttl"), shape("#00ff00")).unwrap();
    git(&repo, &["commit", "-q", "-am", "three"]);
    let third = git(&repo, &["rev-parse", "HEAD"]);
    let r = ureq::post(&format!("{}/$/datasets/kg/shapes/pull", s.base))
        .set("X-Gitlab-Token", "s3cret")
        .send_bytes(push)
        .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(
        (v["version"].as_u64(), v["changed"].as_bool()),
        (Some(2), Some(true))
    );
    assert_eq!(shapes_of(), format!("git; version=2; commit={third}"));
    let r = s
        .admin("POST", "/$/datasets/kg/shapes/pull")
        .call()
        .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(v["changed"], false);

    // Another path at the same commit is another set.
    let rebind = |path: &str| {
        let mut b = binding.clone();
        b["path"] = path.into();
        let r = s
            .admin("PUT", "/$/datasets/kg/shapes/git")
            .send_string(&b.to_string())
            .unwrap();
        serde_json::from_str::<Value>(&r.into_string().unwrap()).unwrap()
    };
    let v = rebind("shapes/more");
    assert_eq!(
        (v["pull"]["changed"].as_bool(), v["pull"]["files"].as_u64()),
        (Some(true), Some(1))
    );
    assert_eq!(shapes_of(), format!("git; version=3; commit={third}"));
    let v = rebind("shapes");
    assert_eq!(v["pull"]["version"], 4);

    // Credentials in the URL are refused, and a refused binding leaves the status of the one
    // in place.
    let mut with_user = binding.clone();
    with_user["url"] = "https://x-access-token:tok123@example.com/r.git".into();
    assert_eq!(
        status(
            s.admin("PUT", "/$/datasets/kg/shapes/git")
                .send_string(&with_user.to_string())
        ),
        400
    );
    let mut missing = binding.clone();
    missing["path"] = "nowhere".into();
    assert_eq!(
        status(
            s.admin("PUT", "/$/datasets/kg/shapes/git")
                .send_string(&missing.to_string())
        ),
        502
    );
    let info = s.get_json("/$/datasets/kg/shapes/git");
    assert_eq!(info["binding"]["path"], "shapes");
    assert!(info["last"]["error"].is_null(), "{info}");
    assert_eq!(info["last"]["version"], 4);

    // The shapes graph names the commit.
    let r = ureq::get(&format!("{}/$/datasets/kg/shapes/sparql", s.base))
        .query(
            "query",
            "SELECT ?c ?r WHERE { GRAPH <urn:tripletrail:shapes:local> { ?s <http://w3id.org/tripletrail/shapes#commit> ?c ; <http://w3id.org/tripletrail/shapes#source> ?r } }",
        )
        .set("Accept", "application/sparql-results+json")
        .call()
        .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(v["results"]["bindings"][0]["c"]["value"], third.as_str());

    // Unbound, the set is the dataset's own again.
    s.admin("DELETE", "/$/datasets/kg/shapes/git")
        .call()
        .unwrap();
    assert_eq!(s.get_json("/$/datasets/kg/shapes/git")["bound"], false);
    assert_eq!(
        status(
            s.admin("DELETE", "/$/datasets/kg/shapes/files/other.ttl")
                .call()
        ),
        200
    );
    assert_eq!(shapes_of(), "local; version=5");
}

#[cfg(feature = "git")]
#[test]
fn a_git_pull_is_capped_in_bytes() {
    let dir = temp_dir("git-cap");
    let shape = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                 <http://example.com/ThingShape> a sh:NodeShape .\n";
    let repository = |name: &str, extra: &str, data: &[u8]| {
        let repo = dir.join(name);
        std::fs::create_dir_all(repo.join("shapes")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("shapes/thing.ttl"), shape).unwrap();
        std::fs::write(repo.join(extra), data).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "one"]);
        format!("file://{}", repo.display())
    };
    // 3 MiB that do not compress, outside the shapes folder: the fetch itself is too large.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let noise: Vec<u8> = (0..3 << 20)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let large_fetch = repository("noise", "noise.bin", &noise);
    // 2 MiB of shapes that compress well: the fetch is small, the files to read are not.
    let comments = "# a comment line that compresses well\n".repeat(2 << 20 >> 5);
    let large_files = repository("comments", "shapes/comments.ttl", comments.as_bytes());

    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            git_fetch: Some(1 << 20),
            ..Options::default()
        },
    )
    .unwrap();
    let s = Running {
        base: format!("http://{}", server.address()),
    };
    server.spawn();
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "mem")
        .call()
        .unwrap();
    let bind = |url: &str| {
        let binding = serde_json::json!({ "url": url, "ref": "main", "path": "shapes" });
        match s
            .admin("PUT", "/$/datasets/kg/shapes/git")
            .send_string(&binding.to_string())
        {
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(e) => panic!("{e}"),
        }
    };
    let (code, message) = bind(&large_fetch);
    assert_eq!(code, 502, "{message}");
    assert!(message.contains("passed the limit of 1.0 MiB"), "{message}");
    // The interrupted pack is not kept.
    let kept = dir_size(&dir.join("data"));
    assert!(kept < 1 << 20, "{kept} bytes kept");

    let (code, message) = bind(&large_files);
    assert_eq!(code, 502, "{message}");
    assert!(
        message.contains("the files to read take more than 1.0 MiB"),
        "{message}"
    );
    assert_eq!(s.get_json("/$/datasets/kg/shapes/git")["bound"], false);
}

#[cfg(feature = "git")]
fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|e| {
                let t = e.file_type().unwrap();
                if t.is_dir() {
                    dir_size(&e.path())
                } else {
                    e.metadata().map_or(0, |m| m.len())
                }
            })
            .sum()
    })
}

/// A store whose files fail their check (#436) is reported in `/$/ready` and the stats with
/// the file, does not hold readiness back, and refuses writes with 503 naming the file (#440).
#[test]
fn a_damaged_store_is_reported_and_refuses_writes_with_503() {
    let dir = temp_dir("damaged");
    let kg = dir.join("kg");
    {
        let store = tripletrail::store::Store::open(&kg).unwrap();
        store.bulk_load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
        store.checkpoint().unwrap();
    }
    let checkpoint = std::fs::read_dir(&kg)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .unwrap()
        .path()
        .join("dict.bin");
    let mut bytes = std::fs::read(&checkpoint).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x55;
    std::fs::write(&checkpoint, bytes).unwrap();
    // Checked in the background, as a large store is.
    tripletrail::store::Store::verify_in_background_above(0);
    let store = tripletrail::store::Store::open(&kg).unwrap();
    assert!(store.wait_verified().is_err());
    let engine = Engine::from_store(store);
    engine.warm_up();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options::default(),
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let get = |path: &str| -> (u16, Value) {
        let (code, body) = match ureq::get(&format!("{base}{path}")).call() {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
            Err(e) => panic!("{e}"),
        };
        (code, serde_json::from_str(&body).unwrap())
    };
    let (code, ready) = get("/$/ready");
    assert_eq!(code, 200, "{ready}");
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["damaged"], serde_json::json!(["kg"]));
    assert_eq!(ready["datasets"]["kg"]["files"], "damaged");
    let file = ready["datasets"]["kg"]["damaged_file"].as_str().unwrap();
    assert!(file.contains("dict.bin"), "{file}");
    let (_, stats) = get("/$/stats/kg");
    assert_eq!(stats["datasets"]["/kg"]["fileCheck"], "damaged");
    assert!(
        stats["datasets"]["/kg"]["damagedFile"]
            .as_str()
            .unwrap()
            .contains("dict.bin")
    );
    // Queries are answered; writes are refused with 503, naming the file.
    let r = ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK { ?s ?p ?o }")
        .call();
    assert_eq!(status(r), 200);
    match ureq::post(&format!("{base}/kg/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <http://example.com/a> <http://example.com/b> 1 }")
    {
        Err(ureq::Error::Status(503, r)) => {
            let body = r.into_string().unwrap();
            assert!(
                body.contains("dict.bin") && body.contains("damaged"),
                "{body}"
            );
        }
        other => panic!("expected 503, got {:?}", other.map(|r| r.status())),
    }
    match ureq::post(&format!("{base}/kg/data"))
        .set("Content-Type", "text/turtle")
        .send_string("<http://example.com/a> <http://example.com/b> 2 .")
    {
        Err(ureq::Error::Status(503, r)) => {
            let body = r.into_string().unwrap();
            assert!(body.contains("dict.bin"), "{body}");
        }
        other => panic!("expected 503, got {:?}", other.map(|r| r.status())),
    }
}

/// A server that stops without having written anything leaves the checkpoint as it is (#440).
#[test]
fn a_stop_without_writes_writes_no_checkpoint() {
    let dir = temp_dir("stop-unchanged");
    let kg = dir.join("kg");
    {
        let store = tripletrail::store::Store::open(&kg).unwrap();
        store.bulk_load(RdfFormat::Turtle, TTL.as_bytes()).unwrap();
        store.checkpoint().unwrap();
    }
    let checkpoints = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&kg)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("checkpoint-"))
            .collect();
        names.sort();
        names
    };
    let before = checkpoints();
    let run = |write: bool| {
        let engine = Engine::from_store(tripletrail::store::Store::open(&kg).unwrap());
        let server = Server::bind(
            "127.0.0.1:0",
            [("kg".to_owned(), engine)],
            Options::default(),
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        let handle = server.handle();
        let running = std::thread::spawn(move || server.run_until_stopped());
        let r = ureq::get(&format!("{base}/kg/query"))
            .query("query", "ASK { ?s ?p ?o }")
            .call();
        assert_eq!(status(r), 200);
        if write {
            let r = ureq::post(&format!("{base}/kg/update"))
                .set("Content-Type", "application/sparql-update")
                .send_string("INSERT DATA { <http://example.com/a> <http://example.com/b> 1 }");
            assert_eq!(status(r), 200);
        }
        handle.stop();
        running.join().unwrap();
    };
    run(false);
    assert_eq!(checkpoints(), before, "no write, no new checkpoint");
    run(true);
    assert_ne!(checkpoints(), before, "a write is checkpointed at the stop");
}

#[test]
fn a_dataset_keeps_its_rdfs_entailment_when_asked() {
    let dir = temp_dir("entailment");
    let s = start(&dir, Vec::new());
    let ask = |ds: &str, triple: &str| {
        let r = ureq::get(&format!("{}/{ds}/query", s.base))
            .query(
                "query",
                &format!("ASK {{ GRAPH <urn:tripletrail:inferred> {{ {triple} }} }}"),
            )
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["boolean"].as_bool().unwrap()
    };
    let x_is_b = "<http://e/x> a <http://e/B>";
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "mem")
        .query("entailment", "rdfs")
        .call()
        .unwrap();
    assert_eq!(
        s.get_json("/$/datasets/kg")["entailment"],
        serde_json::json!({"regime": "rdfs", "graph": "urn:tripletrail:inferred"})
    );
    let data = "INSERT DATA { <http://e/A> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://e/B> . <http://e/x> a <http://e/A> }";
    assert_eq!(s.update("kg", data), 200);
    assert!(ask("kg", x_is_b));
    // Turned on for a dataset that holds data: entailed once, then kept.
    s.admin("POST", "/$/datasets")
        .query("name", "later")
        .query("type", "mem")
        .call()
        .unwrap();
    assert_eq!(s.update("later", data), 200);
    assert!(!ask("later", x_is_b));
    let r = s
        .admin("POST", "/$/datasets/later/entailment")
        .query("entailment", "rdfs")
        .call()
        .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(v["triples"], 1);
    assert!(ask("later", x_is_b));
    assert_eq!(
        s.update("later", "DELETE DATA { <http://e/x> a <http://e/A> }"),
        200
    );
    assert!(!ask("later", x_is_b));
    // And off.
    s.admin("POST", "/$/datasets/later/entailment")
        .query("entailment", "none")
        .call()
        .unwrap();
    assert_eq!(
        s.get_json("/$/datasets/later")["entailment"]["regime"],
        "none"
    );
    // Other regimes are refused.
    assert_eq!(
        status(
            s.admin("POST", "/$/datasets/later/entailment")
                .query("entailment", "owl")
                .call()
        ),
        400
    );
}

#[test]
fn a_dataset_keeps_its_owl_rl_entailment_when_asked() {
    let dir = temp_dir("entailment-owl");
    let s = start(&dir, Vec::new());
    let ask = |triple: &str| {
        let r = ureq::get(&format!("{}/kg/query", s.base))
            .query(
                "query",
                &format!("ASK {{ GRAPH <urn:tripletrail:inferred> {{ {triple} }} }}"),
            )
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["boolean"].as_bool().unwrap()
    };
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "mem")
        .query("entailment", "owl-rl")
        .call()
        .unwrap();
    assert_eq!(
        s.get_json("/$/datasets/kg")["entailment"],
        serde_json::json!({"regime": "owl-rl", "graph": "urn:tripletrail:inferred"})
    );
    let schema = "INSERT DATA { <http://e/partOf> a <http://www.w3.org/2002/07/owl#TransitiveProperty> ; <http://www.w3.org/2002/07/owl#inverseOf> <http://e/hasPart> }";
    assert_eq!(s.update("kg", schema), 200);
    // Data only: entailed from the added triples.
    let data = "INSERT DATA { <http://e/a> <http://e/partOf> <http://e/b> . <http://e/b> <http://e/partOf> <http://e/c> }";
    assert_eq!(s.update("kg", data), 200);
    assert!(ask("<http://e/a> <http://e/partOf> <http://e/c>"));
    assert!(ask("<http://e/c> <http://e/hasPart> <http://e/a>"));
    // A removal entails again.
    assert_eq!(
        s.update(
            "kg",
            "DELETE DATA { <http://e/b> <http://e/partOf> <http://e/c> }"
        ),
        200
    );
    assert!(!ask("<http://e/a> <http://e/partOf> <http://e/c>"));
    assert!(ask("<http://e/b> <http://e/hasPart> <http://e/a>"));
}

#[test]
fn a_refused_entailment_setting_creates_nothing() {
    let dir = temp_dir("entailment-refused");
    let s = start(&dir, Vec::new());
    for (key, value) in [
        ("entailment", "owl"),
        ("entailmentGraph", "not an iri"),
        ("unionDefaultGraph", "maybe"),
    ] {
        let mut r = s
            .admin("POST", "/$/datasets")
            .query("name", "d1")
            .query("type", "disk")
            .query(key, value);
        if key == "entailmentGraph" {
            r = r.query("entailment", "rdfs");
        }
        assert_eq!(status(r.call()), 400, "{key}={value}");
        assert!(!dir.join("data/d1").exists(), "{key}={value}");
        assert!(!dir.join("data/d1.networks").exists(), "{key}={value}");
    }
    // The corrected request works.
    s.admin("POST", "/$/datasets")
        .query("name", "d1")
        .query("type", "disk")
        .query("entailment", "rdfs")
        .call()
        .unwrap();
}

#[test]
fn moving_the_entailment_to_another_graph_empties_the_old_one() {
    let dir = temp_dir("entailment-move");
    let s = start(&dir, Vec::new());
    let count = |graph: &str| {
        let r = ureq::get(&format!("{}/kg/query", s.base))
            .query(
                "query",
                &format!("SELECT (COUNT(*) AS ?n) {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"),
            )
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["results"]["bindings"][0]["n"]["value"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "mem")
        .query("entailment", "rdfs")
        .query("entailmentGraph", "urn:g:a")
        .call()
        .unwrap();
    assert_eq!(
        s.update(
            "kg",
            "INSERT DATA { <http://e/A> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://e/B> . <http://e/x> a <http://e/A> }"
        ),
        200
    );
    assert_eq!(count("urn:g:a"), 1);
    let r = s
        .admin("POST", "/$/datasets/kg/entailment")
        .query("entailment", "rdfs")
        .query("entailmentGraph", "urn:g:b")
        .call()
        .unwrap();
    let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(v["triples"], 1);
    assert_eq!((count("urn:g:a"), count("urn:g:b")), (0, 1));
    assert_eq!(
        s.update("kg", "DELETE DATA { <http://e/x> a <http://e/A> }"),
        200
    );
    assert_eq!((count("urn:g:a"), count("urn:g:b")), (0, 0));
}

#[test]
fn keeping_the_entailment_in_a_graph_with_data_needs_replace() {
    let dir = temp_dir("entailment-replace");
    let s = start(&dir, Vec::new());
    s.admin("POST", "/$/datasets")
        .query("name", "kg")
        .query("type", "mem")
        .query("entailment", "rdfs")
        .query("entailmentGraph", "urn:g:b")
        .call()
        .unwrap();
    assert_eq!(
        s.update(
            "kg",
            "INSERT DATA { <http://e/A> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://e/B> . GRAPH <urn:g:data> { <http://e/y> a <http://e/A> } }"
        ),
        200
    );
    let set = |graph: &str, replace: bool| {
        let mut r = s
            .admin("POST", "/$/datasets/kg/entailment")
            .query("entailment", "rdfs")
            .query("entailmentGraph", graph);
        if replace {
            r = r.query("replace", "true");
        }
        status(r.call())
    };
    let y_is_a = || {
        let r = ureq::get(&format!("{}/kg/query", s.base))
            .query(
                "query",
                "ASK { GRAPH <urn:g:data> { <http://e/y> a <http://e/A> } }",
            )
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        v["boolean"].as_bool().unwrap()
    };
    // The data stays.
    assert_eq!(set("urn:g:data", false), 409);
    assert!(y_is_a());
    // The graph kept now, an empty one, or with replace=true: allowed.
    assert_eq!(set("urn:g:b", false), 200);
    assert_eq!(set("urn:g:c", false), 200);
    assert_eq!(set("urn:g:data", true), 200);
    assert!(!y_is_a());
}

#[test]
fn a_create_refused_after_its_directory_was_made_leaves_none() {
    let dir = temp_dir("created-cleanup");
    let s = start(&dir, Vec::new());
    std::fs::create_dir_all(dir.join("backups")).unwrap();
    std::fs::write(
        dir.join("backups/kg.nq"),
        "<http://e/y> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://e/A> <urn:g:data> .\n",
    )
    .unwrap();
    let create = |from: &str, replace: bool| {
        let mut r = s
            .admin("POST", "/$/datasets")
            .query("name", "d2")
            .query("type", "disk")
            .query("from", from)
            .query("entailment", "rdfs")
            .query("entailmentGraph", "urn:g:data");
        if replace {
            r = r.query("replace", "true");
        }
        status(r.call())
    };
    // Refused after the restore, and a backup that is not there.
    assert_eq!(create("kg.nq", false), 409);
    assert!(!dir.join("data/d2").exists());
    assert!(!dir.join("data/d2.networks").exists());
    assert_eq!(create("missing.nq", false), 404);
    assert!(!dir.join("data/d2").exists());
    // The retry is not told the dataset exists.
    assert_eq!(create("kg.nq", true), 200);
    assert!(dir.join("data/d2").exists());
}

#[test]
fn concurrent_creates_of_one_disk_dataset_keep_the_winner() {
    let dir = temp_dir("create-race");
    let s = start(&dir, Vec::new());
    for round in 0..10 {
        let name = format!("r{round}");
        let answers: Vec<u16> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    scope.spawn(|| {
                        status(
                            s.admin("POST", "/$/datasets")
                                .query("name", &name)
                                .query("type", "disk")
                                .call(),
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            answers.iter().filter(|&&a| a == 200).count(),
            1,
            "round {round}: {answers:?}"
        );
        assert!(dir.join("data").join(&name).is_dir(), "round {round}");
    }
}

#[test]
fn digests_of_isomorphic_datasets_match() {
    let dir = temp_dir("digest");
    let a = Engine::new_tripletrail();
    a.load(
        RdfFormat::Turtle,
        b"<http://ex/s> <http://ex/geo> [ <http://ex/wkt> \"POINT(1 2)\" ] .".as_slice(),
    )
    .unwrap();
    let b = Engine::new_tripletrail();
    b.load(
        RdfFormat::NTriples,
        b"_:other <http://ex/wkt> \"POINT(1 2)\" .\n<http://ex/s> <http://ex/geo> _:other .\n"
            .as_slice(),
    )
    .unwrap();
    let s = start(&dir, vec![("a".into(), a), ("b".into(), b)]);
    // Without the admin token: refused.
    assert_eq!(
        status(ureq::post(&format!("{}/$/digest/a", s.base)).call()),
        401
    );
    let ta = s.task(s.admin("POST", "/$/digest/a").call().unwrap());
    let tb = s.task(s.admin("POST", "/$/digest/b").call().unwrap());
    assert_eq!(ta["success"], true, "{ta}");
    assert_eq!(ta["task"], "Digest");
    assert_eq!(ta["quads"], 2);
    assert_eq!(ta["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(ta["sha256"], tb["sha256"]);
    assert_eq!(status(s.admin("POST", "/$/digest/nothing").call()), 404);

    // A diff of the same data is empty; after a change it counts it.
    let same = s.task(s.admin("POST", "/$/diff/a?to=b").call().unwrap());
    assert_eq!(same["success"], true, "{same}");
    assert_eq!(same["diff"]["added"], 0);
    assert_eq!(
        s.update("b", "INSERT DATA { <http://ex/s> <http://ex/p> 1 }"),
        200
    );
    let changed = s.task(s.admin("POST", "/$/diff/a?to=b").call().unwrap());
    assert_eq!(changed["task"], "Diff");
    assert_eq!(changed["diff"]["added"], 1, "{changed}");
    assert_eq!(changed["diff"]["removed"], 0);
    assert_eq!(changed["diff"]["predicates"][0]["iri"], "http://ex/p");
    assert_eq!(status(s.admin("POST", "/$/diff/a").call()), 400);
}

/// #148: `?dry=true` says what a pack upgrade would do and installs nothing.
#[test]
fn a_dry_run_says_what_a_pack_upgrade_would_do() {
    let dir = temp_dir("pack-dry-run");
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "@prefix ex: <http://example.com/> .\n\
             ex:s1 a ex:Station ; ex:name \"S1\" .\n\
             ex:o1 a ex:Operator ; ex:name \"O1\" .\n"
                .as_bytes(),
        )
        .unwrap();
    let s = start(&dir, vec![("kg".to_owned(), engine)]);
    let url = |path: &str| format!("{}/$/datasets/kg/shapes{path}", s.base);
    let w = |m: &str, path: &str| s.admin(m, &format!("/$/datasets/kg/shapes{path}"));
    let json =
        |r: ureq::Response| -> Value { serde_json::from_str(&r.into_string().unwrap()).unwrap() };
    let shape = |name: &str, class: &str, colour: &str| {
        format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
             <https://example.org/pack/rail#{name}> a sh:NodeShape ;\n\
               sh:targetClass <http://example.com/{class}> ; tts:color \"{colour}\" .\n"
        )
    };
    let pack = |version: &str, files: Value| {
        serde_json::json!({ "name": "rail", "version": version, "files": files }).to_string()
    };
    let put = |path: &str, body: &str| -> Value {
        json(
            w("PUT", path)
                .set("Content-Type", "application/json")
                .send_string(body)
                .unwrap(),
        )
    };
    put(
        "/pack",
        &pack(
            "1.0",
            serde_json::json!({
                "Station.ttl": shape("Station", "Station", "#ff0000"),
                "Operator.ttl": shape("Operator", "Operator", "#00ff00"),
            }),
        ),
    );
    // The own set changes the pack's operator shape.
    w("PUT", "/files/Operator.ttl")
        .send_string(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
             <https://example.org/pack/rail#Operator> tts:refines true ; sh:name \"Operator\" .\n",
        )
        .unwrap();
    let next = pack(
        "2.0",
        serde_json::json!({
            "Station.ttl": shape("Station", "Station", "#000000"),
            "Depot.ttl": shape("Depot", "Depot", "#0000ff"),
        }),
    );
    // Not without the rights an install needs.
    assert_eq!(
        status(
            ureq::put(&format!("{}?dry=true", url("/pack")))
                .set("Content-Type", "application/json")
                .send_string(&next)
        ),
        401
    );
    let report = put("/pack?dry=true", &next);
    assert_eq!(report["dry"], true, "{report}");
    assert_eq!(report["valid"], true, "{report}");
    assert_eq!(report["replaces"], "rail@1.0");
    assert_eq!(
        report["added"],
        serde_json::json!(["https://example.org/pack/rail#Depot"])
    );
    assert_eq!(
        report["removed"],
        serde_json::json!(["https://example.org/pack/rail#Operator"])
    );
    assert_eq!(
        report["changed"],
        serde_json::json!(["https://example.org/pack/rail#Station"])
    );
    assert_eq!(report["overrides"], serde_json::json!([]));
    assert_eq!(
        report["orphaned"],
        serde_json::json!([{
            "shape": "<https://example.org/pack/rail#Operator>",
            "packShape": "https://example.org/pack/rail#Operator",
            "how": "refines",
        }])
    );
    assert_eq!(
        report["noData"],
        serde_json::json!([{
            "shape": "https://example.org/pack/rail#Depot",
            "class": "http://example.com/Depot",
        }])
    );
    // A `dry` the server does not know is refused, never taken for an install.
    for value in ["yes", "TRUE", "on"] {
        assert_eq!(
            status(
                w("PUT", &format!("/pack?dry={value}"))
                    .set("Content-Type", "application/json")
                    .send_string(&next)
            ),
            400,
            "{value}"
        );
    }
    // Nothing was installed.
    let shown = json(ureq::get(&url("/pack")).call().unwrap());
    assert_eq!(shown["version"], "1.0");
    assert_eq!(shown["files"].as_array().unwrap().len(), 2);
    // A release with errors is reported, not refused: the errors name the layer.
    let report = put(
        "/pack?dry=true",
        &pack(
            "3.0",
            serde_json::json!({"Station.ttl": shape("Station", "Station", "red")}),
        ),
    );
    assert_eq!(report["valid"], false, "{report}");
    assert!(
        report["errors"][0].as_str().unwrap().starts_with("pack: "),
        "{report}"
    );
}

/// #148: `serve --pack` installs a pack from a folder as the dataset opens, once per release.
#[test]
fn a_pack_folder_is_installed_once_per_release() {
    let dir = temp_dir("pack-folder");
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 2,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    let handle = server.handle();
    server.spawn();
    let pack = dir.join("rail");
    std::fs::create_dir_all(&pack).unwrap();
    let station = |colour: &str| {
        format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
             <https://example.org/pack/rail#Station> a sh:NodeShape ;\n\
               sh:targetClass <http://example.com/Station> ; tts:color \"{colour}\" .\n"
        )
    };
    std::fs::write(pack.join("Station.ttl"), station("#ff0000")).unwrap();
    let shown = || -> Value {
        serde_json::from_str(
            &ureq::get(&format!("{base}/$/datasets/kg/shapes/pack"))
                .call()
                .unwrap()
                .into_string()
                .unwrap(),
        )
        .unwrap()
    };
    // Without pack.json: the folder's name, version `local`.
    assert_eq!(
        handle.install_pack("kg", &pack).unwrap(),
        "installed pack rail@local"
    );
    assert_eq!(
        (shown()["name"].clone(), shown()["version"].clone()),
        ("rail".into(), "local".into())
    );
    // The same release again: left as it is.
    assert_eq!(
        handle.install_pack("kg", &pack).unwrap(),
        "pack rail@local is installed"
    );
    // A pack.json that cannot be read is an error, not a pack named after the folder.
    std::fs::create_dir_all(pack.join("pack.json")).unwrap();
    assert!(
        handle
            .install_pack("kg", &pack)
            .unwrap_err()
            .contains("pack.json")
    );
    std::fs::remove_dir(pack.join("pack.json")).unwrap();
    // A new release, named by pack.json, replaces it, with where it comes from.
    std::fs::write(
        pack.join("pack.json"),
        r#"{"name": "rail", "version": "2.0", "source": {"commit": "abc123"}}"#,
    )
    .unwrap();
    std::fs::write(pack.join("Station.ttl"), station("#000000")).unwrap();
    assert_eq!(
        handle.install_pack("kg", &pack).unwrap(),
        "installed pack rail@2.0, in place of rail@local"
    );
    assert_eq!(shown()["version"], "2.0");
    assert_eq!(shown()["source"]["commit"], "abc123");
    // Changed files under the same name and version are installed too.
    std::fs::write(pack.join("Station.ttl"), station("#00ff00")).unwrap();
    assert!(
        handle
            .install_pack("kg", &pack)
            .unwrap()
            .starts_with("installed"),
    );
    // Shapes that do not check, a folder without shapes and an unknown dataset are refused,
    // and leave the installed pack.
    std::fs::write(pack.join("Station.ttl"), station("green")).unwrap();
    assert!(handle.install_pack("kg", &pack).is_err());
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        handle
            .install_pack("kg", &empty)
            .unwrap_err()
            .contains("no .ttl files")
    );
    assert_eq!(
        handle.install_pack("nope", &pack).unwrap_err(),
        "no dataset nope"
    );
    assert_eq!(shown()["version"], "2.0");
}

/// Server branding (#262): `--site-name`, `--site-favicon`, `--site-logo` and `--site-color`
/// carry over to the web UI, the REST API and GraphQL pages, `/$/ui/config.json` and the DCAT
/// catalog; without them the pages are as before.
#[test]
fn branded_pages_carry_the_operators_name_logo_and_colour() {
    let dir = temp_dir("branding");
    let favicon = dir.join("favicon.svg");
    std::fs::write(&favicon, "<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
    let branding = tripletrail_server::branding::Branding::new(
        Some("Rail <KG>"),
        favicon.to_str(),
        Some("https://example.org/logo.png"),
        None,
        Some("#00634f"),
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            branding,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let text = |p: &str| {
        ureq::get(&format!("{base}{p}"))
            .call()
            .unwrap()
            .into_string()
            .unwrap()
    };
    let page = text("/$/ui/");
    assert!(page.contains("<title>Rail &lt;KG&gt;</title>"), "{page}");
    assert!(page.contains("data-site-name=\"Rail &lt;KG&gt;\""));
    assert!(page.contains("<span><b>Rail &lt;KG&gt;</b></span>"));
    assert!(page.contains("<link rel=\"icon\" href=\"/$/ui/brand/favicon\">"));
    assert!(page.contains("<img class=\"logo-img\" src=\"/$/ui/brand/logo\""));
    assert!(page.contains("--yasgui-brand:#00634f"));
    assert!(!page.contains("{{"), "a placeholder is left");
    for p in ["/$/ui/api/kg", "/$/ui/graphql/kg"] {
        let page = text(p);
        assert!(page.contains(">Rail &lt;KG&gt;</a>"), "{p}");
        assert!(page.contains("--brand:#00634f"), "{p}");
        assert!(!page.contains("{{"), "{p}");
    }
    let favicon = ureq::get(&format!("{base}/$/ui/brand/favicon"))
        .call()
        .unwrap();
    assert_eq!(favicon.content_type(), "image/svg+xml");
    assert!(
        favicon
            .header("Content-Security-Policy")
            .unwrap()
            .contains("sandbox")
    );
    let r = ureq::AgentBuilder::new().redirects(0).build();
    let logo = r.get(&format!("{base}/$/ui/brand/logo")).call().unwrap();
    assert_eq!(logo.status(), 302);
    assert_eq!(
        logo.header("Location"),
        Some("https://example.org/logo.png")
    );
    assert_eq!(
        status(ureq::get(&format!("{base}/$/ui/brand/logo-dark")).call()),
        404
    );
    let config: Value = serde_json::from_str(&text("/$/ui/config.json")).unwrap();
    assert_eq!(config["site"]["name"], "Rail <KG>");
    assert_eq!(config["site"]["color"], "#00634f");
    assert_eq!(config["site"]["logo"], "/$/ui/brand/logo");
    let catalog = text("/.well-known/dcat");
    assert!(catalog.contains("\"Rail <KG>\""), "{catalog}");

    // Without branding: TripleTrail's name, mark and colours.
    let server = Server::bind("127.0.0.1:0", Vec::new(), Options::default()).unwrap();
    let plain = format!("http://{}", server.address());
    server.spawn();
    let page = ureq::get(&format!("{plain}/$/ui/"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(page.contains("<title>TripleTrail</title>"));
    assert!(page.contains("<b>TripleTrail</b> server"));
    assert!(page.contains("<span class=\"logo\" aria-hidden=\"true\"></span>"));
    assert!(page.contains("<link rel=\"icon\" href=\"/$/ui/icon.svg\">"));
    assert!(!page.contains("--yasgui-brand:"));
    let favicon = ureq::get(&format!("{plain}/$/ui/brand/favicon"))
        .call()
        .unwrap();
    assert_eq!(favicon.content_type(), "image/svg+xml");
}
