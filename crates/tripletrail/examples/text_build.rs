//! Times building the full-text index of a store and measures its heap (#394):
//! `text_build <dir> [data.nt | -] [runs]`. With a data file, the store at `<dir>` is made from it
//! first. Prints, for each build, its time, the heap it peaked at above what was allocated
//! before it, and the heap it keeps (counted by a wrapping allocator, so exact).
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = NOW.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        NOW.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, size) };
        if !q.is_null() {
            if size >= layout.size() {
                let now = NOW.fetch_add(size - layout.size(), Relaxed) + size - layout.size();
                PEAK.fetch_max(now, Relaxed);
            } else {
                NOW.fetch_sub(layout.size() - size, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("a store directory");
    let data = args.next().filter(|d| d != "-");
    let runs: usize = args.next().map_or(3, |r| r.parse().unwrap());
    let store = tripletrail_store::Store::open(&dir).unwrap();
    if let Some(data) = data {
        let t = Instant::now();
        let file = std::io::BufReader::new(std::fs::File::open(&data).unwrap());
        let n = store
            .bulk_load(oxigraph::io::RdfFormat::NTriples, file)
            .unwrap();
        println!("loaded {n} quads in {:?}", t.elapsed());
    }
    for _ in 0..runs {
        let before = NOW.load(Relaxed);
        PEAK.store(before, Relaxed);
        let t = Instant::now();
        let index = tripletrail::text::TextIndex::build(&store).unwrap();
        let took = t.elapsed();
        let peak = PEAK.load(Relaxed) - before;
        let kept = NOW.load(Relaxed) - before;
        println!(
            "build {took:?}: {} documents, peak {:.1} MiB, kept {:.1} MiB ({:.2}x), heap_size {:.1} MiB",
            index.len(),
            mib(peak),
            mib(kept),
            peak as f64 / kept.max(1) as f64,
            mib(index.heap_size()),
        );
        // What a few searches find, to compare builds by.
        for q in [
            "bahnhof", "berlin", "hbf", "tunnel", "strecke", "gleis 1", "brücke",
        ] {
            let hits = index.search(q, None, |_, _| true);
            let sum: f64 = hits.iter().map(|h| h.score).sum();
            let first: Vec<_> = hits
                .iter()
                .take(3)
                .map(|h| (h.subject, h.literal))
                .collect();
            println!(
                "  {q}: {} hits, score sum {sum:.9}, first {first:?}",
                hits.len()
            );
        }
        drop(index);
    }
}
