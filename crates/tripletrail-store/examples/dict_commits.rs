//! Microbenchmark: small commits of new literals on the subjects of a store on disk, with a
//! checkpoint after every ten, so the dictionary gets pieces (#218 step 4: each new term is
//! looked up in the table and the saved indexes before it is added).
//!
//! `cargo run --release -p tripletrail-store --example dict_commits <dir> [commits] [quads per commit]`
use oxigraph::model::{GraphName, Literal, NamedNode, NamedOrBlankNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().expect("a store directory");
    let number = |i: usize, default| args.get(i).map_or(default, |a| a.parse().unwrap());
    let (commits, each): (usize, usize) = (number(1, 40), number(2, 10_000));
    let store = Store::open(dir).unwrap();
    let subjects: Vec<NamedOrBlankNode> = store
        .iter()
        .step_by(13)
        .take(commits * each)
        .map(|q| q.subject)
        .collect();
    let predicate = NamedNode::new_unchecked("http://example.org/note");
    let mut times = Vec::new();
    for c in 0..commits {
        let quads: Vec<Quad> = (c * each..(c + 1) * each)
            .map(|i| {
                Quad::new(
                    subjects[i % subjects.len()].clone(),
                    predicate.clone(),
                    Literal::new_simple_literal(format!("note {i}")),
                    GraphName::DefaultGraph,
                )
            })
            .collect();
        let t = Instant::now();
        store
            .transaction(|txn| {
                for q in &quads {
                    txn.insert(q.as_ref());
                }
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .unwrap();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if c % 10 == 9 {
            store.checkpoint().unwrap();
        }
    }
    for (b, block) in times.chunks(10).enumerate() {
        let mut sorted = block.to_vec();
        sorted.sort_by(f64::total_cmp);
        println!(
            "commits {}-{}: median {:.1} ms",
            b * 10 + 1,
            b * 10 + block.len(),
            sorted[sorted.len() / 2]
        );
    }
}
