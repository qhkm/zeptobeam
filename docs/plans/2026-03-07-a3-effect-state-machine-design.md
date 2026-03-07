# A3: Effect State Machine for Recovery — Design

## Goal

Add an explicit `EffectState` enum that tracks every in-flight effect through its lifecycle (Pending → Dispatched → Retrying/Streaming → Completed). Serves both live observability and crash recovery.

## Architecture

State lives on `PendingEffect` in the scheduler. The Reactor reports transitions via a new `ReactorMessage` enum on its existing channel. The runtime journals transitions for recovery. `RecoveryCoordinator` replays the journal to reconstruct last-known state per effect.

## Tech Stack

Rust, ZeptoVM kernel (core/effect.rs, kernel/scheduler.rs, kernel/reactor.rs, kernel/runtime.rs, kernel/recovery.rs, kernel/event_bus.rs, durability/journal.rs)

---

## EffectState Enum

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum EffectState {
    Pending,
    Dispatched { dispatched_at_ms: u64 },
    Retrying { attempt: u32, next_at_ms: u64 },
    Streaming { chunks_received: u32 },
    Completed(EffectStatus),
}
```

Lives in `core/effect.rs` alongside `EffectRequest`/`EffectResult`. Terminal state is `Completed(Succeeded | Failed | TimedOut | Cancelled)`.

---

## ReactorMessage Enum

```rust
pub enum ReactorMessage {
    StateChanged {
        effect_id: EffectId,
        pid: Pid,
        new_state: EffectState,
    },
    Completion(EffectCompletion),
}
```

Replaces the current `EffectCompletion`-only channel between Reactor and runtime. The Reactor sends `StateChanged` at:
- Before first attempt → `Dispatched`
- On retry → `Retrying { attempt, next_at_ms }`
- On first streaming chunk → `Streaming { chunks_received: 1 }` (increments on subsequent chunks)
- Final result still sent as `Completion` (existing type)

---

## Scheduler Changes

`PendingEffect` gains state and timestamp:

```rust
struct PendingEffect {
    pid: Pid,
    request: EffectRequest,
    state: EffectState,
    created_at_ms: u64,
}
```

New methods on `SchedulerEngine`:

```rust
pub fn update_effect_state(&mut self, effect_id: u64, new_state: EffectState)
pub fn effect_state(&self, effect_id: u64) -> Option<&EffectState>
```

---

## Runtime Wiring

In `tick()`, when draining reactor messages:
- `ReactorMessage::StateChanged` → `engine.update_effect_state()` + emit `RuntimeEvent::EffectStateChanged` + journal `EffectStateChanged` entry
- `ReactorMessage::Completion` → existing logic (compensation, budget, idempotency, deliver result) + set state to `Completed(status)`

---

## Journal Entries for Recovery

New `JournalEntryType::EffectStateChanged` variant. Payload: serialized `{ effect_id, new_state }`.

The runtime journals every state transition alongside existing `EffectRequested` and `EffectResultRecorded`.

---

## RecoveryCoordinator Changes

Returns richer recovery info:

```rust
pub struct PendingEffectRecovery {
    pub effect_id: u64,
    pub last_state: EffectState,
    pub request: EffectRequest,
}
```

Recovery scans journal, replays state transitions, returns last known state per effect:
- `Pending` or `Dispatched` → re-dispatch
- `Retrying { attempt, .. }` → re-dispatch with attempt count preserved
- `Streaming` → re-dispatch from scratch (partial streams not resumable)
- `Completed` → skip

---

## Error Handling

- `update_effect_state` on unknown effect_id is a no-op (effect may have already completed)
- Reactor channel disconnect → existing panic handling in reactor thread
- Journal write failure → existing `warn!` + continue (non-fatal, degrades recovery)

---

## Testing

- EffectState transitions: unit tests on valid/invalid transitions
- Reactor state reporting: mock reactor sends StateChanged, verify scheduler updates
- Observability: `effect_state()` query returns correct state at each phase
- RuntimeEvent emission: verify `EffectStateChanged` events fire
- Journal round-trip: journal transitions, recover, verify `last_state` matches
- Recovery decisions: Pending/Dispatched → re-dispatch, Completed → skip
- Existing metrics still work (effects.completed/failed/timed_out)

---

## Out of Scope

- Scheduler-side timeout watchdog (Reactor handles timeouts)
- Process-side effect cancellation (separate feature)
- Separate SQLite effect state table (journal replay is sufficient)
