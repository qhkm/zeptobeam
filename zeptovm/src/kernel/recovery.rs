use crate::core::behavior::StepBehavior;
use crate::core::effect::{EffectRequest, EffectState};
use crate::core::timer::TimerSpec;
use crate::durability::journal::{Journal, JournalEntryType};
use crate::durability::snapshot::SnapshotStore;
use crate::durability::timer_store::TimerStore;
use crate::kernel::process_table::{
  ProcessEntry, ProcessRuntimeState,
};
use crate::pid::Pid;

/// Coordinates process recovery from durable storage.
pub struct RecoveryCoordinator<'a> {
  journal: &'a Journal,
  snapshots: &'a SnapshotStore,
  timer_store: Option<&'a TimerStore>,
}

/// Rich recovery info for a single pending effect.
pub struct PendingEffectRecovery {
  pub effect_id: u64,
  pub last_state: EffectState,
  pub request: EffectRequest,
}

/// Result of recovering a single process.
pub struct RecoveredProcess {
  pub entry: ProcessEntry,
  pub pending_effects: Vec<PendingEffectRecovery>,
  pub timers: Vec<TimerSpec>,
}

impl<'a> RecoveryCoordinator<'a> {
  pub fn new(
    journal: &'a Journal,
    snapshots: &'a SnapshotStore,
  ) -> Self {
    Self {
      journal,
      snapshots,
      timer_store: None,
    }
  }

  pub fn with_timer_store(
    mut self,
    store: &'a TimerStore,
  ) -> Self {
    self.timer_store = Some(store);
    self
  }

