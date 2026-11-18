//! Threads for queries without a thread spawn per request (#186): a pool of threads with a
//! deep stack, and one timer thread for every query deadline. Spawning a thread with a
//! 256 MB stack (and a second one to wait for the deadline) took a fifth of the server's
//! time on small queries.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tripletrail::oxigraph::sparql::CancellationToken;

/// How long a pool thread waits for work before it ends.
const IDLE: Duration = Duration::from_secs(30);
/// Pool threads kept waiting at most; the others end after their job.
const MAX_IDLE: usize = 64;

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Pool {
    state: Mutex<PoolState>,
    ready: Condvar,
}

#[derive(Default)]
struct PoolState {
    jobs: VecDeque<Job>,
    /// Threads waiting for a job.
    idle: usize,
}

static POOL: LazyLock<Pool> = LazyLock::new(Pool::default);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Spawns a thread that parses, plans, evaluates or serialises queries, with a stack deep
/// enough for the largest query [`tripletrail::complexity`] lets through (#483). Without it, a
/// nested query aborts the process on the default 2 MB stack.
pub(crate) fn spawn_deep<F: FnOnce() + Send + 'static>(f: F) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .stack_size(tripletrail::complexity::QUERY_STACK)
        .spawn(f)
        .expect("spawning a thread")
}

/// Runs `f` on a thread with the stack of [`spawn_deep`], right away: on a waiting pool
/// thread if there is one, else on a new one. `f` may block for as long as it likes (it
/// writes a response body the client reads), since a job never waits for another.
pub(crate) fn run_deep<F: FnOnce() + Send + 'static>(f: F) {
    let pool = &*POOL;
    let mut state = lock(&pool.state);
    // Each queued job has a waiting thread of its own.
    if state.idle > state.jobs.len() {
        state.jobs.push_back(Box::new(f));
        drop(state);
        pool.ready.notify_one();
        return;
    }
    drop(state);
    spawn_deep(move || {
        f();
        let mut state = lock(&pool.state);
        loop {
            if state.idle >= MAX_IDLE {
                return;
            }
            state.idle += 1;
            let job = loop {
                if let Some(job) = state.jobs.pop_front() {
                    break Some(job);
                }
                let (next, wait) = pool
                    .ready
                    .wait_timeout(state, IDLE)
                    .unwrap_or_else(PoisonError::into_inner);
                state = next;
                if wait.timed_out() && state.jobs.is_empty() {
                    break None;
                }
            };
            state.idle -= 1;
            let Some(job) = job else { return };
            drop(state);
            // A panic ends this thread, as it ended the thread of its own before.
            job();
            state = lock(&pool.state);
        }
    });
}

/// Cancels a token at a time unless dropped first.
pub(crate) struct Deadline(Option<(Instant, u64)>);

impl Deadline {
    pub(crate) const NONE: Self = Self(None);

    /// Cancels `token` after `timeout`.
    pub(crate) fn after(timeout: Duration, token: CancellationToken) -> Self {
        let timer = &*TIMER;
        // A timeout too far ahead to be an `Instant` never comes.
        let Some(at) = Instant::now().checked_add(timeout) else {
            return Self::NONE;
        };
        let mut state = lock(&timer.state);
        let id = state.next;
        state.next += 1;
        let first = state
            .due
            .first_key_value()
            .is_none_or(|(k, _)| (at, id) < *k);
        state.due.insert((at, id), token);
        if !state.running {
            state.running = true;
            thread::Builder::new()
                .name("deadlines".into())
                .spawn(|| TIMER.run())
                .expect("spawning a thread");
        } else if first {
            timer.changed.notify_one();
        }
        Self(Some((at, id)))
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            lock(&TIMER.state).due.remove(&key);
        }
    }
}

#[derive(Default)]
struct Timer {
    state: Mutex<TimerState>,
    changed: Condvar,
}

#[derive(Default)]
struct TimerState {
    due: BTreeMap<(Instant, u64), CancellationToken>,
    next: u64,
    running: bool,
}

static TIMER: LazyLock<Timer> = LazyLock::new(Timer::default);

impl Timer {
    fn run(&self) {
        let mut state = lock(&self.state);
        loop {
            let now = Instant::now();
            while let Some(entry) = state.due.first_entry() {
                if entry.key().0 > now {
                    break;
                }
                entry.remove().cancel();
            }
            state = match state.due.first_key_value() {
                Some(((at, _), _)) => {
                    let wait = at.saturating_duration_since(now);
                    self.changed
                        .wait_timeout(state, wait)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn deadlines_cancel_in_time_order_and_not_when_dropped() {
        let late = CancellationToken::new();
        let early = CancellationToken::new();
        let dropped = CancellationToken::new();
        let _late = Deadline::after(Duration::from_millis(400), late.clone());
        let _early = Deadline::after(Duration::from_millis(50), early.clone());
        drop(Deadline::after(Duration::from_millis(10), dropped.clone()));
        let start = Instant::now();
        while !early.is_cancelled() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "early never cancelled"
            );
            thread::yield_now();
        }
        assert!(!late.is_cancelled());
        while !late.is_cancelled() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "late never cancelled"
            );
            thread::yield_now();
        }
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert!(!dropped.is_cancelled());
        let _never = Deadline::after(Duration::MAX, CancellationToken::new());
    }

    #[test]
    fn jobs_that_block_do_not_hold_up_others() {
        let (done_tx, done_rx) = mpsc::channel();
        // Jobs that all block until the last one starts: a job waiting for a thread would
        // hold them all up.
        let gate = std::sync::Arc::new((Mutex::new(0usize), Condvar::new()));
        for _ in 0..50 {
            let gate = std::sync::Arc::clone(&gate);
            let done = done_tx.clone();
            run_deep(move || {
                let (count, all) = &*gate;
                let mut n = count.lock().unwrap();
                *n += 1;
                all.notify_all();
                let (_n, wait) = all
                    .wait_timeout_while(n, Duration::from_secs(5), |n| *n < 50)
                    .unwrap();
                let _ = done.send(!wait.timed_out());
            });
        }
        for _ in 0..50 {
            assert_eq!(done_rx.recv_timeout(Duration::from_secs(10)), Ok(true));
        }
    }

    #[test]
    fn jobs_have_a_deep_stack() {
        fn depth(n: u32) -> u32 {
            let pad = std::hint::black_box([0u8; 1024]);
            if n == 0 {
                u32::from(pad[0])
            } else {
                depth(n - 1) + 1
            }
        }
        let (tx, rx) = mpsc::channel();
        // 16 MB of stack, more than a default thread has.
        run_deep(move || tx.send(depth(16 * 1024)).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 16 * 1024);
    }
}
