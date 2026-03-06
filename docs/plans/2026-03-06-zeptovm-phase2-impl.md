# ZeptoVM Phase 2 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete ZeptoVM kernel (timers, supervision, links/monitors, recovery) and harden the effect plane (retries, idempotency, compensation, observability).

**Architecture:** Two sub-phases. Phase 2a adds durable timers, OneForOne supervision, link/monitor propagation, and supervisor-driven recovery. Phase 2b adds reactor-level retries with backoff, idempotency enforcement via SQLite, saga-style compensation, and tracing+metrics observability. All new code lives in `zeptovm/src/core/`, `zeptovm/src/kernel/`, and `zeptovm/src/durability/`.

**Tech Stack:** Rust 2021, tokio, crossbeam, rusqlite, serde_json, tracing

---

## Phase 2a — Kernel Completion

### Task 1: Timer Types

**Files:**
- Create: `zeptovm/src/core/timer.rs`
- Modify: `zeptovm/src/core/mod.rs`

**Step 1: Write the failing test**

Add to `zeptovm/src/core/timer.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use serde::{Deserialize, Serialize};
use crate::pid::Pid;

static NEXT_TIMER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimerId(u64);

impl TimerId {
    pub fn new() -> Self {
        Self(NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub fn raw(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TimerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timer-{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimerKind {
    SleepUntil,
    Timeout,
    RetryBackoff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerSpec {
    pub id: TimerId,
    pub owner: Pid,
    pub kind: TimerKind,
    pub deadline_ms: u64,
    pub payload: Option<serde_json::Value>,
    pub durable: bool,
}

impl TimerSpec {
    pub fn new(owner: Pid, kind: TimerKind, deadline_ms: u64) -> Self {
        Self {
            id: TimerId::new(),
            owner,
            kind,
            deadline_ms,
            payload: None,
            durable: false,
        }
    }

    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = Some(payload);
        self
    }

    pub fn with_durable(mut self, durable: bool) -> Self {
        self.durable = durable;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;

    #[test]
    fn test_timer_id_unique() {
        let a = TimerId::new();
        let b = TimerId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_timer_id_display() {
        let id = TimerId::from_raw(42);
        assert_eq!(format!("{id}"), "timer-42");
    }

    #[test]
    fn test_timer_spec_builder() {
        let pid = Pid::from_raw(1);
        let spec = TimerSpec::new(pid, TimerKind::SleepUntil, 5000)
            .with_payload(serde_json::json!({"msg": "wake up"}))
            .with_durable(true);
        assert_eq!(spec.owner, pid);
        assert_eq!(spec.kind, TimerKind::SleepUntil);
        assert_eq!(spec.deadline_ms, 5000);
        assert!(spec.durable);
        assert!(spec.payload.is_some());
    }

    #[test]
    fn test_timer_spec_serializable() {
        let pid = Pid::from_raw(1);
        let spec = TimerSpec::new(pid, TimerKind::Timeout, 3000);
        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: TimerSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.kind, TimerKind::Timeout);
        assert_eq!(deserialized.deadline_ms, 3000);
    }

    #[test]
    fn test_timer_kind_variants() {
        assert_eq!(TimerKind::SleepUntil, TimerKind::SleepUntil);
        assert_ne!(TimerKind::SleepUntil, TimerKind::Timeout);
        assert_ne!(TimerKind::Timeout, TimerKind::RetryBackoff);
    }
}
```

**Step 2: Register the module**

Add `pub mod timer;` to `zeptovm/src/core/mod.rs`.

**Step 3: Run tests to verify they pass**

Run: `cargo test -p zeptovm core::timer --lib`
Expected: 5 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/core/timer.rs zeptovm/src/core/mod.rs
git commit -m "feat(core): add TimerId, TimerSpec, TimerKind types"
```

---

### Task 2: Timer Wheel

**Files:**
- Create: `zeptovm/src/kernel/timer_wheel.rs`
- Modify: `zeptovm/src/kernel/mod.rs`

**Step 1: Write the implementation with tests**

Create `zeptovm/src/kernel/timer_wheel.rs`:

```rust
use std::collections::BTreeMap;
use crate::core::timer::{TimerId, TimerSpec};

/// Timer wheel using BTreeMap keyed by deadline.
/// tick() pops all expired timers. schedule() inserts. cancel() removes.
pub struct TimerWheel {
    timers: BTreeMap<u64, Vec<TimerSpec>>,
    by_id: std::collections::HashMap<TimerId, u64>, // id -> deadline for cancel
}

impl TimerWheel {
    pub fn new() -> Self {
        Self {
            timers: BTreeMap::new(),
            by_id: std::collections::HashMap::new(),
        }
    }

    /// Schedule a timer.
    pub fn schedule(&mut self, spec: TimerSpec) {
        let deadline = spec.deadline_ms;
        let id = spec.id;
        self.timers.entry(deadline).or_default().push(spec);
        self.by_id.insert(id, deadline);
    }

    /// Cancel a timer by id. Returns true if found and removed.
    pub fn cancel(&mut self, timer_id: TimerId) -> bool {
        if let Some(deadline) = self.by_id.remove(&timer_id) {
            if let Some(bucket) = self.timers.get_mut(&deadline) {
                bucket.retain(|t| t.id != timer_id);
                if bucket.is_empty() {
                    self.timers.remove(&deadline);
                }
                return true;
            }
        }
        false
    }

    /// Pop all timers whose deadline <= now_ms.
    pub fn tick(&mut self, now_ms: u64) -> Vec<TimerSpec> {
        let mut fired = Vec::new();
        // split_off returns everything > now_ms; we keep everything <= now_ms
        let remaining = self.timers.split_off(&(now_ms + 1));
        let expired = std::mem::replace(&mut self.timers, remaining);
        for (_deadline, bucket) in expired {
            for spec in bucket {
                self.by_id.remove(&spec.id);
                fired.push(spec);
            }
        }
        fired
    }

    /// Number of pending timers.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::timer::{TimerKind, TimerSpec};
    use crate::pid::Pid;

    fn make_timer(pid: Pid, deadline: u64) -> TimerSpec {
        TimerSpec::new(pid, TimerKind::SleepUntil, deadline)
    }

    #[test]
    fn test_timer_wheel_empty() {
        let mut wheel = TimerWheel::new();
        assert!(wheel.is_empty());
        assert_eq!(wheel.tick(1000).len(), 0);
    }

    #[test]
    fn test_timer_wheel_schedule_and_tick() {
        let mut wheel = TimerWheel::new();
        let pid = Pid::from_raw(1);
        wheel.schedule(make_timer(pid, 100));
        wheel.schedule(make_timer(pid, 200));
        wheel.schedule(make_timer(pid, 300));
        assert_eq!(wheel.len(), 3);

        let fired = wheel.tick(150);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].deadline_ms, 100);
        assert_eq!(wheel.len(), 2);
    }

    #[test]
    fn test_timer_wheel_tick_exact_deadline() {
        let mut wheel = TimerWheel::new();
        let pid = Pid::from_raw(1);
        wheel.schedule(make_timer(pid, 100));
        let fired = wheel.tick(100);
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn test_timer_wheel_tick_all() {
        let mut wheel = TimerWheel::new();
        let pid = Pid::from_raw(1);
        wheel.schedule(make_timer(pid, 100));
        wheel.schedule(make_timer(pid, 200));
        let fired = wheel.tick(500);
        assert_eq!(fired.len(), 2);
        assert!(wheel.is_empty());
    }

    #[test]
    fn test_timer_wheel_cancel() {
        let mut wheel = TimerWheel::new();
        let pid = Pid::from_raw(1);
        let spec = make_timer(pid, 100);
        let id = spec.id;
        wheel.schedule(spec);
        assert_eq!(wheel.len(), 1);

        assert!(wheel.cancel(id));
        assert!(wheel.is_empty());
    }

    #[test]
    fn test_timer_wheel_cancel_nonexistent() {
        let mut wheel = TimerWheel::new();
        assert!(!wheel.cancel(crate::core::timer::TimerId::from_raw(999)));
    }

    #[test]
    fn test_timer_wheel_multiple_same_deadline() {
        let mut wheel = TimerWheel::new();
        let pid = Pid::from_raw(1);
        wheel.schedule(make_timer(pid, 100));
        wheel.schedule(make_timer(pid, 100));
        wheel.schedule(make_timer(pid, 100));
        assert_eq!(wheel.len(), 3);

        let fired = wheel.tick(100);
        assert_eq!(fired.len(), 3);
    }
}
```

**Step 2: Register the module**

Add `pub mod timer_wheel;` to `zeptovm/src/kernel/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm kernel::timer_wheel --lib`
Expected: 7 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/timer_wheel.rs zeptovm/src/kernel/mod.rs
git commit -m "feat(kernel): add TimerWheel with BTreeMap-based scheduling"
```

---

### Task 3: Timer SQLite Store

**Files:**
- Create: `zeptovm/src/durability/timer_store.rs`
- Modify: `zeptovm/src/durability/mod.rs`

**Step 1: Write the implementation with tests**

Create `zeptovm/src/durability/timer_store.rs`:

