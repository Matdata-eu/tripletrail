//! Microbenchmark: the first commits into a large store on disk that was just opened, with
//! the page cache dropped (#437): each commit looks up its terms in the dictionary on disk
//! and checks its quads in the segments, which are cold reads.
//!
//! `cargo run --release -p tripletrail-store --example cold_commits build <dir> [quads]`
//! `cargo run --release -p tripletrail-store --example cold_commits run <dir> [commits] [quads per commit]`
//!
//! Drop the page cache between the two (`sync; echo 3 > /proc/sys/vm/drop_caches`). With
//! `ONE_BY_ONE=1`, a commit inserts its quads one at a time instead of with `insert_all`;
//! with `NO_VERIFY=1`, the check of the files in the background (#436) waits.
//! A commit adds, per quad, a new literal to a subject already in the store, picked at random.
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

/// Major page faults of this process so far: pages read from disk (Linux; 0 elsewhere).
fn major_faults() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let rest = &s[s.rfind(')')? + 2..];
            rest.split(' ').nth(9)?.parse().ok()
        })
        .unwrap_or(0)
}

fn quad(s: u64, p: u64, value: &str) -> Quad {
    Quad::new(
        NamedNode::new_unchecked(format!("http://example.org/base/s{s}")),
        NamedNode::new_unchecked(format!("http://example.org/p{p}")),
        Literal::new_simple_literal(value),
        GraphName::DefaultGraph,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.get(1).expect("a store directory");
    let number = |i: usize, default| args.get(i).map_or(default, |a| a.parse().unwrap());
    match args.first().map(String::as_str) {
        Some("build") => {
            let total = number(2, 30_000_000);
            let store = Store::open(dir).unwrap();
            let t = Instant::now();
            let batches = (0..total).step_by(1 << 20).map(|from| {
                (from..(from + (1 << 20)).min(total))
                    .map(|i| quad(i / 4, i % 4, &format!("value {i}")))
                    .collect::<Vec<_>>()
            });
            store.bulk_load_quad_batches(batches).unwrap();
            store.wait_for_merges();
            store.checkpoint().unwrap();
            println!("loaded {total} quads in {:.1} s", t.elapsed().as_secs_f64());
        }
        Some("run") => {
            let (commits, each) = (number(2, 10), number(3, 10_000));
            if std::env::var_os("NO_VERIFY").is_some() {
                Store::hold_verification(true);
            }
            let t = Instant::now();
            let store = Store::open(dir).unwrap();
            let subjects = (store.len() / 4) as u64;
            println!("opened in {:.1} s", t.elapsed().as_secs_f64());
            let mut x = 0x9e37_79b9_7f4a_7c15u64;
            let mut times = Vec::new();
            let one_by_one = std::env::var_os("ONE_BY_ONE").is_some();
            for c in 0..commits {
                let quads: Vec<Quad> = (0..each)
                    .map(|i| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        quad(x % subjects, 4, &format!("new {c} {i}"))
                    })
                    .collect();
                let faults = major_faults();
                let t = Instant::now();
                store
                    .transaction(|txn| {
                        if one_by_one {
                            for q in &quads {
                                txn.insert(q.as_ref());
                            }
                        } else {
                            txn.insert_all(quads);
                        }
                        Ok::<_, tripletrail_store::StoreError>(())
                    })
                    .unwrap();
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                println!(
                    "commit {c}: {ms:.0} ms, {} pages read",
                    major_faults() - faults
                );
                times.push(ms);
            }
            println!("total {:.0} ms", times.iter().sum::<f64>());
        }
        _ => panic!("build or run"),
    }
}
