# A3: Effect State Machine — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add explicit `EffectState` enum tracking every in-flight effect through its lifecycle, serving both live observability and crash recovery.

**Architecture:** `EffectState` enum on `PendingEffect` in the scheduler. Reactor reports transitions via `ReactorMessage` enum on its existing channel. Runtime journals transitions. `RecoveryCoordinator` replays journal to reconstruct last-known state.

**Tech Stack:** Rust, ZeptoVM kernel (core/effect.rs, kernel/scheduler.rs, kernel/reactor.rs, kernel/runtime.rs, kernel/recovery.rs, kernel/event_bus.rs, durability/journal.rs)

---

### Task 1: Add `EffectState` enum and `Serialize`/`Deserialize` derives

**Files:**
- Modify: `zeptovm/src/core/effect.rs:160-168` (after EffectStatus)

**Step 1: Add `EffectState` enum**

After the `EffectStatus` enum (line 168), add:

```rust
/// Lifecycle state of an in-flight effect.
#[derive(
    Debug, Clone, PartialEq, Serialize, Deserialize,
)]
pub enum EffectState {
    /// Queued in scheduler, not yet sent to reactor.
    Pending,
    /// Sent to reactor for execution.
    Dispatched { dispatched_at_ms: u64 },
    /// Execution failed, waiting for backoff before retry.
    Retrying { attempt: u32, next_at_ms: u64 },
    /// Streaming response in progress.
    Streaming { chunks_received: u32 },
    /// Terminal state.
    Completed(EffectStatus),
}

impl EffectState {
    /// Returns true if this is a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, EffectState::Completed(_))
    }
}
```

**Step 2: Add tests**

```rust
#[test]
fn test_effect_state_pending_default() {
    let s = EffectState::Pending;
    assert!(!s.is_terminal());
}

#[test]
fn test_effect_state_dispatched() {
    let s = EffectState::Dispatched {
        dispatched_at_ms: 1000,
    };
    assert!(!s.is_terminal());
}

#[test]
fn test_effect_state_retrying() {
    let s = EffectState::Retrying {
        attempt: 2,
        next_at_ms: 5000,
    };
    assert!(!s.is_terminal());
}

#[test]
fn test_effect_state_streaming() {
    let s = EffectState::Streaming {
        chunks_received: 3,
    };
    assert!(!s.is_terminal());
}

#[test]
fn test_effect_state_completed_is_terminal() {
    let s =
        EffectState::Completed(EffectStatus::Succeeded);
    assert!(s.is_terminal());
    let s2 =
        EffectState::Completed(EffectStatus::Failed);
    assert!(s2.is_terminal());
}

#[test]
fn test_effect_state_serializable() {
    let s = EffectState::Retrying {
        attempt: 1,
        next_at_ms: 2000,
    };
    let json = serde_json::to_string(&s).unwrap();
    let deser: EffectState =
        serde_json::from_str(&json).unwrap();
    assert_eq!(deser, s);
}
```

**Step 3: Run tests**

Run: `cargo test -p zeptovm --lib -- effect::tests`
Expected: All tests pass

**Step 4: Commit**

```bash
git add zeptovm/src/core/effect.rs
git commit -m "feat(effect): add EffectState enum with lifecycle states"
```

---

### Task 2: Add `ReactorMessage` enum and refactor Reactor channel

**Files:**
- Modify: `zeptovm/src/kernel/reactor.rs:80-84` (EffectCompletion)
- Modify: `zeptovm/src/kernel/reactor.rs:92-96` (Reactor struct)
- Modify: `zeptovm/src/kernel/reactor.rs:112-184` (start_with_config)
- Modify: `zeptovm/src/kernel/reactor.rs:192-209` (try_recv, drain_completions)
- Modify: `zeptovm/src/kernel/reactor.rs:304-357` (execute_effect_with_retry_configured)

**Step 1: Add `ReactorMessage` enum**

After `EffectCompletion` (line 84), add:

```rust
/// Message from reactor to runtime. Carries either a
/// state transition update or a final completion.
#[derive(Debug)]
pub enum ReactorMessage {
    /// Intermediate state change (Dispatched, Retrying,
    /// Streaming).
    StateChanged {
        effect_id: EffectId,
        pid: Pid,
        new_state: EffectState,
    },
    /// Final or streaming completion (existing type).
    Completion(EffectCompletion),
}
```