```rust
use rusqlite::{params, Connection, Result as SqlResult};
use crate::core::timer::{TimerId, TimerKind, TimerSpec};
use crate::pid::Pid;

/// SQLite-backed store for durable timers.
pub struct TimerStore {
    conn: Connection,
}

impl TimerStore {
    pub fn open_in_memory() -> SqlResult<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    pub fn open(path: &str) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> SqlResult<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS timers (
                timer_id INTEGER PRIMARY KEY,
                owner_pid INTEGER NOT NULL,
                kind TEXT NOT NULL,
                deadline_ms INTEGER NOT NULL,
                payload_json TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_timers_owner
                ON timers(owner_pid);",
        )?;
        Ok(())
    }

    /// Save a durable timer.
    pub fn save(&self, spec: &TimerSpec) -> SqlResult<()> {
        let kind_str = match &spec.kind {
            TimerKind::SleepUntil => "sleep_until",
            TimerKind::Timeout => "timeout",
            TimerKind::RetryBackoff => "retry_backoff",
        };
        let payload_json = spec.payload.as_ref().map(|v| v.to_string());
        self.conn.execute(
            "INSERT OR REPLACE INTO timers \
             (timer_id, owner_pid, kind, deadline_ms, payload_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                spec.id.raw() as i64,
                spec.owner.raw() as i64,
                kind_str,
                spec.deadline_ms as i64,
                payload_json,
            ],
        )?;
        Ok(())
    }

    /// Delete a timer.
    pub fn delete(&self, timer_id: TimerId) -> SqlResult<bool> {
        let rows = self.conn.execute(
            "DELETE FROM timers WHERE timer_id = ?1",
            params![timer_id.raw() as i64],
        )?;
        Ok(rows > 0)
    }

    /// Load all timers for a process.
    pub fn load_for_pid(&self, pid: Pid) -> SqlResult<Vec<TimerSpec>> {
        let mut stmt = self.conn.prepare(
            "SELECT timer_id, owner_pid, kind, deadline_ms, payload_json \
             FROM timers WHERE owner_pid = ?1",
        )?;
        let specs = stmt
            .query_map(params![pid.raw() as i64], |row| {
                Self::row_to_spec(row)
            })?
            .collect::<SqlResult<Vec<_>>>()?;
        Ok(specs)
    }

    /// Load all timers (for recovery).
    pub fn load_all(&self) -> SqlResult<Vec<TimerSpec>> {
        let mut stmt = self.conn.prepare(
            "SELECT timer_id, owner_pid, kind, deadline_ms, payload_json \
             FROM timers ORDER BY deadline_ms",
        )?;
        let specs = stmt
            .query_map([], |row| Self::row_to_spec(row))?
            .collect::<SqlResult<Vec<_>>>()?;
        Ok(specs)
    }

    fn row_to_spec(row: &rusqlite::Row<'_>) -> rusqlite::Result<TimerSpec> {
        let timer_id: i64 = row.get(0)?;
        let owner_pid: i64 = row.get(1)?;
        let kind_str: String = row.get(2)?;
        let deadline_ms: i64 = row.get(3)?;
        let payload_json: Option<String> = row.get(4)?;

        let kind = match kind_str.as_str() {
            "timeout" => TimerKind::Timeout,
            "retry_backoff" => TimerKind::RetryBackoff,
            _ => TimerKind::SleepUntil,
        };
        let payload = payload_json
            .and_then(|s| serde_json::from_str(&s).ok());

        Ok(TimerSpec {
            id: TimerId::from_raw(timer_id as u64),
            owner: Pid::from_raw(owner_pid as u64),
            kind,
            deadline_ms: deadline_ms as u64,
            payload,
            durable: true,
        })
    }

    /// Count of stored timers.
    pub fn count(&self) -> SqlResult<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM timers",
            [],
            |row| row.get(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::timer::{TimerKind, TimerSpec};
    use crate::pid::Pid;

    fn make_durable(pid: Pid, deadline: u64, kind: TimerKind) -> TimerSpec {
        TimerSpec::new(pid, kind, deadline).with_durable(true)
    }

    #[test]
    fn test_timer_store_save_and_load() {
        let store = TimerStore::open_in_memory().unwrap();
        let pid = Pid::from_raw(1);
        let spec = make_durable(pid, 5000, TimerKind::SleepUntil);
        store.save(&spec).unwrap();

        let loaded = store.load_for_pid(pid).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].deadline_ms, 5000);
        assert_eq!(loaded[0].kind, TimerKind::SleepUntil);
    }

    #[test]
    fn test_timer_store_delete() {
        let store = TimerStore::open_in_memory().unwrap();
        let pid = Pid::from_raw(1);
        let spec = make_durable(pid, 5000, TimerKind::Timeout);
        let id = spec.id;
        store.save(&spec).unwrap();

        assert!(store.delete(id).unwrap());
        assert_eq!(store.load_for_pid(pid).unwrap().len(), 0);
    }

    #[test]
    fn test_timer_store_load_all() {
        let store = TimerStore::open_in_memory().unwrap();
        let pid1 = Pid::from_raw(1);
        let pid2 = Pid::from_raw(2);
        store.save(&make_durable(pid1, 100, TimerKind::SleepUntil)).unwrap();
        store.save(&make_durable(pid2, 200, TimerKind::Timeout)).unwrap();
        store.save(&make_durable(pid1, 300, TimerKind::RetryBackoff)).unwrap();

        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 3);
        // Ordered by deadline
        assert_eq!(all[0].deadline_ms, 100);
        assert_eq!(all[2].deadline_ms, 300);
    }

    #[test]
    fn test_timer_store_with_payload() {
        let store = TimerStore::open_in_memory().unwrap();
        let pid = Pid::from_raw(1);
        let spec = make_durable(pid, 5000, TimerKind::SleepUntil)
            .with_payload(serde_json::json!({"msg": "wake"}));
        store.save(&spec).unwrap();

        let loaded = store.load_for_pid(pid).unwrap();
        assert_eq!(
            loaded[0].payload.as_ref().unwrap()["msg"],
            "wake"
        );
    }

    #[test]
    fn test_timer_store_count() {
        let store = TimerStore::open_in_memory().unwrap();
        assert_eq!(store.count().unwrap(), 0);
        let pid = Pid::from_raw(1);
        store.save(&make_durable(pid, 100, TimerKind::SleepUntil)).unwrap();
        store.save(&make_durable(pid, 200, TimerKind::Timeout)).unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn test_timer_store_isolates_pids() {
        let store = TimerStore::open_in_memory().unwrap();
        let pid1 = Pid::from_raw(1);
        let pid2 = Pid::from_raw(2);
        store.save(&make_durable(pid1, 100, TimerKind::SleepUntil)).unwrap();
        store.save(&make_durable(pid2, 200, TimerKind::Timeout)).unwrap();

        assert_eq!(store.load_for_pid(pid1).unwrap().len(), 1);
        assert_eq!(store.load_for_pid(pid2).unwrap().len(), 1);
    }
}
```

**Step 2: Register the module**

Add `pub mod timer_store;` to `zeptovm/src/durability/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm durability::timer_store --lib`
Expected: 6 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/durability/timer_store.rs zeptovm/src/durability/mod.rs
git commit -m "feat(durability): add TimerStore for durable timer persistence"
```

---

### Task 4: Wire Timers into TurnIntent + SchedulerEngine

**Files:**
- Modify: `zeptovm/src/core/turn_context.rs`
- Modify: `zeptovm/src/kernel/scheduler.rs`

**Step 1: Add timer TurnIntent variants**

In `zeptovm/src/core/turn_context.rs`, add to the `TurnIntent` enum:

```rust
use crate::core::timer::{TimerId, TimerSpec};

pub enum TurnIntent {
    SendMessage(Envelope),
    RequestEffect(EffectRequest),
    PatchState(Vec<u8>),
    ScheduleTimer(TimerSpec),
    CancelTimer(TimerId),
}
```

Add convenience methods to `TurnContext`:

```rust
pub fn schedule_timer(&mut self, spec: TimerSpec) {
    self.intents.push(TurnIntent::ScheduleTimer(spec));
}

pub fn cancel_timer(&mut self, id: TimerId) {
    self.intents.push(TurnIntent::CancelTimer(id));
}
```

**Step 2: Add TimerWheel to SchedulerEngine**

In `zeptovm/src/kernel/scheduler.rs`, add a `timer_wheel: TimerWheel` field to `SchedulerEngine`. In `tick()`, after stepping processes, call `self.timer_wheel.tick(now_ms)` and for each fired timer, deliver a signal to the owner. Also wire the new intent variants into `step_process`.

Add to `SchedulerEngine`:
- Field: `timer_wheel: TimerWheel`
- Field: `clock_ms: u64` (simple monotonic counter, advanced externally or via `std::time::Instant`)
- Method: `advance_clock(&mut self, now_ms: u64)` — sets clock and fires expired timers
- In `step_process` intent handling: `ScheduleTimer` → `self.timer_wheel.schedule(spec)`, `CancelTimer` → `self.timer_wheel.cancel(id)`

For fired timers: deliver `Envelope::signal(owner, Signal::TimerFired(timer_id))` to the owner's control lane. This requires adding `TimerFired(TimerId)` to the `Signal` enum in `core/message.rs`.

**Step 3: Add Signal::TimerFired**

In `zeptovm/src/core/message.rs`, add to `Signal`:

```rust
pub enum Signal {
    Kill,
    Shutdown,
    ExitLinked(Pid, crate::error::Reason),
    MonitorDown(Pid, crate::error::Reason),
    Suspend,
    Resume,
    TimerFired(crate::core::timer::TimerId),
}
```

In `process_table.rs`, add a handler for `TimerFired` in `handle_signal`:

```rust
Signal::TimerFired(_) => StepResult::Continue,
```

**Step 4: Add effect timeout auto-registration**

In `SchedulerEngine::step_process`, when handling `StepResult::Suspend(req)`, if `req.timeout` is non-zero, auto-register a `TimerSpec` with `TimerKind::Timeout` and deadline `self.clock_ms + req.timeout.as_millis() as u64`. Store the timer_id in the `PendingEffect` struct so it can be cancelled when the effect completes. In `deliver_effect_result`, cancel the timeout timer if one was registered.

**Step 5: Write tests**

Add to the scheduler tests:

```rust
#[test]
fn test_scheduler_timer_schedule_and_fire() {
    let mut engine = SchedulerEngine::new();
    // ... spawn process that schedules timer via TurnContext
    // ... advance clock past deadline
    // ... verify timer fired signal delivered
}

#[test]
fn test_scheduler_timer_cancel() {
    // ... schedule then cancel, advance clock, verify not fired
}

#[test]
fn test_scheduler_effect_timeout() {
    // ... process suspends with effect, advance clock past timeout
    // ... verify effect timed out
}
```

**Step 6: Run tests**

Run: `cargo test -p zeptovm kernel::scheduler --lib`
Expected: All existing + new tests PASS

**Step 7: Commit**

```bash
git add zeptovm/src/core/turn_context.rs zeptovm/src/core/message.rs \
  zeptovm/src/kernel/scheduler.rs zeptovm/src/kernel/process_table.rs
git commit -m "feat(kernel): wire TimerWheel into scheduler with effect timeouts"
```

---

### Task 5: Supervisor Types

**Files:**
- Create: `zeptovm/src/core/supervisor.rs`
- Modify: `zeptovm/src/core/mod.rs`

**Step 1: Write the implementation with tests**

Create `zeptovm/src/core/supervisor.rs`:

```rust
use crate::core::behavior::StepBehavior;

