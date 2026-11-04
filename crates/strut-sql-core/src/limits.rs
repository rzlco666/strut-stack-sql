//! Per-principal concurrency limit (R2-08).
//!
//! A pool is shared by every caller of a source. Without a per-principal
//! ceiling, one busy principal can hold every connection and starve the rest.
//! Each principal gets its own semaphore sized to the pool; a query that cannot
//! get a permit fails fast with a retryable error instead of queueing forever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug)]
pub struct PrincipalLimiter {
    per_principal: usize,
    permits: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl PrincipalLimiter {
    /// `per_principal = 0` disables limiting.
    pub fn new(per_principal: usize) -> Self {
        Self {
            per_principal,
            permits: Mutex::new(HashMap::new()),
        }
    }

    pub fn per_principal(&self) -> usize {
        self.per_principal
    }

    /// Try to reserve one slot for this principal. `None` means the principal
    /// already holds its whole allowance.
    pub fn try_acquire(&self, principal: &str) -> Option<OwnedSemaphorePermit> {
        if self.per_principal == 0 {
            return None;
        }
        let semaphore = {
            let mut map = self.permits.lock().expect("limit map poisoned");
            map.entry(principal.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(self.per_principal)))
                .clone()
        };
        semaphore.try_acquire_owned().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_principal_cannot_take_more_than_its_allowance() {
        let limiter = PrincipalLimiter::new(2);
        let a = limiter.try_acquire("alice").expect("first");
        let b = limiter.try_acquire("alice").expect("second");
        assert!(
            limiter.try_acquire("alice").is_none(),
            "third must be refused"
        );
        // Another principal is unaffected.
        let c = limiter.try_acquire("bob").expect("other principal");
        drop((a, b, c));
        assert!(
            limiter.try_acquire("alice").is_some(),
            "permits released on drop"
        );
    }

    #[test]
    fn zero_disables_the_limit() {
        let limiter = PrincipalLimiter::new(0);
        assert!(limiter.try_acquire("alice").is_none());
    }
}