Add needed imports at top of file:
```rust
use crate::core::effect::{
    EffectId, EffectKind, EffectRequest, EffectResult,
    EffectState, EffectStatus,
};
```

**Step 2: Change Reactor channel type**

Change `Reactor` struct (line 92-96):
```rust
pub struct Reactor {
  dispatch_tx: Sender<EffectDispatch>,
  completion_rx: Receiver<ReactorMessage>,
  _handle: thread::JoinHandle<()>,
}
```

In `start_with_config` (line 124-125), change:
```rust
let (completion_tx, completion_rx) =
    unbounded::<ReactorMessage>();
```

**Step 3: Wrap existing completion sends in `ReactorMessage::Completion`**

In `start_with_config`, the inner spawn (line 155-168):
```rust
tokio::spawn(
    async move {
        let result =
            execute_effect_with_retry_configured(
                &dispatch.request,
                &cfg,
                &tx,
                pid,
            )
            .await;
        let _ = tx.send(
            ReactorMessage::Completion(
                EffectCompletion { pid, result },
            ),
        );
    }
    .instrument(span),
);
```

The `completion_tx` type in `execute_effect_with_retry_configured` and `execute_effect_once_configured` changes from `Sender<EffectCompletion>` to `Sender<ReactorMessage>`. Update all signatures accordingly.

**Step 4: Send `StateChanged` for Dispatched before first attempt**

In `execute_effect_with_retry_configured` (around line 313), before the retry loop, add:

```rust
let now_ms = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis() as u64;
let _ = completion_tx.send(ReactorMessage::StateChanged {
    effect_id: request.effect_id,
    pid,
    new_state: EffectState::Dispatched {
        dispatched_at_ms: now_ms,
    },
});
```

**Step 5: Send `StateChanged` for Retrying on each retry**

In the retry backoff branch (around line 339-348), before the sleep:

```rust
let delay =
    request.retry.backoff.delay_ms(attempt);
if delay > 0 {
    let retry_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        + delay;
    let _ = completion_tx.send(
        ReactorMessage::StateChanged {
            effect_id: request.effect_id,
            pid,
            new_state: EffectState::Retrying {
                attempt: attempt + 1,
                next_at_ms: retry_at,
            },
        },
    );
    tokio::time::sleep(
        std::time::Duration::from_millis(delay),
    )
    .await;
}
```

**Step 6: Send `StateChanged` for Streaming on each streaming chunk**

In `execute_effect_once_configured`, the streaming loop (around line 420-427). Add a counter and send state updates:

```rust
let mut chunk_count: u32 = 0;
// ... in the while loop:
if result.status == EffectStatus::Streaming {
    chunk_count += 1;
    let _ = completion_tx.send(
        ReactorMessage::StateChanged {
            effect_id,
            pid,
            new_state: EffectState::Streaming {
                chunks_received: chunk_count,
            },
        },
    );
    let _ = completion_tx.send(
        ReactorMessage::Completion(
            EffectCompletion { pid, result },
        ),
    );
}
```

**Step 7: Update `try_recv` and `drain_completions`**

Change return types:

```rust
pub fn try_recv(&self) -> Option<ReactorMessage> {
    self.completion_rx.try_recv().ok()
}

pub fn drain_messages(&self) -> Vec<ReactorMessage> {
    let mut results = Vec::new();
    while let Some(msg) = self.try_recv() {
        results.push(msg);
    }
    results
}
```

Keep `drain_completions` as a deprecated alias or rename all callers to `drain_messages`.

**Step 8: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass (some reactor tests may need updating for new channel type)

**Step 9: Commit**

```bash
git add zeptovm/src/kernel/reactor.rs
git commit -m "feat(reactor): add ReactorMessage enum with state transition reporting"
```

---

### Task 3: Add `state` and `created_at_ms` to `PendingEffect` + scheduler methods

**Files:**
- Modify: `zeptovm/src/kernel/scheduler.rs:23-27` (PendingEffect struct)
- Modify: `zeptovm/src/kernel/scheduler.rs:304-317` (Suspend handler — set initial state)

**Step 1: Update `PendingEffect` struct**

```rust
struct PendingEffect {
    pid: Pid,
    request: EffectRequest,
    state: EffectState,
    created_at_ms: u64,
}
```

Add import at top:
```rust
use crate::core::effect::EffectState;
```

**Step 2: Update Suspend handler to set initial state**

In `step_process` (around line 310), where `PendingEffect` is constructed:

