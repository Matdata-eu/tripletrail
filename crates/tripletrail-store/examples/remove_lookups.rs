//! Microbenchmark: single removes, each after a lookup, as rdflib's `graph.remove` does through
//! oxrdflib (a `quads_for_pattern` of the triple, then a `remove` of each quad it finds).
//!
//! `cargo run --release -p tripletrail-store --example remove_lookups [quads]`
//!
//! Builds a store of `quads` quads in one named graph (2,000,000 by default), then removes
//! 60,000 of them one at a time, each after a lookup of the quad and a lookup of quads with
//! its subject as object, and prints the time per removal as the write delta grows. Before
//! the reads took their range from the delta's persistent sets, every read after a write
//! copied the whole delta: the time per removal grew with the number of removes before it.

use oxigraph::model::{GraphNameRef, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

fn iri(s: String) -> NamedNode {
    NamedNode::new_unchecked(s)
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000_000);
    let g = iri("http://example.org/g".into());
    let p = iri("http://example.org/p".into());
    let quad = |i: u64| {
        Quad::new(
            iri(format!("http://example.org/s{i}")),
            p.clone(),
            iri(format!("http://example.org/o{}", i % 1000)),
            g.clone(),
        )
    };
    let store = Store::new();
    store.bulk_load_quads((0..n).map(quad)).unwrap();
    let graph = GraphNameRef::NamedNode(g.as_ref());
    let mut done = 0;
    for step in [1_000u64, 9_000, 20_000, 30_000] {
        let t = Instant::now();
        for i in done..done + step {
            let q = quad(i);
            let subject = iri(format!("http://example.org/s{i}"));
            let found: Vec<Quad> = store
                .quads_for_pattern(
                    Some(q.subject.as_ref()),
                    Some(q.predicate.as_ref()),
                    Some(q.object.as_ref()),
                    Some(graph),
                )
                .collect();
            let _ = store
                .quads_for_pattern(None, None, Some(subject.as_ref().into()), Some(graph))
                .count();
            for f in &found {
                store.remove(f).unwrap();
            }
        }
        done += step;
        println!(
            "after {done:>6} removes: {:>7.1} us per lookup and remove",
            t.elapsed().as_secs_f64() * 1e6 / step as f64
        );
    }
}
