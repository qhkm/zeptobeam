use std::{
  collections::{HashMap, VecDeque},
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use crate::agent_rt::observability::RuntimeMetrics;
use tracing::debug;

/// Per-provider rate limiter using a sliding window.
///
/// Each provider has a maximum number of requests allowed
/// within a time window. Requests beyond the limit are
/// rejected with the duration to wait before retrying.
#[derive(Clone)]
pub struct RateLimiter {
  inner: Arc<Mutex<RateLimiterInner>>,
  metrics: Arc<Mutex<Option<Arc<RuntimeMetrics>>>>,
}

struct RateLimiterInner {
  providers: HashMap<String, ProviderLimit>,
  default_limit: Option<(u32, Duration)>,
}

struct ProviderLimit {
  max_requests: u32,
  window: Duration,
  timestamps: VecDeque<Instant>,
}

impl RateLimiter {
  pub fn new() -> Self {
    Self {
      inner: Arc::new(Mutex::new(RateLimiterInner {
        providers: HashMap::new(),
        default_limit: None,
      })),
      metrics: Arc::new(Mutex::new(None)),
    }
  }

  pub fn set_metrics(&self, metrics: Arc<RuntimeMetrics>) {
    *self.metrics.lock().unwrap() = Some(metrics);
  }

  /// Set a rate limit for a specific provider.
  pub fn set_limit(&self, provider: &str, max_requests: u32, window: Duration) {
    let mut inner = self.inner.lock().unwrap();
    inner.providers.insert(
      provider.to_string(),
      ProviderLimit {
        max_requests,
        window,
        timestamps: VecDeque::new(),
      },
    );
  }

  /// Set a default rate limit applied to any provider
  /// without an explicit limit.
  pub fn set_default_limit(&self, max_requests: u32, window: Duration) {
    let mut inner = self.inner.lock().unwrap();
    inner.default_limit = Some((max_requests, window));
  }

  /// Try to acquire a permit for the given provider.
  /// Returns `Ok(())` if the request is allowed, or
  /// `Err(wait_duration)` if the caller should wait.
  pub fn try_acquire(&self, provider: &str) -> Result<(), Duration> {
    let mut inner = self.inner.lock().unwrap();
    let now = Instant::now();

    if !inner.providers.contains_key(provider) {
      if let Some((max_req, window)) = inner.default_limit {
        inner.providers.insert(
          provider.to_string(),
          ProviderLimit {
            max_requests: max_req,
            window,
            timestamps: VecDeque::new(),
          },
        );
      } else {
        return Ok(());
      }
    }

    let limit = inner.providers.get_mut(provider).unwrap();

    // Evict timestamps outside the window
    while let Some(&front) = limit.timestamps.front() {
      if now.duration_since(front) >= limit.window {
        limit.timestamps.pop_front();
      } else {
        break;
      }
    }

    if limit.timestamps.len() < limit.max_requests as usize {
      limit.timestamps.push_back(now);
      Ok(())
    } else {
      let oldest = *limit.timestamps.front().unwrap();
      let wait = limit.window - now.duration_since(oldest);
      if let Some(metrics) = self.metrics.lock().unwrap().clone() {
        metrics.record_rate_limiter_rejection();
      }
      Err(wait)
    }
  }

  /// Blocking acquire: waits until a permit is available.
  /// Suitable for use inside `spawn_blocking`.
  pub fn acquire_blocking(&self, provider: &str) {
    loop {
      match self.try_acquire(provider) {
        Ok(()) => return,
        Err(wait) => {
          if let Some(metrics) = self.metrics.lock().unwrap().clone() {
            metrics.record_rate_limiter_wait(wait);
          }
          debug!(
            provider = provider,
            wait_ms = wait.as_millis() as u64,
            "rate limiter wait"
          );
          std::thread::sleep(wait)
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_rate_limiter_allows_within_limit() {
    let rl = RateLimiter::new();
    rl.set_limit("openai", 3, Duration::from_secs(1));

    assert!(rl.try_acquire("openai").is_ok());
    assert!(rl.try_acquire("openai").is_ok());
    assert!(rl.try_acquire("openai").is_ok());
  }

  #[test]
  fn test_rate_limiter_rejects_over_limit() {
    let rl = RateLimiter::new();
    rl.set_limit("openai", 2, Duration::from_secs(1));

    assert!(rl.try_acquire("openai").is_ok());
    assert!(rl.try_acquire("openai").is_ok());
    let result = rl.try_acquire("openai");
    assert!(result.is_err(), "Third request should be rejected");
    let wait = result.unwrap_err();
    assert!(wait > Duration::ZERO, "Wait duration should be positive");
    assert!(wait <= Duration::from_secs(1));
  }

  #[test]
  fn test_rate_limiter_window_expires() {
    let rl = RateLimiter::new();
    rl.set_limit("anthropic", 1, Duration::from_millis(50));

    assert!(rl.try_acquire("anthropic").is_ok());
    assert!(rl.try_acquire("anthropic").is_err());

    std::thread::sleep(Duration::from_millis(60));

    assert!(
      rl.try_acquire("anthropic").is_ok(),
      "Should allow after window expires"
    );
  }

  #[test]
  fn test_rate_limiter_independent_providers() {
    let rl = RateLimiter::new();
    rl.set_limit("openai", 1, Duration::from_secs(10));
    rl.set_limit("anthropic", 1, Duration::from_secs(10));

    assert!(rl.try_acquire("openai").is_ok());
    assert!(rl.try_acquire("openai").is_err());

    assert!(
      rl.try_acquire("anthropic").is_ok(),
      "Different provider should have its own quota"
    );
  }

  #[test]
  fn test_rate_limiter_unknown_provider_no_default() {
    let rl = RateLimiter::new();
    assert!(
      rl.try_acquire("unknown").is_ok(),
      "Unknown provider with no default should be allowed"
    );
  }

  #[test]
  fn test_rate_limiter_default_limit_applied() {
    let rl = RateLimiter::new();
    rl.set_default_limit(1, Duration::from_secs(10));

    assert!(rl.try_acquire("any_provider").is_ok());
    assert!(
      rl.try_acquire("any_provider").is_err(),
      "Default limit should apply to unknown providers"
    );
  }

  #[test]
  fn test_rate_limiter_explicit_overrides_default() {
    let rl = RateLimiter::new();
    rl.set_default_limit(1, Duration::from_secs(10));
    rl.set_limit("openai", 3, Duration::from_secs(10));

    assert!(rl.try_acquire("openai").is_ok());
    assert!(rl.try_acquire("openai").is_ok());
    assert!(rl.try_acquire("openai").is_ok());
    assert!(
      rl.try_acquire("openai").is_err(),
      "Should use explicit limit of 3, not default of 1"
    );
  }

  #[test]
  fn test_rate_limiter_clone_shares_state() {
    let rl = RateLimiter::new();
    rl.set_limit("openai", 2, Duration::from_secs(10));

    let rl2 = rl.clone();
    assert!(rl.try_acquire("openai").is_ok());
    assert!(rl2.try_acquire("openai").is_ok());
    assert!(
      rl.try_acquire("openai").is_err(),
      "Cloned limiter should share state"
    );
  }

  #[test]
  fn test_acquire_blocking_waits() {
    let rl = RateLimiter::new();
    rl.set_limit("fast", 1, Duration::from_millis(30));

    assert!(rl.try_acquire("fast").is_ok());

    let start = Instant::now();
    rl.acquire_blocking("fast");
    let elapsed = start.elapsed();

    assert!(
      elapsed >= Duration::from_millis(20),
      "acquire_blocking should have waited ~30ms, waited {:?}",
      elapsed
    );
  }
}