```rust
StepResult::Suspend(req) => {
    let effect_raw = req.effect_id.raw();
    if let Some(proc) = self.processes.get_mut(&pid) {
        proc.state =
            ProcessRuntimeState::WaitingEffect(
                effect_raw,
            );
    }
    self.pending_effects.insert(
        effect_raw,
        PendingEffect {
            pid,
            request: req.clone(),
            state: EffectState::Pending,
            created_at_ms: self.clock_ms,
        },
    );
    self.outbound_effects.push((pid, req));
    return;
}
```

**Step 3: Add `update_effect_state` method**

```rust
/// Update the lifecycle state of an in-flight effect.
/// No-op if the effect_id is not found (already completed).
pub fn update_effect_state(
    &mut self,
    effect_id: u64,
    new_state: EffectState,
) {
    if let Some(pending) =
        self.pending_effects.get_mut(&effect_id)
    {
        pending.state = new_state;
    }
}
```

**Step 4: Add `effect_state` query method**

```rust
/// Query the current lifecycle state of an in-flight
/// effect. Returns None if the effect is not pending.
pub fn effect_state(
    &self,
    effect_id: u64,
) -> Option<&EffectState> {
    self.pending_effects
        .get(&effect_id)
        .map(|p| &p.state)
}
```

**Step 5: Add tests**

```rust
#[test]
fn test_scheduler_effect_state_pending_on_suspend() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Suspender));
    engine.send(Envelope::text(pid, "go"));
    engine.tick();

    let effects = engine.take_outbound_effects();
    assert_eq!(effects.len(), 1);
    let effect_id = effects[0].1.effect_id.raw();

    let state = engine.effect_state(effect_id);
    assert!(
        matches!(state, Some(EffectState::Pending)),
        "effect should be Pending after Suspend"
    );
}

#[test]
fn test_scheduler_update_effect_state() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Suspender));
    engine.send(Envelope::text(pid, "go"));
    engine.tick();

    let effects = engine.take_outbound_effects();
    let effect_id = effects[0].1.effect_id.raw();

    engine.update_effect_state(
        effect_id,
        EffectState::Dispatched {
            dispatched_at_ms: 1000,
        },
    );
    assert!(matches!(
        engine.effect_state(effect_id),
        Some(EffectState::Dispatched { .. })
    ));

    engine.update_effect_state(
        effect_id,
        EffectState::Retrying {
            attempt: 1,
            next_at_ms: 2000,
        },
    );
    assert!(matches!(
        engine.effect_state(effect_id),
        Some(EffectState::Retrying { .. })
    ));
}

#[test]
fn test_scheduler_effect_state_unknown_id_noop() {
    let mut engine = SchedulerEngine::new();
    engine.update_effect_state(
        99999,
        EffectState::Dispatched {
            dispatched_at_ms: 0,
        },
    );
    assert!(engine.effect_state(99999).is_none());
}

#[test]
fn test_scheduler_effect_state_cleared_on_delivery() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Suspender));
    engine.send(Envelope::text(pid, "go"));
    engine.tick();

    let effects = engine.take_outbound_effects();
    let effect_id = effects[0].1.effect_id;

    // Deliver result — removes from pending_effects
    engine.deliver_effect_result(
        EffectResult::success(
            effect_id,
            serde_json::json!("done"),
        ),
    );

    assert!(
        engine.effect_state(effect_id.raw()).is_none(),
        "effect state should be gone after delivery"
    );
}
```

**Step 6: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 7: Commit**

```bash
git add zeptovm/src/kernel/scheduler.rs
git commit -m "feat(scheduler): track EffectState on PendingEffect with query/update methods"
```

---

### Task 4: Add `EffectStateChanged` journal entry type

**Files:**
- Modify: `zeptovm/src/durability/journal.rs:8-34` (JournalEntryType enum)
- Modify: `zeptovm/src/durability/journal.rs:37-64` (as_str)
- Modify: `zeptovm/src/durability/journal.rs:67-96` (from_str)

**Step 1: Add variant to enum**

After `EffectRetried` (line 29), add:
```rust
EffectStateChanged,
```

**Step 2: Add to `as_str`**

After the `EffectRetried` arm (line 59), add:
```rust
Self::EffectStateChanged => "effect_state_changed",
```

**Step 3: Add to `from_str`**

After the `"effect_retried"` arm (line 89), add:
```rust
"effect_state_changed" => Some(Self::EffectStateChanged),
```

