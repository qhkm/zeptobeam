use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::pid::Pid;

static NEXT_TIMER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimerId(u64);

impl TimerId {
  pub fn new() -> Self {
    Self(NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed))
  }

  pub fn from_raw(raw: u64) -> Self {
    Self(raw)
  }

  pub fn raw(&self) -> u64 {
    self.0
  }
}

impl std::fmt::Display for TimerId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "timer-{}", self.0)
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimerKind {
  SleepUntil,
  Timeout,
  RetryBackoff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerSpec {
  pub id: TimerId,
  pub owner: Pid,
  pub kind: TimerKind,
  pub deadline_ms: u64,
  pub payload: Option<serde_json::Value>,
  pub durable: bool,
}

impl TimerSpec {
  pub fn new(owner: Pid, kind: TimerKind, deadline_ms: u64) -> Self {
    Self {
      id: TimerId::new(),
      owner,
      kind,
      deadline_ms,
      payload: None,
      durable: false,
    }
  }

  pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
    self.payload = Some(payload);
    self
  }

  pub fn with_durable(mut self, durable: bool) -> Self {
    self.durable = durable;
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::pid::Pid;

  #[test]
  fn test_timer_id_unique() {
    let a = TimerId::new();
    let b = TimerId::new();
    assert_ne!(a, b);
  }

  #[test]
  fn test_timer_id_display() {
    let id = TimerId::from_raw(42);
    assert_eq!(format!("{id}"), "timer-42");
  }

  #[test]
  fn test_timer_spec_builder() {
    let pid = Pid::from_raw(1);
    let spec = TimerSpec::new(pid, TimerKind::SleepUntil, 5000)
      .with_payload(serde_json::json!({"msg": "wake up"}))
      .with_durable(true);
    assert_eq!(spec.owner, pid);
    assert_eq!(spec.kind, TimerKind::SleepUntil);
    assert_eq!(spec.deadline_ms, 5000);
    assert!(spec.durable);
    assert!(spec.payload.is_some());
  }

  #[test]
  fn test_timer_spec_serializable() {
    let pid = Pid::from_raw(1);
    let spec = TimerSpec::new(pid, TimerKind::Timeout, 3000);
    let json = serde_json::to_string(&spec).unwrap();
    let deserialized: TimerSpec =
      serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.kind, TimerKind::Timeout);
    assert_eq!(deserialized.deadline_ms, 3000);
  }

  #[test]
  fn test_timer_kind_variants() {
    assert_eq!(TimerKind::SleepUntil, TimerKind::SleepUntil);
    assert_ne!(TimerKind::SleepUntil, TimerKind::Timeout);
    assert_ne!(TimerKind::Timeout, TimerKind::RetryBackoff);
  }
}
