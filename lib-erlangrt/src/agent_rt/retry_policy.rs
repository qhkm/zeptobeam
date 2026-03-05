use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum RetryStrategy {
  None,
  Immediate {
    max_attempts: u32,
  },
  Backoff {
    max_attempts: u32,
    base_ms: u64,
    max_ms: u64,
  },
  Skip,
}

impl Default for RetryStrategy {
  fn default() -> Self {
    RetryStrategy::None
  }
}

pub struct RetryTracker {
  attempts: HashMap<String, u32>,
  default_strategy: RetryStrategy,
  per_task: HashMap<String, RetryStrategy>,
}

impl RetryTracker {
  pub fn new(default_strategy: RetryStrategy) -> Self {
    Self {
      attempts: HashMap::new(),
      default_strategy,
      per_task: HashMap::new(),
    }
  }

  pub fn set_task_strategy(&mut self, task_id: &str, strategy: RetryStrategy) {
    self.per_task.insert(task_id.to_string(), strategy);
  }

  /// Returns Some(delay_ms) if retry, None if exhausted.
  pub fn should_retry(&mut self, task_id: &str) -> Option<u64> {
    let strategy = self
      .per_task
      .get(task_id)
      .cloned()
      .unwrap_or_else(|| self.default_strategy.clone());

    match strategy {
      RetryStrategy::None => None,
      RetryStrategy::Skip => None,
      RetryStrategy::Immediate { max_attempts } => {
        let count = self.attempts.entry(task_id.to_string()).or_insert(0);
        *count += 1;
        if *count <= max_attempts {
          Some(0)
        } else {
          None
        }
      }
      RetryStrategy::Backoff {
        max_attempts,
        base_ms,
        max_ms,
      } => {
        let count = self.attempts.entry(task_id.to_string()).or_insert(0);
        *count += 1;
        if *count <= max_attempts {
          let delay = base_ms.saturating_mul(2_u64.saturating_pow(*count - 1));
          Some(delay.min(max_ms))
        } else {
          None
        }
      }
    }
  }

  pub fn parse_task_retry(task: &serde_json::Value) -> Option<RetryStrategy> {
    let retry = task.get("retry")?;
    let strategy_str = retry.get("strategy")?.as_str()?;

    match strategy_str {
      "none" => Some(RetryStrategy::None),
      "immediate" => {
        let max_attempts = retry
          .get("max_attempts")
          .and_then(|v| v.as_u64())
          .map(|v| v as u32)
          .unwrap_or(3);
        Some(RetryStrategy::Immediate { max_attempts })
      }
      "backoff" => {
        let max_attempts = retry
          .get("max_attempts")
          .and_then(|v| v.as_u64())
          .map(|v| v as u32)
          .unwrap_or(3);
        let base_ms = retry.get("base_ms").and_then(|v| v.as_u64()).unwrap_or(100);
        let max_ms = retry.get("max_ms").and_then(|v| v.as_u64()).unwrap_or(1000);
        Some(RetryStrategy::Backoff {
          max_attempts,
          base_ms,
          max_ms,
        })
      }
      "skip" => Some(RetryStrategy::Skip),
      _ => None,
    }
  }

  pub fn attempt_count(&self, task_id: &str) -> u32 {
    self.attempts.get(task_id).copied().unwrap_or(0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_no_retry_strategy() {
    let mut tracker = RetryTracker::new(RetryStrategy::None);
    assert!(tracker.should_retry("task-1").is_none());
  }

  #[test]
  fn test_immediate_retry() {
    let mut tracker = RetryTracker::new(RetryStrategy::Immediate { max_attempts: 3 });
    assert_eq!(tracker.should_retry("t1"), Some(0));
    assert_eq!(tracker.should_retry("t1"), Some(0));
    assert_eq!(tracker.should_retry("t1"), Some(0));
    assert_eq!(tracker.should_retry("t1"), None);
  }

  #[test]
  fn test_backoff_retry() {
    let mut tracker = RetryTracker::new(RetryStrategy::Backoff {
      max_attempts: 3,
      base_ms: 100,
      max_ms: 1000,
    });
    assert_eq!(tracker.should_retry("t1"), Some(100));
    assert_eq!(tracker.should_retry("t1"), Some(200));
    assert_eq!(tracker.should_retry("t1"), Some(400));
    assert_eq!(tracker.should_retry("t1"), None);
  }

  #[test]
  fn test_backoff_capped_at_max() {
    let mut tracker = RetryTracker::new(RetryStrategy::Backoff {
      max_attempts: 5,
      base_ms: 500,
      max_ms: 1000,
    });
    assert_eq!(tracker.should_retry("t1"), Some(500));
    assert_eq!(tracker.should_retry("t1"), Some(1000));
    assert_eq!(tracker.should_retry("t1"), Some(1000));
  }

  #[test]
  fn test_skip_strategy() {
    let mut tracker = RetryTracker::new(RetryStrategy::Skip);
    assert!(tracker.should_retry("t1").is_none());
  }

  #[test]
  fn test_per_task_override() {
    let mut tracker = RetryTracker::new(RetryStrategy::None);
    tracker.set_task_strategy("special", RetryStrategy::Immediate { max_attempts: 2 });
    assert!(tracker.should_retry("normal").is_none());
    assert_eq!(tracker.should_retry("special"), Some(0));
    assert_eq!(tracker.should_retry("special"), Some(0));
    assert_eq!(tracker.should_retry("special"), None);
  }

  #[test]
  fn test_parse_task_retry_backoff() {
    let task = serde_json::json!({
        "task_id": "t1",
        "retry": { "strategy": "backoff", "max_attempts": 3, "base_ms": 1000, "max_ms": 30000 }
    });
    let strategy = RetryTracker::parse_task_retry(&task).unwrap();
    match strategy {
      RetryStrategy::Backoff {
        max_attempts,
        base_ms,
        max_ms,
      } => {
        assert_eq!(max_attempts, 3);
        assert_eq!(base_ms, 1000);
        assert_eq!(max_ms, 30000);
      }
      _ => panic!("expected Backoff"),
    }
  }

  #[test]
  fn test_parse_task_retry_none() {
    let task = serde_json::json!({"task_id": "t1"});
    assert!(RetryTracker::parse_task_retry(&task).is_none());
  }

  #[test]
  fn test_independent_task_tracking() {
    let mut tracker = RetryTracker::new(RetryStrategy::Immediate { max_attempts: 2 });
    assert_eq!(tracker.should_retry("a"), Some(0));
    assert_eq!(tracker.should_retry("b"), Some(0));
    assert_eq!(tracker.attempt_count("a"), 1);
    assert_eq!(tracker.attempt_count("b"), 1);
  }
}