/// How to restart children after failure.
#[derive(Debug, Clone)]
pub enum BackoffPolicy {
    Immediate,
    Fixed(u64),                            // ms between attempts
    Exponential { base_ms: u64, max_ms: u64 }, // doubles, capped
}

/// Whether to restart a child after it exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartStrategy {
    Permanent,  // always restart
    Transient,  // restart only on abnormal exit
    Temporary,  // never restart
}

/// Specification for a supervisor.
#[derive(Debug, Clone)]
pub struct SupervisorSpec {
    pub max_restarts: u32,
    pub restart_window_ms: u64,
    pub backoff: BackoffPolicy,
}

impl Default for SupervisorSpec {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            restart_window_ms: 5000,
            backoff: BackoffPolicy::Immediate,
        }
    }
}

/// Specification for a supervised child.
pub struct ChildSpec {
    pub id: String,
    pub behavior_factory: Box<dyn Fn() -> Box<dyn StepBehavior> + Send>,
    pub restart: RestartStrategy,
    pub shutdown_timeout_ms: u64,
}

impl ChildSpec {
    pub fn new(
        id: impl Into<String>,
        factory: impl Fn() -> Box<dyn StepBehavior> + Send + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            behavior_factory: Box::new(factory),
            restart: RestartStrategy::Permanent,
            shutdown_timeout_ms: 5000,
        }
    }

    pub fn with_restart(mut self, strategy: RestartStrategy) -> Self {
        self.restart = strategy;
        self
    }

    pub fn with_shutdown_timeout(mut self, ms: u64) -> Self {
        self.shutdown_timeout_ms = ms;
        self
    }
}

