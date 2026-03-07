# ZeptoVM Phase 2 Design — Kernel Completion + Effect Hardening

**Goal:** Complete the kernel gaps from Phase 1 (timers, supervision, links/monitors, recovery) and harden the effect plane (retries, idempotency, compensation, observability).

**Structure:** Two sub-phases executed sequentially:
- **Phase 2a "Kernel Completion"** (~10 tasks): timers, supervision, links/monitors, recovery
- **Phase 2b "Effect Hardening"** (~8 tasks): retries, idempotency, compensation, observability

---

## Phase 2a — Kernel Completion

### 1. Durable Timers

**New types in `core/timer.rs`:**

```rust
pub struct TimerId(pub u64);

pub struct TimerSpec {
    pub id: TimerId,
    pub owner: Pid,
    pub kind: TimerKind,
    pub deadline_ms: u64,
    pub payload: Option<serde_json::Value>,
    pub durable: bool,
}

pub enum TimerKind {
    SleepUntil,
    Timeout,
    RetryBackoff,
}
```

**TimerWheel** in `kernel/timer_wheel.rs`:
- `BTreeMap<u64, Vec<TimerSpec>>` keyed by deadline
- `tick(now_ms) -> Vec<TimerSpec>` pops all entries where key <= now_ms
- `schedule(spec)` inserts into the map
- `cancel(timer_id)` removes by scanning (acceptable at expected scale)

**SQLite persistence** — `timers` table:
```sql
CREATE TABLE timers (
    timer_id INTEGER PRIMARY KEY,
    owner_pid INTEGER NOT NULL,
    kind TEXT NOT NULL,
    deadline_ms INTEGER NOT NULL,
    payload_json TEXT,
    created_at INTEGER NOT NULL
);
```

Only timers with `durable: true` are persisted. On recovery, durable timers are reloaded into the TimerWheel.

**TurnIntent integration:**
- `TurnIntent::ScheduleTimer(TimerSpec)` — process requests a timer
- `TurnIntent::CancelTimer(TimerId)` — process cancels a timer

**Effect timeouts:** When a process enters `Suspend(EffectRequest)`, if `timeout_ms` is set, the scheduler auto-registers a `TimerKind::Timeout` timer. If the timer fires before the effect completes, `EffectResult::timed_out()` is delivered and the effect is cancelled.

**Journal entries:** `TimerScheduled`, `TimerFired`, `TimerCancelled`.

---

### 2. Kernel Supervision (OneForOne)

**Supervisor as a process** — implements `StepBehavior`, participates in normal scheduler tick loop.

**SupervisorSpec:**

| Field | Type | Purpose |
|-------|------|---------|
| `max_restarts` | `u32` | Max restarts in window (default: 3) |
| `restart_window_ms` | `u64` | Window duration (default: 5000) |
| `backoff` | `BackoffPolicy` | Delay before restart |

**BackoffPolicy:**
- `Immediate`
- `Fixed(u64)` — ms between attempts
- `Exponential { base_ms, max_ms }` — doubles each attempt, capped

**ChildSpec:**

| Field | Type | Purpose |
|-------|------|---------|
| `id` | `String` | Unique child identifier |
| `behavior_factory` | `Box<dyn Fn() -> Box<dyn StepBehavior>>` | Creates fresh behavior |
| `restart` | `RestartStrategy` | `Permanent` / `Transient` / `Temporary` |
| `shutdown_timeout_ms` | `u64` | Grace period (default: 5000) |

**RestartStrategy:**
- `Permanent` — always restart
- `Transient` — restart only on abnormal exit
- `Temporary` — never restart

**Supervisor behavior flow:**
1. Child exits → `Signal::ChildExited` delivered to supervisor's control lane
2. Check `RestartStrategy` — should we restart?
3. Record restart timestamp in rolling window
4. If `restart_count_in_window > max_restarts` → supervisor exits with `Reason::Shutdown` (escalation)
5. Otherwise, schedule restart via `TurnIntent::ScheduleTimer` with backoff delay
6. Timer fires → spawn fresh process via `behavior_factory`, update child pid mapping

