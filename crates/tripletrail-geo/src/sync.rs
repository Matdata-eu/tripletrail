//! Locks that stay usable after a panic.
//!
//! A panic while a `Mutex` or `RwLock` is held poisons it, and `lock().unwrap()` then panics
//! for every later caller. The server catches a panic per request, so one bad request would
//! leave the store, the caches or the spatial index failing for the rest of the process. The
//! state behind the locks of the tripletrail crates is replaced whole or changed in steps that
//! leave it consistent, so the guard of a poisoned lock is taken as it is.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// [`Mutex::lock`] without poisoning.
pub trait LockUnpoisoned<T: ?Sized> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockUnpoisoned<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// [`RwLock::read`] and [`RwLock::write`] without poisoning.
pub trait RwLockUnpoisoned<T: ?Sized> {
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T>;
    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockUnpoisoned<T> for RwLock<T> {
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_still_usable() {
        let m = Mutex::new(1);
        let r = RwLock::new(2);
        let _ = std::panic::catch_unwind(|| {
            let _m = m.lock().unwrap();
            let _r = r.write().unwrap();
            panic!("poison both");
        });
        assert!(m.is_poisoned() && r.is_poisoned());
        *m.lock_unpoisoned() += 1;
        *r.write_unpoisoned() += 1;
        assert_eq!((*m.lock_unpoisoned(), *r.read_unpoisoned()), (2, 3));
    }
}