**Step 4: Add test**

```rust
#[test]
fn test_journal_entry_type_effect_state_changed() {
    let t = JournalEntryType::EffectStateChanged;
    assert_eq!(t.as_str(), "effect_state_changed");
    assert_eq!(
        JournalEntryType::from_str(
            "effect_state_changed"
        ),
        Some(JournalEntryType::EffectStateChanged)
    );
}
```

**Step 5: Run tests**

Run: `cargo test -p zeptovm --lib -- journal`
Expected: All tests pass

**Step 6: Commit**

```bash
git add zeptovm/src/durability/journal.rs
git commit -m "feat(journal): add EffectStateChanged entry type"
```

---

### Task 5: Add `EffectStateChanged` RuntimeEvent variant

**Files:**
- Modify: `zeptovm/src/kernel/event_bus.rs:9-56` (RuntimeEvent enum)

**Step 1: Add variant**

After `EffectCompleted` (line 33), add:

```rust
EffectStateChanged {
    pid: Pid,
    effect_id: u64,
    new_state: String,
},
```

We use `String` for `new_state` (not `EffectState`) because `RuntimeEvent` is a display-oriented type — it would require `EffectState` import in event_bus and `Clone` on EffectState (which we already have).

Actually, let's use the real type since we have `Clone + Debug`:

```rust
EffectStateChanged {
    pid: Pid,
    effect_id: u64,
    new_state: crate::core::effect::EffectState,
},
```

**Step 2: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 3: Commit**

```bash
git add zeptovm/src/kernel/event_bus.rs
git commit -m "feat(event_bus): add EffectStateChanged RuntimeEvent variant"
```

---

### Task 6: Wire ReactorMessage into runtime tick()

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs` (tick method — reactor drain section)

**Step 1: Change `drain_completions()` to `drain_messages()`**

In `tick()`, replace the reactor drain loop. Currently (around line 220-305):

```rust
let completions = reactor.drain_completions();
for completion in completions { ... }
```

Replace with:

```rust
let messages = reactor.drain_messages();
for msg in messages {
    match msg {
        ReactorMessage::StateChanged {
            effect_id,
            pid,
            new_state,
        } => {
            self.engine.update_effect_state(
                effect_id.raw(),
                new_state.clone(),
            );
            self.event_bus.emit(
                RuntimeEvent::EffectStateChanged {
                    pid,
                    effect_id: effect_id.raw(),
                    new_state: new_state.clone(),
                },
            );
            // Journal the state transition
            if let Some(ref mut executor) =
                self.turn_executor
            {
                let payload = serde_json::to_vec(
                    &serde_json::json!({
                        "effect_id": effect_id.raw(),
                        "new_state": new_state,
                    }),
                )
                .ok();
                let _ = executor.journal_entry(
                    pid,
                    JournalEntryType::EffectStateChanged,
                    payload,
                );
            }
        }
        ReactorMessage::Completion(completion) => {
            // Set terminal state before removing
            self.engine.update_effect_state(
                completion.result.effect_id.raw(),
                EffectState::Completed(
                    completion.result.status.clone(),
                ),
            );

            // ... existing completion handling
            // (streaming check, compensation, budget,
            //  idempotency, metrics, deliver_effect_result)
        }
    }
}
```

The existing completion handling code moves inside the `ReactorMessage::Completion` arm, unchanged.

Add needed imports:
```rust
use crate::core::effect::EffectState;
use crate::kernel::reactor::ReactorMessage;
```

Note: `TurnExecutor` may not have a `journal_entry` method — check. If not, use the journal directly:

```rust
if let Some(ref mut executor) = self.turn_executor {
    use crate::durability::journal::JournalEntry;
    let entry = JournalEntry::new(
        pid,
        JournalEntryType::EffectStateChanged,
        payload,
    );
    let _ = executor.journal().append(&entry);
}
```

Check the `TurnExecutor` API before implementing. The executor wraps a `Journal` — find the right method to append a standalone entry.

**Step 2: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass (runtime tests use placeholder reactor — no real ReactorMessages)

**Step 3: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire ReactorMessage state transitions into tick()"
```

---

### Task 7: Enhance RecoveryCoordinator with `PendingEffectRecovery`

**Files:**
- Modify: `zeptovm/src/kernel/recovery.rs:18-23` (RecoveredProcess struct)
- Modify: `zeptovm/src/kernel/recovery.rs:73-105` (journal replay logic)

