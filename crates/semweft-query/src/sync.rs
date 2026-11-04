use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks a mutex, continuing with the guarded value if a panicking thread poisoned it.
///
/// One panicked worker must not turn every later query into a panic as well.
pub(crate) trait MutexExt<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
