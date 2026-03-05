#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Configuration for deterministic fault injection.
#[derive(Debug, Clone)]
pub struct FaultConfig {
  pub fail_rate: f64,
  pub seed: u64,
  pub modes: Vec<FaultMode>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FaultMode {
  Error,
  Timeout,
  CorruptData,
}

impl FaultConfig {
  /// Deterministically decide whether attempt N fails.
  pub fn should_fail(&self, attempt: u64) -> bool {
    if self.fail_rate <= 0.0 {
      return false;
    }
    if self.fail_rate >= 1.0 {
      return true;
    }
    let hash = self
      .seed
      .wrapping_mul(6364136223846793005)
      .wrapping_add(attempt);
    let normalized = (hash % 10_000) as f64 / 10_000.0;
    normalized < self.fail_rate
  }

  pub fn pick_mode(&self, attempt: u64) -> &FaultMode {
    static DEFAULT_MODE: FaultMode = FaultMode::Error;
    if self.modes.is_empty() {
      return &DEFAULT_MODE;
    }
    &self.modes[(attempt as usize) % self.modes.len()]
  }
}

/// CheckpointStore wrapper that injects deterministic failures.
#[cfg(test)]
pub struct FaultyCheckpointStore<S: crate::agent_rt::checkpoint::CheckpointStore> {
  inner: S,
  config: FaultConfig,
  attempt: AtomicU64,
}

#[cfg(test)]
impl<S: crate::agent_rt::checkpoint::CheckpointStore> FaultyCheckpointStore<S> {
  pub fn new(inner: S, config: FaultConfig) -> Self {
    Self {
      inner,
      config,
      attempt: AtomicU64::new(0),
    }
  }
}

#[cfg(test)]
use crate::agent_rt::error::AgentRtError;

#[cfg(test)]
impl<S: crate::agent_rt::checkpoint::CheckpointStore> crate::agent_rt::checkpoint::CheckpointStore
  for FaultyCheckpointStore<S>
{
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError> {
    let attempt = self.attempt.fetch_add(1, Ordering::Relaxed);
    if self.config.should_fail(attempt) {
      return Err(AgentRtError::Checkpoint("chaos: injected save failure".into()));
    }
    self.inner.save(key, checkpoint)
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError> {
    let attempt = self.attempt.fetch_add(1, Ordering::Relaxed);
    if self.config.should_fail(attempt) {
      let mode = self.config.pick_mode(attempt);
      if *mode == FaultMode::CorruptData {
        return Ok(Some(serde_json::json!({"corrupted": true})));
      }
      return Err(AgentRtError::Checkpoint("chaos: injected load failure".into()));
    }
    self.inner.load(key)
  }

  fn delete(&self, key: &str) -> Result<(), AgentRtError> {
    let attempt = self.attempt.fetch_add(1, Ordering::Relaxed);
    if self.config.should_fail(attempt) {
      return Err(AgentRtError::Checkpoint("chaos: injected delete failure".into()));
    }
    self.inner.delete(key)
  }
}

/// Runtime chaos configuration from environment.
#[cfg(feature = "chaos_testing")]
pub struct ChaosConfig {
  pub enabled: bool,
  pub fail_rate: f64,
  pub seed: u64,
  pub modes: Vec<FaultMode>,
}

#[cfg(feature = "chaos_testing")]
impl ChaosConfig {
  pub fn from_env() -> Self {
    let enabled = std::env::var("ZEPTOCLAW_CHAOS_ENABLED")
      .ok()
      .map(|v| v == "true" || v == "1")
      .unwrap_or(false);
    let fail_rate = std::env::var("ZEPTOCLAW_CHAOS_FAIL_RATE")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(0.05);
    let seed = std::env::var("ZEPTOCLAW_CHAOS_SEED")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(42);
    let modes = std::env::var("ZEPTOCLAW_CHAOS_MODES")
      .ok()
      .map(|v| {
        v.split(',')
          .filter_map(|s| match s.trim() {
            "error" => Some(FaultMode::Error),
            "timeout" => Some(FaultMode::Timeout),
            "corrupt" => Some(FaultMode::CorruptData),
            _ => None,
          })
          .collect()
      })
      .unwrap_or_else(|| vec![FaultMode::Error]);
    Self {
      enabled,
      fail_rate,
      seed,
      modes,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::checkpoint::{CheckpointStore, InMemoryCheckpointStore};

  #[test]
  fn test_fault_config_deterministic() {
    let config = FaultConfig {
      fail_rate: 0.5,
      seed: 42,
      modes: vec![FaultMode::Error],
    };
    let r1 = config.should_fail(0);
    let r2 = config.should_fail(0);
    assert_eq!(r1, r2, "deterministic with same seed+attempt");
  }

  #[test]
  fn test_fault_config_zero_rate_never_fails() {
    let config = FaultConfig {
      fail_rate: 0.0,
      seed: 42,
      modes: vec![FaultMode::Error],
    };
    for i in 0..100 {
      assert!(!config.should_fail(i));
    }
  }

  #[test]
  fn test_fault_config_full_rate_always_fails() {
    let config = FaultConfig {
      fail_rate: 1.0,
      seed: 42,
      modes: vec![FaultMode::Error],
    };
    for i in 0..100 {
      assert!(config.should_fail(i));
    }
  }

  #[test]
  fn test_faulty_checkpoint_store_injects_failures() {
    let inner = InMemoryCheckpointStore::new();
    let store = FaultyCheckpointStore::new(
      inner,
      FaultConfig {
        fail_rate: 1.0,
        seed: 0,
        modes: vec![FaultMode::Error],
      },
    );
    let result = store.save("key", &serde_json::json!({"v": 1}));
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("chaos"));
  }

  #[test]
  fn test_faulty_checkpoint_store_passes_when_no_fault() {
    let inner = InMemoryCheckpointStore::new();
    let store = FaultyCheckpointStore::new(
      inner,
      FaultConfig {
        fail_rate: 0.0,
        seed: 0,
        modes: vec![FaultMode::Error],
      },
    );
    store.save("key", &serde_json::json!({"v": 1})).unwrap();
    let loaded = store.load("key").unwrap().unwrap();
    assert_eq!(loaded["v"], 1);
  }
}