**Step 1: Add `PendingEffectRecovery` struct**

After `RecoveredProcess` (line 23), add:

```rust
/// Rich recovery info for a single pending effect.
pub struct PendingEffectRecovery {
    pub effect_id: u64,
    pub last_state: EffectState,
    pub request: EffectRequest,
}
```

Add import:
```rust
use crate::core::effect::{
    EffectRequest, EffectState, EffectStatus,
};
```

**Step 2: Change `RecoveredProcess` field**

Replace `pending_effect_ids: Vec<u64>` with:
```rust
pub pending_effects: Vec<PendingEffectRecovery>,
```

**Step 3: Update journal replay logic**

Replace the current pending effects tracking (lines 73-105) with:

```rust
let mut effect_requests: HashMap<u64, EffectRequest> =
    HashMap::new();
let mut effect_states: HashMap<u64, EffectState> =
    HashMap::new();
let mut completed_effects =
    std::collections::HashSet::new();

for entry in &entries {
    match &entry.entry_type {
        JournalEntryType::EffectRequested => {
            if let Some(ref payload) = entry.payload {
                if let Ok(req) =
                    serde_json::from_slice::<
                        EffectRequest,
                    >(payload)
                {
                    let id = req.effect_id.raw();
                    effect_states.insert(
                        id,
                        EffectState::Pending,
                    );
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
                    if let Some(id) =
                        val.get("effect_id")
                            .and_then(|v| v.as_u64())
                    {
                        if let Ok(state) =
                            serde_json::from_value::<
                                EffectState,
                            >(
                                val["new_state"].clone()
                            )
                        {
                            effect_states
                                .insert(id, state);
                        }
                    }
                }
            }
        }
        JournalEntryType::EffectResultRecorded => {
            if let Some(ref payload) = entry.payload {
                if let Ok(id) =
                    serde_json::from_slice::<u64>(
                        payload,
                    )
                {
                    completed_effects.insert(id);
                }
            }
        }
        _ => {}
    }
}

// Build PendingEffectRecovery for non-completed effects
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
```

Add `use std::collections::HashMap;` at top.

**Step 4: Update return**

```rust
Ok(RecoveredProcess {
    entry,
    pending_effects,
    timers,
})
```

**Step 5: Update existing tests**

The 4 existing tests reference `pending_effect_ids` — update to `pending_effects`:

- `test_recover_fresh_no_snapshot`: `assert!(recovered.pending_effects.is_empty());`
- `test_recover_with_pending_effects`: `assert_eq!(recovered.pending_effects.len(), 1);` + verify `last_state` is `Pending`
- `test_recover_completed_effects_filtered`: `assert!(recovered.pending_effects.is_empty());`

**Step 6: Add new tests**

```rust
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
                serde_json::to_vec(
                    &serde_json::json!({
                        "effect_id": effect_id,
                        "new_state":
                            EffectState::Dispatched {
                                dispatched_at_ms: 1000
                            },
                    }),
                )
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
                serde_json::to_vec(
                    &serde_json::json!({
                        "effect_id": effect_id,
                        "new_state":
                            EffectState::Retrying {
                                attempt: 1,
                                next_at_ms: 2000,
                            },
                    }),
                )
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
        EffectState::Retrying {
            attempt: 1,
            ..
        }
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
            Some(
                serde_json::to_vec(&effect_id).unwrap(),
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

    assert!(
        result.pending_effects.is_empty(),
        "completed effects should be filtered out"
    );
}
```

**Step 7: Update any callers of `pending_effect_ids`**

Search for `pending_effect_ids` in the codebase. If `runtime.rs` uses it in the recovery path, update accordingly.

**Step 8: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 9: Commit**

```bash
git add zeptovm/src/kernel/recovery.rs
git commit -m "feat(recovery): return PendingEffectRecovery with last_state from journal replay"
```

---

### Task 8: Update gap analysis

**Files:**
- Modify: `docs/plans/2026-03-06-spec-v03-gap-analysis.md`

**Step 1: Update A3 status**

In the "Things to Add Now" table, update A3:
- Status → `DONE`
- Details → `Explicit EffectState enum (Pending/Dispatched/Retrying/Streaming/Completed) + ReactorMessage state reporting + journal transitions + RecoveryCoordinator replays last_state`

**Step 2: Commit**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark A3 (effect state machine) as DONE in gap analysis"
```
