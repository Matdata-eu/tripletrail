//! Microbenchmark: one large transaction into a large store (#292), split into the time
//! spent inserting into the transaction and the time spent committing it.
//!
//! `cargo run --release -p tripletrail-store --example large_commit [store quads] [new quads] [dir]`
//!
//! With a directory the store is on disk there, and the commit includes the write-ahead log
//! and any checkpoint it triggers.
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

fn quad(i: u64, tag: &str) -> Quad {
    Quad::new(
        NamedNode::new_unchecked(format!("http://example.org/{tag}/s{}", i / 8)),
        NamedNode::new_unchecked(format!("http://example.org/p{}", i % 8)),
        Literal::new_simple_literal(format!("{tag} value {i}")),
        GraphName::DefaultGraph,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |i: usize, default| args.get(i).map_or(default, |a| a.parse().unwrap());
    let (total, new) = (number(0, 12_500_000), number(1, 1_300_000));
    let store = match args.get(2) {
        Some(dir) => Store::open(dir).unwrap(),
        None => Store::new(),
    };
    let t = Instant::now();
    store
        .bulk_load_quads((0..total).map(|i| quad(i, "base")))
        .unwrap();
    println!("loaded {total} quads in {:.1} s", t.elapsed().as_secs_f64());
    let quads: Vec<Quad> = (0..new).map(|i| quad(i, "new")).collect();
    let mut inserting = 0.0;
    let t = Instant::now();
    store
        .transaction(|txn| {
            let t = Instant::now();
            for q in &quads {
                txn.insert(q.as_ref());
            }
            inserting = t.elapsed().as_secs_f64();
            Ok::<_, tripletrail_store::StoreError>(())
        })
        .unwrap();
    let all = t.elapsed().as_secs_f64();
    println!(
        "transaction of {new} quads: insert {inserting:.2} s, commit {:.2} s",
        all - inserting
    );
    assert_eq!(store.len() as u64, total + new);
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let line = |k: &str| {
        status
            .lines()
            .find(|l| l.starts_with(k))
            .unwrap_or("")
            .to_owned()
    };
    println!(
        "heap quads {} MB | {} | {}",
        store.memory_use().quads >> 20,
        line("RssAnon"),
        line("RssFile")
    );
}
