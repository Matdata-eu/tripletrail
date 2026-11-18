//! Microbenchmark: scan speed per quad (issue #169).
//!
//! `cargo run --release -p tripletrail-store --example scan_speed [quads]`
//!
//! Builds a store of `quads` default-graph quads (1,000,000 by default) with ten predicates
//! and times, best of 9 runs:
//! - `all`: every quad (`?s ?p ?o` in the default graph);
//! - `pred`: one predicate, by object (`?s <p3> ?o`, POS order);
//! - `pred by s`: the same, sorted by subject (PSO order);
//! - `points`: 100,000 subject lookups (`<s> ?p ?o`);
//! - `join`: the id-level join of `?s <p3> ?a . ?s <p4> ?b` (100,000 rows);
//! - `count p`: the id-level count of `?s <p3> ?o`;
//! - `rows p`: the id-level rows of `?s <p3> ?o`;
//! - `join 1/N`: the id-level join of `?s <selN> ?x . ?s <p3> ?a`, where `<selN>` is on every
//!   Nth subject (a small side joined with a large one).

use oxigraph::model::{Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::{BgpCount, GraphSelector, IdSlot, IdTriple, Store, TermId};

fn iri(s: String) -> NamedNode {
    NamedNode::new_unchecked(s)
}

fn time<T>(f: impl Fn() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut out = None;
    for _ in 0..9 {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
        out = Some(r);
    }
    (best, out.unwrap())
}

fn main() {
    let total: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let subjects = total / 10;
    let store = Store::new();
    store
        .bulk_load_quads((0..total).map(|i| {
            let (s, p) = (i % subjects, i / subjects);
            Quad::new(
                iri(format!("http://example.org/s{s}")),
                iri(format!("http://example.org/p{p}")),
                Literal::from((s * 7 + p) as i64 % 1_000_003 + 10_000_000_000),
                oxigraph::model::GraphName::DefaultGraph,
            )
        }))
        .unwrap();
    store
        .bulk_load_quads((0..subjects).flat_map(|s| {
            [20, 100, 1000]
                .into_iter()
                .filter(move |n| s % n == 0)
                .map(move |n| {
                    Quad::new(
                        iri(format!("http://example.org/s{s}")),
                        iri(format!("http://example.org/sel{n}")),
                        Literal::from(s as i64),
                        oxigraph::model::GraphName::DefaultGraph,
                    )
                })
        }))
        .unwrap();
    let snap = store.snapshot();
    let id = |s: String| snap.term_id(iri(s).as_ref().into()).unwrap();
    let p3 = id("http://example.org/p3".into());
    let ids: Vec<TermId> = (0..100_000u64)
        .map(|i| id(format!("http://example.org/s{}", i * 37 % subjects)))
        .collect();
    let per = |ms: f64, n: usize| ms * 1e6 / n.max(1) as f64;
    let (ms, n) = time(|| {
        snap.quad_ids_for_pattern(None, None, None, GraphSelector::Default)
            .count()
    });
    println!("all        {ms:8.2} ms {:6.2} ns/quad ({n})", per(ms, n));
    let (ms, n) = time(|| {
        snap.quad_ids_for_pattern(None, Some(p3), None, GraphSelector::Default)
            .count()
    });
    println!("pred       {ms:8.2} ms {:6.2} ns/quad ({n})", per(ms, n));
    let (ms, n) = time(|| {
        snap.quad_ids_sorted(None, Some(p3), None, GraphSelector::Default, 0)
            .count()
    });
    println!("pred by s  {ms:8.2} ms {:6.2} ns/quad ({n})", per(ms, n));
    let (ms, n) = time(|| {
        ids.iter()
            .map(|s| {
                snap.quad_ids_for_pattern(Some(*s), None, None, GraphSelector::Default)
                    .count()
            })
            .sum::<usize>()
    });
    println!(
        "points     {ms:8.2} ms {:6.1} ns/lookup ({n})",
        per(ms, ids.len())
    );
    let p4 = id("http://example.org/p4".into());
    let (v0, v1, v2) = (IdSlot::Var(0), IdSlot::Var(1), IdSlot::Var(2));
    let triples = [
        IdTriple {
            s: v0,
            p: IdSlot::Const(p3),
            o: v1,
        },
        IdTriple {
            s: v0,
            p: IdSlot::Const(p4),
            o: v2,
        },
    ];
    let (ms, n) = time(|| {
        snap.join_ids(&triples, 3, GraphSelector::Default, &|| false)
            .map_or(0, |r| r.len())
    });
    println!("join       {ms:8.2} ms {:6.2} ns/row ({n})", per(ms, n));
    let (ms, n) = time(|| {
        snap.count_ids(
            &triples[..1],
            2,
            GraphSelector::Default,
            BgpCount::Rows,
            &|| false,
        )
        .unwrap_or(0) as usize
    });
    println!("count p    {ms:8.4} ms ({n})");
    let (ms, n) = time(|| {
        snap.join_ids(&triples[..1], 2, GraphSelector::Default, &|| false)
            .map_or(0, |r| r.len())
    });
    println!("rows p     {ms:8.2} ms {:6.2} ns/row ({n})", per(ms, n));
    for n in [20, 100, 1000] {
        let sel = id(format!("http://example.org/sel{n}"));
        let triples = [
            IdTriple {
                s: v0,
                p: IdSlot::Const(sel),
                o: v1,
            },
            IdTriple {
                s: v0,
                p: IdSlot::Const(p3),
                o: v2,
            },
        ];
        let (ms, rows) = time(|| {
            snap.join_ids(&triples, 3, GraphSelector::Default, &|| false)
                .map_or(0, |r| r.len())
        });
        println!("join 1/{n:<4} {ms:7.3} ms ({rows})");
    }
}
