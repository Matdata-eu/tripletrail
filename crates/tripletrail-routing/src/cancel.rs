//! Cooperative cancellation for long-running algorithms.
//!
//! A caller (the `pgr` SERVICE, with the query's timeout) runs an algorithm inside
//! [`scope`] with a check; the algorithms call [`stopped`] in their main loops and end early
//! once it says so. Their result is then incomplete, so the caller must test the check itself
//! afterwards and discard it. Outside a scope [`stopped`] is always `false`.
//!
//! The check is per thread: work handed to other threads takes it along with [`current`] and
//! a [`scope`] of its own.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

/// Whether the work should stop. Must be cheap and safe to call from any thread.
pub type Check = Arc<dyn Fn() -> bool + Send + Sync>;

/// How many calls to [`stopped`] go by between two calls of the check.
const EVERY: u32 = 1024;

thread_local! {
    static CHECK: RefCell<Option<Check>> = const { RefCell::new(None) };
    static COUNT: Cell<u32> = const { Cell::new(0) };
    static STOPPED: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` with `check` as this thread's cancellation check, then restores the previous one
/// (also when `f` panics).
pub fn scope<R>(check: Option<Check>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Check>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            CHECK.with(|c| *c.borrow_mut() = self.0.take());
            STOPPED.with(|s| s.set(self.1));
        }
    }
    let previous = CHECK.with(|c| c.replace(check));
    let _restore = Restore(previous, STOPPED.with(|s| s.replace(false)));
    f()
}

/// This thread's check, to pass to the threads it hands work to.
pub fn current() -> Option<Check> {
    CHECK.with(|c| c.borrow().clone())
}

/// Whether the current work should stop. Calls the check only every [`EVERY`] calls, so it
/// can sit in an inner loop; once it said yes, stays yes until the scope ends.
#[inline]
pub fn stopped() -> bool {
    if STOPPED.with(Cell::get) {
        return true;
    }
    let n = COUNT.with(|c| {
        let n = c.get().wrapping_add(1);
        c.set(n);
        n
    });
    if !n.is_multiple_of(EVERY) {
        return false;
    }
    stopped_now()
}

/// [`stopped`], calling the check now: for outer loops whose iterations are long.
pub fn stopped_now() -> bool {
    if STOPPED.with(Cell::get) {
        return true;
    }
    let hit = CHECK.with(|c| c.borrow().as_ref().is_some_and(|check| check()));
    if hit {
        STOPPED.with(|s| s.set(true));
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn stops_inside_the_scope_only() {
        let flag = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&flag);
        let check: Check = Arc::new(move || f.load(Ordering::Relaxed));
        assert!(!stopped_now());
        scope(Some(check), || {
            assert!(!stopped_now());
            flag.store(true, Ordering::Relaxed);
            assert!((0..=EVERY).any(|_| stopped()));
            assert!(stopped());
            let inner = current();
            assert!(inner.is_some());
            // A nested scope without a check does not stop, and the outer state comes back.
            scope(None, || assert!(!stopped_now()));
            assert!(stopped());
        });
        assert!(!stopped_now());
        assert!(!stopped());
    }
}
