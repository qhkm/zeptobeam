# G4: Behavior Versioning + Migration — Design Document

**Date:** 2026-03-08
**Status:** Approved
**Goal:** Record behavior version in the journal at spawn. On recovery, detect version changes and call a migration callback to transform snapshot state.

**Architecture:** Recovery-time migration only. No live hot-reload. Version metadata persisted in journal, migration callback on StepBehavior trait, fail-safe on mismatch without migration.

---

## Scope

Record `behavior_module` and `behavior_version` in the journal when a process spawns. On recovery, compare the journaled version against the current behavior's `meta().version`. If they differ, call `migrate(old_version, snapshot_state)` on the behavior. If the behavior doesn't implement `migrate()`, fail the process.

No new structs, no registries, no hot-reload.

---

## Components

### 1. StepBehavior trait — `migrate()` callback

```rust
fn migrate(
  &self,
  old_version: &str,
  state: serde_json::Value,
) -> Option<serde_json::Value> {
  None // default: no migration supported
}
```

Returning `Some(new_state)` means the behavior can transform old state to the new format. Returning `None` means version mismatch is fatal.

### 2. Journal recording at spawn

`Runtime::spawn()` currently emits `behavior_module: "unknown"`. Change to extract `meta()` from the behavior before spawning and record real values.

Add `behavior_module` and `behavior_version` columns to the journal's process spawn entries. For backward compatibility, these are `Option<String>` — old entries without versions are treated as "no version recorded" and skip the version check on recovery.

### 3. Recovery check in RecoveryCoordinator

When replaying a process from journal:

1. Read the journaled `behavior_version`
2. Get the current behavior's `meta().version`
3. If versions match (or no version was recorded in journal): proceed normally
4. If versions differ: call `behavior.migrate(old_version, snapshot_state)`
   - `Some(new_state)` → use transformed state for restore
   - `None` → fail the process: `"behavior version mismatch: {old} -> {new}, no migration provided"`

### 4. ProcessEntry metadata fields

Add `behavior_module: Option<String>` and `behavior_version: Option<String>` fields to `ProcessEntry`. Populated from `meta()` at construction time so the runtime doesn't need to call `meta()` repeatedly.

---

## Existing Integration Points

- `BehaviorMeta` struct — already defined in `core/behavior.rs` with `module` and `version` fields
- `StepBehavior::meta()` — already returns `Option<BehaviorMeta>`, default `None`
- `ProcessEntry::behavior_meta()` — already delegates to `self.behavior.meta()`
- `Runtime::spawn()` — emits `RuntimeEvent::ProcessSpawned` with hardcoded `"unknown"` module
- `RecoveryCoordinator` — already replays journal entries and restores snapshots
- Journal SQLite schema — needs `behavior_module` and `behavior_version` columns added

---

## Testing Strategy

1. `test_behavior_migrate_default_none` — default returns None
2. `test_behavior_migrate_custom` — migration transforms state
3. `test_process_entry_stores_version_fields` — module/version populated from meta()
4. `test_journal_records_behavior_version` — spawn records version in journal
5. `test_recovery_same_version` — same version, normal restore
6. `test_recovery_version_mismatch_with_migration` — version changed, migration succeeds
7. `test_recovery_version_mismatch_no_migration` — version changed, no migrate(), process fails
8. `test_recovery_no_version_recorded` — old journal without version, recover normally

---

## Non-Goals (this iteration)

- Live hot-code-reload (Erlang-style code_change on running processes)
- Behavior factory/registry pattern
- HotCodeRegistry with version tracking across deployments
- Multi-version concurrent execution
