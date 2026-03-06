# Tier 1 Spec Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Address the 4 items the Codex design review flagged as "fix now": soften deterministic shell claim (C1), atomic turn commit (C2), watchdog for handler overruns (C4/A4), and behavior version metadata (A2).

**Architecture:** Four independent changes. C1 is doc-only. C2 adds a shared-connection constructor to TurnExecutor for atomic journal+snapshot commits. C4/A4 adds wall-clock timing to ProcessEntry::step(). A2 adds BehaviorMeta struct and trait method.

**Tech Stack:** Rust, SQLite (rusqlite), std::time::Instant

---

### Task 1: C1 — Soften "deterministic shell" claim in spec

**Files:**
- Modify: `docs/ZEPTOVM-SPEC.md:335-357`
- Create: `docs/HANDLER-CONVENTIONS.md`

**Step 1: Update ZEPTOVM-SPEC.md section 8**

Change the section title and wording. Replace:

```markdown
## 8. Deterministic Shell / Nondeterministic Effects

This is the central design principle.

### 8.1 Deterministic Orchestration Layer (replayable)

Handles: mailbox progression, state transitions, timers, retries, spawn decisions, supervisor semantics, budget counters, policy checks, commit boundaries.

### 8.2 Nondeterministic Effect Layer (recorded, not replayed)
```

With:

```markdown
## 8. Replay-Safe Shell / Nondeterministic Effects

This is the central design principle. The orchestration layer is **intended to be replay-safe**, not strictly deterministic — Rust `handle()` implementations can technically read wall clock, generate randomness, or access environment variables. Replay safety is a convention enforced by handler discipline, not by the runtime. See `docs/HANDLER-CONVENTIONS.md` for guidelines.

### 8.1 Replay-Safe Orchestration Layer

Handles: mailbox progression, state transitions, timers, retries, spawn decisions, supervisor semantics, budget counters, policy checks, commit boundaries.

### 8.2 Nondeterministic Effect Layer (recorded, not replayed)
```

Also update the execution flow comment:

```text
message arrives
  -> replay-safe turn executes
  -> emits EffectRequest
  -> effect runs externally on reactor
  -> result recorded durably
  -> process resumes from recorded EffectResult
```

Also update line 79 ("Replayability" bullet):

```markdown
6. **Replayability** — replay-safe shell + recorded effect results (see Handler Conventions)
```

**Step 2: Create docs/HANDLER-CONVENTIONS.md**

```markdown
# Handler Conventions

## Replay Safety

ZeptoVM handlers (`StepBehavior::handle()`) are **intended to be replay-safe**: given the same message and state, they should produce the same intents. The runtime does not enforce this — it is the handler author's responsibility.

## What handlers SHOULD do

- Read from `Envelope` (the incoming message)
- Read from `&self` (process state)
- Emit intents via `TurnContext` (`ctx.send()`, `ctx.request_effect()`, `ctx.set_state()`)
- Return a `StepResult`

## What handlers SHOULD NOT do

- **Read wall clock** (`std::time::Instant`, `SystemTime`) — use `ctx.request_effect()` for time-sensitive operations
- **Generate randomness** (`rand::thread_rng()`) — use a seeded RNG stored in process state, or request randomness as an effect
- **Read environment variables** (`std::env::var()`) — pass configuration at spawn time via init checkpoint
- **Perform I/O** (file, network, database) — use `EffectRequest` instead
- **Access global mutable state** (`static mut`, `lazy_static` with mutation)

## Why this matters

Replay safety enables:
- **Recovery**: replay journal entries after crash to reconstruct state
- **Debugging**: reproduce exact execution by replaying the same messages
- **Testing**: deterministic test outcomes from fixed message sequences

## When replay safety doesn't matter

For one-shot processes that won't be replayed (e.g., CLI wrappers, test harnesses), these conventions can be relaxed. Mark such behaviors clearly in their documentation.
```

**Step 3: Commit**

```bash
git add docs/ZEPTOVM-SPEC.md docs/HANDLER-CONVENTIONS.md
git commit -m "docs: soften deterministic shell claim, add handler conventions (C1)"
```

---

### Task 2: C2 — Atomic turn commit (shared SQLite connection)

**Files:**
- Modify: `zeptovm/src/durability/journal.rs`
- Modify: `zeptovm/src/durability/snapshot.rs`
- Modify: `zeptovm/src/kernel/turn_executor.rs`

