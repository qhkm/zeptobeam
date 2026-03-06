use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Per-provider gate enforcing RPM/TPM limits and in-flight request caps.
///
/// Each LLM provider (e.g. OpenAI, Anthropic) gets its own gate that tracks:
/// - Requests per minute (RPM) via a semaphore
/// - Tokens per minute (TPM) via an atomic budget
/// - Concurrent in-flight requests via an atomic counter
pub struct ProviderGate {
    /// Semaphore with permits = max requests per minute
    rpm_semaphore: Arc<Semaphore>,
    /// Maximum RPM (for refill)
    max_rpm: u32,
    /// Tokens per minute budget (refilled every minute)
    tpm_budget: AtomicU64,
    /// Max TPM (for refill)
    max_tpm: u64,
    /// Current in-flight LLM requests
    in_flight: AtomicU32,
    /// Maximum concurrent in-flight requests
    max_in_flight: u32,
}

impl ProviderGate {
    /// Create a new provider gate with the given limits.
    ///
    /// - `max_rpm`: maximum requests per minute
    /// - `max_tpm`: maximum tokens per minute
    /// - `max_in_flight`: maximum concurrent in-flight requests
    pub fn new(max_rpm: u32, max_tpm: u64, max_in_flight: u32) -> Self {
        Self {
            rpm_semaphore: Arc::new(Semaphore::new(max_rpm as usize)),
            max_rpm,
            tpm_budget: AtomicU64::new(max_tpm),
            max_tpm,
            in_flight: AtomicU32::new(0),
            max_in_flight,
        }
    }

    /// Try to acquire a slot for an LLM request.
    ///
    /// Checks (in order):
    /// 1. In-flight count < max_in_flight
    /// 2. RPM semaphore has available permits
    /// 3. TPM budget has enough tokens for `estimated_tokens`
    ///
    /// Returns a [`ProviderPermit`] guard that releases the in-flight slot
    /// when dropped. The RPM permit is permanently consumed (not returned on
    /// drop) -- permits are only restored by [`refill`].
    ///
    /// Returns `None` if any check fails. On failure, no resources are consumed.
    pub fn try_acquire(&self, estimated_tokens: u64) -> Option<ProviderPermit<'_>> {
        // Check in-flight limit
        let current = self.in_flight.load(Ordering::Relaxed);
        if current >= self.max_in_flight {
            return None;
        }

        // Try RPM semaphore (non-blocking)
        let permit = self.rpm_semaphore.clone().try_acquire_owned().ok()?;

        // Check TPM budget
        let remaining = self.tpm_budget.load(Ordering::Relaxed);
        if estimated_tokens > remaining {
            // Drop permit to release RPM slot back (request didn't happen)
            drop(permit);
            return None;
        }

        // Deduct TPM budget
        self.tpm_budget.fetch_sub(estimated_tokens, Ordering::Relaxed);

        // Increment in-flight
        self.in_flight.fetch_add(1, Ordering::Relaxed);

        // Forget the semaphore permit so it is NOT returned on drop.
        // RPM permits are consumed for the minute and only restored by refill().
        permit.forget();

        Some(ProviderPermit {
            in_flight: &self.in_flight,
        })
    }

    /// Refill RPM and TPM budgets. Call once per minute.
    ///
    /// Adds back consumed RPM permits (up to max_rpm) and resets the TPM
    /// budget to max_tpm.
    pub fn refill(&self) {
        // Add permits back to semaphore
        let available = self.rpm_semaphore.available_permits();
        let to_add = (self.max_rpm as usize).saturating_sub(available);
        self.rpm_semaphore.add_permits(to_add);

        // Reset TPM budget
        self.tpm_budget.store(self.max_tpm, Ordering::Relaxed);
    }

    /// Get current number of in-flight requests.
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Get available RPM permits.
    pub fn available_rpm(&self) -> usize {
        self.rpm_semaphore.available_permits()
    }

    /// Get remaining TPM budget.
    pub fn remaining_tpm(&self) -> u64 {
        self.tpm_budget.load(Ordering::Relaxed)
    }
}

/// RAII guard that decrements the in-flight count when dropped.
///
/// The RPM permit is intentionally *not* held here -- it is consumed
/// (forgotten) on acquisition and only restored by [`ProviderGate::refill`].
pub struct ProviderPermit<'a> {
    in_flight: &'a AtomicU32,
}