  /// Recover a single process from snapshot + journal replay.
  pub fn recover_process(
    &self,
    pid: Pid,
    factory: &dyn Fn() -> Box<dyn StepBehavior>,
  ) -> Result<RecoveredProcess, String> {
    let mut behavior = factory();
    let mut snapshot_seq = 0i64;

    // Step 1: Load latest snapshot
    let snapshot = self
      .snapshots
      .load_latest(pid)
      .map_err(|e| format!("snapshot load failed: {e}"))?;

    if let Some(ref snap) = snapshot {
      behavior.restore(&snap.state_blob)?;
      snapshot_seq = snap.version;
    } else {
      behavior.init(None);
    }

    // Step 1.5: Check behavior version compatibility
    let all_entries = self
      .journal
      .replay(pid, 0)
      .map_err(|e| format!("journal read failed: {e}"))?;
    let journaled_version = all_entries
      .iter()
      .find(|e| {
        e.entry_type == JournalEntryType::ProcessSpawned
      })
      .and_then(|e| e.payload.as_ref())
      .and_then(|p| {
        serde_json::from_slice::<serde_json::Value>(p)
          .ok()
      })
      .and_then(|v| {
        v.get("behavior_version")
          .and_then(|bv| bv.as_str().map(String::from))
      });

    let current_meta = behavior.meta();
    let current_version =
      current_meta.as_ref().map(|m| m.version.as_str());

    if let (Some(ref old_ver), Some(new_ver)) =
      (&journaled_version, current_version)
    {
      if old_ver != new_ver {
        // Version mismatch — attempt migration
        let snapshot_state =
          if let Some(ref snap) = snapshot {
            serde_json::from_slice(&snap.state_blob)
              .unwrap_or(serde_json::Value::Null)
          } else {
            serde_json::Value::Null
          };

        match behavior.migrate(old_ver, snapshot_state) {
          Some(new_state) => {
            let new_blob =
              serde_json::to_vec(&new_state).map_err(
                |e| {
                  format!(
                    "migration state serialization \
                     failed: {e}"
                  )
                },
              )?;
            behavior.restore(&new_blob)?;
          }
          None => {
            return Err(format!(
              "behavior version mismatch: \
               {} -> {}, no migration provided",
              old_ver, new_ver,
            ));
          }
        }
      }
    }

    // Step 2: Replay journal entries after snapshot
    let entries = self
      .journal
      .replay(pid, snapshot_seq)
      .map_err(|e| format!("journal replay failed: {e}"))?;

    let mut effect_requests:
      std::collections::HashMap<u64, EffectRequest> =
      std::collections::HashMap::new();
    let mut effect_states:
      std::collections::HashMap<u64, EffectState> =
      std::collections::HashMap::new();
    let mut completed_effects =
      std::collections::HashSet::new();

    for entry in &entries {
      match &entry.entry_type {
        JournalEntryType::EffectRequested => {
          if let Some(ref payload) = entry.payload {
            if let Ok(req) =
              serde_json::from_slice::<EffectRequest>(
                payload,
              )
            {
              let id = req.effect_id.raw();
              effect_states
                .insert(id, EffectState::Pending);
              effect_requests.insert(id, req);
            }
          }
        }
        JournalEntryType::EffectStateChanged => {
          if let Some(ref payload) = entry.payload {
            if let Ok(val) =
              serde_json::from_slice::<
                serde_json::Value,
              >(payload)
            {
              if let Some(id) = val
                .get("effect_id")
                .and_then(|v| v.as_u64())
              {
                if let Ok(state) =
                  serde_json::from_value::<EffectState>(
                    val["new_state"].clone(),
                  )
                {
                  effect_states.insert(id, state);
                }
              }
            }
          }
        }
        JournalEntryType::EffectResultRecorded => {
          if let Some(ref payload) = entry.payload {
            if let Ok(id) =
              serde_json::from_slice::<u64>(payload)
            {
              completed_effects.insert(id);
            }
          }
        }
        _ => {}
      }
    }

    // Build PendingEffectRecovery for non-completed
    let pending_effects: Vec<PendingEffectRecovery> =
      effect_requests
        .into_iter()
        .filter(|(id, _)| {
          !completed_effects.contains(id)
        })
        .map(|(id, req)| PendingEffectRecovery {
          effect_id: id,
          last_state: effect_states
            .remove(&id)
            .unwrap_or(EffectState::Pending),
          request: req,
        })
        .collect();

    // Step 3: Load durable timers
    let timers = if let Some(store) = self.timer_store {
      store
        .load_for_pid(pid)
        .map_err(|e| format!("timer load failed: {e}"))?
    } else {
      Vec::new()
    };

    // Step 4: Build ProcessEntry
    let entry = ProcessEntry::new(pid, behavior);

    Ok(RecoveredProcess {
      entry,
      pending_effects,
      timers,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::behavior::StepBehavior;
  use crate::core::effect::{
    EffectKind, EffectRequest, EffectState,
  };
  use crate::core::message::Envelope;
  use crate::core::step_result::StepResult;
  use crate::core::turn_context::TurnContext;
  use crate::durability::journal::{
    Journal, JournalEntry, JournalEntryType,
  };
  use crate::durability::snapshot::{Snapshot, SnapshotStore};
  use crate::error::Reason;

  struct Restorable {
    state: String,
  }
  impl StepBehavior for Restorable {
    fn init(
      &mut self,
      _: Option<Vec<u8>>,
    ) -> StepResult {
      self.state = "initialized".into();
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _: Envelope,
      _: &mut TurnContext,
    ) -> StepResult {
      StepResult::Continue
    }
    fn terminate(&mut self, _: &Reason) {}
    fn snapshot(&self) -> Option<Vec<u8>> {
      Some(self.state.as_bytes().to_vec())
    }
    fn restore(
      &mut self,
      state: &[u8],
    ) -> Result<(), String> {
      self.state = String::from_utf8(state.to_vec())
        .map_err(|e| e.to_string())?;
      Ok(())
    }
  }

  #[test]
  fn test_recover_fresh_no_snapshot() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);

    let pid = Pid::from_raw(1);
    let result = coord.recover_process(pid, &|| {
      Box::new(Restorable {
        state: String::new(),
      })
    });
    assert!(result.is_ok());
    let recovered = result.unwrap();
    assert_eq!(
      recovered.entry.state,
      ProcessRuntimeState::Ready
    );
    assert!(recovered.pending_effects.is_empty());
  }

  #[test]
  fn test_recover_from_snapshot() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    snapshots
      .save(&Snapshot {
        pid,
        version: 5,
        state_blob: b"recovered-state".to_vec(),
        mailbox_cursor: 0,
        pending_effects: None,
      })
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(pid, &|| {
      Box::new(Restorable {
        state: String::new(),
      })
    });
    assert!(result.is_ok());
  }