**Step 1: Write failing test for atomic commit**

Add to `zeptovm/src/kernel/turn_executor.rs` tests:

```rust
#[test]
fn test_atomic_commit_both_journal_and_snapshot() {
    let mut executor =
        TurnExecutor::open_in_memory().unwrap();
    // Force every turn to snapshot
    executor = executor.with_snapshot_interval(1);
    let pid = Pid::from_raw(1);

    let turn = TurnCommit {
        pid,
        turn_id: 1,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(b"atomic-state".to_vec()),
    };

    executor.commit(&turn).unwrap();

    // Both journal and snapshot should exist
    let entries =
        executor.journal().replay(pid, 0).unwrap();
    assert!(entries.len() >= 1);

    let snap = executor
        .snapshot_store()
        .load_latest(pid)
        .unwrap();
    assert!(snap.is_some());
    assert_eq!(snap.unwrap().state_blob, b"atomic-state");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p zeptovm test_atomic_commit_both_journal_and_snapshot`
Expected: FAIL — `TurnExecutor::open_in_memory()` doesn't exist yet.

**Step 3: Add `from_connection` constructors to Journal and SnapshotStore**

In `zeptovm/src/durability/journal.rs`, add after `open()`:

```rust
/// Create a Journal using an existing connection (for shared-connection
/// atomic commits).
pub fn from_connection(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS journal (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pid INTEGER NOT NULL,
            entry_type TEXT NOT NULL,
            payload BLOB,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_journal_pid
          ON journal(pid);",
    )?;
    Ok(())
}
```

In `zeptovm/src/durability/snapshot.rs`, add after `open()`:

```rust
/// Create snapshot table on an existing connection (for shared-connection
/// atomic commits).
pub fn init_on_connection(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS snapshots (
            pid INTEGER NOT NULL,
            version INTEGER NOT NULL,
            state_blob BLOB NOT NULL,
            mailbox_cursor INTEGER NOT NULL,
            pending_effects TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (pid, version)
        );",
    )?;
    Ok(())
}
```

**Step 4: Add `open_in_memory()` and `atomic_commit()` to TurnExecutor**

In `zeptovm/src/kernel/turn_executor.rs`, add `use rusqlite::Connection;` to imports and add:

```rust
impl TurnExecutor {
    /// Create a TurnExecutor with a single shared in-memory SQLite
    /// connection, enabling truly atomic journal+snapshot commits.
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("open in-memory DB: {e}"))?;
        Journal::from_connection(&conn)
            .map_err(|e| format!("init journal schema: {e}"))?;
        SnapshotStore::init_on_connection(&conn)
            .map_err(|e| format!("init snapshot schema: {e}"))?;

        // Create Journal and SnapshotStore wrapping the same DB path
        // For in-memory, we use the shared connection for atomic ops
        // but still need the separate structs for reads.
        let journal = Journal::open_in_memory()
            .map_err(|e| format!("journal: {e}"))?;
        let snapshot_store = SnapshotStore::open_in_memory()
            .map_err(|e| format!("snapshot: {e}"))?;

        Ok(Self {
            journal,
            snapshot_store,
            snapshot_interval: 10,
            turn_counter: std::collections::HashMap::new(),
            shared_conn: Some(conn),
        })
    }
    // ...existing methods...
}
```

Add `shared_conn: Option<Connection>` field to TurnExecutor struct. Update the existing `new()` constructor to set `shared_conn: None`.

Update `commit()` to use `shared_conn` when available:

```rust
pub fn commit(&mut self, turn: &TurnCommit) -> Result<(), String> {
    let _span = info_span!(
        "turn_commit",
        pid = turn.pid.raw(),
        turn_id = turn.turn_id,
        intents = turn.journal_entries.len(),
    )
    .entered();

    // Build journal entries (same as before)
    let mut entries = Vec::new();
    entries.push(JournalEntry::new(
        turn.pid,
        JournalEntryType::TurnStarted,
        None,
    ));
    for (_pid, req) in &turn.effect_requests {
        let payload = serde_json::to_vec(req).ok();
        entries.push(JournalEntry::new(
            turn.pid,
            JournalEntryType::EffectRequested,
            payload,
        ));
    }
    for _msg in &turn.outbound_messages {
        entries.push(JournalEntry::new(
            turn.pid,
            JournalEntryType::MessageSent,
            None,
        ));
    }
    entries.extend(turn.journal_entries.clone());

    // Check if we should take a snapshot this turn
    let counter =
        self.turn_counter.entry(turn.pid.raw()).or_insert(0);
    *counter += 1;
    let should_snapshot = *counter % self.snapshot_interval == 0;

    if let Some(ref conn) = self.shared_conn {
        // === Atomic path: single transaction for journal + snapshot ===
        let tx = conn.unchecked_transaction()
            .map_err(|e| format!("begin transaction: {e}"))?;

        let mut batch_ids = Vec::with_capacity(entries.len());
        for entry in &entries {
            tx.execute(
                "INSERT INTO journal (pid, entry_type, payload) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    entry.pid.raw() as i64,
                    entry.entry_type.as_str(),
                    entry.payload,
                ],
            ).map_err(|e| format!("journal insert: {e}"))?;
            batch_ids.push(tx.last_insert_rowid());
        }

        if should_snapshot {
            if let Some(ref state_blob) = turn.state_snapshot {
                tx.execute(
                    "INSERT OR REPLACE INTO snapshots \
                     (pid, version, state_blob, mailbox_cursor, \
                      pending_effects) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        turn.pid.raw() as i64,
                        *counter as i64,
                        state_blob,
                        0i64,
                        Option::<String>::None,
                    ],
                ).map_err(|e| format!("snapshot insert: {e}"))?;

                // Compact journal within same transaction
                if let Some(&first_id) = batch_ids.first() {
                    let _ = tx.execute(
                        "DELETE FROM journal WHERE pid = ?1 AND id < ?2",
                        rusqlite::params![
                            turn.pid.raw() as i64,
                            first_id,
                        ],
                    );
                }
            }
        }

        tx.commit()
            .map_err(|e| format!("commit transaction: {e}"))?;
    } else {
        // === Legacy path: separate connections (backward compat) ===
        let batch_ids = self
            .journal
            .append_batch(&entries)
            .map_err(|e| format!("journal commit failed: {e}"))?;

        if should_snapshot {
            if let Some(ref state_blob) = turn.state_snapshot {
                let snapshot = Snapshot {
                    pid: turn.pid,
                    version: *counter as i64,
                    state_blob: state_blob.clone(),
                    mailbox_cursor: 0,
                    pending_effects: None,
                };
                self.snapshot_store
                    .save(&snapshot)
                    .map_err(|e| format!("snapshot save failed: {e}"))?;

                if let Some(&first_id) = batch_ids.first() {
                    if let Err(e) =
                        self.journal.truncate_before(turn.pid, first_id)
                    {
                        tracing::warn!(
                            pid = turn.pid.raw(),
                            error = %e,
                            "journal compaction failed (non-fatal)"
                        );
                    }
                }
            }
        }
    }

    Ok(())
}
```

Also update `journal()` and `snapshot_store()` to work with shared connection. Add helper methods for reading from shared connection:

```rust
/// Replay journal entries (works with both shared and separate connections).
pub fn replay_journal(
    &self,
    pid: Pid,
    since_id: i64,
) -> Result<Vec<JournalEntry>, String> {
    if let Some(ref conn) = self.shared_conn {
        // Read from shared connection
        let mut stmt = conn
            .prepare(
                "SELECT id, pid, entry_type, payload FROM journal \
                 WHERE pid = ?1 AND id > ?2 ORDER BY id",
            )
            .map_err(|e| format!("prepare: {e}"))?;
        let entries = stmt
            .query_map(
                rusqlite::params![pid.raw() as i64, since_id],
                |row| {
                    let id: i64 = row.get(0)?;
                    let pid_raw: i64 = row.get(1)?;
                    let entry_type_str: String = row.get(2)?;
                    let payload: Option<Vec<u8>> = row.get(3)?;
                    Ok(JournalEntry {
                        id: Some(id),
                        pid: Pid::from_raw(pid_raw as u64),
                        entry_type: JournalEntryType::from_str(
                            &entry_type_str,
                        )
                        .unwrap_or(JournalEntryType::TurnStarted),
                        payload,
                    })
                },
            )
            .map_err(|e| format!("query: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("collect: {e}"))?;
        Ok(entries)
    } else {
        self.journal
            .replay(pid, since_id)
            .map_err(|e| format!("replay: {e}"))
    }
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm turn_executor`
Expected: All tests pass including the new atomic test.

**Step 6: Commit**

