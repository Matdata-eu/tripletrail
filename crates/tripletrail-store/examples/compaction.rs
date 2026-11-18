//! Microbenchmark: how long folding the write delta into the indexes takes (issue #213).
//!
//! `cargo run --release -p tripletrail-store --example compaction [quads]`
//!
//! Builds a store of `quads` default-graph quads (1,000,000 by default), then times commits
//! of 4,096 new quads each until one of them folds the delta in (at 65,536 quads), and
//! prints the slowest commit (the one that compacts) and the median of the others. Then
//! times bulk loads of `quads / 20` and `quads / 50` new quads (spread like the commits) into
//! the store, best of 3.

use oxigraph::model::{Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

fn quad(i: u64, subjects: u64, tag: &str) -> Quad {
    let s = i % subjects;
    Quad::new(
        NamedNode::new_unchecked(format!("http://example.org/{tag}{s}")),
        NamedNode::new_unchecked(format!("http://example.org/p{}", i / subjects % 10)),
        Literal::from(i as i64),
        oxigraph::model::GraphName::DefaultGraph,
    )
}

fn main() {
    let total: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let subjects = (total / 10).max(1);
    let store = Store::new();
    store
        .bulk_load_quads((0..total).map(|i| quad(i, subjects, "s")))
        .unwrap();
    // Writes spread over the whole key range: existing subjects, new objects.
    let mut times = Vec::new();
    for batch in 0..17u64 {
        let quads: Vec<Quad> = (0..4096u64)
            .map(|i| {
                let j = total + batch * 4096 + i;
                quad(j * 7919 % total, subjects, "s")
            })
            .map(|q| {
                Quad::new(
                    q.subject,
                    q.predicate,
                    Literal::from(-(batch as i64) * 10_000 - 1),
                    q.graph_name,
                )
            })
            .collect();
        let t = Instant::now();
        store.extend(quads).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    println!(
        "{total} quads: slowest commit {:.1} ms, median {:.1} ms, store {}",
        sorted[sorted.len() - 1],
        sorted[sorted.len() / 2],
        store.len()
    );
    for (div, tag) in [(20, 0), (50, 1)] {
        let mut best = f64::MAX;
        for run in 0..3i64 {
            let n = total / div;
            let quads: Vec<Quad> = (0..n)
                .map(|i| {
                    let q = quad(i * 7919 % total, subjects, "s");
                    Quad::new(
                        q.subject,
                        q.predicate,
                        Literal::from(-(tag * 3 + run) * 1_000_000 - 7),
                        q.graph_name,
                    )
                })
                .collect();
            let t = Instant::now();
            store.bulk_load_quads(quads).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("load of 1/{div} of the store: {best:.1} ms");
    }
}
