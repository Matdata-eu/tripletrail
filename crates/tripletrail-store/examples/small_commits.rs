//! Microbenchmark: many small transactions into a large store on disk (#218): the cost of
//! the folds they trigger, which grows with the store when a fold rewrites every block.
//!
//! `cargo run --release -p tripletrail-store --example small_commits [store quads] [commits] [quads per commit] [dir]`
//! With `VERBOSE=1`, prints each commit's time, whether a merge runs in the background, and
//! what the process has written to disk. With `IDLE_MS=<ms>`, also waits until the store
//! has been idle that long and reads again; before that (with `TICKS=<n>`, first) writes
//! one quad a little after each idle time, n times (each with a checkpoint, with
//! `TICK_CHECKPOINT=1`). With `CHECKPOINT_MB=<n>`, the store checkpoints once its log
//! passes n MB (256 by default), so that a run sees many checkpoints.
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::time::Instant;
use tripletrail_store::Store;

fn quad(i: u64, tag: &str) -> Quad {
    Quad::new(
        NamedNode::new_unchecked(format!("http://example.org/{tag}/s{}", i / 8)),
        NamedNode::new_unchecked(format!("http://example.org/p{}", i % 8)),
        Literal::new_simple_literal(format!("{tag} value {i}")),
        GraphName::DefaultGraph,
    )
}

/// Bytes this process has written to disk so far, in MB (Linux; 0 elsewhere).
fn written_mb() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|io| {
            io.lines().find_map(|l| {
                l.strip_prefix("write_bytes: ")
                    .map(|n| n.trim().parse::<u64>().ok())
            })
        })
        .flatten()
        .map_or(0, |b| b >> 20)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |i: usize, default| args.get(i).map_or(default, |a| a.parse().unwrap());
    let (total, commits, each) = (number(0, 25_000_000), number(1, 40), number(2, 10_000));
    let store = match args.get(3) {
        Some(dir) => Store::open(dir).unwrap(),
        None => Store::new(),
    };
    let t = Instant::now();
    store
        .bulk_load_quads((0..total).map(|i| quad(i, "base")))
        .unwrap();
    store.wait_for_merges();
    println!("loaded {total} quads in {:.1} s", t.elapsed().as_secs_f64());
    if let Some(mb) = std::env::var("CHECKPOINT_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        store.set_checkpoint_log_size(mb << 20);
    }
    let verbose = std::env::var_os("VERBOSE").is_some();
    let loaded_mb = written_mb();
    let mut times = Vec::new();
    let all = Instant::now();
    for c in 0..commits {
        let quads: Vec<Quad> = (c * each..(c + 1) * each)
            .map(|i| quad(i * 7919 % (total * 4), "new"))
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
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        times.push(ms);
        if verbose {
            println!(
                "commit {c}: {ms:.0} ms, merging {}, written {} MB",
                store.is_merging(),
                written_mb()
            );
        }
    }
    let total_s = all.elapsed().as_secs_f64();
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)];
    println!(
        "commit times: p50 {:.0} ms, p95 {:.0} ms, p99 {:.0} ms, max {:.0} ms",
        at(0.5),
        at(0.95),
        at(0.99),
        at(1.0)
    );
    let t = Instant::now();
    store.wait_for_merges();
    println!(
        "merges finished {:.1} s after the last commit; {} MB written since the load",
        t.elapsed().as_secs_f64(),
        written_mb() - loaded_mb
    );
    // What the runs left unmerged cost reads: lookups of single quads, and a full scan.
    let reads = |when: &str| {
        let t = Instant::now();
        let found = (0..100_000u64)
            .filter(|i| store.contains(&quad(i * 104_729 % total, "base")))
            .count();
        let lookups = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let all_quads = store
            .snapshot()
            .quad_ids_for_pattern(None, None, None, tripletrail_store::GraphSelector::All)
            .count();
        println!(
            "reads {when}: {found} lookups in {:.0} ms, a scan of {all_quads} quads in {:.0} ms",
            lookups * 1000.0,
            t.elapsed().as_secs_f64() * 1000.0
        );
    };
    reads("after the commits");
    // Then once the store has been idle (#442), with IDLE_MS set.
    if let Some(ms) = std::env::var("IDLE_MS").ok().and_then(|v| v.parse().ok()) {
        Store::set_idle_merge_after(std::time::Duration::from_millis(ms));
        // A write now and then, each a little after the idle time: the store merges once
        // (#442), not again at every write.
        let ticks: usize = std::env::var("TICKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        for tick in 0..ticks {
            store
                .insert(&quad(total * 8 + tick as u64, "tick"))
                .unwrap();
            if std::env::var_os("TICK_CHECKPOINT").is_some() {
                store.checkpoint().unwrap();
            }
            if std::env::var_os("VERBOSE").is_some() {
                println!(
                    "tick {tick} written: {} MB, merging {}, runs {}",
                    written_mb() - loaded_mb,
                    store.is_merging(),
                    store.run_count()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(ms + 400));
            println!(
                "tick {tick}: {} MB written since the load, merging {}, runs {}",
                written_mb() - loaded_mb,
                store.is_merging(),
                store.run_count()
            );
        }
        let t = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(ms + 1500));
        store.wait_for_merges();
        println!(
            "idle {:.1} s (merges done), {} MB written since the load",
            t.elapsed().as_secs_f64(),
            written_mb() - loaded_mb
        );
        reads("once idle");
    }
    if times.is_empty() {
        return;
    }
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    let pct = |p: f64| sorted[((sorted.len() - 1) as f64 * p) as usize];
    println!(
        "{commits} commits of {each} quads: {total_s:.2} s, p50 {:.0} ms, p95 {:.0} ms, max {:.0} ms",
        pct(0.5),
        pct(0.95),
        pct(1.0)
    );
}