  #[test]
  fn test_recover_with_pending_effects() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({"prompt": "test"}),
    );
    let payload = serde_json::to_vec(&req).unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(payload),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord
      .recover_process(pid, &|| {
        Box::new(Restorable {
          state: String::new(),
        })
      })
      .unwrap();
    assert_eq!(result.pending_effects.len(), 1);
    assert!(matches!(
      result.pending_effects[0].last_state,
      EffectState::Pending
    ));
  }

  #[test]
  fn test_recover_completed_effects_filtered() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({}),
    );
    let effect_id_raw = req.effect_id.raw();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(serde_json::to_vec(&req).unwrap()),
      ))
      .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectResultRecorded,
        Some(
          serde_json::to_vec(&effect_id_raw).unwrap(),
        ),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord
      .recover_process(pid, &|| {
        Box::new(Restorable {
          state: String::new(),
        })
      })
      .unwrap();
    assert!(result.pending_effects.is_empty());
  }

  #[test]
  fn test_recover_with_effect_state_transitions() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({}),
    );
    let effect_id = req.effect_id.raw();

    // Journal: requested
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(serde_json::to_vec(&req).unwrap()),
      ))
      .unwrap();

    // Journal: state changed to Dispatched
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectStateChanged,
        Some(
          serde_json::to_vec(&serde_json::json!({
            "effect_id": effect_id,
            "new_state": EffectState::Dispatched {
              dispatched_at_ms: 1000
            },
          }))
          .unwrap(),
        ),
      ))
      .unwrap();

    // Journal: state changed to Retrying
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectStateChanged,
        Some(
          serde_json::to_vec(&serde_json::json!({
            "effect_id": effect_id,
            "new_state": EffectState::Retrying {
              attempt: 1,
              next_at_ms: 2000,
            },
          }))
          .unwrap(),
        ),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord
      .recover_process(pid, &|| {
        Box::new(Restorable {
          state: String::new(),
        })
      })
      .unwrap();

    assert_eq!(result.pending_effects.len(), 1);
    let pe = &result.pending_effects[0];
    assert_eq!(pe.effect_id, effect_id);
    assert!(matches!(
      pe.last_state,
      EffectState::Retrying { attempt: 1, .. }
    ));
  }

  #[test]
  fn test_recover_completed_state_filtered() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let req = EffectRequest::new(
      EffectKind::Http,
      serde_json::json!({}),
    );
    let effect_id = req.effect_id.raw();

    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(serde_json::to_vec(&req).unwrap()),
      ))
      .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectResultRecorded,
        Some(serde_json::to_vec(&effect_id).unwrap()),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord
      .recover_process(pid, &|| {
        Box::new(Restorable {
          state: String::new(),
        })
      })
      .unwrap();

    assert!(
      result.pending_effects.is_empty(),
      "completed effects should be filtered out"
    );
  }

  #[test]
  fn test_recovery_same_version_succeeds() {
    use crate::core::behavior::BehaviorMeta;

    struct V1;
    impl StepBehavior for V1 {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        _: &mut TurnContext,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
      fn meta(&self) -> Option<BehaviorMeta> {
        Some(BehaviorMeta {
          module: "agent".to_string(),
          version: "1.0.0".to_string(),
        })
      }
    }

    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let payload = serde_json::to_vec(
      &serde_json::json!({
        "behavior_module": "agent",
        "behavior_version": "1.0.0",
      }),
    )
    .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::ProcessSpawned,
        Some(payload),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result =
      coord.recover_process(pid, &|| Box::new(V1));
    assert!(
      result.is_ok(),
      "same version should recover fine",
    );
  }

  #[test]
  fn test_recovery_version_mismatch_with_migration() {
    use crate::core::behavior::BehaviorMeta;

    struct V2;
    impl StepBehavior for V2 {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        _: &mut TurnContext,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
      fn restore(
        &mut self,
        _state: &[u8],
      ) -> Result<(), String> {
        Ok(())
      }
      fn meta(&self) -> Option<BehaviorMeta> {
        Some(BehaviorMeta {
          module: "agent".to_string(),
          version: "2.0.0".to_string(),
        })
      }
      fn migrate(
        &self,
        old_version: &str,
        state: serde_json::Value,
      ) -> Option<serde_json::Value> {
        if old_version == "1.0.0" {
          let mut s = state;
          s["migrated"] = serde_json::json!(true);
          Some(s)
        } else {
          None
        }
      }
    }

    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let payload = serde_json::to_vec(
      &serde_json::json!({
        "behavior_module": "agent",
        "behavior_version": "1.0.0",
      }),
    )
    .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::ProcessSpawned,
        Some(payload),
      ))
      .unwrap();

    snapshots
      .save(&Snapshot {
        pid,
        version: 1,
        state_blob: serde_json::to_vec(
          &serde_json::json!({"count": 42}),
        )
        .unwrap(),
        mailbox_cursor: 0,
        pending_effects: None,
      })
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result =
      coord.recover_process(pid, &|| Box::new(V2));
    assert!(
      result.is_ok(),
      "version mismatch with migration should succeed: \
       {:?}",
      result.err(),
    );
  }

  #[test]
  fn test_recovery_version_mismatch_no_migration_fails()
  {
    use crate::core::behavior::BehaviorMeta;

    struct V2NoMigrate;
    impl StepBehavior for V2NoMigrate {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        _: &mut TurnContext,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
      fn meta(&self) -> Option<BehaviorMeta> {
        Some(BehaviorMeta {
          module: "agent".to_string(),
          version: "2.0.0".to_string(),
        })
      }
    }

    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let payload = serde_json::to_vec(
      &serde_json::json!({
        "behavior_module": "agent",
        "behavior_version": "1.0.0",
      }),
    )
    .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::ProcessSpawned,
        Some(payload),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(
      pid,
      &|| Box::new(V2NoMigrate),
    );
    assert!(
      result.is_err(),
      "version mismatch without migration should fail",
    );
    let err = result.err().unwrap();
    assert!(
      err.contains("behavior version mismatch"),
      "error should mention version mismatch, got: {err}",
    );
  }

  #[test]
  fn test_recovery_no_version_recorded_succeeds() {
    use crate::core::behavior::BehaviorMeta;

    struct V1;
    impl StepBehavior for V1 {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        _: &mut TurnContext,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
      fn meta(&self) -> Option<BehaviorMeta> {
        Some(BehaviorMeta {
          module: "agent".to_string(),
          version: "1.0.0".to_string(),
        })
      }
    }

    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    // Old-style spawn: no payload
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::ProcessSpawned,
        None,
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result =
      coord.recover_process(pid, &|| Box::new(V1));
    assert!(
      result.is_ok(),
      "no version recorded should recover normally",
    );
  }

  #[test]
  fn test_full_versioned_recovery_flow() {
    use crate::core::behavior::BehaviorMeta;

    // V1 behavior
    struct AgentV1 {
      count: u32,
    }
    impl StepBehavior for AgentV1 {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        self.count = 0;
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        _: &mut TurnContext,
      ) -> StepResult {
        self.count += 1;
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
      fn snapshot(&self) -> Option<Vec<u8>> {
        Some(
          serde_json::to_vec(&serde_json::json!({
            "count": self.count,
          }))
          .unwrap(),
        )
      }
      fn restore(
        &mut self,
        state: &[u8],
      ) -> Result<(), String> {
        let val: serde_json::Value =
          serde_json::from_slice(state)
            .map_err(|e| e.to_string())?;
        self.count =
          val["count"].as_u64().unwrap_or(0) as u32;
        Ok(())
      }
      fn meta(&self) -> Option<BehaviorMeta> {
        Some(BehaviorMeta {
          module: "counter".to_string(),
          version: "1.0.0".to_string(),
        })
      }
    }

    // V2 behavior — adds "label" field
    struct AgentV2 {
      count: u32,
      label: String,
    }
    impl StepBehavior for AgentV2 {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        _: &mut TurnContext,
      ) -> StepResult {
        self.count += 1;
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
      fn snapshot(&self) -> Option<Vec<u8>> {
        Some(
          serde_json::to_vec(&serde_json::json!({
            "count": self.count,
            "label": self.label,
          }))
          .unwrap(),
        )
      }
      fn restore(
        &mut self,
        state: &[u8],
      ) -> Result<(), String> {
        let val: serde_json::Value =
          serde_json::from_slice(state)
            .map_err(|e| e.to_string())?;
        self.count =
          val["count"].as_u64().unwrap_or(0) as u32;
        self.label = val["label"]
          .as_str()
          .unwrap_or("default")
          .to_string();
        Ok(())
      }
      fn meta(&self) -> Option<BehaviorMeta> {
        Some(BehaviorMeta {
          module: "counter".to_string(),
          version: "2.0.0".to_string(),
        })
      }
      fn migrate(
        &self,
        old_version: &str,
        state: serde_json::Value,
      ) -> Option<serde_json::Value> {
        if old_version == "1.0.0" {
          let mut s = state;
          s["label"] =
            serde_json::json!("migrated-from-v1");
          Some(s)
        } else {
          None
        }
      }
    }

    // Simulate: V1 ran, took snapshot, wrote ProcessSpawned
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(42);

    // V1 spawn entry
    let spawn_payload = serde_json::to_vec(
      &serde_json::json!({
        "behavior_module": "counter",
        "behavior_version": "1.0.0",
      }),
    )
    .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::ProcessSpawned,
        Some(spawn_payload),
      ))
      .unwrap();

    // V1 snapshot: count=10
    snapshots
      .save(&Snapshot {
        pid,
        version: 1,
        state_blob: serde_json::to_vec(
          &serde_json::json!({"count": 10}),
        )
        .unwrap(),
        mailbox_cursor: 0,
        pending_effects: None,
      })
      .unwrap();

    // Recover with V2 factory
    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(pid, &|| {
      Box::new(AgentV2 {
        count: 0,
        label: String::new(),
      })
    });
    assert!(
      result.is_ok(),
      "migration from v1 to v2 should succeed: {:?}",
      result.err(),
    );
  }
}
