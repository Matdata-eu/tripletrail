//! Times opening a dataset and building its spatial index: `index_build <dir> [runs]`.
use std::time::Instant;

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("RssAnon:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("a dataset directory");
    let runs: usize = args.next().map_or(3, |r| r.parse().unwrap());
    let t = Instant::now();
    let engine = tripletrail::Engine::open_tripletrail(&dir).unwrap();
    println!("open {:?}, rss {} MB", t.elapsed(), rss_mb());
    let mem = |e: &tripletrail::Engine| match e.store() {
        tripletrail_store::Dataset::TripleTrail(s) => format!("{:?}", s.memory_use()),
        _ => String::new(),
    };
    println!("mem {}", mem(&engine));
    for _ in 0..runs {
        engine.set_indexed_predicates(Default::default());
        let t = Instant::now();
        let index = engine.spatial_index().unwrap();
        println!(
            "index {:?} ({} geometries, stats.build {:?}), rss {} MB",
            t.elapsed(),
            index.stats().geometries,
            index.stats().build,
            rss_mb()
        );
        println!("mem {}", mem(&engine));
    }
}