**ProcessTable changes:**
- `ProcessEntry` gains `parent: Option<Pid>`
- On exit, scheduler delivers `Signal::ChildExited` to parent if present

**New TurnIntent:**
```rust
TurnIntent::SpawnChild {
    child_spec: ChildSpec,
    supervisor_pid: Pid,
}
```

**Journal entries:** `ChildStarted`, `ChildRestarted`.

---

### 3. Links & Monitors

**Links (bidirectional):**
- Existing `LinkTable` (`link.rs`) wired into kernel
- `TurnIntent::Link(Pid)` and `TurnIntent::Unlink(Pid)`
- On process exit, deliver `Signal::ExitLinked { from_pid, reason }` to each linked process's control lane
- `trap_exit: bool` flag on `ProcessEntry` — if true, converts signal to normal mailbox message

**Monitors (unidirectional):**

```rust
pub struct MonitorRef(u64);

pub struct MonitorTable {
    monitors: HashMap<Pid, Vec<(MonitorRef, Pid)>>,
}
```

- `TurnIntent::Monitor(Pid)` → returns `MonitorRef`
- `TurnIntent::Demonitor(MonitorRef)`
- On monitored process exit, deliver `Signal::MonitorDown { ref, pid, reason }` to watcher's user lane

**Integration:**
- `LinkTable` and `MonitorTable` live on `SchedulerEngine`
- `TurnExecutor` processes Link/Unlink/Monitor/Demonitor intents during commit
- Exit propagation in scheduler's "reap terminated processes" phase

**Journal entries:** `Linked`, `Unlinked`, `MonitorCreated`, `MonitorRemoved`.

---

### 4. Supervisor-Driven Recovery

**RecoveryCoordinator** in `kernel/recovery.rs`:

```rust
pub struct RecoveryCoordinator<'a> {
    journal: &'a Journal,
    snapshots: &'a SnapshotStore,
    timer_wheel: &'a mut TimerWheel,
    link_table: &'a mut LinkTable,
    monitor_table: &'a mut MonitorTable,
}
```

**Recovery flow:**

1. **Snapshot restore** — Load latest snapshot, deserialize via `StepBehavior::restore(state_bytes)`. If none, call `behavior_factory()` + `init()`.
2. **Journal replay** — Replay entries after snapshot sequence:
   - `EffectDispatched`/`EffectCompleted` → rebuild pending effects
   - `TimerScheduled`/`TimerFired`/`TimerCancelled` → rebuild timer state
   - `ChildStarted`/`ChildRestarted` → rebuild supervisor child map
   - `Linked`/`MonitorCreated` → rebuild relations
3. **Rebuild runtime state** — Set `ProcessRuntimeState::Ready`, restore mailbox from unprocessed entries, restore pending effects
4. **Re-inject** — Insert into `ProcessTable`, push onto `RunQueue`

**StepBehavior addition:**
```rust
fn restore(&mut self, state: &[u8]) -> Result<(), String> {
    Ok(())  // default: start fresh
}
```

**Runtime boot recovery:**
1. Query all snapshots
2. For supervised processes, recover through their supervisor
3. Unsupervised processes with snapshots get best-effort recovery (log warning)

---

## Phase 2b — Effect Hardening

### 5. Retry Policy & Execution

Retries handled by the **Reactor**, transparent to the process.

**RetryPolicy** (already exists in `core/effect.rs`):
```rust
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: BackoffKind,
    pub retry_on: Vec<String>,
}

pub enum BackoffKind {
    Fixed(u64),
    Exponential { base_ms: u64, max_ms: u64 },
    ExponentialJitter { base_ms: u64, max_ms: u64 },
}
```

**Reactor retry loop:**
1. `attempt = 1`
2. Execute effect
3. Success → return `EffectResult::Completed`
4. Failure → if `attempt >= max_attempts` or error not in `retry_on` → return `Failed`
5. Compute delay from `BackoffKind`, sleep, increment attempt, goto 2

**Interactions:**
- Timeout timer starts on first dispatch, not per-retry. Timer fire cancels remaining attempts.
- Budget checked per attempt. Exhaustion mid-retry stops retries.
- `CancellationToken` (tokio_util) checked between attempts.