```bash
git add zeptovm/src/durability/journal.rs zeptovm/src/durability/snapshot.rs zeptovm/src/kernel/turn_executor.rs
git commit -m "feat(durability): atomic turn commit via shared SQLite connection (C2)"
```

---

### Task 3: C4/A4 — Watchdog for handler overruns

**Files:**
- Modify: `zeptovm/src/kernel/process_table.rs`

**Step 1: Write failing test for watchdog**

Add to `zeptovm/src/kernel/process_table.rs` tests:

```rust
#[test]
fn test_watchdog_slow_handler_sets_overrun() {
    struct SlowHandler;
    impl StepBehavior for SlowHandler {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(
            &mut self,
            _msg: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            std::thread::sleep(std::time::Duration::from_millis(20));
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(SlowHandler));
    // Set a very low threshold so the test handler triggers it
    p.set_max_turn_wall_clock(std::time::Duration::from_millis(5));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Continue));
    assert!(p.turn_overrun(), "expected overrun flag to be set");
}

#[test]
fn test_watchdog_fast_handler_no_overrun() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.set_max_turn_wall_clock(std::time::Duration::from_secs(5));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Continue));
    assert!(
        !p.turn_overrun(),
        "expected no overrun for fast handler"
    );
}

#[test]
fn test_watchdog_overrun_resets_each_step() {
    struct SlowHandler;
    impl StepBehavior for SlowHandler {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(
            &mut self,
            _msg: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            std::thread::sleep(std::time::Duration::from_millis(20));
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(SlowHandler));
    p.set_max_turn_wall_clock(std::time::Duration::from_millis(5));
    p.init(None);

    // First step: slow -> overrun
    p.mailbox.push(Envelope::text(pid, "go"));
    p.step();
    assert!(p.turn_overrun());

    // Second step: no message -> Wait, overrun resets
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Wait));
    assert!(!p.turn_overrun());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_watchdog`
Expected: FAIL — `set_max_turn_wall_clock()` and `turn_overrun()` don't exist.

**Step 3: Add watchdog fields and logic to ProcessEntry**

Add to `ProcessEntry` struct:

```rust
pub struct ProcessEntry {
    pub pid: Pid,
    pub state: ProcessRuntimeState,
    pub mailbox: MultiLaneMailbox,
    pub parent: Option<Pid>,
    pub trap_exit: bool,
    behavior: Box<dyn StepBehavior>,
    reductions: u32,
    kill_requested: bool,
    exit_reason: Option<Reason>,
    max_turn_wall_clock: std::time::Duration,
    turn_overrun_flag: bool,
    last_turn_duration: Option<std::time::Duration>,
}
```

Update `new()`:

```rust
pub fn new(pid: Pid, behavior: Box<dyn StepBehavior>) -> Self {
    Self {
        pid,
        state: ProcessRuntimeState::Ready,
        mailbox: MultiLaneMailbox::new(),
        parent: None,
        trap_exit: false,
        behavior,
        reductions: 0,
        kill_requested: false,
        exit_reason: None,
        max_turn_wall_clock: std::time::Duration::from_secs(5),
        turn_overrun_flag: false,
        last_turn_duration: None,
    }
}
```

Add methods:

```rust
pub fn set_max_turn_wall_clock(&mut self, d: std::time::Duration) {
    self.max_turn_wall_clock = d;
}

pub fn turn_overrun(&self) -> bool {
    self.turn_overrun_flag
}

pub fn last_turn_duration(&self) -> Option<std::time::Duration> {
    self.last_turn_duration
}
```

In `step()`, wrap the `behavior.handle()` call with timing:

```rust
// User/effect messages and fall-through signals go to
// behavior.handle() with catch_unwind
self.reductions += 1;
self.turn_overrun_flag = false; // reset each step

let start = std::time::Instant::now();
let result = panic::catch_unwind(AssertUnwindSafe(|| {
    self.behavior.handle(env, &mut ctx)
}));
let elapsed = start.elapsed();
self.last_turn_duration = Some(elapsed);

if elapsed > self.max_turn_wall_clock {
    self.turn_overrun_flag = true;
    tracing::warn!(
        pid = self.pid.raw(),
        elapsed_ms = elapsed.as_millis() as u64,
        limit_ms = self.max_turn_wall_clock.as_millis() as u64,
        "handler overrun: turn exceeded wall-clock limit"
    );
}
```

