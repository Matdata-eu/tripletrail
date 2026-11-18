//! Microbenchmark: the cost of small commits into a large store (no compaction).
//!
//! `cargo run --release -p tripletrail-store --example commit_speed [quads]`
use oxigraph::model::{Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

fn quad(i: u64, subjects: u64, o: i64) -> Quad {
    Quad::new(
        NamedNode::new_unchecked(format!("http://example.org/s{}", i % subjects)),
        NamedNode::new_unchecked(format!("http://example.org/p{}", i / subjects % 10)),
        Literal::from(o),
        oxigraph::model::GraphName::DefaultGraph,
    )
}

fn main() {
    let total: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000_000);
    let subjects = (total / 10).max(1);
    let store = Store::new();
    store
        .bulk_load_quads((0..total).map(|i| quad(i, subjects, i as i64)))
        .unwrap();
    let mut times = Vec::new();
    for round in 0..200u64 {
        let quads: Vec<Quad> = (0..1000u64)
            .map(|i| {
                quad(
                    (round * 1000 + i) * 7919 % total,
                    subjects,
                    -(round as i64) - 1,
                )
            })
            .collect();
        let t = Instant::now();
        store.extend(quads).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1e6 / 1000.0);
        if store.len() as u64 > total + 60_000 {
            break;
        }
    }
    times.sort_by(f64::total_cmp);
    // Large transactions on top of the delta left above: each fills the delta.
    for n in [70_000u64, 100_000] {
        let mut best = f64::MAX;
        for run in 0..3u64 {
            let quads: Vec<Quad> = (0..n)
                .map(|i| quad(i * 104_729 % total, subjects, -1_000 - (n + run) as i64))
                .collect();
            let t = Instant::now();
            store.extend(quads).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("extend of {n} quads: {best:.1} ms");
    }
    println!(
        "{} commits of 1000 quads: median {:.2} us/quad, p90 {:.2}",
        times.len(),
        times[times.len() / 2],
        times[times.len() * 9 / 10]
    );
}