**Journal entry:** `EffectRetried { effect_id, attempt, error }`.

---

### 6. Idempotency Enforcement

**IdempotencyStore** in `durability/idempotency.rs`:

```rust
pub struct IdempotencyStore {
    conn: Connection,
}

impl IdempotencyStore {
    pub fn check(&self, effect_id: &EffectId) -> Option<EffectResult>;
    pub fn record(&self, effect_id: &EffectId, result: &EffectResult);
    pub fn expire_before(&self, timestamp_ms: u64);
}
```

**SQLite table:**
```sql
CREATE TABLE idempotency (
    effect_id TEXT PRIMARY KEY,
    result_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
```

**Flow:**
1. Reactor receives `EffectRequest`
2. Check `IdempotencyStore` — if found, return cached result (no execution)
3. Execute effect (with retries)
4. Record result in `IdempotencyStore`
5. Return `EffectResult`

**Guarantees:** At-most-once for recorded effects, at-least-once for the gap between external completion and recording. TTL cleanup (default 24h) via `expire_before()`.

**Recovery interaction:** During journal replay, `EffectDispatched` without matching `EffectCompleted` checks `IdempotencyStore` first — if cached, synthesize completion without re-dispatch.

---

### 7. Compensation / Saga

**CompensationSpec** on `EffectRequest`:

```rust
pub struct CompensationSpec {
    pub undo_effect: EffectKind,
    pub undo_payload: serde_json::Value,
}
```

**CompensationLog** in `kernel/compensation.rs`:

```rust
pub struct CompensationLog {
    pending: HashMap<Pid, Vec<CompensationEntry>>,
}

pub struct CompensationEntry {
    pub effect_id: EffectId,
    pub spec: CompensationSpec,
    pub completed_at: u64,
}

impl CompensationLog {
    pub fn record(&mut self, pid: Pid, entry: CompensationEntry);
    pub fn rollback_all(&mut self, pid: Pid) -> Vec<EffectRequest>;  // reverse order
    pub fn clear(&mut self, pid: Pid);
}
```

**Happy path:** Effects with `CompensationSpec` are recorded after completion. Workflow succeeds → `clear(pid)`.

**Rollback:** Process emits `TurnIntent::Rollback` → `CompensationLog::rollback_all(pid)` returns undo effects in reverse order → each dispatched through reactor with own retry policy.

**Failure during compensation:** Failed undo steps are logged but rollback continues. `CompensationFinished { all_succeeded: false }` signals manual intervention needed.

**Journal entries:** `CompensationRecorded`, `CompensationStarted`, `CompensationStepCompleted`, `CompensationFinished`.

---

### 8. Observability (Tracing + Metrics)

**Tracing** — `tracing` crate integration. ZeptoVM instruments, consumers configure subscriber.

Key spans:

| Span | Fields |
|------|--------|
| `scheduler_tick` | `tick_number` |
| `process_step` | `pid`, `turn_id`, `state` |
| `effect_dispatch` | `effect_id`, `kind`, `pid` |
| `effect_retry` | `effect_id`, `attempt`, `error` |
| `turn_commit` | `pid`, `turn_id`, `intents_count` |
| `timer_fire` | `timer_id`, `pid`, `kind` |
| `supervisor_restart` | `supervisor_pid`, `child_id`, `restart_count` |
| `recovery` | `pid`, `snapshot_seq`, `journal_entries` |
| `compensation_rollback` | `pid`, `steps` |

**Metrics** — in-process counters and gauges in `kernel/metrics.rs`:

```rust
pub struct Metrics {
    counters: HashMap<&'static str, AtomicU64>,
    gauges: HashMap<&'static str, AtomicI64>,
}
```

Tracked metrics: `processes.spawned`, `processes.exited`, `processes.active`, `effects.dispatched`, `effects.completed`, `effects.failed`, `effects.retries`, `effects.timed_out`, `turns.committed`, `timers.scheduled`, `timers.fired`, `supervisor.restarts`, `compensation.triggered`, `budget.blocked`, `scheduler.ticks`, `mailbox.depth_total`.

All updates atomic — no locks. `Metrics` owned by `SchedulerRuntime`, passed as `&Metrics` to subsystems.
