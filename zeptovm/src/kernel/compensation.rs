use std::collections::HashMap;

use crate::core::effect::{
  CompensationSpec, EffectId, EffectRequest,
};
use crate::pid::Pid;

/// A recorded compensatable effect step.
#[derive(Debug, Clone)]
pub struct CompensationEntry {
  pub effect_id: EffectId,
  pub spec: CompensationSpec,
  pub completed_at_ms: u64,
}

/// Tracks compensatable effects per process for saga-style
/// rollback.
pub struct CompensationLog {
  pending: HashMap<Pid, Vec<CompensationEntry>>,
}

impl CompensationLog {
  pub fn new() -> Self {
    Self {
      pending: HashMap::new(),
    }
  }

  /// Record a completed compensatable effect.
  pub fn record(
    &mut self,
    pid: Pid,
    entry: CompensationEntry,
  ) {
    self.pending.entry(pid).or_default().push(entry);
  }

  /// Generate rollback effect requests in reverse order.
  /// Removes entries for the process.
  pub fn rollback_all(
    &mut self,
    pid: Pid,
  ) -> Vec<EffectRequest> {
    let entries =
      self.pending.remove(&pid).unwrap_or_default();
    entries
      .into_iter()
      .rev()
      .map(|entry| {
        EffectRequest::new(
          entry.spec.undo_kind,
          entry.spec.undo_input,
        )
      })
      .collect()
  }

  /// Clear all entries for a process (workflow succeeded).
  pub fn clear(&mut self, pid: Pid) {
    self.pending.remove(&pid);
  }

  /// Number of pending entries for a process.
  pub fn count(&self, pid: Pid) -> usize {
    self.pending
      .get(&pid)
      .map(|v| v.len())
      .unwrap_or(0)
  }

  /// Total entries across all processes.
  pub fn total_count(&self) -> usize {
    self.pending.values().map(|v| v.len()).sum()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::effect::{EffectId, EffectKind};

  fn make_entry(
    undo_kind: EffectKind,
  ) -> CompensationEntry {
    CompensationEntry {
      effect_id: EffectId::new(),
      spec: CompensationSpec {
        undo_kind,
        undo_input: serde_json::json!({"action": "undo"}),
      },
      completed_at_ms: 1000,
    }
  }

  #[test]
  fn test_compensation_log_empty() {
    let log = CompensationLog::new();
    assert_eq!(log.total_count(), 0);
  }

  #[test]
  fn test_compensation_log_record_and_count() {
    let mut log = CompensationLog::new();
    let pid = Pid::from_raw(1);
    log.record(pid, make_entry(EffectKind::Http));
    log.record(pid, make_entry(EffectKind::DbWrite));
    assert_eq!(log.count(pid), 2);
  }

  #[test]
  fn test_compensation_log_rollback_reverse_order() {
    let mut log = CompensationLog::new();
    let pid = Pid::from_raw(1);
    log.record(
      pid,
      CompensationEntry {
        effect_id: EffectId::new(),
        spec: CompensationSpec {
          undo_kind: EffectKind::Http,
          undo_input: serde_json::json!({"step": 1}),
        },
        completed_at_ms: 1000,
      },
    );
    log.record(
      pid,
      CompensationEntry {
        effect_id: EffectId::new(),
        spec: CompensationSpec {
          undo_kind: EffectKind::DbWrite,
          undo_input: serde_json::json!({"step": 2}),
        },
        completed_at_ms: 2000,
      },
    );

    let rollbacks = log.rollback_all(pid);
    assert_eq!(rollbacks.len(), 2);
    // Reverse order: step 2 undo first, then step 1
    assert_eq!(rollbacks[0].kind, EffectKind::DbWrite);
    assert_eq!(rollbacks[1].kind, EffectKind::Http);
    // Entries cleared
    assert_eq!(log.count(pid), 0);
  }

  #[test]
  fn test_compensation_log_clear() {
    let mut log = CompensationLog::new();
    let pid = Pid::from_raw(1);
    log.record(pid, make_entry(EffectKind::Http));
    log.clear(pid);
    assert_eq!(log.count(pid), 0);
  }

  #[test]
  fn test_compensation_log_rollback_empty() {
    let mut log = CompensationLog::new();
    let pid = Pid::from_raw(1);
    let rollbacks = log.rollback_all(pid);
    assert!(rollbacks.is_empty());
  }

  #[test]
  fn test_compensation_log_isolates_pids() {
    let mut log = CompensationLog::new();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);
    log.record(pid1, make_entry(EffectKind::Http));
    log.record(pid2, make_entry(EffectKind::DbWrite));
    log.record(pid2, make_entry(EffectKind::Http));
    assert_eq!(log.count(pid1), 1);
    assert_eq!(log.count(pid2), 2);
  }
}
