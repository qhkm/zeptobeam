# G4: Behavior Versioning + Migration — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Record behavior version in the journal at spawn. On recovery, detect version changes and call a migration callback to transform snapshot state.

**Architecture:** Recovery-time migration only. No live hot-reload. Version metadata persisted in journal, migration callback on StepBehavior trait, fail-safe on mismatch without migration. No new structs beyond what exists (BehaviorMeta already defined).

**Tech Stack:** Rust, serde_json, rusqlite (existing journal)

---

### Task 1: Add `migrate()` callback to StepBehavior trait

**Files:**
- Modify: `zeptovm/src/core/behavior.rs:21-52` (StepBehavior trait)
- Test: `zeptovm/src/core/behavior.rs` (inline tests)

**Step 1: Write the failing test**

Add two tests after the existing `test_behavior_meta_custom` test (line 208):

```rust
#[test]
fn test_behavior_migrate_default_none() {
    let b = EchoStep;
    let result = b.migrate("0.1.0", serde_json::json!({"count": 1}));
    assert!(result.is_none(), "default migrate should return None");
}

#[test]
fn test_behavior_migrate_custom() {
    struct Migrator;
    impl StepBehavior for Migrator {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(
            &mut self,
            _msg: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
        fn meta(&self) -> Option<BehaviorMeta> {
            Some(BehaviorMeta {
                module: "migrator".to_string(),
                version: "2.0.0".to_string(),
            })
        }
        fn migrate(
            &self,
            old_version: &str,
            state: serde_json::Value,
        ) -> Option<serde_json::Value> {
            if old_version == "1.0.0" {
                let mut new_state = state;
                new_state["migrated"] = serde_json::json!(true);
                Some(new_state)
            } else {
                None
            }
        }
    }

    let b = Migrator;
    let state = serde_json::json!({"count": 5});
    let result = b.migrate("1.0.0", state);
    assert!(result.is_some());
    let new_state = result.unwrap();
    assert_eq!(new_state["count"], 5);
    assert_eq!(new_state["migrated"], true);

    // Unknown old version returns None
    let result2 = b.migrate("0.5.0", serde_json::json!({}));
    assert!(result2.is_none());
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib core::behavior::tests::test_behavior_migrate -- --nocapture 2>&1 | head -30`
Expected: FAIL — `migrate` method does not exist on `StepBehavior`

**Step 3: Write minimal implementation**

Add the `migrate()` method to the `StepBehavior` trait in `behavior.rs`, after the existing `meta()` method (after line 51):

```rust
  /// Opt-in: transform state from an old behavior version to the
  /// current version. Called during recovery when the journaled
  /// version differs from meta().version.
  /// Return Some(new_state) to proceed with the transformed state,
  /// or None to indicate migration is not supported (process will fail).
  fn migrate(
    &self,
    _old_version: &str,
    _state: serde_json::Value,
  ) -> Option<serde_json::Value> {
    None
  }
```

**Step 4: Run tests to verify they pass**

Run: `cd zeptovm && cargo test --lib core::behavior::tests -- --nocapture`
Expected: ALL PASS (including the two new tests)

**Step 5: Commit**

