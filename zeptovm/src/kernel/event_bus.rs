use std::collections::VecDeque;

use crate::core::effect::EffectKind;
use crate::error::Reason;
use crate::pid::Pid;

/// Events emitted by the runtime for observability.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
  ProcessSpawned {
    pid: Pid,
    behavior_module: String,
    behavior_version: Option<String>,
  },
  ProcessExited {
    pid: Pid,
    reason: Reason,
  },
  EffectRequested {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
  },
  EffectDispatched {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
  },
  EffectCompleted {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
    status: String,
  },
  EffectStateChanged {
    pid: Pid,
    effect_id: u64,
    new_state: crate::core::effect::EffectState,
  },
  EffectBlocked {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
    reason: String,
  },
  PolicyEvaluated {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
    decision: String,
  },
  SupervisorRestart {
    supervisor_pid: Pid,
    child_id: String,
    strategy: String,
  },
  BudgetExhausted {
    pid: Pid,
    tokens_used: u64,
    limit: u64,
  },
}

/// Ring-buffer event bus for runtime observability.
///
/// Stores `(timestamp_ms, event)` pairs in a bounded
/// `VecDeque`. When the buffer is full the oldest entry
/// is evicted before the new one is pushed.
pub struct EventBus {
  buffer: VecDeque<(u64, RuntimeEvent)>,
  capacity: usize,
  clock_ms: u64,
}

impl EventBus {
  /// Create an event bus with the given ring-buffer capacity.
  pub fn new(capacity: usize) -> Self {
    Self {
      buffer: VecDeque::with_capacity(capacity),
      capacity,
      clock_ms: 0,
    }
  }

  /// Advance the logical clock to `ms`.
  pub fn set_clock(&mut self, ms: u64) {
    self.clock_ms = ms;
  }

  /// Emit an event into the ring buffer.
  ///
  /// If the buffer is at capacity the oldest entry is
  /// dropped first. Every event is also logged via
  /// `tracing::info!`.
  pub fn emit(&mut self, event: RuntimeEvent) {
    if self.buffer.len() >= self.capacity {
      self.buffer.pop_front();
    }
    tracing::info!(event = ?event, "runtime_event");
    self.buffer.push_back((self.clock_ms, event));
  }

  /// Drain all events from the buffer and return them.
  pub fn drain_events(
    &mut self,
  ) -> Vec<(u64, RuntimeEvent)> {
    self.buffer.drain(..).collect()
  }

  /// Return the last `n` events without removing them.
  pub fn recent(
    &self,
    n: usize,
  ) -> Vec<(u64, RuntimeEvent)> {
    let len = self.buffer.len();
    let start = len.saturating_sub(n);
    self.buffer.iter().skip(start).cloned().collect()
  }

  /// Number of events currently in the buffer.
  pub fn event_count(&self) -> usize {
    self.buffer.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn spawned_event(pid: Pid) -> RuntimeEvent {
    RuntimeEvent::ProcessSpawned {
      pid,
      behavior_module: "test_mod".into(),
      behavior_version: None,
    }
  }

  fn exited_event(pid: Pid) -> RuntimeEvent {
    RuntimeEvent::ProcessExited {
      pid,
      reason: Reason::Normal,
    }
  }

  #[test]
  fn test_emit_and_drain() {
    let mut bus = EventBus::new(16);
    let p1 = Pid::from_raw(1);
    let p2 = Pid::from_raw(2);

    bus.emit(spawned_event(p1));
    bus.emit(exited_event(p2));

    let events = bus.drain_events();
    assert_eq!(events.len(), 2);

    // First event is the spawn
    assert!(matches!(
      &events[0].1,
      RuntimeEvent::ProcessSpawned { pid, .. }
        if *pid == Pid::from_raw(1)
    ));
    // Second event is the exit
    assert!(matches!(
      &events[1].1,
      RuntimeEvent::ProcessExited { pid, .. }
        if *pid == Pid::from_raw(2)
    ));
  }

  #[test]
  fn test_drain_clears_buffer() {
    let mut bus = EventBus::new(16);
    bus.emit(spawned_event(Pid::from_raw(1)));

    let first = bus.drain_events();
    assert_eq!(first.len(), 1);

    let second = bus.drain_events();
    assert!(second.is_empty());
  }

  #[test]
  fn test_ring_buffer_eviction() {
    let mut bus = EventBus::new(3);
    for i in 1..=5 {
      bus.emit(spawned_event(Pid::from_raw(i)));
    }

    assert_eq!(bus.event_count(), 3);
    let events = bus.drain_events();

    // Only pids 3, 4, 5 should remain
    let pids: Vec<u64> = events
      .iter()
      .map(|(_, e)| match e {
        RuntimeEvent::ProcessSpawned { pid, .. } => {
          pid.raw()
        }
        _ => panic!("unexpected variant"),
      })
      .collect();
    assert_eq!(pids, vec![3, 4, 5]);
  }

  #[test]
  fn test_recent_without_drain() {
    let mut bus = EventBus::new(16);
    for i in 1..=4 {
      bus.emit(spawned_event(Pid::from_raw(i)));
    }

    let recent = bus.recent(2);
    assert_eq!(recent.len(), 2);

    // Should be pids 3 and 4 (the last two)
    let pids: Vec<u64> = recent
      .iter()
      .map(|(_, e)| match e {
        RuntimeEvent::ProcessSpawned { pid, .. } => {
          pid.raw()
        }
        _ => panic!("unexpected variant"),
      })
      .collect();
    assert_eq!(pids, vec![3, 4]);

    // Buffer should still have all 4
    assert_eq!(bus.event_count(), 4);
  }

  #[test]
  fn test_recent_more_than_buffer() {
    let mut bus = EventBus::new(16);
    bus.emit(spawned_event(Pid::from_raw(1)));

    let recent = bus.recent(10);
    assert_eq!(recent.len(), 1);
  }

  #[test]
  fn test_event_count() {
    let mut bus = EventBus::new(16);
    assert_eq!(bus.event_count(), 0);

    bus.emit(spawned_event(Pid::from_raw(1)));
    assert_eq!(bus.event_count(), 1);

    bus.emit(exited_event(Pid::from_raw(2)));
    assert_eq!(bus.event_count(), 2);

    bus.emit(spawned_event(Pid::from_raw(3)));
    assert_eq!(bus.event_count(), 3);
  }
}
