//! Crash recovery: a child process writes transactions and checkpoints to an on-disk store
//! and is killed (SIGKILL) at a random moment, again and again. After every kill the store
//! must open and hold exactly the transactions up to some point: none torn in half
//! (atomicity), none lost that the child had seen committed (durability).
//!
//! The child is this test binary again, running the ignored `crash_child` test with
//! `TRIPLETRAIL_CRASH_DIR` set. `TRIPLETRAIL_CRASH_ROUNDS` sets the number of kills (default 20).

use oxigraph::model::{GraphNameRef, Literal, NamedNode, Quad};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use tripletrail_store::Store;

/// Each transaction inserts two quads and, every fifth one, removes one inserted earlier.
fn quad(i: u64, part: &str) -> Quad {
    Quad::new(
        NamedNode::new_unchecked(format!("http://example.com/t{i}")),
        NamedNode::new_unchecked(format!("http://example.com/{part}")),
        Literal::from(i as i64),
        GraphNameRef::DefaultGraph,
    )
}

/// The store's quads after transactions `0..k`.
fn expected(k: u64) -> BTreeSet<String> {
    let mut quads = BTreeSet::new();
    for i in 0..k {
        quads.insert(quad(i, "a").to_string());
        quads.insert(quad(i, "b").to_string());
        if i % 5 == 4 {
            quads.remove(&quad(i - 2, "a").to_string());
        }
    }
    quads
}

/// The number of transactions a store holds (the `b` quads are never removed), after
/// checking that it holds exactly those.
fn recovered(dir: &Path) -> u64 {
    let store = Store::open(dir).expect("the store opens after a crash");
    let quads: BTreeSet<String> = store.iter().map(|q| q.to_string()).collect();
    let k = quads.iter().filter(|q| q.contains("/b>")).count() as u64;
    assert_eq!(quads, expected(k), "the store after {k} transactions");
    k
}

#[test]
#[ignore = "run by crash_recovery as a child process"]
fn crash_child() {
    let Ok(dir) = std::env::var("TRIPLETRAIL_CRASH_DIR") else {
        return;
    };
    let store = Store::open(&dir).unwrap();
    let mut i = store
        .iter()
        .filter(|q| q.predicate.as_str().ends_with("/b"))
        .count() as u64;
    let mut out = std::io::stdout().lock();
    loop {
        store
            .transaction(|t| {
                t.insert(&quad(i, "a"));
                t.insert(&quad(i, "b"));
                if i % 5 == 4 {
                    t.remove(&quad(i - 2, "a"));
                }
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .unwrap();
        // Committed: the parent may now count on it.
        writeln!(out, "{i}").unwrap();
        out.flush().unwrap();
        if i % 25 == 24 {
            store.checkpoint().unwrap();
        }
        i += 1;
    }
}

#[cfg(unix)]
#[test]
fn crash_recovery() {
    let dir = tripletrail_testdir::tempdir("store-crash");
    let rounds: u64 = std::env::var("TRIPLETRAIL_CRASH_ROUNDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(20);
    let mut rng = 0x2545_f491_4f6c_dd1d_u64;
    let mut seen = 0;
    for round in 0..rounds {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TRIPLETRAIL_CRASH_DIR", &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Read what the child reports as committed until it dies.
        let stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .filter_map(|l| l.trim().parse::<u64>().ok())
                .last()
        });
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(30 + rng % 300));
        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();
        let committed = reader.join().unwrap();
        let k = recovered(&dir);
        if let Some(last) = committed {
            assert!(
                k > last,
                "round {round}: transaction {last} was committed but the store has {k}"
            );
        }
        assert!(k >= seen, "round {round}: {k} transactions after {seen}");
        seen = k;
    }
    assert!(seen > 0, "the child never committed anything");
}
