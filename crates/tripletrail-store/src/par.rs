//! Several cores for one query (#185): a range of rows split into consecutive chunks,
//! worked by the calling thread with help from a pool, the results back in chunk order. So
//! the result is the same, in the same order, as one pass over the whole range.
//!
//! Queries have a pool of their own: on rayon's global pool, a query's chunks queued behind
//! the store's background work after writes (compaction, index rebuilds), and ran slower
//! than on the request thread alone.

use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// The most threads one query uses; 0 is all cores.
static QUERY_THREADS: AtomicUsize = AtomicUsize::new(0);

/// The pool queries share, made on first use with [`query_threads`] threads.
static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();

fn pool() -> Option<&'static rayon::ThreadPool> {
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(query_threads())
            .thread_name(|i| format!("tripletrail-query-{i}"))
            .build()
            .ok()
    })
    .as_ref()
}

/// Below this many rows, work stays on the calling thread: handing chunks to the pool costs
/// more than it saves.
pub const PARALLEL_ROWS: usize = 65_536;

/// Sets the threads of the query pool (0: as many as cores; 1: queries run on their own
/// thread only). Set it before the first query: the pool is made once, at its first use. 1
/// still turns parallel work off later. Queries share the pool, so concurrent queries don't
/// start more threads than it has.
pub fn set_query_threads(threads: usize) {
    QUERY_THREADS.store(threads, Ordering::Relaxed);
}

/// The threads one query may use, at least 1.
pub fn query_threads() -> usize {
    match QUERY_THREADS.load(Ordering::Relaxed) {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    }
}

/// `f` over consecutive chunks of `0..n`, in order: one call on the whole range below
/// [`PARALLEL_ROWS`] rows or with one thread, else chunks of at least `PARALLEL_ROWS / 16`
/// rows (a few per thread, so an uneven chunk doesn't hold the others up).
///
/// The calling thread works through the chunks itself, and the query pool's threads only
/// help: each takes the next chunk not yet taken, if it gets to run. When the cores are busy
/// (the store's background work after writes), the query goes at the speed of its own
/// thread instead of waiting for pool threads to be scheduled.
pub fn chunks<T: Send>(n: usize, f: impl Fn(Range<usize>) -> T + Sync) -> Vec<T> {
    chunks_sized(n, PARALLEL_ROWS, PARALLEL_ROWS / 16, f)
}

/// [`chunks`] for rows that cost more: one call on the whole range below `parallel` rows,
/// else chunks of at least `least` rows.
pub fn chunks_sized<T: Send>(
    n: usize,
    parallel: usize,
    least: usize,
    f: impl Fn(Range<usize>) -> T + Sync,
) -> Vec<T> {
    let threads = query_threads();
    let pool = match pool() {
        Some(pool) if n >= parallel && threads > 1 => pool,
        _ => return vec![f(0..n)],
    };
    let helpers = pool.current_num_threads();
    let parts = ((helpers + 1) * 4).min(n / least.max(1)).max(1);
    let size = n.div_ceil(parts);
    // Fewer parts when the last ones would be empty (50 rows in 16 parts of 4: 13 parts).
    let parts = n.div_ceil(size);
    let next = AtomicUsize::new(0);
    let done: Vec<Mutex<Option<T>>> = (0..parts).map(|_| Mutex::new(None)).collect();
    let work = || {
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= parts {
                break;
            }
            let out = f(i * size..((i + 1) * size).min(n));
            *done[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
        }
    };
    // The helpers keep the caller's query memory limit (#189).
    let ids = crate::bgp::query_ids();
    let work = &work;
    pool.in_place_scope(|scope| {
        for _ in 0..helpers.min(parts - 1) {
            scope.spawn(move |_| crate::bgp::with_query_ids(ids, work));
        }
        work();
    });
    done.into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every chunk is done before the scope ends")
        })
        .collect()
}

/// `v` sorted, on the query pool from [`PARALLEL_ROWS`] items, else here. Unstable, so
/// for items whose equal ones can't be told apart (as `(key, row)` pairs): then the order is
/// the same with any number of threads.
pub fn sort_unstable<T: Ord + Send>(v: &mut [T]) {
    use rayon::slice::ParallelSliceMut;
    match pool() {
        Some(pool) if v.len() >= PARALLEL_ROWS && query_threads() > 1 => {
            pool.install(|| v.par_sort_unstable());
        }
        _ => v.sort_unstable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_the_range_in_order() {
        for n in [0, 1, 100, PARALLEL_ROWS - 1, PARALLEL_ROWS, 1_000_003] {
            let parts = chunks(n, |r| r);
            let mut next = 0;
            for r in &parts {
                assert_eq!(r.start, next);
                next = r.end;
            }
            assert_eq!(next, n);
        }
        // Small ranges in chunks of one row and more: none starts past the end (#4).
        for n in [2, 50, 61, 97] {
            let parts = chunks_sized(n, 2, 1, |r| r);
            assert!(
                parts.iter().all(|r| r.start < r.end && r.end <= n),
                "{n}: {parts:?}"
            );
            assert_eq!(parts.last().map(|r| r.end), Some(n));
        }
    }

    #[test]
    fn helpers_keep_the_callers_query_memory() {
        let caller = std::thread::current().id();
        let seen = crate::with_join_memory(64 << 10, || {
            chunks(1_000_003, |_| {
                // Slow enough for the helpers to take chunks too.
                std::thread::sleep(std::time::Duration::from_millis(2));
                (std::thread::current().id(), crate::join_memory())
            })
        });
        if query_threads() > 1 {
            assert!(seen.iter().any(|(t, _)| *t != caller), "no helper ran");
        }
        assert!(seen.iter().all(|&(_, l)| l == 64 << 10), "{seen:?}");
    }
}