impl<'a> Drop for ProviderPermit<'a> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acquire_and_release() {
        let gate = ProviderGate::new(10, 1000, 5);
        let permit = gate.try_acquire(100).unwrap();
        assert_eq!(gate.in_flight(), 1);
        assert_eq!(gate.available_rpm(), 9);
        assert_eq!(gate.remaining_tpm(), 900);
        drop(permit);
        assert_eq!(gate.in_flight(), 0);
        // RPM permit is NOT refilled on drop (consumed for the minute)
        assert_eq!(gate.available_rpm(), 9);
    }

    #[test]
    fn test_rpm_limit() {
        let gate = ProviderGate::new(2, 100_000, 100);
        let _p1 = gate.try_acquire(10).unwrap();
        let _p2 = gate.try_acquire(10).unwrap();
        // 3rd should fail (RPM exhausted)
        assert!(gate.try_acquire(10).is_none());
    }

    #[test]
    fn test_tpm_limit() {
        let gate = ProviderGate::new(100, 500, 100);
        let _p1 = gate.try_acquire(300).unwrap();
        // 200 TPM remaining
        assert!(gate.try_acquire(201).is_none());
        let _p2 = gate.try_acquire(200).unwrap();
    }

    #[test]
    fn test_in_flight_limit() {
        let gate = ProviderGate::new(100, 100_000, 2);
        let _p1 = gate.try_acquire(10).unwrap();
        let _p2 = gate.try_acquire(10).unwrap();
        // 3rd should fail (in-flight limit)
        assert!(gate.try_acquire(10).is_none());
        drop(_p1);
        // Now should succeed
        let _p3 = gate.try_acquire(10).unwrap();
    }

    #[test]
    fn test_refill() {
        let gate = ProviderGate::new(3, 1000, 100);
        let _p1 = gate.try_acquire(400).unwrap();
        let _p2 = gate.try_acquire(400).unwrap();
        drop(_p1);
        drop(_p2);
        // RPM: 1 remaining, TPM: 200 remaining
        assert_eq!(gate.available_rpm(), 1);
        assert_eq!(gate.remaining_tpm(), 200);

        gate.refill();
        assert_eq!(gate.available_rpm(), 3);
        assert_eq!(gate.remaining_tpm(), 1000);
    }

    #[test]
    fn test_tpm_not_deducted_on_rpm_failure() {
        let gate = ProviderGate::new(1, 1000, 100);
        let _p1 = gate.try_acquire(100).unwrap();
        // RPM exhausted, TPM should NOT be deducted
        assert!(gate.try_acquire(100).is_none());
        assert_eq!(gate.remaining_tpm(), 900); // only the first 100 deducted
    }

    #[test]
    fn test_rpm_not_consumed_on_tpm_failure() {
        let gate = ProviderGate::new(10, 50, 100);
        let _p1 = gate.try_acquire(50).unwrap();
        // RPM: 9 remaining (1 consumed by _p1, forgotten)
        assert_eq!(gate.available_rpm(), 9);
        // TPM exhausted -- RPM permit should be released back (not consumed)
        assert!(gate.try_acquire(1).is_none());
        // The failed attempt should have returned the RPM permit
        assert_eq!(gate.available_rpm(), 9);
    }

    #[test]
    fn test_zero_token_request() {
        let gate = ProviderGate::new(10, 1000, 5);
        let _p = gate.try_acquire(0).unwrap();
        assert_eq!(gate.remaining_tpm(), 1000);
        assert_eq!(gate.in_flight(), 1);
    }

    #[test]
    fn test_multiple_acquire_release_cycles() {
        let gate = ProviderGate::new(100, 10_000, 3);
        // Fill up in-flight
        let p1 = gate.try_acquire(100).unwrap();
        let p2 = gate.try_acquire(100).unwrap();
        let p3 = gate.try_acquire(100).unwrap();
        assert_eq!(gate.in_flight(), 3);
        assert!(gate.try_acquire(100).is_none());

        // Release one, acquire another
        drop(p2);
        assert_eq!(gate.in_flight(), 2);
        let _p4 = gate.try_acquire(100).unwrap();
        assert_eq!(gate.in_flight(), 3);

        drop(p1);
        drop(p3);
        assert_eq!(gate.in_flight(), 1);
    }
}
