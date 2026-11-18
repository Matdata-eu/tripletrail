//! Read replicas by shipping the write-ahead log (#217): a follower copies a base of the
//! leader's dataset, applies each transaction the leader commits, refuses writes, reports its
//! lag, and copies a new base when the leader's dataset is another one.
#![cfg(feature = "replication")]

use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};
use tripletrail_server::{Follow, Options, Server};

const TOKEN: &str = "s3cret";

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("replication-{name}"))
}

fn start(location: &Path) -> (String, Running) {
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(location.to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    (base, Running::new(server))
}

/// A server running on a thread of its own, stopped when dropped (also when the test fails):
/// its followers stop copying and applying the log, so that nothing writes into the test's
/// folder while it is removed. Declared after the folder, it is dropped first.
struct Running {
    handle: tripletrail_server::Handle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn new(server: Server) -> Self {
        let handle = server.handle();
        let thread = std::thread::spawn(move || server.run_until_stopped());
        Running {
            handle,
            thread: Some(thread),
        }
    }
}

impl std::ops::Deref for Running {
    type Target = tripletrail_server::Handle;

    fn deref(&self) -> &tripletrail_server::Handle {
        &self.handle
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.handle.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn admin(method: &str, url: &str) -> ureq::Request {
    ureq::request(method, url).set("Authorization", &format!("Bearer {TOKEN}"))
}

fn count(base: &str, ds: &str) -> u64 {
    let r = ureq::get(&format!("{base}/{ds}/query"))
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

fn update(base: &str, ds: &str, update: &str) -> u16 {
    match ureq::post(&format!("{base}/{ds}/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(update)
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

fn create(base: &str, name: &str) {
    let r = admin("POST", &format!("{base}/$/datasets"))
        .send_form(&[("dbName", name), ("dbType", "disk")])
        .unwrap();
    assert_eq!(r.status(), 200);
}

fn insert(base: &str, ds: &str, from: usize, to: usize) {
    let triples: String = (from..to)
        .map(|i| format!("<http://example.com/s{i}> <http://example.com/p> {i} .\n"))
        .collect();
    assert_eq!(
        update(base, ds, &format!("INSERT DATA {{ {triples} }}")),
        200
    );
}

/// Waits until the follower's dataset holds `n` triples.
fn wait_for(base: &str, ds: &str, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let got = ureq::get(&format!("{base}/{ds}/query"))
            .query("query", "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }")
            .call()
            .ok()
            .and_then(|r| serde_json::from_str::<Value>(&r.into_string().ok()?).ok())
            .and_then(|v| {
                v["results"]["bindings"][0]["n"]["value"]
                    .as_str()?
                    .parse()
                    .ok()
            });
        if got == Some(n) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the follower holds {got:?} triples, not {n}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn status(base: &str) -> Value {
    serde_json::from_str(
        &ureq::get(&format!("{base}/$/status"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap()
}

/// An `ASK {}` on `base`'s replica, with a `TripleTrail-Min-Version`: its status and time.
fn ask(base: &str, ds: &str, version: Option<&str>) -> (u16, Duration) {
    let started = Instant::now();
    let mut r = ureq::get(&format!("{base}/{ds}/query")).query("query", "ASK {}");
    if let Some(v) = version {
        r = r.set("TripleTrail-Min-Version", v);
    }
    let status = match r.call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    };
    (status, started.elapsed())
}

#[test]
fn a_follower_keeps_up_with_its_leader() {
    let dir = temp_dir("follow");
    let (leader, _leader_handle) = start(&dir.join("leader"));
    create(&leader, "ds");
    insert(&leader, "ds", 0, 50);

    // Only the admin reads the log.
    let refused = ureq::get(&format!("{leader}/$/replication/ds")).call();
    assert!(matches!(refused, Err(ureq::Error::Status(401, _))));
    let position: Value = serde_json::from_str(
        &admin("GET", &format!("{leader}/$/replication/ds"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert!(position["length"].as_u64().unwrap() > 0, "{position}");

    let follower_dir = dir.join("follower");
    let (follower, handle) = start(&follower_dir);
    let follow = Follow::parse(
        &format!("rep={leader}/ds"),
        Some(TOKEN.to_owned()),
        Duration::from_millis(200),
    )
    .unwrap();
    tripletrail_server::follow(&handle, &follower_dir, follow).unwrap();
    wait_for(&follower, "rep", 50);
    assert_eq!(count(&follower, "rep"), 50);

    // Commits reach the follower, deletes too.
    insert(&leader, "ds", 50, 80);
    assert_eq!(
        update(
            &leader,
            "ds",
            "DELETE WHERE { <http://example.com/s3> ?p ?o }"
        ),
        200
    );
    wait_for(&follower, "rep", 79);
    // A burst of writes the follower applies in one commit: a triple added and taken away
    // again in later transactions of the same read is gone on the follower too.
    for i in 0..20 {
        let q = format!("INSERT DATA {{ <http://example.com/burst> <http://example.com/p> {i} }}");
        assert_eq!(update(&leader, "ds", &q), 200);
        let q = format!("DELETE DATA {{ <http://example.com/burst> <http://example.com/p> {i} }}");
        assert_eq!(update(&leader, "ds", &q), 200);
    }
    let last = "<http://example.com/burst> <http://example.com/p> 20";
    assert_eq!(
        update(&leader, "ds", &format!("INSERT DATA {{ {last} }}")),
        200
    );
    wait_for(&follower, "rep", 80);
    assert_eq!(count(&follower, "rep"), 80);
    assert_eq!(
        update(&leader, "ds", &format!("DELETE DATA {{ {last} }}")),
        200
    );
    wait_for(&follower, "rep", 79);
    // Across checkpoints of the leader (a detach checkpoints; attached again, it goes on
    // with the next log): the follower reads the retired log to its end, then the next one,
    // without copying a new base.
    let leader_ds = dir.join("leader").join("ds");
    for round in 0..3 {
        let from = 100 + round * 10;
        insert(&leader, "ds", from, from + 10);
        let r = admin("DELETE", &format!("{leader}/$/datasets/ds"))
            .call()
            .unwrap();
        assert_eq!(r.status(), 200);
        let r = admin("POST", &format!("{leader}/$/datasets"))
            .query("name", "ds")
            .query("location", leader_ds.to_str().unwrap())
            .call()
            .unwrap();
        assert_eq!(r.status(), 200);
        // With --keep-logs 1, a replica more than one checkpoint behind copies a new base.
        wait_for(&follower, "rep", 79 + 10 * (round as u64 + 1));
    }
    insert(&leader, "ds", 200, 210);
    wait_for(&follower, "rep", 119);

    // Read your writes: the leader's TripleTrail-Version after a write, sent to the follower as
    // TripleTrail-Min-Version, makes the read wait for that write.
    for i in 0..5 {
        let r = ureq::post(&format!("{leader}/ds/update"))
            .set("Content-Type", "application/sparql-update")
            .send_string(&format!(
                "INSERT DATA {{ <http://example.com/ryw{i}> <http://example.com/p> {i} }}"
            ))
            .unwrap();
        let version = r
            .header("TripleTrail-Version")
            .expect("a version")
            .to_owned();
        let r = ureq::get(&format!("{follower}/rep/query"))
            .set("TripleTrail-Min-Version", &version)
            .query(
                "query",
                &format!("ASK {{ <http://example.com/ryw{i}> ?p ?o }}"),
            )
            .set("Accept", "application/sparql-results+json")
            .call()
            .unwrap();
        let v: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
        assert_eq!(v["boolean"], true, "write {i} at {version}");
    }
    let bad = ureq::get(&format!("{follower}/rep/query"))
        .set("TripleTrail-Min-Version", "nonsense")
        .query("query", "ASK {}")
        .call();
    assert!(matches!(bad, Err(ureq::Error::Status(400, _))));
    wait_for(&follower, "rep", 124);

    // Waiting requests cannot hold every worker: past half of them, a wait answers 503 at once,
    // and a plain read is answered meanwhile. A version of another timeline, or beyond the
    // leader's log, answers at once too.
    // The cap is the server's, over all its replicas: a second one does not get its own.
    create(&leader, "ds2");
    insert(&leader, "ds2", 0, 3);
    let follow2 = Follow::parse(
        &format!("rep2={leader}/ds2"),
        Some(TOKEN.to_owned()),
        Duration::from_millis(200),
    )
    .unwrap();
    tripletrail_server::follow(&handle, &follower_dir, follow2).unwrap();
    wait_for(&follower, "rep2", 3);
    let timeline = position["timeline"].as_str().unwrap().to_owned();
    let replica: Value = serde_json::from_str(
        &admin("GET", &format!("{leader}/$/replication/ds2"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    let timeline2 = replica["timeline"].as_str().unwrap().to_owned();
    let waiters: Vec<_> = (0..8)
        .map(|i| {
            let (ds, v) = if i % 2 == 0 {
                ("rep", format!("{timeline}:999:0"))
            } else {
                ("rep2", format!("{timeline2}:999:0"))
            };
            let base = follower.clone();
            std::thread::spawn(move || ask(&base, ds, Some(&v)))
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let (plain, took) = ask(&follower, "rep", None);
    assert_eq!(plain, 200);
    assert!(took < Duration::from_secs(2), "a plain read took {took:?}");
    let answers: Vec<(u16, Duration)> = waiters.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(answers.iter().all(|(s, _)| *s == 503), "{answers:?}");
    let quick = answers
        .iter()
        .filter(|(_, t)| *t < Duration::from_secs(2))
        .count();
    assert!(quick >= 6, "{answers:?}");
    let (other, took) = ask(&follower, "rep", Some("another:0:0"));
    assert_eq!(other, 503);
    assert!(took < Duration::from_secs(2), "{took:?}");

    // The follower refuses writes of its own.
    assert_eq!(
        update(&follower, "rep", "INSERT DATA { <x:a> <x:b> <x:c> }"),
        405
    );

    let s = status(&follower);
    let replica = &s["datasets"]["rep"]["replica"];
    assert_eq!(replica["state"], "following", "{s}");
    assert_eq!(replica["bases"], 1, "{s}");
    assert_eq!(replica["leader"], format!("{leader}/ds"));
    let described: Value = serde_json::from_str(
        &ureq::get(&format!("{follower}/$/datasets/rep"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(described["mode"], "read-only", "{described}");
    assert_eq!(described["replica"]["state"], "following", "{described}");

    // The leader's dataset deleted and created again is another timeline: a new base.
    let r = admin("DELETE", &format!("{leader}/$/datasets/ds"))
        .query("delete-files", "true")
        .call()
        .unwrap();
    assert_eq!(r.status(), 200);
    create(&leader, "ds");
    insert(&leader, "ds", 0, 7);
    wait_for(&follower, "rep", 7);
    let s = status(&follower);
    assert_eq!(s["datasets"]["rep"]["replica"]["bases"], 2, "{s}");
    insert(&leader, "ds", 7, 9);
    wait_for(&follower, "rep", 9);

    // A position file says where a restart goes on from.
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(follower_dir.join("rep.replica")).unwrap())
            .unwrap();
    assert_eq!(saved["dir"], "rep.r2", "{saved}");
    assert!(follower_dir.join("rep.r2").join("CURRENT").is_file());
}

/// A follower started before its leader's dataset exists (both started together, or the
/// leader down) keeps retrying, unready, and serves its other datasets meanwhile.
#[test]
fn a_follower_waits_for_its_leader() {
    let dir = temp_dir("early");
    let (leader, _leader_handle) = start(&dir.join("leader"));
    let follower_dir = dir.join("follower");
    let (follower, handle) = start(&follower_dir);
    create(&follower, "own");
    let follow = Follow::parse(
        &format!("rep={leader}/ds"),
        Some(TOKEN.to_owned()),
        Duration::from_millis(200),
    )
    .unwrap();
    tripletrail_server::follow(&handle, &follower_dir, follow).unwrap();
    let ready = |base: &str| match ureq::get(&format!("{base}/$/ready")).call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    };
    // The leader has no dataset ds yet: 404 to the follower, which retries.
    std::thread::sleep(Duration::from_millis(1500));
    let unready: Value = match ureq::get(&format!("{follower}/$/ready")).call() {
        Err(ureq::Error::Status(503, r)) => {
            serde_json::from_str(&r.into_string().unwrap()).unwrap()
        }
        other => panic!("{:?}", other.map(|r| r.status())),
    };
    assert_eq!(unready["copying"], serde_json::json!(["rep"]), "{unready}");
    assert_eq!(ready(&follower), 503);
    assert_eq!(count(&follower, "own"), 0);
    create(&leader, "ds");
    insert(&leader, "ds", 0, 5);
    wait_for(&follower, "rep", 5);
    // Ready once the copy is in place and its spatial index built.
    let deadline = Instant::now() + Duration::from_secs(30);
    while ready(&follower) != 200 {
        assert!(Instant::now() < deadline, "{}", status(&follower));
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn follow_takes_a_dataset_url() {
    let wait = Duration::from_secs(1);
    let mut f = Follow::parse("rep=http://leader:3030/ds/", None, wait).unwrap();
    assert_eq!(f.name, "rep");
    assert_eq!(f.url(), "http://leader:3030/ds");
    f.add_url("http://standby:3030/ds").unwrap();
    f.add_url("http://leader:3030/ds/").unwrap();
    assert_eq!(f.urls, ["http://leader:3030/ds", "http://standby:3030/ds"]);
    assert!(f.add_url("standby:3030").is_err());
    for bad in ["rep", "=http://x/ds", "rep=leader:3030", "rep=ftp://x/ds"] {
        assert!(Follow::parse(bad, None, wait).is_err(), "{bad}");
    }
}

#[test]
fn a_write_waits_for_its_synchronous_replicas() {
    let dir = temp_dir("sync");
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("leader")),
            sync_replicas: 1,
            sync_timeout: Duration::from_millis(800),
            ..Options::default()
        },
    )
    .unwrap();
    let leader = format!("http://{}", server.address());
    server.spawn();
    create(&leader, "ds");
    let write = |i: usize| {
        let t = Instant::now();
        let r = ureq::post(&format!("{leader}/ds/update"))
            .set("Content-Type", "application/sparql-update")
            .send_string(&format!(
                "INSERT DATA {{ <http://example.com/s{i}> <http://example.com/p> {i} }}"
            ))
            .unwrap();
        let held = r.header("TripleTrail-Replicas").map(str::to_owned);
        (held, t.elapsed())
    };

    // No replica yet: the write waits out the timeout, then answers with none holding it.
    let (held, took) = write(0);
    assert_eq!(held.as_deref(), Some("0"));
    assert!(took >= Duration::from_millis(800), "{took:?}");

    let follower_dir = dir.join("follower");
    let (follower, handle) = start(&follower_dir);
    // A replica whose reads of the log wait long for a commit: it holds a write as soon as
    // it asks for the log past it, which is before that read waits.
    let follow = Follow::parse(
        &format!("rep={leader}/ds"),
        Some(TOKEN.to_owned()),
        Duration::from_secs(5),
    )
    .unwrap();
    tripletrail_server::follow(&handle, &follower_dir, follow).unwrap();
    wait_for(&follower, "rep", 1);

    // With a replica: each write answers once the replica holds it, so a plain read of the
    // replica right after sees it, without TripleTrail-Min-Version.
    for i in 1..=10 {
        let (held, took) = write(i);
        assert_eq!(held.as_deref(), Some("1"), "write {i}");
        assert!(took < Duration::from_millis(800), "write {i}: {took:?}");
        assert_eq!(count(&follower, "rep"), i as u64 + 1, "write {i}");
    }
}

/// A leader on disk with `threads` workers whose writes wait for one replica.
fn sync_leader(dir: &Path, threads: usize, timeout: Duration) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("leader")),
            sync_replicas: 1,
            sync_timeout: timeout,
            ..Options::default()
        },
    )
    .unwrap();
    let leader = format!("http://{}", server.address());
    server.spawn();
    create(&leader, "ds");
    leader
}

/// A write of one triple: the replicas holding it (the `TripleTrail-Replicas` header) and how
/// long it took.
fn sync_write(leader: &str, i: usize) -> (String, Duration) {
    let t = Instant::now();
    let r = ureq::post(&format!("{leader}/ds/update"))
        .set("Content-Type", "application/sparql-update")
        .send_string(&format!(
            "INSERT DATA {{ <http://example.com/s{i}> <http://example.com/p> {i} }}"
        ))
        .unwrap();
    let held = r.header("TripleTrail-Replicas").unwrap_or("").to_owned();
    (held, t.elapsed())
}

/// A follower of `leader`'s `ds` (as `rep`) in a server of its own that the test can stop.
fn sync_follower(dir: &Path, leader: &str) -> (String, Running) {
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    let handle = Running::new(server);
    let follow = Follow::parse(
        &format!("rep={leader}/ds"),
        Some(TOKEN.to_owned()),
        Duration::from_millis(200),
    )
    .unwrap();
    tripletrail_server::follow(&handle, dir, follow).unwrap();
    (base, handle)
}

fn sync_status(leader: &str) -> Value {
    serde_json::from_str(
        &admin("GET", &format!("{leader}/$/replication/ds"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn more_writers_than_threads_still_reach_their_replica() {
    // Writes waiting for the replica must not hold the workers that answer its reads of the
    // log: with every worker held, the replica could not ask past them, and each write would
    // wait out the timeout.
    let dir = temp_dir("sync-writers");
    let leader = sync_leader(&dir, 2, Duration::from_secs(5));
    let (follower, _handle) = sync_follower(&dir.join("follower"), &leader);
    sync_write(&leader, 0);
    wait_for(&follower, "rep", 1);
    let started = Instant::now();
    let writers: Vec<_> = (0..8)
        .map(|w| {
            let leader = leader.clone();
            std::thread::spawn(move || {
                (0..10)
                    .map(|i| sync_write(&leader, 1 + w * 10 + i))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for writer in writers {
        for (held, took) in writer.join().unwrap() {
            assert_eq!(held, "1");
            assert!(took < Duration::from_secs(5), "{took:?}");
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    wait_for(&follower, "rep", 81);
}

#[test]
fn writes_stop_waiting_for_a_replica_that_is_down_until_one_catches_up() {
    let dir = temp_dir("sync-down");
    let timeout = Duration::from_millis(600);
    let leader = sync_leader(&dir, 4, timeout);
    let (follower, handle) = sync_follower(&dir.join("first"), &leader);
    let (held, _) = sync_write(&leader, 0);
    wait_for(&follower, "rep", 1);
    // The first write may have waited out the timeout before the replica asked for the log;
    // once it holds the log, writes wait for it again.
    let (held1, took) = sync_write(&leader, 1);
    assert_eq!(held1, "1", "first write held by {held}");
    assert!(took < timeout, "{took:?}");
    assert_eq!(sync_status(&leader)["sync"]["waiting"], true);

    // The replica goes down: one write waits out the timeout, and the ones after answer at
    // once, held by none.
    handle.stop();
    std::thread::sleep(Duration::from_millis(500));
    let (held, took) = sync_write(&leader, 2);
    assert_eq!(held, "0");
    assert!(took >= timeout, "{took:?}");
    assert_eq!(sync_status(&leader)["sync"]["waiting"], false);
    for i in 3..6 {
        let (held, took) = sync_write(&leader, i);
        assert_eq!(held, "0", "write {i}");
        assert!(took < timeout / 2, "write {i}: {took:?}");
    }

    // A replica catches up: it holds the last write that answered, so writes wait again.
    let (follower, _handle) = sync_follower(&dir.join("second"), &leader);
    wait_for(&follower, "rep", 6);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut i = 6;
    loop {
        let (held, took) = sync_write(&leader, i);
        i += 1;
        if held == "1" {
            assert!(took < timeout, "{took:?}");
            break;
        }
        assert!(Instant::now() < deadline, "writes never waited again");
    }
    assert_eq!(sync_status(&leader)["sync"]["waiting"], true);
    let (held, took) = sync_write(&leader, i);
    assert_eq!(held, "1");
    assert!(took < timeout, "{took:?}");
}

/// A server on disk at `dir` that the test can stop, with its base URL and handle.
fn stoppable(dir: &Path) -> (String, tripletrail_server::Handle) {
    let server = Server::bind(
        "127.0.0.1:0",
        Vec::new(),
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    let handle = server.handle();
    std::thread::spawn(move || server.run_until_stopped());
    (base, handle)
}

/// `GET /$/role/{name}`: its status and body.
fn role(base: &str, name: &str) -> (u16, String) {
    match ureq::get(&format!("{base}/$/role/{name}")).call() {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn a_promoted_replica_takes_writes_and_the_other_replicas_follow_it() {
    let dir = temp_dir("promote");
    let (leader, leader_handle) = stoppable(&dir.join("leader"));
    create(&leader, "ds");
    assert_eq!(role(&leader, "ds"), (200, "leader\n".to_owned()));
    let wait = Duration::from_millis(200);
    // A standby that follows the leader, and a replica that knows both.
    let (standby, standby_handle) = start(&dir.join("standby"));
    let follow = Follow::parse(&format!("rep={leader}/ds"), Some(TOKEN.to_owned()), wait).unwrap();
    tripletrail_server::follow(&standby_handle, &dir.join("standby"), follow).unwrap();
    let (replica, replica_handle) = start(&dir.join("replica"));
    let mut follow =
        Follow::parse(&format!("rep={leader}/ds"), Some(TOKEN.to_owned()), wait).unwrap();
    follow.add_url(&format!("{standby}/rep")).unwrap();
    tripletrail_server::follow(&replica_handle, &dir.join("replica"), follow).unwrap();
    insert(&leader, "ds", 0, 10);
    wait_for(&standby, "rep", 10);
    wait_for(&replica, "rep", 10);
    assert_eq!(role(&standby, "rep"), (503, "follower\n".to_owned()));
    assert_eq!(
        update(&standby, "rep", "INSERT DATA { <x:a> <x:b> 1 }"),
        405
    );
    assert_eq!(role(&leader, "nope").0, 404);

    // The leader goes down (here it stops answering, as a machine that is down does); the
    // standby is promoted.
    leader_handle.stop();
    std::thread::sleep(Duration::from_millis(500));
    let r = admin("POST", &format!("{standby}/$/replication/rep/promote"))
        .call()
        .unwrap();
    let promoted: Value = serde_json::from_str(&r.into_string().unwrap()).unwrap();
    assert_eq!(
        promoted["promotion"]["generation"].as_u64(),
        Some(0),
        "{promoted}"
    );
    assert_ne!(
        promoted["timeline"], promoted["promotion"]["parent"],
        "{promoted}"
    );
    assert_eq!(role(&standby, "rep"), (200, "leader\n".to_owned()));
    // Promoting twice: it is no replica now.
    let again = admin("POST", &format!("{standby}/$/replication/rep/promote")).call();
    assert!(matches!(again, Err(ureq::Error::Status(409, _))));
    let v: Value = serde_json::from_str(
        &admin("GET", &format!("{standby}/$/replication/rep"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["role"], "leader", "{v}");
    assert_eq!(v["history"][0]["timeline"], promoted["timeline"], "{v}");

    // It takes writes, and the replica goes on from it without copying a new base.
    insert(&standby, "rep", 10, 15);
    wait_for(&replica, "rep", 15);
    let s = status(&replica);
    let r = &s["datasets"]["rep"]["replica"];
    assert_eq!(r["bases"], 1, "{s}");
    assert_eq!(r["leader"], format!("{standby}/rep"), "{s}");
    assert_eq!(r["timeline"], promoted["timeline"], "{s}");
    assert_eq!(role(&replica, "rep"), (503, "follower\n".to_owned()));

    // A new replica given both URLs copies its base from the one that is the leader now.
    let (late, late_handle) = start(&dir.join("late"));
    let mut follow =
        Follow::parse(&format!("rep={leader}/ds"), Some(TOKEN.to_owned()), wait).unwrap();
    follow.add_url(&format!("{standby}/rep")).unwrap();
    tripletrail_server::follow(&late_handle, &dir.join("late"), follow).unwrap();
    wait_for(&late, "rep", 15);
    insert(&standby, "rep", 15, 16);
    wait_for(&late, "rep", 16);
    wait_for(&replica, "rep", 16);
}

/// A port free a moment ago.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `tripletrail serve` on `port` with `args`, until dropped.
fn serve_process(port: u16, args: &[&str]) -> Process {
    Process(
        std::process::Command::new(env!("CARGO_BIN_EXE_tripletrail"))
            .args(["serve", "--bind", &format!("127.0.0.1:{port}")])
            .args(["--admin-token", TOKEN])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    )
}

/// A server process, killed (and waited for) when dropped: also when the test fails, so that
/// it neither outlives the test nor writes into its folder while the folder is removed.
struct Process(std::process::Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ready(base: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while ureq::get(&format!("{base}/$/ready")).call().is_err() {
        assert!(Instant::now() < deadline, "{base} not ready");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_promoted_replica_restarts_as_the_leader_with_or_without_its_follow() {
    let dir = temp_dir("promoted-restart");
    let (leader, _leader_handle) = start(&dir.join("leader"));
    create(&leader, "ds");
    insert(&leader, "ds", 0, 5);
    let location = dir.join("standby");
    let location = location.to_str().unwrap();
    let port = free_port();
    let standby = format!("http://127.0.0.1:{port}");
    let follow = format!("rep={leader}/ds");
    let with_follow = [
        "--location",
        location,
        "--follow",
        &follow,
        "--follow-token",
        TOKEN,
    ];
    let process = serve_process(port, &with_follow);
    ready(&standby);
    wait_for(&standby, "rep", 5);
    admin("POST", &format!("{standby}/$/replication/rep/promote"))
        .call()
        .unwrap();
    insert(&standby, "rep", 5, 6);
    drop(process);

    // Without its --follow: the promoted copy is served, writable.
    let process = serve_process(port, &["--location", location]);
    ready(&standby);
    assert_eq!(role(&standby, "rep"), (200, "leader\n".to_owned()));
    assert_eq!(count(&standby, "rep"), 6);
    // And no empty default dataset next to it.
    assert!(!Path::new(location).join("ds").exists());
    insert(&standby, "rep", 6, 7);
    drop(process);

    // With it, the same.
    let process = serve_process(port, &with_follow);
    ready(&standby);
    assert_eq!(role(&standby, "rep"), (200, "leader\n".to_owned()));
    assert_eq!(count(&standby, "rep"), 7);
    drop(process);

    // A --dataset of its name would serve another directory: refused.
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .args(["serve", "--bind", &format!("127.0.0.1:{port}")])
        .args(["--location", location, "--dataset", "rep"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("promoted"), "{stderr}");
}

/// A proxy in front of `target` that answers 500 to reads of the log and passes on the rest
/// (GETs only); returns its base URL and the count of log reads it refused.
fn failing_log_proxy(target: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    log_proxy(target, None)
}

/// A proxy to `target` that answers every log read with a 500 (`format` `None`), or passes it
/// on marked as a log of store format `format`. Counts the log reads.
fn log_proxy(
    target: String,
    format: Option<u64>,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let refused = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = refused.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                    break;
                }
            }
            let mut out = stream;
            if path.contains("/log?")
                && let Some(format) = format
            {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let r = admin("GET", &format!("{target}{path}")).call().unwrap();
                let end = r.header("X-TripleTrail-Log-End").unwrap().to_owned();
                let retired = r.header("X-TripleTrail-Log-Retired").unwrap().to_owned();
                let mut body = Vec::new();
                std::io::Read::read_to_end(&mut r.into_reader(), &mut body).unwrap();
                let _ = write!(
                    out,
                    "HTTP/1.1 200 OK\r\nX-TripleTrail-Log-End: {end}\r\nX-TripleTrail-Log-Retired: {retired}\r\nX-TripleTrail-Store-Format: {format}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = out.write_all(&body);
                continue;
            }
            if path.contains("/log?") {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = out.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail",
                );
                continue;
            }
            let mut body = Vec::new();
            let (status, kind) = match admin("GET", &format!("{target}{path}")).call() {
                Ok(r) => {
                    let kind = r.content_type().to_owned();
                    std::io::Read::read_to_end(&mut r.into_reader(), &mut body).unwrap();
                    (200, kind)
                }
                Err(ureq::Error::Status(code, r)) => (
                    code,
                    r.into_string()
                        .map_or(String::new(), |_| "text/plain".to_owned()),
                ),
                Err(_) => (502, "text/plain".to_owned()),
            };
            let _ = write!(
                out,
                "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = out.write_all(&body);
        }
    });
    (base, refused)
}

#[test]
fn a_failing_log_read_backs_off_with_more_leader_urls() {
    // The URL read from answers its status but fails each read of the log; the other URL is
    // down. The follower must keep the error and back off, not ask again at once.
    let dir = temp_dir("failing-log");
    let (leader, _leader_handle) = start(&dir.join("leader"));
    create(&leader, "ds");
    insert(&leader, "ds", 0, 3);
    let (proxy, refused) = failing_log_proxy(leader.clone());
    let (follower, handle) = start(&dir.join("follower"));
    let mut follow = Follow::parse(
        &format!("rep={proxy}/ds"),
        Some(TOKEN.to_owned()),
        Duration::from_millis(200),
    )
    .unwrap();
    follow
        .add_url(&format!("http://127.0.0.1:{}/ds", free_port()))
        .unwrap();
    tripletrail_server::follow(&handle, &dir.join("follower"), follow).unwrap();
    wait_for(&follower, "rep", 3);
    std::thread::sleep(Duration::from_secs(3));
    let reads = refused.load(std::sync::atomic::Ordering::SeqCst);
    assert!((1..10).contains(&reads), "{reads} log reads in 3 s");
    let s = status(&follower);
    assert_eq!(s["datasets"]["rep"]["replica"]["state"], "error", "{s}");
}

#[test]
fn a_replica_does_not_apply_the_log_of_a_leader_of_a_newer_store_format() {
    // The leader says its log is in store format 99 (#221): the replica keeps what it has,
    // applies nothing of the newer log, and says why.
    let dir = temp_dir("newer-leader");
    let (leader, _leader_handle) = start(&dir.join("leader"));
    create(&leader, "ds");
    insert(&leader, "ds", 0, 3);
    let (proxy, reads) = log_proxy(leader.clone(), Some(99));
    let (follower, handle) = start(&dir.join("follower"));
    let follow = Follow::parse(
        &format!("rep={proxy}/ds"),
        Some(TOKEN.to_owned()),
        Duration::from_millis(200),
    )
    .unwrap();
    tripletrail_server::follow(&handle, &dir.join("follower"), follow).unwrap();
    // The base holds the first three triples.
    wait_for(&follower, "rep", 3);
    insert(&leader, "ds", 3, 6);
    let deadline = Instant::now() + Duration::from_secs(10);
    while reads.load(std::sync::atomic::Ordering::SeqCst) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(count(&follower, "rep"), 3);
    let s = status(&follower);
    let replica = &s["datasets"]["rep"]["replica"];
    assert_eq!(replica["state"], "error", "{s}");
    let error = replica["error"].as_str().unwrap();
    assert!(
        error.contains("store format 99") && error.contains("newer TripleTrail"),
        "{error}"
    );
}

/// The value of the sample `name{dataset="…"}` in `/$/metrics` of `base`.
fn metric(base: &str, name: &str, dataset: &str) -> Option<f64> {
    let text = ureq::get(&format!("{base}/$/metrics"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    let prefix = format!("{name}{{dataset=\"{dataset}\"}} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.parse().unwrap())
}

#[test]
fn metrics_show_the_log_commits_and_replicas() {
    let dir = temp_dir("metrics");
    let leader = sync_leader(&dir, 4, Duration::from_secs(5));
    let (follower, _handle) = sync_follower(&dir.join("follower"), &leader);
    for i in 0..3 {
        assert_eq!(sync_write(&leader, i).0, "1");
    }
    wait_for(&follower, "rep", 3);
    // The leader: its log, its commits, the replica it hears from, writes waiting for it.
    assert!(metric(&leader, "tripletrail_wal_bytes", "ds").unwrap() > 0.0);
    assert_eq!(
        metric(&leader, "tripletrail_commits_total", "ds"),
        Some(3.0)
    );
    assert!(metric(&leader, "tripletrail_commit_seconds_total", "ds").unwrap() > 0.0);
    assert_eq!(
        metric(&leader, "tripletrail_slow_commits_total", "ds"),
        Some(0.0)
    );
    assert!(metric(&leader, "tripletrail_checkpoints_total", "ds").is_some());
    assert!(metric(&leader, "tripletrail_runs", "ds").is_some());
    assert_eq!(
        metric(&leader, "tripletrail_replication_followers", "ds"),
        Some(1.0)
    );
    assert_eq!(
        metric(&leader, "tripletrail_sync_replicas_waiting", "ds"),
        Some(1.0)
    );
    assert_eq!(
        metric(&leader, "tripletrail_replica_lag_seconds", "ds"),
        None
    );
    // The replica: following, what it applied, its lag.
    assert_eq!(
        metric(&follower, "tripletrail_replica_following", "rep"),
        Some(1.0)
    );
    assert!(metric(&follower, "tripletrail_replica_applied_total", "rep").unwrap() >= 3.0);
    assert_eq!(
        metric(&follower, "tripletrail_replica_bases_total", "rep"),
        Some(1.0)
    );
    assert!(metric(&follower, "tripletrail_replica_lag_seconds", "rep").is_some());
    let text = ureq::get(&format!("{leader}/$/metrics"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        text.contains("tripletrail_file_check{dataset=\"ds\",state=\"verified\"} 1"),
        "{text}"
    );
}