Also reset overrun flag at the top of `step()` (before the mailbox pop) so it resets on Wait paths too:

```rust
pub fn step(&mut self) -> (StepResult, TurnContext) {
    let mut ctx = TurnContext::new(self.pid);
    self.turn_overrun_flag = false;
    self.last_turn_duration = None;
    // ... rest of step()
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm process_table`
Expected: All tests pass including watchdog tests.

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/process_table.rs
git commit -m "feat(watchdog): wall-clock timing for handler overruns (C4/A4)"
```

---

### Task 4: A2 — Behavior version metadata

**Files:**
- Modify: `zeptovm/src/core/behavior.rs`
- Modify: `zeptovm/src/kernel/process_table.rs`

**Step 1: Write failing test for BehaviorMeta**

Add to `zeptovm/src/core/behavior.rs` tests:

```rust
#[test]
fn test_behavior_meta_default_none() {
    let b = EchoStep;
    assert!(b.meta().is_none());
}

#[test]
fn test_behavior_meta_custom() {
    struct Versioned;
    impl StepBehavior for Versioned {
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
                module: "my_agent".to_string(),
                version: "1.2.0".to_string(),
            })
        }
    }

    let b = Versioned;
    let meta = b.meta().unwrap();
    assert_eq!(meta.module, "my_agent");
    assert_eq!(meta.version, "1.2.0");
}
```

Add to `zeptovm/src/kernel/process_table.rs` tests:

```rust
#[test]
fn test_process_entry_stores_behavior_meta() {
    use crate::core::behavior::BehaviorMeta;

    struct MetaBehavior;
    impl StepBehavior for MetaBehavior {
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
                module: "test_agent".to_string(),
                version: "0.1.0".to_string(),
            })
        }
    }

    let pid = Pid::new();
    let p = ProcessEntry::new(pid, Box::new(MetaBehavior));
    let meta = p.behavior_meta();
    assert!(meta.is_some());
    let meta = meta.unwrap();
    assert_eq!(meta.module, "test_agent");
    assert_eq!(meta.version, "0.1.0");
}

#[test]
fn test_process_entry_no_meta_default() {
    let pid = Pid::new();
    let p = ProcessEntry::new(pid, Box::new(Echo));
    assert!(p.behavior_meta().is_none());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_behavior_meta`
Expected: FAIL — `BehaviorMeta` struct and `meta()` method don't exist.

**Step 3: Add BehaviorMeta struct and trait method**

In `zeptovm/src/core/behavior.rs`, add before the trait:

```rust
/// Metadata about a behavior implementation for versioning and migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BehaviorMeta {
    pub module: String,
    pub version: String,
}
```

Add to `StepBehavior` trait:

```rust
/// Opt-in: return metadata about this behavior (module name, version).
/// Recorded in the journal at process spawn for future migration support.
fn meta(&self) -> Option<BehaviorMeta> {
    None
}
```

**Step 4: Add behavior_meta() accessor to ProcessEntry**

In `zeptovm/src/kernel/process_table.rs`, add:

```rust
/// Get behavior metadata (module, version) if the behavior provides it.
pub fn behavior_meta(&self) -> Option<BehaviorMeta> {
    self.behavior.meta()
}
```

Add `use crate::core::behavior::BehaviorMeta;` to the imports at the top of process_table.rs (it's already importing `StepBehavior`).

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm behavior_meta && cargo test -p zeptovm process_entry`
Expected: All tests pass.

**Step 6: Commit**

```bash
git add zeptovm/src/core/behavior.rs zeptovm/src/kernel/process_table.rs
git commit -m "feat(behavior): add BehaviorMeta for version metadata (A2)"
```

---

### Task 5: Run full test suite and final commit

**Step 1: Run all tests**

Run: `cargo test -p zeptovm`
Expected: All tests pass (297 existing + new tests).

**Step 2: Verify no warnings**

Run: `cargo test -p zeptovm 2>&1 | grep -i warning`
Expected: No unexpected warnings.

**Step 3: Update gap analysis**

Edit `docs/plans/2026-03-06-spec-v03-gap-analysis.md`:
- Change C1 status to `DONE`
- Change C2 status to `DONE`
- Change C4 status to `DONE`
- Change A2 status to `DONE`
- Change A4 status to `DONE`

**Step 4: Commit gap analysis update**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark Tier 1 spec fixes as DONE in gap analysis"
```
