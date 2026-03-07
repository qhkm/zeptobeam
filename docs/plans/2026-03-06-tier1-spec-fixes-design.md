# Tier 1 — Spec Review Fixes Design

## Goal

Address the 4 items the Codex design review explicitly flagged as "fix now": soften deterministic shell claim, atomic turn commit, watchdog for handler overruns, and behavior version metadata.

## Architecture

Four independent, focused changes. No new modules — modifications to existing code plus one conventions doc.

## Tech Stack

Rust, SQLite (rusqlite)

---

## Item C1: Soften "deterministic shell" claim

The review says: handler is not truly deterministic — Rust `handle()` can read wall clock, generate randomness, read env vars. Soften the claim to "intended to be replay-safe" and document conventions.

**Change:** Update `docs/ZEPTOVM-SPEC.md` wording. Add a `docs/HANDLER-CONVENTIONS.md` documenting what handlers should and should not do.

No code change needed.

---

## Item C2: Atomic turn commit (SQLite transaction)

Currently `TurnExecutor::commit()` writes journal entries and snapshots as separate operations. If the process crashes between journal write and snapshot write, state is inconsistent.

**Change:** Use a single SQLite transaction wrapping both journal append and snapshot save. This requires either:

1. Both `Journal` and `SnapshotStore` share a single SQLite connection, or
2. `TurnExecutor` holds a single connection and manages both tables.

Current state: `Journal` and `SnapshotStore` each open their own in-memory SQLite connection. For in-memory DBs, they can't share a transaction across connections.

**Approach:** Add an `atomic_commit()` method to `TurnExecutor` that opens a single connection owning both journal and snapshot tables. For the existing in-memory constructors, both Journal and SnapshotStore already use separate connections, so we need a new constructor where TurnExecutor owns a shared connection.

Simplest path: add `TurnExecutor::open_in_memory()` that creates one shared SQLite connection, creates both tables, and wraps journal+snapshot writes in a single transaction. The existing `new(journal, snapshot_store)` constructor remains for backward compat.

---

## Item C4/A4: Watchdog for handler overruns

The review says: add max wall-clock per turn, max mailbox burst rate, supervisor signal on overrun.

**Change:** In `ProcessEntry::step()`, measure wall-clock duration of the `behavior.handle()` call. If it exceeds a configurable threshold, set a flag that the scheduler can use to emit a warning or send a supervisor signal.

Components:
- `max_turn_wall_clock: Duration` field on ProcessEntry (default 5s)
- `turn_overrun: bool` flag set when a turn exceeds the limit
- Scheduler checks flag after stepping, emits warning + optional signal
- `max_mailbox_burst: u32` — limit messages processed per tick per process (already partially implemented via `max_reductions`)

For v1: measure and warn. Don't kill — let the supervisor decide.

---

## Item A2: Behavior version metadata

The review says: add behavior_module, behavior_version, behavior_checksum to ProcessEntry/journal now, even if migration is unimplemented.

**Change:** Add metadata fields to `ProcessEntry`:
```rust
pub behavior_module: Option<String>,
pub behavior_version: Option<String>,
```

Add a `BehaviorMeta` trait method to `StepBehavior`:
```rust
fn meta(&self) -> Option<BehaviorMeta> { None }
```

Where `BehaviorMeta` is:
```rust
pub struct BehaviorMeta {
    pub module: String,
    pub version: String,
}
```

Record in journal on process spawn. No migration logic — just metadata.

---

## Error Handling

- C2: If the atomic transaction fails, the entire turn is rolled back. No partial writes.
- C4: Watchdog overruns produce warnings, not crashes. Supervisor can opt in to kill.

## Testing

- C1: No code tests (doc only)
- C2: Test that journal + snapshot are either both written or neither. Test crash recovery consistency.
- C4: Test that a slow handler triggers the overrun flag. Test that scheduler emits warning.
- A2: Test that BehaviorMeta is stored in ProcessEntry and appears in journal.
