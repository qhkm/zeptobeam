use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// In-process metrics collection (counters + gauges).
/// All operations are atomic -- no locks needed.
pub struct Metrics {
  counters: HashMap<&'static str, AtomicU64>,
  gauges: HashMap<&'static str, AtomicI64>,
}

impl Metrics {
  pub fn new() -> Self {
    let mut counters = HashMap::new();
    let mut gauges = HashMap::new();

    // Pre-register known metrics
    for name in [
      "processes.spawned",
      "processes.exited",
      "effects.dispatched",
      "effects.completed",
      "effects.failed",
      "effects.retries",
      "effects.timed_out",
      "turns.committed",
      "timers.scheduled",
      "timers.fired",
      "supervisor.restarts",
      "compensation.triggered",
      "budget.blocked",
      "policy.blocked",
      "scheduler.ticks",
      "messages.expired",
      "artifacts.stored",
      "artifacts.fetched",
      "artifacts.deleted",
      "artifacts.expired",
      "approvals.requested",
      "approvals.approved",
      "approvals.denied",
      "approvals.timed_out",
      "approvals.input_received",
    ] {
      counters.insert(name, AtomicU64::new(0));
    }

    for name in ["processes.active", "mailbox.depth_total"] {
      gauges.insert(name, AtomicI64::new(0));
    }

    Self { counters, gauges }
  }

  /// Increment a counter by 1.
  pub fn inc(&self, name: &'static str) {
    if let Some(c) = self.counters.get(name) {
      c.fetch_add(1, Ordering::Relaxed);
    }
  }

  /// Increment a counter by n.
  pub fn inc_by(&self, name: &'static str, n: u64) {
    if let Some(c) = self.counters.get(name) {
      c.fetch_add(n, Ordering::Relaxed);
    }
  }

  /// Set a gauge value.
  pub fn gauge_set(&self, name: &'static str, val: i64) {
    if let Some(g) = self.gauges.get(name) {
      g.store(val, Ordering::Relaxed);
    }
  }

  /// Get a counter value.
  pub fn counter(&self, name: &'static str) -> u64 {
    self.counters
      .get(name)
      .map(|c| c.load(Ordering::Relaxed))
      .unwrap_or(0)
  }

  /// Get a gauge value.
  pub fn gauge(&self, name: &'static str) -> i64 {
    self.gauges
      .get(name)
      .map(|g| g.load(Ordering::Relaxed))
      .unwrap_or(0)
  }

  /// Snapshot all metrics as (name, value) pairs.
  pub fn snapshot(&self) -> Vec<(&'static str, i64)> {
    let mut result: Vec<_> = self
      .counters
      .iter()
      .map(|(k, v)| (*k, v.load(Ordering::Relaxed) as i64))
      .chain(
        self
          .gauges
          .iter()
          .map(|(k, v)| (*k, v.load(Ordering::Relaxed))),
      )
      .collect();
    result.sort_by_key(|(k, _)| *k);
    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_metrics_new() {
    let m = Metrics::new();
    assert_eq!(m.counter("processes.spawned"), 0);
    assert_eq!(m.gauge("processes.active"), 0);
  }

  #[test]
  fn test_metrics_inc() {
    let m = Metrics::new();
    m.inc("processes.spawned");
    m.inc("processes.spawned");
    assert_eq!(m.counter("processes.spawned"), 2);
  }

  #[test]
  fn test_metrics_inc_by() {
    let m = Metrics::new();
    m.inc_by("effects.dispatched", 5);
    assert_eq!(m.counter("effects.dispatched"), 5);
  }

  #[test]
  fn test_metrics_gauge() {
    let m = Metrics::new();
    m.gauge_set("processes.active", 42);
    assert_eq!(m.gauge("processes.active"), 42);
    m.gauge_set("processes.active", 10);
    assert_eq!(m.gauge("processes.active"), 10);
  }

  #[test]
  fn test_metrics_unknown_name_ignored() {
    let m = Metrics::new();
    m.inc("nonexistent.counter");
    assert_eq!(m.counter("nonexistent.counter"), 0);
  }

  #[test]
  fn test_metrics_snapshot() {
    let m = Metrics::new();
    m.inc("processes.spawned");
    m.gauge_set("processes.active", 5);
    let snap = m.snapshot();
    assert!(!snap.is_empty());
    let spawned =
      snap.iter().find(|(k, _)| *k == "processes.spawned");
    assert_eq!(spawned.unwrap().1, 1);
  }

  #[test]
  fn test_metrics_concurrent_safe() {
    use std::sync::Arc;
    let m = Arc::new(Metrics::new());
    let handles: Vec<_> = (0..10)
      .map(|_| {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
          for _ in 0..100 {
            m.inc("scheduler.ticks");
          }
        })
      })
      .collect();
    for h in handles {
      h.join().unwrap();
    }
    assert_eq!(m.counter("scheduler.ticks"), 1000);
  }

  #[test]
  fn test_metrics_messages_expired() {
    let m = Metrics::new();
    m.inc_by("messages.expired", 5);
    assert_eq!(m.counter("messages.expired"), 5);
  }

  #[test]
  fn test_metrics_artifact_counters() {
    let m = Metrics::new();
    m.inc("artifacts.stored");
    m.inc("artifacts.fetched");
    m.inc("artifacts.deleted");
    m.inc("artifacts.expired");
    assert_eq!(m.counter("artifacts.stored"), 1);
    assert_eq!(m.counter("artifacts.fetched"), 1);
    assert_eq!(m.counter("artifacts.deleted"), 1);
    assert_eq!(m.counter("artifacts.expired"), 1);
  }

  #[test]
  fn test_metrics_approval_counters() {
    let m = Metrics::new();
    m.inc("approvals.requested");
    m.inc("approvals.approved");
    m.inc("approvals.denied");
    m.inc("approvals.timed_out");
    m.inc("approvals.input_received");
    assert_eq!(m.counter("approvals.requested"), 1);
    assert_eq!(m.counter("approvals.approved"), 1);
    assert_eq!(m.counter("approvals.denied"), 1);
    assert_eq!(m.counter("approvals.timed_out"), 1);
    assert_eq!(m.counter("approvals.input_received"), 1);
  }
}