impl BackoffPolicy {
    /// Compute delay for a given restart attempt (0-indexed).
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        match self {
            BackoffPolicy::Immediate => 0,
            BackoffPolicy::Fixed(ms) => *ms,
            BackoffPolicy::Exponential { base_ms, max_ms } => {
                let delay = base_ms.saturating_mul(1u64 << attempt.min(30));
                delay.min(*max_ms)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supervisor_spec_default() {
        let spec = SupervisorSpec::default();
        assert_eq!(spec.max_restarts, 3);
        assert_eq!(spec.restart_window_ms, 5000);
    }

    #[test]
    fn test_backoff_immediate() {
        let b = BackoffPolicy::Immediate;
        assert_eq!(b.delay_ms(0), 0);
        assert_eq!(b.delay_ms(5), 0);
    }

    #[test]
    fn test_backoff_fixed() {
        let b = BackoffPolicy::Fixed(1000);
        assert_eq!(b.delay_ms(0), 1000);
        assert_eq!(b.delay_ms(5), 1000);
    }

    #[test]
    fn test_backoff_exponential() {
        let b = BackoffPolicy::Exponential { base_ms: 100, max_ms: 5000 };
        assert_eq!(b.delay_ms(0), 100);  // 100 * 2^0
        assert_eq!(b.delay_ms(1), 200);  // 100 * 2^1
        assert_eq!(b.delay_ms(2), 400);  // 100 * 2^2
        assert_eq!(b.delay_ms(10), 5000); // capped
    }

    #[test]
    fn test_restart_strategy() {
        assert_eq!(RestartStrategy::Permanent, RestartStrategy::Permanent);
        assert_ne!(RestartStrategy::Permanent, RestartStrategy::Transient);
    }

    #[test]
    fn test_child_spec_builder() {
        use crate::core::behavior::StepBehavior;
        use crate::core::message::Envelope;
        use crate::core::step_result::StepResult;
        use crate::core::turn_context::TurnContext;
        use crate::error::Reason;

        struct Dummy;
        impl StepBehavior for Dummy {
            fn init(&mut self, _: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
            fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult { StepResult::Continue }
            fn terminate(&mut self, _: &Reason) {}
        }

        let spec = ChildSpec::new("worker-1", || Box::new(Dummy))
            .with_restart(RestartStrategy::Transient)
            .with_shutdown_timeout(3000);
        assert_eq!(spec.id, "worker-1");
        assert_eq!(spec.restart, RestartStrategy::Transient);
        assert_eq!(spec.shutdown_timeout_ms, 3000);
        // Verify factory works
        let _b = (spec.behavior_factory)();
    }
}
```

**Step 2: Register the module**

Add `pub mod supervisor;` to `zeptovm/src/core/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm core::supervisor --lib`
Expected: 6 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/core/supervisor.rs zeptovm/src/core/mod.rs
git commit -m "feat(core): add SupervisorSpec, ChildSpec, BackoffPolicy types"
```

---

### Task 6: ProcessEntry parent/trap_exit + ChildExited Signal

**Files:**
- Modify: `zeptovm/src/kernel/process_table.rs`
- Modify: `zeptovm/src/core/message.rs`

**Step 1: Add ChildExited signal variant**

In `zeptovm/src/core/message.rs`, add to `Signal`:

```rust
ChildExited { child_pid: Pid, reason: crate::error::Reason },
```

**Step 2: Add parent and trap_exit to ProcessEntry**

In `zeptovm/src/kernel/process_table.rs`, add fields:

```rust
pub struct ProcessEntry {
    pub pid: Pid,
    pub state: ProcessRuntimeState,
    pub mailbox: MultiLaneMailbox,
    pub parent: Option<Pid>,     // NEW
    pub trap_exit: bool,         // NEW
    behavior: Box<dyn StepBehavior>,
    reductions: u32,
    kill_requested: bool,
    exit_reason: Option<Reason>,
}
```

Update `new()` to initialize `parent: None, trap_exit: false`.

Add setter methods:

```rust
pub fn with_parent(mut self, parent: Pid) -> Self {
    self.parent = Some(parent);
    self
}

pub fn set_trap_exit(&mut self, trap: bool) {
    self.trap_exit = trap;
}
```

**Step 3: Handle ChildExited and trap_exit in handle_signal**

In `handle_signal`, add:

```rust
Signal::ChildExited { .. } => {
    // Supervisor handles this via behavior.handle()
    // Re-deliver as a normal message to the behavior
    StepResult::Continue
}
```

For `ExitLinked`, check `trap_exit`:

```rust
Signal::ExitLinked(_from, reason) => {
    if self.trap_exit {
        // Convert to normal message — behavior handles it
        StepResult::Continue
    } else if reason.is_abnormal() {
        self.state = ProcessRuntimeState::Done;
        StepResult::Done(reason.clone())
    } else {
        StepResult::Continue
    }
}
```

**Step 4: Handle TimerFired and ChildExited in process_table**

If `handle_signal` returns `Continue` for `ChildExited` or `TimerFired`, the message should be re-delivered to the behavior. Modify `step()` so that signals that return `Continue` still get passed to the behavior handler (instead of swallowing them). Specifically, for `ChildExited` and `TimerFired` signals (and `ExitLinked` when `trap_exit` is true), call `self.behavior.handle(env, &mut ctx)` instead of `self.handle_signal()`.

**Step 5: Write tests**

```rust
#[test]
fn test_process_with_parent() {
    let parent = Pid::from_raw(100);
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.parent = Some(parent);
    assert_eq!(p.parent, Some(parent));
}

#[test]
fn test_process_trap_exit_converts_signal() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.set_trap_exit(true);
    p.init(None);
    let other = Pid::new();
    p.mailbox.push(Envelope::signal(
        pid,
        Signal::ExitLinked(other, Reason::Custom("crash".into())),
    ));
    let (result, _) = p.step();
    // trap_exit: signal becomes normal message, behavior returns Continue
    assert!(matches!(result, StepResult::Continue));
    // Process should NOT be Done
    assert_ne!(p.state, ProcessRuntimeState::Done);
}
```

**Step 6: Run tests**

Run: `cargo test -p zeptovm kernel::process_table --lib`
Expected: All existing + new tests PASS

**Step 7: Commit**

```bash
git add zeptovm/src/kernel/process_table.rs zeptovm/src/core/message.rs
git commit -m "feat(kernel): add parent/trap_exit to ProcessEntry, ChildExited signal"
```

---

### Task 7: SupervisorBehavior

**Files:**
- Create: `zeptovm/src/kernel/supervisor_behavior.rs`
- Modify: `zeptovm/src/kernel/mod.rs`

**Step 1: Write the implementation**

Create `zeptovm/src/kernel/supervisor_behavior.rs`:

```rust
use std::collections::HashMap;

use crate::core::behavior::StepBehavior;
use crate::core::message::{Envelope, EnvelopePayload, Signal};
use crate::core::step_result::StepResult;
use crate::core::supervisor::{BackoffPolicy, ChildSpec, RestartStrategy, SupervisorSpec};
use crate::core::timer::{TimerKind, TimerSpec};
use crate::core::turn_context::TurnContext;
use crate::error::Reason;
use crate::pid::Pid;

struct ChildState {
    id: String,
    pid: Pid,
    restart: RestartStrategy,
    restart_timestamps: Vec<u64>,
    restart_count: u32,
}

/// OneForOne supervisor implemented as a StepBehavior.
pub struct SupervisorBehavior {
    spec: SupervisorSpec,
    children: HashMap<Pid, ChildState>,
    id_to_pid: HashMap<String, Pid>,
    clock_ms: u64,
}

impl SupervisorBehavior {
    pub fn new(spec: SupervisorSpec) -> Self {
        Self {
            spec,
            children: HashMap::new(),
            id_to_pid: HashMap::new(),
            clock_ms: 0,
        }
    }

    /// Register a child that was spawned externally.
    /// In real use, children are spawned via TurnIntent::SpawnChild,
    /// but for tracking, the supervisor needs to know about them.
    pub fn register_child(
        &mut self,
        child_id: String,
        child_pid: Pid,
        restart: RestartStrategy,
    ) {
        self.id_to_pid.insert(child_id.clone(), child_pid);
        self.children.insert(child_pid, ChildState {
            id: child_id,
            pid: child_pid,
            restart,
            restart_timestamps: Vec::new(),
            restart_count: 0,
        });
    }

    /// Check if restart intensity has been exceeded.
    fn intensity_exceeded(&self, child: &ChildState) -> bool {
        let cutoff = self.clock_ms.saturating_sub(self.spec.restart_window_ms);
        let recent = child.restart_timestamps
            .iter()
            .filter(|&&ts| ts >= cutoff)
            .count();
        recent as u32 >= self.spec.max_restarts
    }

    fn should_restart(&self, child: &ChildState, reason: &Reason) -> bool {
        match child.restart {
            RestartStrategy::Permanent => true,
            RestartStrategy::Transient => reason.is_abnormal(),
            RestartStrategy::Temporary => false,
        }
    }

    /// Advance the supervisor's view of time.
    pub fn set_clock(&mut self, ms: u64) {
        self.clock_ms = ms;
    }

    /// Get the child count.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }
}

impl StepBehavior for SupervisorBehavior {
    fn init(&mut self, _checkpoint: Option<Vec<u8>>) -> StepResult {
        StepResult::Continue
    }

    fn handle(&mut self, msg: Envelope, ctx: &mut TurnContext) -> StepResult {
        match &msg.payload {
            EnvelopePayload::Signal(Signal::ChildExited { child_pid, reason }) => {
                let child_pid = *child_pid;
                let reason = reason.clone();

                // Look up child
                let child = match self.children.get_mut(&child_pid) {
                    Some(c) => c,
                    None => return StepResult::Continue, // Unknown child
                };

                if !self.should_restart(child, &reason) {
                    // Remove child, don't restart
                    let id = child.id.clone();
                    self.children.remove(&child_pid);
                    self.id_to_pid.remove(&id);
                    return StepResult::Continue;
                }

                // Check restart intensity
                child.restart_timestamps.push(self.clock_ms);
                child.restart_count += 1;

                if self.intensity_exceeded(child) {
                    // Too many restarts — supervisor itself shuts down
                    return StepResult::Fail(Reason::Shutdown);
                }

                // Schedule restart with backoff
                let delay = self.spec.backoff.delay_ms(child.restart_count - 1);
                let deadline = self.clock_ms + delay;
                let timer = TimerSpec::new(ctx.pid, TimerKind::RetryBackoff, deadline)
                    .with_payload(serde_json::json!({
                        "action": "restart_child",
                        "child_id": child.id,
                        "child_pid": child_pid.raw(),
                    }));
                ctx.schedule_timer(timer);

                StepResult::Continue
            }
            _ => StepResult::Continue,
        }
    }

    fn terminate(&mut self, _reason: &Reason) {}

    fn snapshot(&self) -> Option<Vec<u8>> {
        // Serialize child map for recovery
        let data: Vec<(String, u64)> = self.children.values()
            .map(|c| (c.id.clone(), c.pid.raw()))
            .collect();
        serde_json::to_vec(&data).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::supervisor::{BackoffPolicy, SupervisorSpec};

    fn make_supervisor(max_restarts: u32, window_ms: u64) -> SupervisorBehavior {
        SupervisorBehavior::new(SupervisorSpec {
            max_restarts,
            restart_window_ms: window_ms,
            backoff: BackoffPolicy::Immediate,
        })
    }

    #[test]
    fn test_supervisor_register_child() {
        let mut sup = make_supervisor(3, 5000);
        let child_pid = Pid::from_raw(10);
        sup.register_child("w1".into(), child_pid, RestartStrategy::Permanent);
        assert_eq!(sup.child_count(), 1);
    }

    #[test]
    fn test_supervisor_child_exit_triggers_restart() {
        let mut sup = make_supervisor(3, 5000);
        let sup_pid = Pid::from_raw(1);
        let child_pid = Pid::from_raw(10);
        sup.register_child("w1".into(), child_pid, RestartStrategy::Permanent);
        sup.init(None);

        let mut ctx = TurnContext::new(sup_pid);
        let msg = Envelope::signal(
            sup_pid,
            Signal::ChildExited {
                child_pid,
                reason: Reason::Custom("crash".into()),
            },
        );
        let result = sup.handle(msg, &mut ctx);
        assert!(matches!(result, StepResult::Continue));
        // Should have scheduled a restart timer
        assert_eq!(ctx.intent_count(), 1);
    }

    #[test]
    fn test_supervisor_temporary_not_restarted() {
        let mut sup = make_supervisor(3, 5000);
        let sup_pid = Pid::from_raw(1);
        let child_pid = Pid::from_raw(10);
        sup.register_child("w1".into(), child_pid, RestartStrategy::Temporary);
        sup.init(None);

        let mut ctx = TurnContext::new(sup_pid);
        let msg = Envelope::signal(
            sup_pid,
            Signal::ChildExited {
                child_pid,
                reason: Reason::Custom("crash".into()),
            },
        );
        sup.handle(msg, &mut ctx);
        assert_eq!(ctx.intent_count(), 0); // No restart scheduled
        assert_eq!(sup.child_count(), 0);  // Child removed
    }

    #[test]
    fn test_supervisor_transient_normal_exit_no_restart() {
        let mut sup = make_supervisor(3, 5000);
        let sup_pid = Pid::from_raw(1);
        let child_pid = Pid::from_raw(10);
        sup.register_child("w1".into(), child_pid, RestartStrategy::Transient);
        sup.init(None);

        let mut ctx = TurnContext::new(sup_pid);
        let msg = Envelope::signal(
            sup_pid,
            Signal::ChildExited {
                child_pid,
                reason: Reason::Normal,
            },
        );
        sup.handle(msg, &mut ctx);
        assert_eq!(ctx.intent_count(), 0);
    }

    #[test]
    fn test_supervisor_max_restarts_exceeded() {
        let mut sup = make_supervisor(2, 5000);
        let sup_pid = Pid::from_raw(1);
        let child_pid = Pid::from_raw(10);
        sup.register_child("w1".into(), child_pid, RestartStrategy::Permanent);
        sup.init(None);

        // First restart
        let mut ctx = TurnContext::new(sup_pid);
        let msg = Envelope::signal(
            sup_pid,
            Signal::ChildExited { child_pid, reason: Reason::Custom("crash".into()) },
        );
        let r = sup.handle(msg, &mut ctx);
        assert!(matches!(r, StepResult::Continue));

        // Second restart — hits max_restarts=2
        let mut ctx = TurnContext::new(sup_pid);
        let msg = Envelope::signal(
            sup_pid,
            Signal::ChildExited { child_pid, reason: Reason::Custom("crash".into()) },
        );
        let r = sup.handle(msg, &mut ctx);
        // Supervisor shuts down
        assert!(matches!(r, StepResult::Fail(Reason::Shutdown)));
    }

    #[test]
    fn test_supervisor_backoff_delay() {
        let mut sup = SupervisorBehavior::new(SupervisorSpec {
            max_restarts: 5,
            restart_window_ms: 10000,
            backoff: BackoffPolicy::Fixed(1000),
        });
        let sup_pid = Pid::from_raw(1);
        let child_pid = Pid::from_raw(10);
        sup.register_child("w1".into(), child_pid, RestartStrategy::Permanent);
        sup.set_clock(5000);
        sup.init(None);

        let mut ctx = TurnContext::new(sup_pid);
        let msg = Envelope::signal(
            sup_pid,
            Signal::ChildExited { child_pid, reason: Reason::Custom("crash".into()) },
        );
        sup.handle(msg, &mut ctx);
        let intents = ctx.take_intents();
        // Should have a timer with deadline = 5000 + 1000 = 6000
        match &intents[0] {
            crate::core::turn_context::TurnIntent::ScheduleTimer(spec) => {
                assert_eq!(spec.deadline_ms, 6000);
            }
            _ => panic!("expected ScheduleTimer"),
        }
    }

    #[test]
    fn test_supervisor_snapshot() {
        let mut sup = make_supervisor(3, 5000);
        let child_pid = Pid::from_raw(10);
        sup.register_child("w1".into(), child_pid, RestartStrategy::Permanent);
        let snap = sup.snapshot();
        assert!(snap.is_some());
    }
}
```

**Step 2: Register the module**

Add `pub mod supervisor_behavior;` to `zeptovm/src/kernel/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm kernel::supervisor_behavior --lib`
Expected: 7 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/supervisor_behavior.rs zeptovm/src/kernel/mod.rs
git commit -m "feat(kernel): add SupervisorBehavior with OneForOne strategy"
```

---

### Task 8: Wire LinkTable into SchedulerEngine

**Files:**
- Modify: `zeptovm/src/core/turn_context.rs`
- Modify: `zeptovm/src/kernel/scheduler.rs`

**Step 1: Add Link/Monitor TurnIntent variants**

In `zeptovm/src/core/turn_context.rs`, add to `TurnIntent`:

```rust
Link(Pid),
Unlink(Pid),
Monitor(Pid),
Demonitor(crate::link::MonitorRef),
```

Add methods to `TurnContext`:

```rust
pub fn link(&mut self, target: Pid) {
    self.intents.push(TurnIntent::Link(target));
}

pub fn unlink(&mut self, target: Pid) {
    self.intents.push(TurnIntent::Unlink(target));
}

pub fn monitor(&mut self, target: Pid) {
    self.intents.push(TurnIntent::Monitor(target));
}

pub fn demonitor(&mut self, mref: crate::link::MonitorRef) {
    self.intents.push(TurnIntent::Demonitor(mref));
}
```

**Step 2: Add LinkTable to SchedulerEngine**

In `zeptovm/src/kernel/scheduler.rs`:

- Add field: `link_table: crate::link::LinkTable`
- Initialize in `new()`: `link_table: crate::link::LinkTable::new()`
- In intent handling: wire `Link`, `Unlink`, `Monitor`, `Demonitor` to the link table
- In `step_process` completion handling (when a process exits): propagate `ExitLinked` to linked processes, `MonitorDown` to monitor watchers, deliver `ChildExited` to parent
- In `reap_completed`: also clean up links and monitors for reaped pids

**Step 3: Exit propagation logic**

When a process exits (Done or Fail), before removing it:

```rust
// Propagate to linked processes
let linked = self.link_table.remove_all(&pid);
for linked_pid in linked {
    if let Some(proc) = self.processes.get_mut(&linked_pid) {
        proc.mailbox.push(Envelope::signal(
            linked_pid,
            Signal::ExitLinked(pid, reason.clone()),
        ));
        if proc.state == ProcessRuntimeState::WaitingMessage {
            proc.state = ProcessRuntimeState::Ready;
            self.ready_queue.push(linked_pid);
        }
    }
}

// Propagate to monitors
let monitors = self.link_table.remove_monitors_of(&pid);
for (mref, watcher_pid) in monitors {
    if let Some(proc) = self.processes.get_mut(&watcher_pid) {
        proc.mailbox.push(Envelope::signal(
            watcher_pid,
            Signal::MonitorDown(pid, reason.clone()),
        ));
        if proc.state == ProcessRuntimeState::WaitingMessage {
            proc.state = ProcessRuntimeState::Ready;
            self.ready_queue.push(watcher_pid);
        }
    }
}

// Notify parent supervisor
if let Some(parent_pid) = {
    self.processes.get(&pid).and_then(|p| p.parent)
} {
    if let Some(parent) = self.processes.get_mut(&parent_pid) {
        parent.mailbox.push(Envelope::signal(
            parent_pid,
            Signal::ChildExited { child_pid: pid, reason: reason.clone() },
        ));
        if parent.state == ProcessRuntimeState::WaitingMessage {
            parent.state = ProcessRuntimeState::Ready;
            self.ready_queue.push(parent_pid);
        }
    }
}
```

**Step 4: Write tests**

```rust
#[test]
fn test_scheduler_link_exit_propagation() {
    let mut engine = SchedulerEngine::new();
    let a = engine.spawn(Box::new(Stopper)); // exits on first msg
    let b = engine.spawn(Box::new(Echo));
    // Link a and b
    engine.link(a, b);  // add a public link() method
    engine.send(Envelope::text(a, "stop"));
    engine.tick();
    // b should receive ExitLinked signal
    // ... verify b gets the signal
}

#[test]
fn test_scheduler_monitor_down_notification() {
    // ... similar test for monitors
}

#[test]
fn test_scheduler_parent_child_exit_notification() {
    // ... spawn with parent, child exits, parent receives ChildExited
}
```

**Step 5: Run tests**

Run: `cargo test -p zeptovm kernel::scheduler --lib`
Expected: All PASS

**Step 6: Commit**

```bash
git add zeptovm/src/core/turn_context.rs zeptovm/src/kernel/scheduler.rs
git commit -m "feat(kernel): wire LinkTable into scheduler with exit propagation"
```

---

### Task 9: Add new JournalEntryTypes for Phase 2

**Files:**
- Modify: `zeptovm/src/durability/journal.rs`

**Step 1: Add new entry types**

Add to `JournalEntryType`:

```rust
TimerCancelled,
ChildStarted,
ChildRestarted,
Linked,
Unlinked,
MonitorCreated,
MonitorRemoved,
EffectRetried,
CompensationRecorded,
CompensationStarted,
CompensationStepCompleted,
CompensationFinished,
```

Update `as_str()` and `from_str()` with the new variants.

**Step 2: Run tests**

Run: `cargo test -p zeptovm durability::journal --lib`
Expected: All existing tests PASS

**Step 3: Commit**

```bash
git add zeptovm/src/durability/journal.rs
git commit -m "feat(durability): add Phase 2 JournalEntryType variants"
```

---

### Task 10: Recovery Coordinator

**Files:**
- Create: `zeptovm/src/kernel/recovery.rs`
- Modify: `zeptovm/src/kernel/mod.rs`
- Modify: `zeptovm/src/core/behavior.rs`

**Note:** The existing `zeptovm/src/durability/recovery.rs` is a pre-existing file from the old async system. This new `kernel/recovery.rs` is for the Phase 1 kernel's recovery coordinator.

**Step 1: Add restore() to StepBehavior**

In `zeptovm/src/core/behavior.rs`, add to the trait:

```rust
fn restore(&mut self, _state: &[u8]) -> Result<(), String> {
    Ok(())
}
```

**Step 2: Write the RecoveryCoordinator**

Create `zeptovm/src/kernel/recovery.rs`:

```rust
use crate::core::behavior::StepBehavior;
use crate::core::timer::TimerSpec;
use crate::durability::journal::{Journal, JournalEntry, JournalEntryType};
use crate::durability::snapshot::SnapshotStore;
use crate::durability::timer_store::TimerStore;
use crate::kernel::process_table::{ProcessEntry, ProcessRuntimeState};
use crate::kernel::timer_wheel::TimerWheel;
use crate::link::LinkTable;
use crate::pid::Pid;

/// Coordinates process recovery from durable storage.
pub struct RecoveryCoordinator<'a> {
    journal: &'a Journal,
    snapshots: &'a SnapshotStore,
    timer_store: Option<&'a TimerStore>,
}

/// Result of recovering a single process.
pub struct RecoveredProcess {
    pub entry: ProcessEntry,
    pub pending_effect_ids: Vec<u64>,
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

    pub fn with_timer_store(mut self, store: &'a TimerStore) -> Self {
        self.timer_store = Some(store);
        self
    }

    /// Recover a single process from snapshot + journal replay.
    ///
    /// `factory` creates a fresh behavior instance.
    /// The behavior's `restore()` is called with snapshot state if available.
    pub fn recover_process(
        &self,
        pid: Pid,
        factory: &dyn Fn() -> Box<dyn StepBehavior>,
    ) -> Result<RecoveredProcess, String> {
        let mut behavior = factory();
        let mut snapshot_seq = 0i64;

        // Step 1: Load latest snapshot
        let snapshot = self.snapshots.load_latest(pid)
            .map_err(|e| format!("snapshot load failed: {e}"))?;

        if let Some(ref snap) = snapshot {
            behavior.restore(&snap.state_blob)?;
            snapshot_seq = snap.version;
        } else {
            behavior.init(None);
        }

        // Step 2: Replay journal entries after snapshot
        let entries = self.journal.replay(pid, snapshot_seq)
            .map_err(|e| format!("journal replay failed: {e}"))?;

        let mut pending_effects = Vec::new();
        let mut completed_effects = std::collections::HashSet::new();

        for entry in &entries {
            match &entry.entry_type {
                JournalEntryType::EffectRequested => {
                    if let Some(ref payload) = entry.payload {
                        // Extract effect_id from payload
                        if let Ok(req) = serde_json::from_slice::<
                            crate::core::effect::EffectRequest,
                        >(payload) {
                            pending_effects.push(req.effect_id.raw());
                        }
                    }
                }
                JournalEntryType::EffectResultRecorded => {
                    if let Some(ref payload) = entry.payload {
                        if let Ok(id) = serde_json::from_slice::<u64>(payload) {
                            completed_effects.insert(id);
                        }
                    }
                }
                _ => {} // Other entries inform state but don't need action
            }
        }

        // Remove completed effects from pending
        pending_effects.retain(|id| !completed_effects.contains(id));

        // Step 3: Load durable timers
        let timers = if let Some(store) = self.timer_store {
            store.load_for_pid(pid)
                .map_err(|e| format!("timer load failed: {e}"))?
        } else {
            Vec::new()
        };

        // Step 4: Build ProcessEntry
        let mut entry = ProcessEntry::new(pid, behavior);
        entry.state = ProcessRuntimeState::Ready;

        Ok(RecoveredProcess {
            entry,
            pending_effect_ids: pending_effects,
            timers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::behavior::StepBehavior;
    use crate::core::effect::{EffectKind, EffectRequest};
    use crate::core::message::Envelope;
    use crate::core::step_result::StepResult;
    use crate::core::turn_context::TurnContext;
    use crate::durability::journal::{Journal, JournalEntry, JournalEntryType};
    use crate::durability::snapshot::{Snapshot, SnapshotStore};
    use crate::error::Reason;

    struct Restorable {
        state: String,
    }
    impl StepBehavior for Restorable {
        fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
            self.state = "initialized".into();
            StepResult::Continue
        }
        fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _: &Reason) {}
        fn snapshot(&self) -> Option<Vec<u8>> {
            Some(self.state.as_bytes().to_vec())
        }
        fn restore(&mut self, state: &[u8]) -> Result<(), String> {
            self.state = String::from_utf8(state.to_vec())
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    #[test]
    fn test_recover_fresh_no_snapshot() {
        let journal = Journal::open_in_memory().unwrap();
        let snapshots = SnapshotStore::open_in_memory().unwrap();
        let coord = RecoveryCoordinator::new(&journal, &snapshots);

        let pid = Pid::from_raw(1);
        let result = coord.recover_process(pid, &|| {
            Box::new(Restorable { state: String::new() })
        });
        assert!(result.is_ok());
        let recovered = result.unwrap();
        assert_eq!(recovered.entry.state, ProcessRuntimeState::Ready);
        assert!(recovered.pending_effect_ids.is_empty());
    }

    #[test]
    fn test_recover_from_snapshot() {
        let journal = Journal::open_in_memory().unwrap();
        let snapshots = SnapshotStore::open_in_memory().unwrap();
        let pid = Pid::from_raw(1);

        // Save a snapshot
        snapshots.save(&Snapshot {
            pid,
            version: 5,
            state_blob: b"recovered-state".to_vec(),
            mailbox_cursor: 0,
            pending_effects: None,
        }).unwrap();

        let coord = RecoveryCoordinator::new(&journal, &snapshots);
        let result = coord.recover_process(pid, &|| {
            Box::new(Restorable { state: String::new() })
        });
        assert!(result.is_ok());
    }

    #[test]
    fn test_recover_with_pending_effects() {
        let journal = Journal::open_in_memory().unwrap();
        let snapshots = SnapshotStore::open_in_memory().unwrap();
        let pid = Pid::from_raw(1);

        // Add effect requested entry
        let req = EffectRequest::new(
            EffectKind::LlmCall,
            serde_json::json!({"prompt": "test"}),
        );
        let payload = serde_json::to_vec(&req).unwrap();
        journal.append(&JournalEntry::new(
            pid,
            JournalEntryType::EffectRequested,
            Some(payload),
        )).unwrap();

        let coord = RecoveryCoordinator::new(&journal, &snapshots);
        let result = coord.recover_process(pid, &|| {
            Box::new(Restorable { state: String::new() })
        }).unwrap();
        assert_eq!(result.pending_effect_ids.len(), 1);
    }

    #[test]
    fn test_recover_completed_effects_filtered() {
        let journal = Journal::open_in_memory().unwrap();
        let snapshots = SnapshotStore::open_in_memory().unwrap();
        let pid = Pid::from_raw(1);

        let req = EffectRequest::new(
            EffectKind::LlmCall,
            serde_json::json!({}),
        );
        let effect_id_raw = req.effect_id.raw();
        journal.append(&JournalEntry::new(
            pid,
            JournalEntryType::EffectRequested,
            Some(serde_json::to_vec(&req).unwrap()),
        )).unwrap();
        journal.append(&JournalEntry::new(
            pid,
            JournalEntryType::EffectResultRecorded,
            Some(serde_json::to_vec(&effect_id_raw).unwrap()),
        )).unwrap();

        let coord = RecoveryCoordinator::new(&journal, &snapshots);
        let result = coord.recover_process(pid, &|| {
            Box::new(Restorable { state: String::new() })
        }).unwrap();
        // Effect was completed — should not be pending
        assert!(result.pending_effect_ids.is_empty());
    }
}
```

**Step 3: Register the module**

Note: `zeptovm/src/kernel/mod.rs` may already have a `recovery` module (check). If not, add `pub mod recovery;`. If the name conflicts with `durability/recovery.rs`, use a different name or path. Since the kernel `mod.rs` doesn't currently have `recovery`, add it.

Add `pub mod recovery;` to `zeptovm/src/kernel/mod.rs`.

**Step 4: Run tests**

Run: `cargo test -p zeptovm kernel::recovery --lib`
Expected: 4 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/recovery.rs zeptovm/src/kernel/mod.rs \
  zeptovm/src/core/behavior.rs
git commit -m "feat(kernel): add RecoveryCoordinator for snapshot+journal recovery"
```

---

## Phase 2b — Effect Hardening

### Task 11: Enhance RetryPolicy with BackoffKind

**Files:**
- Modify: `zeptovm/src/core/effect.rs`

**Step 1: Replace RetryPolicy fields**

Replace the current `RetryPolicy` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackoffKind {
    Fixed(u64),                                    // ms between attempts
    Exponential { base_ms: u64, max_ms: u64 },    // doubles, capped
    ExponentialJitter { base_ms: u64, max_ms: u64 }, // + random jitter
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: BackoffKind,
    pub retry_on: Vec<String>,  // error categories; empty = retry all
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: BackoffKind::Exponential { base_ms: 500, max_ms: 30_000 },
            retry_on: Vec::new(),
        }
    }
}

impl BackoffKind {
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        match self {
            BackoffKind::Fixed(ms) => *ms,
            BackoffKind::Exponential { base_ms, max_ms } => {
                let delay = base_ms.saturating_mul(1u64 << attempt.min(30));
                delay.min(*max_ms)
            }
            BackoffKind::ExponentialJitter { base_ms, max_ms } => {
                let base_delay = base_ms.saturating_mul(1u64 << attempt.min(30));
                let capped = base_delay.min(*max_ms);
                // Jitter: add 0..base_ms (deterministic in tests, random in prod)
                // For simplicity, use attempt as pseudo-jitter seed
                let jitter = (attempt as u64 * 37) % base_ms;
                capped.saturating_add(jitter).min(*max_ms)
            }
        }
    }
}
```

Also remove the old `backoff_base` and `backoff_max` Duration fields. Update `EffectRequest::new()` to use the new `RetryPolicy::default()`.

Remove `use std::time::Duration;` from the retry-related code if no longer needed (keep it for `EffectRequest.timeout`).

**Step 2: Fix compilation errors**

The `retry` field on `EffectRequest` now uses the new struct. Search for any code referencing `retry.backoff_base` or `retry.backoff_max` and update. The reactor's `execute_effect` doesn't currently use retry fields, so this should be minimal.

**Step 3: Add compensation field**

Also add to `EffectRequest`:

```rust
pub compensation: Option<CompensationSpec>,
```

Where `CompensationSpec` is defined in `core/effect.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompensationSpec {
    pub undo_kind: EffectKind,
    pub undo_input: serde_json::Value,
}
```

Initialize as `compensation: None` in `EffectRequest::new()`. Add builder:

```rust
pub fn with_compensation(mut self, spec: CompensationSpec) -> Self {
    self.compensation = Some(spec);
    self
}
```

**Step 4: Add/update tests**

```rust
#[test]
fn test_backoff_fixed() {
    let b = BackoffKind::Fixed(1000);
    assert_eq!(b.delay_ms(0), 1000);
    assert_eq!(b.delay_ms(5), 1000);
}

#[test]
fn test_backoff_exponential() {
    let b = BackoffKind::Exponential { base_ms: 100, max_ms: 5000 };
    assert_eq!(b.delay_ms(0), 100);
    assert_eq!(b.delay_ms(1), 200);
    assert_eq!(b.delay_ms(2), 400);
    assert_eq!(b.delay_ms(10), 5000);
}

#[test]
fn test_backoff_exponential_jitter() {
    let b = BackoffKind::ExponentialJitter { base_ms: 100, max_ms: 5000 };
    let d0 = b.delay_ms(0);
    let d1 = b.delay_ms(1);
    assert!(d0 >= 100);
    assert!(d1 >= 200);
}

#[test]
fn test_retry_policy_default() {
    let p = RetryPolicy::default();
    assert_eq!(p.max_attempts, 3);
    assert!(p.retry_on.is_empty());
}

#[test]
fn test_effect_request_with_compensation() {
    let req = EffectRequest::new(
        EffectKind::Http,
        serde_json::json!({"url": "https://api.example.com/charge"}),
    ).with_compensation(CompensationSpec {
        undo_kind: EffectKind::Http,
        undo_input: serde_json::json!({"url": "https://api.example.com/refund"}),
    });
    assert!(req.compensation.is_some());
}
```

**Step 5: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests PASS (existing tests may need minor updates for the new RetryPolicy shape)

**Step 6: Commit**

```bash
git add zeptovm/src/core/effect.rs
git commit -m "feat(core): enhance RetryPolicy with BackoffKind, add CompensationSpec"
```

---

### Task 12: Reactor Retry Loop

**Files:**
- Modify: `zeptovm/src/kernel/reactor.rs`

**Step 1: Implement retry logic in execute_effect**

Replace the simple `execute_effect` with a retry-aware version:

```rust
async fn execute_effect_with_retry(request: &EffectRequest) -> EffectResult {
    let max_attempts = request.retry.max_attempts.max(1);
    let mut last_error = String::new();

    for attempt in 0..max_attempts {
        let result = execute_effect_once(request).await;

        match result.status {
            EffectStatus::Succeeded => return result,
            EffectStatus::Failed => {
                last_error = result.error.clone().unwrap_or_default();

                // Check retry_on filter
                if !request.retry.retry_on.is_empty()
                    && !request.retry.retry_on.iter().any(|cat| last_error.contains(cat))
                {
                    return result; // Error category not retryable
                }

                if attempt + 1 < max_attempts {
                    let delay = request.retry.backoff.delay_ms(attempt);
                    if delay > 0 {
                        tokio::time::sleep(
                            std::time::Duration::from_millis(delay),
                        ).await;
                    }
                } else {
                    return result; // Final attempt failed
                }
            }
            _ => return result, // TimedOut, Cancelled — don't retry
        }
    }

    EffectResult::failure(request.effect_id, last_error)
}

/// Execute a single attempt.
async fn execute_effect_once(request: &EffectRequest) -> EffectResult {
    // Same as the current execute_effect body
    match &request.kind {
        EffectKind::SleepUntil => {
            tokio::time::sleep(request.timeout).await;
            EffectResult::success(
                request.effect_id,
                serde_json::json!({"slept_ms": request.timeout.as_millis()}),
            )
        }
        EffectKind::LlmCall => EffectResult::success(
            request.effect_id,
            serde_json::json!({"response": "placeholder LLM response"}),
        ),
        EffectKind::Http => EffectResult::success(
            request.effect_id,
            serde_json::json!({"status": 200, "body": "placeholder"}),
        ),
        other => EffectResult::success(
            request.effect_id,
            serde_json::json!({"status": "completed", "kind": format!("{:?}", other)}),
        ),
    }
}
```

Update the `Reactor::start()` to call `execute_effect_with_retry` instead of `execute_effect`.

**Step 2: Write tests**

```rust
#[tokio::test]
async fn test_execute_effect_with_retry_succeeds_first_try() {
    let req = EffectRequest::new(EffectKind::LlmCall, serde_json::json!({}));
    let result = execute_effect_with_retry(&req).await;
    assert_eq!(result.status, EffectStatus::Succeeded);
}

#[test]
fn test_reactor_retry_integration() {
    // Use the full Reactor with a request that has retry policy
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);
    let req = EffectRequest::new(EffectKind::LlmCall, serde_json::json!({}));
    reactor.dispatch(pid, req);
    // Wait and verify completion
    std::thread::sleep(std::time::Duration::from_millis(100));
    let completions = reactor.drain_completions();
    assert_eq!(completions.len(), 1);
}
```

**Step 3: Run tests**

Run: `cargo test -p zeptovm kernel::reactor --lib`
Expected: All PASS

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/reactor.rs
git commit -m "feat(reactor): add retry loop with BackoffKind support"
```

---

### Task 13: Idempotency Store

**Files:**
- Create: `zeptovm/src/durability/idempotency.rs`
- Modify: `zeptovm/src/durability/mod.rs`

**Step 1: Write the implementation with tests**

Create `zeptovm/src/durability/idempotency.rs`:

```rust
use rusqlite::{params, Connection, Result as SqlResult};
use crate::core::effect::{EffectId, EffectResult};

/// SQLite-backed idempotency cache for effect results.
pub struct IdempotencyStore {
    conn: Connection,
}

impl IdempotencyStore {
    pub fn open_in_memory() -> SqlResult<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    pub fn open(path: &str) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> SqlResult<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS idempotency (
                effect_id TEXT PRIMARY KEY,
                result_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    /// Check if an effect result is already cached.
    pub fn check(&self, effect_id: &EffectId) -> SqlResult<Option<EffectResult>> {
        let mut stmt = self.conn.prepare(
            "SELECT result_json FROM idempotency WHERE effect_id = ?1",
        )?;
        let key = format!("{}", effect_id);
        let mut rows = stmt.query_map(params![key], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        match rows.next() {
            Some(Ok(json)) => {
                let result: EffectResult = serde_json::from_str(&json)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(
                        Box::new(e),
                    ))?;
                Ok(Some(result))
            }
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// Record an effect result.
    pub fn record(
        &self,
        effect_id: &EffectId,
        result: &EffectResult,
    ) -> SqlResult<()> {
        let key = format!("{}", effect_id);
        let json = serde_json::to_string(result)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(
                Box::new(e),
            ))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO idempotency \
             (effect_id, result_json, created_at) VALUES (?1, ?2, ?3)",
            params![key, json, now],
        )?;
        Ok(())
    }

    /// Remove entries older than the given timestamp (ms since epoch).
    pub fn expire_before(&self, timestamp_ms: u64) -> SqlResult<usize> {
        self.conn.execute(
            "DELETE FROM idempotency WHERE created_at < ?1",
            params![timestamp_ms as i64],
        )
    }

    /// Count of cached entries.
    pub fn count(&self) -> SqlResult<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM idempotency",
            [],
            |row| row.get(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectId, EffectResult, EffectStatus};

    #[test]
    fn test_idempotency_check_miss() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        assert!(store.check(&id).unwrap().is_none());
    }

    #[test]
    fn test_idempotency_record_and_check() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        let result = EffectResult::success(id, serde_json::json!("ok"));
        store.record(&id, &result).unwrap();

        let cached = store.check(&id).unwrap().unwrap();
        assert_eq!(cached.effect_id, id);
        assert_eq!(cached.status, EffectStatus::Succeeded);
    }

    #[test]
    fn test_idempotency_expire() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        let result = EffectResult::success(id, serde_json::json!("ok"));
        store.record(&id, &result).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        // Expire everything before far future
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64 + 100_000;
        store.expire_before(future_ms).unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn test_idempotency_failure_cached() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        let result = EffectResult::failure(id, "network error");
        store.record(&id, &result).unwrap();

        let cached = store.check(&id).unwrap().unwrap();
        assert_eq!(cached.status, EffectStatus::Failed);
        assert_eq!(cached.error.as_deref(), Some("network error"));
    }

    #[test]
    fn test_idempotency_count() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        assert_eq!(store.count().unwrap(), 0);
        for _ in 0..3 {
            let id = EffectId::new();
            store.record(
                &id,
                &EffectResult::success(id, serde_json::json!("ok")),
            ).unwrap();
        }
        assert_eq!(store.count().unwrap(), 3);
    }
}
```

**Step 2: Register the module**

Add `pub mod idempotency;` to `zeptovm/src/durability/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm durability::idempotency --lib`
Expected: 5 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/durability/idempotency.rs zeptovm/src/durability/mod.rs
git commit -m "feat(durability): add IdempotencyStore for effect deduplication"
```

---

### Task 14: Compensation Log

**Files:**
- Create: `zeptovm/src/kernel/compensation.rs`
- Modify: `zeptovm/src/kernel/mod.rs`
- Modify: `zeptovm/src/core/turn_context.rs`

**Step 1: Write the CompensationLog**

Create `zeptovm/src/kernel/compensation.rs`:

```rust
use std::collections::HashMap;

use crate::core::effect::{CompensationSpec, EffectId, EffectKind, EffectRequest};
use crate::pid::Pid;

/// A recorded compensatable effect step.
#[derive(Debug, Clone)]
pub struct CompensationEntry {
    pub effect_id: EffectId,
    pub spec: CompensationSpec,
    pub completed_at_ms: u64,
}

/// Tracks compensatable effects per process for saga-style rollback.
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
    pub fn record(&mut self, pid: Pid, entry: CompensationEntry) {
        self.pending.entry(pid).or_default().push(entry);
    }

