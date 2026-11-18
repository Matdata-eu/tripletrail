//! Microbenchmark: point lookups of quads already in a store on disk, each of which looks up
//! its four terms in the dictionary (#218 step 4: the saved dictionary index).
//!
//! `cargo run --release -p tripletrail-store --example term_lookups <dir> [quads]`
use std::time::Instant;
use tripletrail_store::Store;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().expect("a store directory");
    let n: usize = args
        .get(1)
        .map_or(1_000_000, |a| a.parse().expect("a number"));
    let store = Store::open_read_only(dir).unwrap();
    // Every 7th quad, so the terms are spread over the dictionary.
    let quads: Vec<_> = store.iter().step_by(7).take(n).collect();
    for round in ["first", "again"] {
        let t = Instant::now();
        let found = quads.iter().filter(|q| store.contains(*q)).count();
        let secs = t.elapsed().as_secs_f64();
        assert_eq!(found, quads.len());
        println!(
            "{round}: {} lookups in {secs:.2} s, {:.2} µs each",
            quads.len(),
            secs * 1e6 / quads.len() as f64
        );
    }
}