```bash
git add zeptovm/src/core/behavior.rs
git commit -m "feat(behavior): add migrate() callback to StepBehavior trait

Default returns None (no migration). Behaviors can override to
transform snapshot state from old versions during recovery.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: Add version fields to ProcessEntry

**Files:**
- Modify: `zeptovm/src/kernel/process_table.rs:31-66` (ProcessEntry struct + new())
- Test: `zeptovm/src/kernel/process_table.rs` (inline tests)

**Step 1: Write the failing test**

Add after the existing `test_process_entry_no_meta_default` test (line 720):

```rust
#[test]
fn test_process_entry_version_fields_from_meta() {
    use crate::core::behavior::BehaviorMeta;

    struct VersionedBehavior;
    impl StepBehavior for VersionedBehavior {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(
            &mut self,
            _msg: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
        fn meta(&self) -> Option<BehaviorMeta> {
            Some(BehaviorMeta {
                module: "agent_v2".to_string(),
                version: "2.1.0".to_string(),
            })
        }
    }

    let pid = Pid::new();
    let p = ProcessEntry::new(pid, Box::new(VersionedBehavior));
    assert_eq!(
        p.behavior_module.as_deref(),
        Some("agent_v2"),
    );
    assert_eq!(
        p.behavior_version.as_deref(),
        Some("2.1.0"),
    );
}

#[test]
fn test_process_entry_version_fields_none_without_meta() {
    let pid = Pid::new();
    let p = ProcessEntry::new(pid, Box::new(Echo));
    assert!(p.behavior_module.is_none());
    assert!(p.behavior_version.is_none());
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::process_table::tests::test_process_entry_version_fields -- --nocapture 2>&1 | head -20`
Expected: FAIL — `behavior_module` field does not exist

**Step 3: Write minimal implementation**

Add two fields to `ProcessEntry` struct (after `behavior` field, line 40):

```rust
  pub behavior_module: Option<String>,
  pub behavior_version: Option<String>,
```

Update `ProcessEntry::new()` to populate them from `behavior.meta()`:

```rust
  pub fn new(pid: Pid, behavior: Box<dyn StepBehavior>) -> Self {
    let (behavior_module, behavior_version) =
      if let Some(meta) = behavior.meta() {
        (Some(meta.module), Some(meta.version))
      } else {
        (None, None)
      };
    Self {
      pid,
      state: ProcessRuntimeState::Ready,
      mailbox: MultiLaneMailbox::new(),
      parent: None,
      trap_exit: false,
      selective_tag: None,
      behavior,
      behavior_module,
      behavior_version,
      reductions: 0,
      kill_requested: false,
      exit_reason: None,
      max_turn_wall_clock: std::time::Duration::from_secs(5),
      turn_overrun_flag: false,
      last_turn_duration: None,
    }
  }
```

**Step 4: Run tests to verify they pass**

Run: `cd zeptovm && cargo test --lib kernel::process_table::tests -- --nocapture`
Expected: ALL PASS

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/process_table.rs
git commit -m "feat(process): add behavior_module and behavior_version fields to ProcessEntry

Populated from behavior.meta() at construction time so runtime
can record version metadata without repeated meta() calls.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: Record behavior version in journal at spawn

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs:222-238` (spawn method)
- Modify: `zeptovm/src/kernel/event_bus.rs:10-13` (RuntimeEvent::ProcessSpawned)
- Test: `zeptovm/src/kernel/runtime.rs` (inline tests)

**Step 1: Write the failing test**

Add a test to `runtime.rs` tests:

```rust
#[test]
fn test_spawn_records_behavior_version_in_journal() {
    use crate::core::behavior::{BehaviorMeta, StepBehavior};

    struct VersionedAgent;
    impl StepBehavior for VersionedAgent {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(
            &mut self,
            _msg: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
        fn meta(&self) -> Option<BehaviorMeta> {
            Some(BehaviorMeta {
                module: "versioned_agent".to_string(),
                version: "1.0.0".to_string(),
            })
        }
    }

    let mut rt = Runtime::builder().build();
    let pid = rt.spawn(Box::new(VersionedAgent));

    // Check event bus has the real module name
    let events = rt.events();
    let spawn_event = events.iter().find(|(_, e)| {
        matches!(e, RuntimeEvent::ProcessSpawned { .. })
    });
    assert!(spawn_event.is_some());
    match &spawn_event.unwrap().1 {
        RuntimeEvent::ProcessSpawned {
            behavior_module,
            behavior_version,
            ..
        } => {
            assert_eq!(behavior_module, "versioned_agent");
            assert_eq!(
                behavior_version.as_deref(),
                Some("1.0.0"),
            );
        }
        _ => panic!("wrong event type"),
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::runtime::tests::test_spawn_records_behavior_version_in_journal -- --nocapture 2>&1 | head -30`
Expected: FAIL — `behavior_version` field does not exist on `RuntimeEvent::ProcessSpawned`

**Step 3: Write minimal implementation**

**3a.** Add `behavior_version: Option<String>` to `RuntimeEvent::ProcessSpawned` in `event_bus.rs`:

```rust
ProcessSpawned {
    pid: Pid,
    behavior_module: String,
    behavior_version: Option<String>,
},
```

**3b.** Update all existing `RuntimeEvent::ProcessSpawned` constructions in `event_bus.rs` tests to include `behavior_version: None`.

**3c.** Update `Runtime::spawn()` in `runtime.rs` to extract meta and record real values:

```rust
  pub fn spawn(
    &mut self,
    behavior: Box<dyn StepBehavior>,
  ) -> Pid {
    let meta = behavior.meta();
    let pid = self.engine.spawn(behavior);
    let (module, version) = match meta {
      Some(m) => (m.module, Some(m.version)),
      None => ("unknown".into(), None),
    };
    self.event_bus.emit(RuntimeEvent::ProcessSpawned {
      pid,
      behavior_module: module,
      behavior_version: version,
    });
    self.metrics.inc("processes.spawned");
    self.metrics.gauge_set(
      "processes.active",
      self.engine.process_count() as i64,
    );
    pid
  }
```

**3d.** Fix any other `RuntimeEvent::ProcessSpawned` pattern matches in `runtime.rs` and `event_bus.rs` to include the new field.

**Step 4: Run tests to verify they pass**

Run: `cd zeptovm && cargo test --lib -- --nocapture 2>&1 | tail -5`
Expected: ALL PASS

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs zeptovm/src/kernel/event_bus.rs
git commit -m "feat(runtime): record behavior version in events at spawn

Extract meta() from behavior before spawning and emit real
module name and version in ProcessSpawned event. Previously
hardcoded behavior_module to 'unknown'.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: Record behavior version in journal ProcessSpawned payload

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs` (spawn method — journal write)
- Test: `zeptovm/src/kernel/runtime.rs` (inline tests)

**Context:** The journal already records `ProcessSpawned` entries but with `None` payload. This task adds a JSON payload containing `behavior_module` and `behavior_version` so recovery can read it back.

**Step 1: Write the failing test**

```rust
#[test]
fn test_journal_records_behavior_version_payload() {
    use crate::core::behavior::{BehaviorMeta, StepBehavior};

    struct VersionedAgent;
    impl StepBehavior for VersionedAgent {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(
            &mut self,
            _msg: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
        fn meta(&self) -> Option<BehaviorMeta> {
            Some(BehaviorMeta {
                module: "journal_agent".to_string(),
                version: "3.0.0".to_string(),
            })
        }
    }

    let journal =
        crate::durability::journal::Journal::open_in_memory().unwrap();
    let snapshots =
        crate::durability::snapshot::SnapshotStore::open_in_memory()
            .unwrap();

    let mut rt = Runtime::builder()
        .with_journal(journal)
        .with_snapshots(snapshots)
        .build();
    let pid = rt.spawn(Box::new(VersionedAgent));

    // Read journal for this pid
    let entries = rt
        .journal()
        .unwrap()
        .replay(pid, 0)
        .unwrap();
    let spawn_entry = entries.iter().find(|e| {
        e.entry_type
            == crate::durability::journal::JournalEntryType::ProcessSpawned
    });
    assert!(
        spawn_entry.is_some(),
        "should have ProcessSpawned journal entry",
    );
    let payload = spawn_entry.unwrap().payload.as_ref().unwrap();
    let val: serde_json::Value =
        serde_json::from_slice(payload).unwrap();
    assert_eq!(val["behavior_module"], "journal_agent");
    assert_eq!(val["behavior_version"], "3.0.0");
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::runtime::tests::test_journal_records_behavior_version_payload -- --nocapture 2>&1 | head -30`
Expected: FAIL — ProcessSpawned entry has no payload (or payload doesn't contain version)

**Step 3: Write minimal implementation**

In `Runtime::spawn()`, after extracting meta, if the runtime has a journal, write a `ProcessSpawned` entry with the version payload. Find where `ProcessSpawned` journal entries are currently written. If they're written in the turn executor, update that location. If spawn doesn't write to journal yet, add the journal write in `spawn()`:

```rust
// After the event_bus.emit, if we have a journal, write spawn entry
if let Some(ref journal) = self.journal {
    let payload = serde_json::json!({
        "behavior_module": module,
        "behavior_version": version,
    });
    let _ = journal.append(&JournalEntry::new(
        pid,
        JournalEntryType::ProcessSpawned,
        Some(serde_json::to_vec(&payload).unwrap()),
    ));
}
```

Note: Check if `spawn()` already writes to journal. If so, modify the existing write to include the payload. If not, add a new write. The key is that `ProcessSpawned` entries in the journal now carry a JSON blob with `behavior_module` and `behavior_version` fields.

**Step 4: Run tests to verify they pass**

Run: `cd zeptovm && cargo test --lib -- --nocapture 2>&1 | tail -5`
Expected: ALL PASS

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(journal): record behavior version in ProcessSpawned payload

ProcessSpawned journal entries now carry a JSON payload with
behavior_module and behavior_version. This enables version
comparison during recovery.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: Add version check + migration to RecoveryCoordinator

**Files:**
- Modify: `zeptovm/src/kernel/recovery.rs:54-174` (recover_process method)
- Test: `zeptovm/src/kernel/recovery.rs` (inline tests)

**Context:** This is the core logic. After loading the snapshot but before building the ProcessEntry, compare the journaled version against the current behavior's `meta().version`. If they differ, call `migrate()`. If it returns `None`, fail.

**Step 1: Write the failing tests**

Add four tests to `recovery.rs`:

```rust
#[test]
fn test_recovery_same_version_succeeds() {
    use crate::core::behavior::BehaviorMeta;

    struct V1;
    impl StepBehavior for V1 {
        fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
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
    let snapshots = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    // Record spawn with version 1.0.0
    let payload = serde_json::to_vec(&serde_json::json!({
        "behavior_module": "agent",
        "behavior_version": "1.0.0",
    }))
    .unwrap();
    journal
        .append(&JournalEntry::new(
            pid,
            JournalEntryType::ProcessSpawned,
            Some(payload),
        ))
        .unwrap();

    let coord = RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(pid, &|| Box::new(V1));
    assert!(result.is_ok(), "same version should recover fine");
}

#[test]
fn test_recovery_version_mismatch_with_migration() {
    use crate::core::behavior::BehaviorMeta;

    struct V2;
    impl StepBehavior for V2 {
        fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _: &Reason) {}
        fn restore(&mut self, state: &[u8]) -> Result<(), String> {
            // Accept any state
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
    let snapshots = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    // Spawn was recorded at version 1.0.0
    let payload = serde_json::to_vec(&serde_json::json!({
        "behavior_module": "agent",
        "behavior_version": "1.0.0",
    }))
    .unwrap();
    journal
        .append(&JournalEntry::new(
            pid,
            JournalEntryType::ProcessSpawned,
            Some(payload),
        ))
        .unwrap();

    // Save a snapshot so there's state to migrate
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

    let coord = RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(pid, &|| Box::new(V2));
    assert!(
        result.is_ok(),
        "version mismatch with migration should succeed: {:?}",
        result.err(),
    );
}

#[test]
fn test_recovery_version_mismatch_no_migration_fails() {
    use crate::core::behavior::BehaviorMeta;

    struct V2NoMigrate;
    impl StepBehavior for V2NoMigrate {
        fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _: &Reason) {}
        fn meta(&self) -> Option<BehaviorMeta> {
            Some(BehaviorMeta {
                module: "agent".to_string(),
                version: "2.0.0".to_string(),
            })
        }
        // No migrate() override — default returns None
    }

    let journal = Journal::open_in_memory().unwrap();
    let snapshots = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    // Spawn was recorded at version 1.0.0
    let payload = serde_json::to_vec(&serde_json::json!({
        "behavior_module": "agent",
        "behavior_version": "1.0.0",
    }))
    .unwrap();
    journal
        .append(&JournalEntry::new(
            pid,
            JournalEntryType::ProcessSpawned,
            Some(payload),
        ))
        .unwrap();

    let coord = RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(pid, &|| Box::new(V2NoMigrate));
    assert!(result.is_err(), "version mismatch without migration should fail");
    let err = result.unwrap_err();
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
        fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
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
    let snapshots = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    // Old-style spawn entry: no payload (pre-versioning)
    journal
        .append(&JournalEntry::new(
            pid,
            JournalEntryType::ProcessSpawned,
            None,
        ))
        .unwrap();

    let coord = RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(pid, &|| Box::new(V1));
    assert!(
        result.is_ok(),
        "no version recorded should recover normally (backward compat)",
    );
}
```

**Step 2: Run tests to verify they fail**

Run: `cd zeptovm && cargo test --lib kernel::recovery::tests::test_recovery_version_mismatch -- --nocapture 2>&1 | head -30`
Expected: FAIL — recovery doesn't check versions yet

**Step 3: Write minimal implementation**

In `RecoveryCoordinator::recover_process()`, after Step 1 (snapshot load) and before Step 2 (journal replay), add version checking logic:

```rust
    // Step 1.5: Check behavior version compatibility
    // Find the ProcessSpawned entry to get journaled version
    let spawn_entries = self
        .journal
        .replay(pid, 0)
        .map_err(|e| format!("journal read failed: {e}"))?;
    let journaled_version = spawn_entries
        .iter()
        .find(|e| e.entry_type == JournalEntryType::ProcessSpawned)
        .and_then(|e| e.payload.as_ref())
        .and_then(|p| serde_json::from_slice::<serde_json::Value>(p).ok())
        .and_then(|v| {
            v.get("behavior_version")
                .and_then(|bv| bv.as_str().map(String::from))
        });

    let current_meta = behavior.meta();
    let current_version =
        current_meta.as_ref().map(|m| m.version.as_str());

    // Only check if both old and new have versions
    if let (Some(ref old_ver), Some(new_ver)) =
        (&journaled_version, current_version)
    {
        if old_ver != new_ver {
            // Version mismatch — attempt migration
            let snapshot_state = if let Some(ref snap) = snapshot {
                serde_json::from_slice(&snap.state_blob)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            };

            match behavior.migrate(old_ver, snapshot_state) {
                Some(new_state) => {
                    // Migration succeeded — restore with transformed state
                    let new_blob = serde_json::to_vec(&new_state)
                        .map_err(|e| {
                            format!("migration state serialization failed: {e}")
                        })?;
                    behavior.restore(&new_blob)?;
                }
                None => {
                    return Err(format!(
                        "behavior version mismatch: {} -> {}, no migration provided",
                        old_ver, new_ver,
                    ));
                }
            }
        }
    }
```

Note: The existing snapshot restore (line 68-73) still runs first. If a migration happens, it calls `restore()` again with the migrated state. This means the flow is: restore original → check version → if mismatch, migrate → restore migrated. This is correct because `restore()` replaces state entirely.

**Step 4: Run tests to verify they pass**

Run: `cd zeptovm && cargo test --lib kernel::recovery::tests -- --nocapture`
Expected: ALL PASS (including 4 new + 6 existing)

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/recovery.rs
git commit -m "feat(recovery): add behavior version check and migration on recovery

During process recovery, compare journaled behavior version against
current version. If mismatch: call migrate() on behavior. If
migrate returns None, fail the process. Old journal entries without
version are handled gracefully (skip check).

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: Full integration test

**Files:**
- Test: `zeptovm/src/kernel/recovery.rs` (or `runtime.rs`, whichever has access to both journal + recovery)

**Context:** End-to-end test: spawn a versioned behavior → journal records version → simulate crash → recover with new version behavior that has migrate() → verify recovery succeeds with migrated state.

**Step 1: Write the integration test**

```rust
#[test]
fn test_full_versioned_recovery_flow() {
    use crate::core::behavior::BehaviorMeta;

    // V1 behavior — saves state as JSON
    struct AgentV1 {
        count: u32,
    }
    impl StepBehavior for AgentV1 {
        fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
            self.count = 0;
            StepResult::Continue
        }
        fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
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
        fn restore(&mut self, state: &[u8]) -> Result<(), String> {
            let val: serde_json::Value =
                serde_json::from_slice(state).map_err(|e| e.to_string())?;
            self.count = val["count"].as_u64().unwrap_or(0) as u32;
            Ok(())
        }
        fn meta(&self) -> Option<BehaviorMeta> {
            Some(BehaviorMeta {
                module: "counter".to_string(),
                version: "1.0.0".to_string(),
            })
        }
    }

    // V2 behavior — adds a "label" field
    struct AgentV2 {
        count: u32,
        label: String,
    }
    impl StepBehavior for AgentV2 {
        fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
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
        fn restore(&mut self, state: &[u8]) -> Result<(), String> {
            let val: serde_json::Value =
                serde_json::from_slice(state).map_err(|e| e.to_string())?;
            self.count = val["count"].as_u64().unwrap_or(0) as u32;
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
                s["label"] = serde_json::json!("migrated-from-v1");
                Some(s)
            } else {
                None
            }
        }
    }

    // Simulate: V1 ran, took snapshot, wrote ProcessSpawned
    let journal = Journal::open_in_memory().unwrap();
    let snapshots = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(42);

    // V1 spawn entry
    let spawn_payload = serde_json::to_vec(&serde_json::json!({
        "behavior_module": "counter",
        "behavior_version": "1.0.0",
    }))
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

    // Now recover with V2 factory
    let coord = RecoveryCoordinator::new(&journal, &snapshots);
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
```

**Step 2: Run test to verify it passes**

Run: `cd zeptovm && cargo test --lib kernel::recovery::tests::test_full_versioned_recovery_flow -- --nocapture`
Expected: PASS (if Tasks 1-5 are implemented correctly)

**Step 3: Commit**

```bash
git add zeptovm/src/kernel/recovery.rs
git commit -m "test(recovery): add full versioned recovery integration test

End-to-end test: V1 behavior snapshotted → recover with V2 behavior
that implements migrate() → state transformed with new field.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: Update gap analysis to mark G4 done

**Files:**
- Modify: `docs/plans/2026-03-06-spec-v03-gap-analysis.md:35`

**Step 1: Update the gap analysis**

Change line 35 from:

```
| G4 | Behavior versioning + migration | NOT DONE | No version metadata, no migration |
```

to:

```
| G4 | Behavior versioning + migration | DONE | Version in journal, migrate() callback, recovery-time check |
```

Also update the "Remaining Gaps" section at the bottom to remove G4, leaving only G1.

**Step 2: Commit**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark G4 behavior versioning as DONE in gap analysis

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Summary

| Task | Description | New Tests |
|------|-------------|-----------|
| 1 | Add `migrate()` to StepBehavior trait | 2 |
| 2 | Add version fields to ProcessEntry | 2 |
| 3 | Record version in RuntimeEvent at spawn | 1 |
| 4 | Record version in journal ProcessSpawned payload | 1 |
| 5 | Version check + migration in RecoveryCoordinator | 4 |
| 6 | Full integration test | 1 |
| 7 | Update gap analysis | 0 |
| **Total** | | **11** |