    /// Generate rollback effect requests in reverse order.
    /// Removes entries for the process.
    pub fn rollback_all(&mut self, pid: Pid) -> Vec<EffectRequest> {
        let entries = self.pending.remove(&pid).unwrap_or_default();
        entries.into_iter().rev().map(|entry| {
            EffectRequest::new(entry.spec.undo_kind, entry.spec.undo_input)
        }).collect()
    }

    /// Clear all entries for a process (workflow succeeded).
    pub fn clear(&mut self, pid: Pid) {
        self.pending.remove(&pid);
    }

    /// Number of pending entries for a process.
    pub fn count(&self, pid: Pid) -> usize {
        self.pending.get(&pid).map(|v| v.len()).unwrap_or(0)
    }

    /// Total entries across all processes.
    pub fn total_count(&self) -> usize {
        self.pending.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{CompensationSpec, EffectId, EffectKind};

    fn make_entry(undo_kind: EffectKind) -> CompensationEntry {
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
        log.record(pid, CompensationEntry {
            effect_id: EffectId::new(),
            spec: CompensationSpec {
                undo_kind: EffectKind::Http,
                undo_input: serde_json::json!({"step": 1}),
            },
            completed_at_ms: 1000,
        });
        log.record(pid, CompensationEntry {
            effect_id: EffectId::new(),
            spec: CompensationSpec {
                undo_kind: EffectKind::DbWrite,
                undo_input: serde_json::json!({"step": 2}),
            },
            completed_at_ms: 2000,
        });

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
```

**Step 2: Add TurnIntent::Rollback**

In `zeptovm/src/core/turn_context.rs`, add:

```rust
Rollback,
```

And convenience method:

```rust
pub fn rollback(&mut self) {
    self.intents.push(TurnIntent::Rollback);
}
```

**Step 3: Register the module**

Add `pub mod compensation;` to `zeptovm/src/kernel/mod.rs`.

**Step 4: Run tests**

Run: `cargo test -p zeptovm kernel::compensation --lib`
Expected: 6 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/compensation.rs zeptovm/src/kernel/mod.rs \
  zeptovm/src/core/turn_context.rs
git commit -m "feat(kernel): add CompensationLog for saga-style rollback"
```

---

### Task 15: Metrics

**Files:**
- Create: `zeptovm/src/kernel/metrics.rs`
- Modify: `zeptovm/src/kernel/mod.rs`

**Step 1: Write the implementation with tests**

Create `zeptovm/src/kernel/metrics.rs`:

```rust
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// In-process metrics collection (counters + gauges).
/// All operations are atomic — no locks needed.
pub struct Metrics {
    counters: HashMap<&'static str, AtomicU64>,
    gauges: HashMap<&'static str, AtomicI64>,
}

impl Metrics {
    pub fn new() -> Self {
        let mut counters = HashMap::new();
        let mut gauges = HashMap::new();

        // Pre-register known metrics
        for name in [
            "processes.spawned",
            "processes.exited",
            "effects.dispatched",
            "effects.completed",
            "effects.failed",
            "effects.retries",
            "effects.timed_out",
            "turns.committed",
            "timers.scheduled",
            "timers.fired",
            "supervisor.restarts",
            "compensation.triggered",
            "budget.blocked",
            "scheduler.ticks",
        ] {
            counters.insert(name, AtomicU64::new(0));
        }

        for name in [
            "processes.active",
            "mailbox.depth_total",
        ] {
            gauges.insert(name, AtomicI64::new(0));
        }

        Self { counters, gauges }
    }

    /// Increment a counter by 1.
    pub fn inc(&self, name: &'static str) {
        if let Some(c) = self.counters.get(name) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Increment a counter by n.
    pub fn inc_by(&self, name: &'static str, n: u64) {
        if let Some(c) = self.counters.get(name) {
            c.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Set a gauge value.
    pub fn gauge_set(&self, name: &'static str, val: i64) {
        if let Some(g) = self.gauges.get(name) {
            g.store(val, Ordering::Relaxed);
        }
    }

    /// Get a counter value.
    pub fn counter(&self, name: &'static str) -> u64 {
        self.counters.get(name).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// Get a gauge value.
    pub fn gauge(&self, name: &'static str) -> i64 {
        self.gauges.get(name).map(|g| g.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// Snapshot all metrics as (name, value) pairs.
    pub fn snapshot(&self) -> Vec<(&'static str, i64)> {
        let mut result: Vec<_> = self.counters.iter()
            .map(|(k, v)| (*k, v.load(Ordering::Relaxed) as i64))
            .chain(self.gauges.iter()
                .map(|(k, v)| (*k, v.load(Ordering::Relaxed))))
            .collect();
        result.sort_by_key(|(k, _)| *k);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_new() {
        let m = Metrics::new();
        assert_eq!(m.counter("processes.spawned"), 0);
        assert_eq!(m.gauge("processes.active"), 0);
    }

    #[test]
    fn test_metrics_inc() {
        let m = Metrics::new();
        m.inc("processes.spawned");
        m.inc("processes.spawned");
        assert_eq!(m.counter("processes.spawned"), 2);
    }

    #[test]
    fn test_metrics_inc_by() {
        let m = Metrics::new();
        m.inc_by("effects.dispatched", 5);
        assert_eq!(m.counter("effects.dispatched"), 5);
    }

    #[test]
    fn test_metrics_gauge() {
        let m = Metrics::new();
        m.gauge_set("processes.active", 42);
        assert_eq!(m.gauge("processes.active"), 42);
        m.gauge_set("processes.active", 10);
        assert_eq!(m.gauge("processes.active"), 10);
    }

    #[test]
    fn test_metrics_unknown_name_ignored() {
        let m = Metrics::new();
        m.inc("nonexistent.counter");
        assert_eq!(m.counter("nonexistent.counter"), 0);
    }

    #[test]
    fn test_metrics_snapshot() {
        let m = Metrics::new();
        m.inc("processes.spawned");
        m.gauge_set("processes.active", 5);
        let snap = m.snapshot();
        assert!(!snap.is_empty());
        // Find spawned
        let spawned = snap.iter().find(|(k, _)| *k == "processes.spawned");
        assert_eq!(spawned.unwrap().1, 1);
    }

    #[test]
    fn test_metrics_concurrent_safe() {
        use std::sync::Arc;
        let m = Arc::new(Metrics::new());
        let handles: Vec<_> = (0..10).map(|_| {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                for _ in 0..100 {
                    m.inc("scheduler.ticks");
                }
            })
        }).collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(m.counter("scheduler.ticks"), 1000);
    }
}
```

**Step 2: Register the module**

Add `pub mod metrics;` to `zeptovm/src/kernel/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm kernel::metrics --lib`
Expected: 7 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/metrics.rs zeptovm/src/kernel/mod.rs
git commit -m "feat(kernel): add Metrics with atomic counters and gauges"
```

---

### Task 16: Tracing Instrumentation

**Files:**
- Modify: `zeptovm/src/kernel/scheduler.rs`
- Modify: `zeptovm/src/kernel/reactor.rs`
- Modify: `zeptovm/src/kernel/turn_executor.rs`
- Modify: `zeptovm/src/kernel/runtime.rs`

**Step 1: Add tracing spans to scheduler**

In `SchedulerEngine::tick()`:

```rust
use tracing::{info_span, info, warn};

pub fn tick(&mut self) -> usize {
    let _span = info_span!("scheduler_tick", tick = self.tick_count).entered();
    // ... existing logic ...
}
```

In `step_process`:

```rust
let _span = info_span!("process_step", %pid).entered();
```

**Step 2: Add tracing to reactor**

In the dispatch loop:

```rust
let _span = info_span!("effect_dispatch",
    effect_id = %dispatch.request.effect_id,
    kind = ?dispatch.request.kind,
).entered();
```

**Step 3: Add tracing to turn_executor**

In `commit()`:

```rust
let _span = info_span!("turn_commit",
    %turn.pid,
    turn_id = turn.turn_id,
    intents = turn.journal_entries.len(),
).entered();
```

**Step 4: Add tracing to runtime**

In `SchedulerRuntime::tick()`:

```rust
tracing::debug!(stepped, "runtime tick complete");
```

On process exit:

```rust
tracing::info!(%pid, %reason, "process exited");
```

On budget violation:

```rust
tracing::warn!(%pid, kind = ?req.kind, "effect blocked by budget");
```

**Step 5: Run tests to verify nothing breaks**

Run: `cargo test -p zeptovm --lib`
Expected: All tests PASS (tracing is no-op without a subscriber)

**Step 6: Commit**

```bash
git add zeptovm/src/kernel/scheduler.rs zeptovm/src/kernel/reactor.rs \
  zeptovm/src/kernel/turn_executor.rs zeptovm/src/kernel/runtime.rs
git commit -m "feat(kernel): add tracing spans to scheduler, reactor, turn executor"
```

---

### Task 17: Wire Metrics into SchedulerRuntime

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`

**Step 1: Add Metrics to SchedulerRuntime**

Add `metrics: Metrics` field. Update `new()` and `with_durability()`.

Instrument key operations:

```rust
// In spawn():
self.metrics.inc("processes.spawned");
self.metrics.gauge_set("processes.active", self.process_count() as i64);

// In tick(), after stepping:
self.metrics.inc("scheduler.ticks");

// When effect dispatched:
self.metrics.inc("effects.dispatched");

// When budget blocks:
self.metrics.inc("budget.blocked");

// In take_completed():
// for each completed process:
self.metrics.inc("processes.exited");
self.metrics.gauge_set("processes.active", self.process_count() as i64);
```

Add accessor:

```rust
pub fn metrics(&self) -> &Metrics {
    &self.metrics
}
```

**Step 2: Write tests**

```rust
#[test]
fn test_runtime_metrics_spawn() {
    let mut rt = SchedulerRuntime::new();
    rt.spawn(Box::new(Echo));
    rt.spawn(Box::new(Echo));
    assert_eq!(rt.metrics().counter("processes.spawned"), 2);
    assert_eq!(rt.metrics().gauge("processes.active"), 2);
}

#[test]
fn test_runtime_metrics_ticks() {
    let mut rt = SchedulerRuntime::new();
    rt.spawn(Box::new(Echo));
    rt.tick();
    assert_eq!(rt.metrics().counter("scheduler.ticks"), 1);
}

#[test]
fn test_runtime_metrics_budget_blocked() {
    let budget = BudgetState { usd_remaining: 0.0, token_remaining: 0 };
    let mut rt = SchedulerRuntime::new().with_budget(budget);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();
    assert_eq!(rt.metrics().counter("budget.blocked"), 1);
}
```

**Step 3: Run tests**

Run: `cargo test -p zeptovm kernel::runtime --lib`
Expected: All PASS

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(kernel): wire Metrics into SchedulerRuntime"
```

---

### Task 18: Phase 2 Gate Tests

**Files:**
- Create: `zeptovm/tests/phase2_gate.rs`

**Step 1: Write integration tests**

Create `zeptovm/tests/phase2_gate.rs`:

```rust
//! Phase 2 gate tests: verify timer, supervision, and effect hardening.

use zeptovm::core::behavior::StepBehavior;
use zeptovm::core::effect::{EffectKind, EffectRequest};
use zeptovm::core::message::{Envelope, EnvelopePayload, Signal};
use zeptovm::core::step_result::StepResult;
use zeptovm::core::supervisor::{BackoffPolicy, RestartStrategy, SupervisorSpec};
use zeptovm::core::timer::{TimerKind, TimerSpec};
use zeptovm::core::turn_context::TurnContext;
use zeptovm::error::Reason;
use zeptovm::kernel::compensation::{CompensationEntry, CompensationLog};
use zeptovm::kernel::metrics::Metrics;
use zeptovm::kernel::supervisor_behavior::SupervisorBehavior;
use zeptovm::kernel::timer_wheel::TimerWheel;
use zeptovm::pid::Pid;

/// Gate test: timer wheel fires at correct time.
#[test]
fn phase2_gate_timer_wheel() {
    let mut wheel = TimerWheel::new();
    let pid = Pid::from_raw(1);

    // Schedule 3 timers at different deadlines
    wheel.schedule(TimerSpec::new(pid, TimerKind::SleepUntil, 100));
    wheel.schedule(TimerSpec::new(pid, TimerKind::Timeout, 200));
    wheel.schedule(TimerSpec::new(pid, TimerKind::RetryBackoff, 300));

    assert_eq!(wheel.len(), 3);

    // Tick at 150ms — only first timer should fire
    let fired = wheel.tick(150);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].kind, TimerKind::SleepUntil);

    // Tick at 300ms — remaining two fire
    let fired = wheel.tick(300);
    assert_eq!(fired.len(), 2);
    assert!(wheel.is_empty());
}

/// Gate test: supervisor detects max restarts exceeded.
#[test]
fn phase2_gate_supervisor_escalation() {
    let mut sup = SupervisorBehavior::new(SupervisorSpec {
        max_restarts: 2,
        restart_window_ms: 5000,
        backoff: BackoffPolicy::Immediate,
    });
    let sup_pid = Pid::from_raw(1);
    let child = Pid::from_raw(10);
    sup.register_child("w1".into(), child, RestartStrategy::Permanent);
    sup.init(None);

    // First crash — restart
    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(sup_pid, Signal::ChildExited {
        child_pid: child,
        reason: Reason::Custom("crash".into()),
    });
    let r = sup.handle(msg, &mut ctx);
    assert!(matches!(r, StepResult::Continue));

    // Second crash — exceeds max_restarts=2, supervisor shuts down
    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(sup_pid, Signal::ChildExited {
        child_pid: child,
        reason: Reason::Custom("crash".into()),
    });
    let r = sup.handle(msg, &mut ctx);
    assert!(matches!(r, StepResult::Fail(Reason::Shutdown)));
}

/// Gate test: compensation rollback in reverse order.
#[test]
fn phase2_gate_compensation_rollback() {
    use zeptovm::core::effect::{CompensationSpec, EffectId};

    let mut log = CompensationLog::new();
    let pid = Pid::from_raw(1);

    // Record 3 compensatable steps
    for i in 1..=3 {
        log.record(pid, CompensationEntry {
            effect_id: EffectId::new(),
            spec: CompensationSpec {
                undo_kind: EffectKind::Http,
                undo_input: serde_json::json!({"step": i}),
            },
            completed_at_ms: i as u64 * 1000,
        });
    }

    let rollbacks = log.rollback_all(pid);
    assert_eq!(rollbacks.len(), 3);
    // Reverse order: step 3, 2, 1
    assert_eq!(rollbacks[0].input["step"], 3);
    assert_eq!(rollbacks[1].input["step"], 2);
    assert_eq!(rollbacks[2].input["step"], 1);
}

/// Gate test: metrics are thread-safe and accurate.
#[test]
fn phase2_gate_metrics_concurrent() {
    use std::sync::Arc;

    let m = Arc::new(Metrics::new());
    let handles: Vec<_> = (0..4).map(|_| {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            for _ in 0..250 {
                m.inc("scheduler.ticks");
            }
        })
    }).collect();

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(m.counter("scheduler.ticks"), 1000);
}

