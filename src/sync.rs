//! Locking helpers shared by the daemon and the relay.

use std::sync::{Mutex, MutexGuard};

/// Extension trait for locking a [`Mutex`] while tolerating poisoning.
pub(crate) trait MutexExt<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    /// Lock the mutex, recovering the guard even if a previous holder panicked
    /// and poisoned it (`unwrap_or_else(|p| p.into_inner())`).
    ///
    /// Recovering is correct here because the guarded structures (session
    /// state, the pane runtime map, workspace/metadata caches, the relay's
    /// device store, etc.) remain structurally valid after a panic: a panic
    /// mid-update can at worst leave stale or partial data. That is strictly
    /// better than the alternative of `lock().unwrap()`, where a single
    /// panicking connection/pane thread would poison the mutex and cascade into
    /// every later lock panicking too, crashing the daemon and killing every
    /// user's session.
    ///
    /// Both long-lived servers here are thread-per-connection, which is exactly
    /// the shape that turns one poisoned mutex into a permanently bricked
    /// process, so both must use this rather than `lock().unwrap()`.
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_survives_a_poisoning_panic() {
        let lock = std::sync::Arc::new(Mutex::new(vec![1u8]));
        let poisoner = std::sync::Arc::clone(&lock);
        // Poison the mutex from another thread.
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison it");
        })
        .join();

        assert!(lock.lock().is_err(), "mutex should now be poisoned");
        // The whole point: a later lock still works instead of cascading.
        assert_eq!(*lock.lock_or_recover(), vec![1u8]);
    }
}
