//! Microbenchmark: reads on a compacted store, then with a small write delta (issue #213).
//!
//! `cargo run --release -p tripletrail-store --example delta_reads [quads]`
//!
//! Builds a store of `quads` default-graph quads (1,000,000 by default) with ten predicates,
//! then times, with no delta, after inserting 80 quads, and after also deleting one
//! subject's quads:
//! - `scan p`: a full scan of one predicate (`?s <p3> ?o`, a tenth of the store);
//! - `points`: 20,000 subject lookups (`<s> ?p ?o`), as an index nested-loop join does;
//! - `count`: `COUNT(*)` of `?s ?p ?o` from the stored counts (or `-` when they step aside);
//! - `estimate`: 20,000 pattern estimates of `?s <p> ?o`, as the planner makes them.

use oxigraph::model::{Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::{GraphSelector, Store, TripleCount};

fn iri(s: String) -> NamedNode {
    NamedNode::new_unchecked(s)
}

fn quad(i: u64, subjects: u64) -> Quad {
    let s = i % subjects;
    let p = i / subjects;
    Quad::new(
        iri(format!("http://example.org/s{s}")),
        iri(format!("http://example.org/p{p}")),
        Literal::from((s * 7 + p) as i64 % 1_000_003 + 10_000_000_000),
        oxigraph::model::GraphName::DefaultGraph,
    )
}

fn time<T>(f: impl Fn() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut out = None;
    for _ in 0..7 {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
        out = Some(r);
    }
    (best, out.unwrap())
}

fn report(store: &Store, label: &str, subjects: u64) {
    let snap = store.snapshot();
    let p3 = snap
        .term_id(iri("http://example.org/p3".into()).as_ref().into())
        .unwrap();
    let ids: Vec<_> = (0..20_000u64)
        .filter_map(|i| {
            snap.term_id(
                iri(format!("http://example.org/s{}", i * 37 % subjects))
                    .as_ref()
                    .into(),
            )
        })
        .collect();
    let preds: Vec<_> = (0..10)
        .filter_map(|p| snap.term_id(iri(format!("http://example.org/p{p}")).as_ref().into()))
        .collect();
    let (scan, n) = time(|| {
        snap.quad_ids_for_pattern(None, Some(p3), None, GraphSelector::Default)
            .count()
    });
    let (points, m) = time(|| {
        ids.iter()
            .map(|s| {
                snap.quad_ids_for_pattern(Some(*s), None, None, GraphSelector::Default)
                    .count()
            })
            .sum::<usize>()
    });
    let (count, c) = time(|| snap.count_triples(GraphSelector::Default, TripleCount::Rows, None));
    let (estimate, e) = time(|| {
        (0..20_000)
            .map(|i| {
                snap.estimate_ids(
                    None,
                    Some(preds[i % preds.len()]),
                    None,
                    GraphSelector::Default,
                )
            })
            .sum::<usize>()
    });
    println!(
        "{label:<22} scan p {scan:7.2} ms ({n})  points {points:7.2} ms ({m})  count {}  estimate {estimate:6.2} ms ({e})",
        c.map_or("      -".to_string(), |c| format!("{count:7.4} ms ({c})")),
    );
}

fn main() {
    let total: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let subjects = total / 10;
    let store = Store::new();
    store
        .bulk_load_quads((0..total).map(|i| quad(i, subjects)))
        .unwrap();
    report(&store, "compacted", subjects);
    store
        .extend((0..80).map(|i| {
            Quad::new(
                iri(format!("http://example.org/new{i}")),
                iri(format!("http://example.org/p{}", i % 10)),
                Literal::from(i as i64),
                oxigraph::model::GraphName::DefaultGraph,
            )
        }))
        .unwrap();
    report(&store, "+80 inserted", subjects);
    store
        .transaction(|t| {
            for p in 0..10 {
                t.remove(quad(p * subjects + 12_345, subjects).as_ref());
            }
            Ok::<_, tripletrail_store::StoreError>(())
        })
        .unwrap();
    report(&store, "+80 ins, 10 deleted", subjects);
    store
        .extend((0..20_000).map(|i| {
            Quad::new(
                iri(format!("http://example.org/bulk{i}")),
                iri(format!("http://example.org/p{}", i % 10)),
                Literal::from(i as i64),
                oxigraph::model::GraphName::DefaultGraph,
            )
        }))
        .unwrap();
    report(&store, "+20k inserted", subjects);
    // Writes and reads taking turns: one insert, then one subject lookup and one predicate
    // scan, so every read sees a new version of the delta.
    let p3 = store
        .term_id(iri("http://example.org/p3".into()).as_ref().into())
        .unwrap();
    let s1 = store
        .term_id(iri("http://example.org/s1".into()).as_ref().into())
        .unwrap();
    let t = Instant::now();
    let mut n = 0;
    for i in 0..500 {
        store
            .insert(&Quad::new(
                iri(format!("http://example.org/turn{i}")),
                iri("http://example.org/p3".into()),
                Literal::from(i as i64),
                oxigraph::model::GraphName::DefaultGraph,
            ))
            .unwrap();
        let snap = store.snapshot();
        n += snap
            .quad_ids_for_pattern(Some(s1), None, None, GraphSelector::Default)
            .count();
        n += snap
            .quad_ids_for_pattern(None, Some(p3), None, GraphSelector::Default)
            .count();
    }
    println!(
        "write, then read x500   {:7.3} ms per turn ({n})",
        t.elapsed().as_secs_f64() * 1e3 / 500.0
    );
}