/// Gate test: idempotency store deduplicates effects.
#[test]
fn phase2_gate_idempotency() {
    use zeptovm::core::effect::{EffectId, EffectResult, EffectStatus};
    use zeptovm::durability::idempotency::IdempotencyStore;

    let store = IdempotencyStore::open_in_memory().unwrap();
    let id = EffectId::new();

    // First check — miss
    assert!(store.check(&id).unwrap().is_none());

    // Record
    let result = EffectResult::success(id, serde_json::json!("response"));
    store.record(&id, &result).unwrap();

    // Second check — hit
    let cached = store.check(&id).unwrap().unwrap();
    assert_eq!(cached.status, EffectStatus::Succeeded);
    assert_eq!(cached.effect_id, id);
}
```

**Step 2: Run all tests**

Run: `cargo test -p zeptovm`
Expected: All unit tests + gate tests PASS

**Step 3: Commit**

```bash
git add zeptovm/tests/phase2_gate.rs
git commit -m "test: add Phase 2 gate tests for timers, supervision, compensation, metrics, idempotency"
```

---

## Summary

| Task | Component | New Files | Tests |
|------|-----------|-----------|-------|
| 1 | Timer types | `core/timer.rs` | 5 |
| 2 | Timer wheel | `kernel/timer_wheel.rs` | 7 |
| 3 | Timer SQLite store | `durability/timer_store.rs` | 6 |
| 4 | Wire timers into scheduler | (modify scheduler, turn_context, message) | ~3 |
| 5 | Supervisor types | `core/supervisor.rs` | 6 |
| 6 | ProcessEntry parent/trap_exit | (modify process_table, message) | ~2 |
| 7 | SupervisorBehavior | `kernel/supervisor_behavior.rs` | 7 |
| 8 | Wire LinkTable into scheduler | (modify scheduler, turn_context) | ~3 |
| 9 | New JournalEntryTypes | (modify journal.rs) | 0 (existing pass) |
| 10 | Recovery coordinator | `kernel/recovery.rs` | 4 |
| 11 | Enhance RetryPolicy | (modify effect.rs) | ~5 |
| 12 | Reactor retry loop | (modify reactor.rs) | ~2 |
| 13 | Idempotency store | `durability/idempotency.rs` | 5 |
| 14 | Compensation log | `kernel/compensation.rs` | 6 |
| 15 | Metrics | `kernel/metrics.rs` | 7 |
| 16 | Tracing instrumentation | (modify 4 kernel files) | 0 |
| 17 | Wire metrics into runtime | (modify runtime.rs) | ~3 |
| 18 | Phase 2 gate tests | `tests/phase2_gate.rs` | 5 |

**Total: ~18 tasks, ~76 new tests**
