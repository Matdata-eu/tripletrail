//! Microbenchmark: the memory an `INSERT … WHERE` takes (#382): the peak and the resident
//! memory after it, for a store on disk.
//!
//! `cargo run --release -p tripletrail-store --example insert_memory <store dir> <update file> [union]`
//!
//! With `union`, the WHERE reads the union of all graphs as its default graph.
use oxigraph::sparql::SparqlEvaluator;
use spargebra::SparqlParser;
use std::time::Instant;
use tripletrail_store::Store;

fn status(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
        / 1024
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let store = Store::open(&args[0]).unwrap();
    let text = std::fs::read_to_string(&args[1]).unwrap();
    let update = SparqlParser::new().parse_update(&text).unwrap();
    let union = args.get(2).is_some_and(|a| a == "union");
    let before = store.len();
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    println!("idle: {} MB", status("VmRSS:"));
    let t = Instant::now();
    let changes = store
        .update_with(&update, SparqlEvaluator::new, union)
        .unwrap();
    println!(
        "update: {:.1} s, {} quads added, peak {} MB, after {} MB",
        t.elapsed().as_secs_f64(),
        store.len() - before,
        status("VmHWM:"),
        status("VmRSS:"),
    );
    drop(changes);
}
