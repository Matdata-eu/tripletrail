//! Microbenchmark: the cost of checkpoints of a few changes on a store on disk with many
//! geometry literals (#218: a checkpoint encodes only the geometries of the dictionary pieces
//! it writes).
//!
//! `cargo run --release -p tripletrail-store --example checkpoint_cost <dir> [checkpoints]`
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

/// The process's resident heap (`RssAnon`) in MB, where `/proc` has it.
fn heap_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("RssAnon:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().expect("a store directory");
    let n: usize = args.get(1).map_or(4, |a| a.parse().unwrap());
    let store = Store::open(dir).unwrap();
    for c in 0..n {
        store
            .insert(&Quad::new(
                NamedNode::new_unchecked(format!("http://example.org/checkpoint/{c}")),
                NamedNode::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT"),
                Literal::new_typed_literal(
                    format!("POINT({c} 1)"),
                    NamedNode::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
                ),
                GraphName::DefaultGraph,
            ))
            .unwrap();
        let t = Instant::now();
        store.checkpoint().unwrap();
        println!(
            "checkpoint {}: {:.2} s, heap {:.0} MB",
            c + 1,
            t.elapsed().as_secs_f64(),
            heap_mb()
        );
    }
}
