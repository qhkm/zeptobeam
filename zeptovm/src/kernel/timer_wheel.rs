use std::collections::BTreeMap;

use crate::core::timer::{TimerId, TimerSpec};

/// Timer wheel using BTreeMap keyed by deadline.
/// tick() pops all expired timers. schedule() inserts. cancel() removes.
pub struct TimerWheel {
  timers: BTreeMap<u64, Vec<TimerSpec>>,
  by_id: std::collections::HashMap<TimerId, u64>,
}

impl TimerWheel {
  pub fn new() -> Self {
    Self {
      timers: BTreeMap::new(),
      by_id: std::collections::HashMap::new(),
    }
  }

  pub fn schedule(&mut self, spec: TimerSpec) {
    let deadline = spec.deadline_ms;
    let id = spec.id;
    self.timers.entry(deadline).or_default().push(spec);
    self.by_id.insert(id, deadline);
  }

  pub fn cancel(&mut self, timer_id: TimerId) -> bool {
    if let Some(deadline) = self.by_id.remove(&timer_id) {
      if let Some(bucket) = self.timers.get_mut(&deadline) {
        bucket.retain(|t| t.id != timer_id);
        if bucket.is_empty() {
          self.timers.remove(&deadline);
        }
        return true;
      }
    }
    false
  }

  pub fn tick(&mut self, now_ms: u64) -> Vec<TimerSpec> {
    let mut fired = Vec::new();
    let remaining = self.timers.split_off(&(now_ms + 1));
    let expired = std::mem::replace(&mut self.timers, remaining);
    for (_deadline, bucket) in expired {
      for spec in bucket {
        self.by_id.remove(&spec.id);
        fired.push(spec);
      }
    }
    fired
  }

  pub fn len(&self) -> usize {
    self.by_id.len()
  }

  pub fn is_empty(&self) -> bool {
    self.by_id.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::timer::{TimerKind, TimerSpec};
  use crate::pid::Pid;

  fn make_timer(pid: Pid, deadline: u64) -> TimerSpec {
    TimerSpec::new(pid, TimerKind::SleepUntil, deadline)
  }

  #[test]
  fn test_timer_wheel_empty() {
    let mut wheel = TimerWheel::new();
    assert!(wheel.is_empty());
    assert_eq!(wheel.tick(1000).len(), 0);
  }

  #[test]
  fn test_timer_wheel_schedule_and_tick() {
    let mut wheel = TimerWheel::new();
    let pid = Pid::from_raw(1);
    wheel.schedule(make_timer(pid, 100));
    wheel.schedule(make_timer(pid, 200));
    wheel.schedule(make_timer(pid, 300));
    assert_eq!(wheel.len(), 3);

    let fired = wheel.tick(150);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].deadline_ms, 100);
    assert_eq!(wheel.len(), 2);
  }

  #[test]
  fn test_timer_wheel_tick_exact_deadline() {
    let mut wheel = TimerWheel::new();
    let pid = Pid::from_raw(1);
    wheel.schedule(make_timer(pid, 100));
    let fired = wheel.tick(100);
    assert_eq!(fired.len(), 1);
  }

  #[test]
  fn test_timer_wheel_tick_all() {
    let mut wheel = TimerWheel::new();
    let pid = Pid::from_raw(1);
    wheel.schedule(make_timer(pid, 100));
    wheel.schedule(make_timer(pid, 200));
    let fired = wheel.tick(500);
    assert_eq!(fired.len(), 2);
    assert!(wheel.is_empty());
  }

  #[test]
  fn test_timer_wheel_cancel() {
    let mut wheel = TimerWheel::new();
    let pid = Pid::from_raw(1);
    let spec = make_timer(pid, 100);
    let id = spec.id;
    wheel.schedule(spec);
    assert_eq!(wheel.len(), 1);
    assert!(wheel.cancel(id));
    assert!(wheel.is_empty());
  }

  #[test]
  fn test_timer_wheel_cancel_nonexistent() {
    let mut wheel = TimerWheel::new();
    assert!(!wheel.cancel(crate::core::timer::TimerId::from_raw(999)));
  }

  #[test]
  fn test_timer_wheel_multiple_same_deadline() {
    let mut wheel = TimerWheel::new();
    let pid = Pid::from_raw(1);
    wheel.schedule(make_timer(pid, 100));
    wheel.schedule(make_timer(pid, 100));
    wheel.schedule(make_timer(pid, 100));
    assert_eq!(wheel.len(), 3);
    let fired = wheel.tick(100);
    assert_eq!(fired.len(), 3);
  }
}
