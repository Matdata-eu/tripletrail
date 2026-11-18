//! Microbenchmark: how long a store on disk takes to open, and the heap it holds after (#218
//! step 4: the dictionary's hash table).
//!
//! `cargo run --release -p tripletrail-store --example open_time <dir> [read-only]`
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
    let read_only = args.get(1).is_some_and(|a| a == "read-only");
    let before = heap_mb();
    let t = Instant::now();
    let store = if read_only {
        Store::open_read_only(dir).unwrap()
    } else {
        Store::open(dir).unwrap()
    };
    println!(
        "opened {} in {:.2} s, heap +{:.0} MB, {} quads",
        if read_only { "read-only" } else { "writable" },
        t.elapsed().as_secs_f64(),
        heap_mb() - before,
        store.len(),
    );
}
