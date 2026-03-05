# BEAM-Level Reliability Hardening Design

**Goal:** Harden zeptoclaw-rt agent runtime with BEAM-inspired reliability patterns across 7 areas: crash isolation, supervisor restart trees, mailbox bounds, timeouts/cancellation, durable recovery, observability, and fault injection.

**Approach:** Layered bottom-up — build in dependency order so each layer is independently testable before the next.

**Working directory:** `~/ios/zeptoclaw-rt`

---

## Priority Tiers

| Tier | Area | Status |
|------|------|--------|
| Must-Have | Mailbox Bounds & Backpressure | New |
| Must-Have | Supervisor Backoff + Tests | Extend existing |
| Must-Have | Timeouts & Cancellation | New |
| Should-Have | Dead-Letter Queue + Observability | New |
| Should-Have | Durable Recovery (SQLite) | Extend existing |
| Nice-to-Have | Fault Injection / Chaos Testing | New |
| Done | Crash Isolation | Verify only |

## Implementation Order

1. Mailbox Bounds & Backpressure
2. Supervisor Backoff + Tests
3. Timeouts & Cancellation
4. Dead-Letter Queue + Observability
5. Durable Recovery (SQLite)
6. Fault Injection / Chaos Testing
7. Crash Isolation verification tests

---

## 1. Mailbox Bounds & Backpressure (Must-Have)

**Current state:** `AgentProcess.mailbox` is an unbounded `VecDeque<Message>`. `deliver_message()` always succeeds.

**Changes:**
- Add `mailbox_capacity: usize` to `AgentProcess` (default 1024, configurable per-process)
- `deliver_message()` returns `Result<(), MailboxFull>` instead of `()`
- When mailbox is full, caller gets `MailboxFull` error immediately (reject-sender policy)
- Orchestration layer translates `MailboxFull` into `worker_busy` response back to parent
- Messages rejected due to full mailbox are routed to the dead-letter queue (Section 4)
- `RuntimeMetrics` gains `mailbox_rejections: AtomicU64` counter

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/process.rs`
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs` (deliver_message callers)
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs` (handle MailboxFull → worker_busy)
- Modify: `lib-erlangrt/src/agent_rt/observability.rs` (add mailbox_rejections metric)
- Test: `lib-erlangrt/src/agent_rt/tests/` (mailbox bounds tests)

**Rationale:** Reject-sender (not drop-oldest) because every message represents user-initiated work. Dropping silently causes invisible data loss. Reject-sender gives the caller a clear signal to retry or escalate.

---

## 2. Supervisor Restart Trees + Backoff (Must-Have)

**Current state:** `supervision.rs` has `RestartStrategy` (OneForOne/OneForAll/RestForOne), `ChildRestart` (Permanent/Transient/Temporary), and `SupervisorSpec` with `max_restarts`/`max_seconds` intensity. Zero tests, no backoff delay between restarts.

**Changes:**

Add `BackoffStrategy` to `SupervisorSpec`:
```rust
enum BackoffStrategy {
    None,                          // restart immediately (current behavior)
    Fixed { delay_ms: u64 },       // constant delay between restarts
    Exponential { base_ms: u64, max_ms: u64, multiplier: f64 },  // exponential with cap
}
```

- Track `restart_count` per child in supervisor state for backoff calculation
- When `exceeds_max_restarts()` returns true, escalate to parent supervisor (or terminate if top-level)
- Reset backoff counter on successful run (child runs longer than `max_seconds` without crashing)

**Tests (currently zero — all new):**
- OneForOne: crash one child, verify only that child restarts
- OneForAll: crash one, verify all restart
- RestForOne: crash one, verify it + later children restart
- Intensity limit: exceed `max_restarts` in `max_seconds`, verify escalation
- Backoff: verify delays increase exponentially, cap at `max_ms`
- Transient vs Permanent: verify transient doesn't restart on normal exit
- Temporary: verify temporary never restarts

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/supervision.rs`
- Test: `lib-erlangrt/src/agent_rt/tests/` (supervision tests)

---

## 3. Timeouts & Cancellation (Must-Have)

**Current state:** `receive_timeout` on processes. Worker idle timeout in orchestration. No per-IO-operation cancellation — once `AgentChat` is dispatched to the bridge, it runs to completion or panics.

**Changes:**
- Add `timeout_ms: Option<u64>` to `IoOp::AgentChat` and `IoOp::HttpRequest`
- Bridge wraps each operation in `tokio::time::timeout(duration, future)`
- On timeout, bridge returns `IoResponse::Error { kind: Timeout, .. }` back to scheduler
- Scheduler delivers timeout error as a message to the requesting process
- Process can handle it (retry, escalate to supervisor, or fail)

**Cancellation tokens:**
- Add `CancellationToken` (from `tokio_util`) to bridge's in-flight operation tracking
- When a process is terminated (by supervisor or explicitly), scheduler sends cancel signal
- Bridge checks token before starting work, and selects on it during long operations
- Prevents orphaned agent chats running after their owning process is dead

**Key invariant:** No IO operation can run indefinitely. Every `IoOp` variant either has an explicit timeout or uses a configurable default (120s for AgentChat, 30s for HttpRequest).

**Metrics:** `RuntimeMetrics` gains `io_timeouts: AtomicU64` counter.

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/types.rs` (add timeout_ms to IoOps)
- Modify: `lib-erlangrt/src/agent_rt/bridge.rs` (timeout wrapping, cancellation tokens)
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs` (cancel on terminate, deliver timeout errors)
- Modify: `lib-erlangrt/src/agent_rt/observability.rs` (io_timeouts metric)
- Add dep: `tokio-util` (for CancellationToken)
- Test: `lib-erlangrt/src/agent_rt/tests/` (timeout + cancellation tests)

---

## 4. Dead-Letter Queue + Observability (Should-Have)

**Current state:** `RuntimeMetrics` tracks messages_sent/received, IO counts, latency. `BridgeMetrics` tracks destroy_failures, busy_rejections, chat_panics. No dead-letter queue. No structured tracing spans.

### Dead-Letter Queue

- New `DeadLetterQueue` struct: bounded ring buffer (`VecDeque<DeadLetter>`, capacity 256)
- `DeadLetter { timestamp, sender_pid, target_pid, message, reason: DeadLetterReason }`
- `DeadLetterReason`: `MailboxFull`, `ProcessNotFound`, `ProcessTerminated`
- Scheduler routes undeliverable messages here instead of silently dropping
- Exposed via `RuntimeMetrics::dead_letter_count: AtomicU64`
- Each dead letter logged at `WARN` level with structured fields

### Observability Wiring

Add `tracing` crate spans to key operations:
- `process.spawn` (pid, parent, name)
- `process.terminate` (pid, reason)
- `supervisor.restart` (child_pid, strategy, attempt_count, backoff_ms)
- `bridge.agent_chat` (pid, duration_ms, success/fail)
- `bridge.timeout` (pid, op_type, timeout_ms)
- `mailbox.reject` (sender, target, queue_depth)

These are structured events compatible with any tracing subscriber. No new external dependencies beyond `tracing` (already in dependency tree via tokio).

**Files:**
- Create: `lib-erlangrt/src/agent_rt/dead_letter.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add dead_letter module)
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs` (route to DLQ)
- Modify: `lib-erlangrt/src/agent_rt/observability.rs` (dead_letter_count)
- Modify: `lib-erlangrt/src/agent_rt/bridge.rs` (tracing spans)
- Modify: `lib-erlangrt/src/agent_rt/supervision.rs` (tracing spans)
- Add dep: `tracing` crate
- Test: `lib-erlangrt/src/agent_rt/tests/` (DLQ tests)

---

## 5. Durable Recovery with SQLite (Should-Have)

**Current state:** `CheckpointStore` trait with `save/load/delete/list_keys`. `InMemoryCheckpointStore` works. `FileCheckpointStore` exists but is untested. Orchestrator can checkpoint/resume tasks.

### SQLite Checkpoint Store

- New `SqliteCheckpointStore` implementing `CheckpointStore` trait
- Dependency: `rusqlite` with `bundled` feature (bundles SQLite, no system dependency)
- Schema:
  ```sql
  CREATE TABLE checkpoints (
      key TEXT PRIMARY KEY,
      data BLOB NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
  );
  ```
- `save()` uses `INSERT OR REPLACE` (upsert)
- `load()` returns `Option<Vec<u8>>`
- Connection: single `rusqlite::Connection` behind a `Mutex` (sufficient for single-node)
- WAL mode enabled for concurrent reads during writes
- DB path configurable, defaults to `./zeptoclaw-rt.db`

### Recovery Flow Enhancement

- Checkpoint data includes: `{ task_id, worker_pid, turn_count, last_response, worker_state, timestamp }`
- On scheduler restart, load all checkpoints and rebuild WorkerState for each
- Stale checkpoints (older than configurable TTL, default 24h) are auto-pruned
- `FileCheckpointStore` kept for backward compatibility, `SqliteCheckpointStore` is the recommended default

**Files:**
- Create: `lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add checkpoint_sqlite module)
- Modify: `lib-erlangrt/Cargo.toml` (add rusqlite dep)
- Test: `lib-erlangrt/src/agent_rt/tests/` (SQLite checkpoint tests)

---

## 6. Fault Injection / Chaos Testing (Nice-to-Have)

**Current state:** Nothing exists.

### Test-Only Traits (`#[cfg(test)]`)

- `FaultyBridge`: wraps real bridge, randomly fails `AgentChat` responses with configurable failure rate
  - Failure modes: `Timeout`, `Panic`, `ErrorResponse`, `SlowResponse(delay_ms)`
  - Deterministic via seeded RNG for reproducible test failures
- `FaultyCheckpointStore`: wraps real store, randomly fails `save()` or corrupts `load()` data
- `FaultyMailbox`: injectable wrapper that randomly rejects messages (simulates full mailbox)
- Each faulty impl takes `FaultConfig { fail_rate: f64, seed: u64, modes: Vec<FaultMode> }`

### Runtime Chaos (`chaos_testing` Cargo Feature Flag)

- `ChaosConfig` loaded from env vars:
  - `ZEPTOCLAW_CHAOS_ENABLED=true`
  - `ZEPTOCLAW_CHAOS_FAIL_RATE=0.05` (5% failure rate)
  - `ZEPTOCLAW_CHAOS_MODES=timeout,slow` (comma-separated)
  - `ZEPTOCLAW_CHAOS_SEED=42` (reproducible)
- `ChaosInterceptor` trait injected into bridge's IO processing pipeline
- When feature is disabled (default), zero runtime overhead — compiled out
- When enabled, interceptor checks each IO operation against fail rate before executing

### Integration Tests

- Supervisor recovers workers after bridge failures
- Checkpoint store corruption triggers clean re-initialization
- Mailbox backpressure under sustained chaos maintains system stability
- Dead-letter queue captures all chaos-induced failures
- System reaches steady state after chaos stops

**Files:**
- Create: `lib-erlangrt/src/agent_rt/chaos.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add chaos module)
- Modify: `lib-erlangrt/Cargo.toml` (add `chaos_testing` feature)
- Test: `lib-erlangrt/src/agent_rt/tests/` (chaos integration tests)
- Test: `lib-erlangrt/src/agent_rt/integration_tests/` (end-to-end chaos scenarios)

---

## 7. Crash Isolation (Done — Verify Only)

**Current state:** Already implemented. `catch_unwind` in bridge's `execute_agent_chat`, panic counter in `BridgeMetrics`, scheduler continues after worker crash.

**Verification tests to add:**
- Worker panic doesn't affect sibling workers
- Panic counter increments correctly
- Crashed worker's process gets terminated and supervisor notified
- Agent registry evicts crashed agent

**Files:**
- Test: `lib-erlangrt/src/agent_rt/tests/` (crash isolation verification)

---

## New Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `rusqlite` | latest, `bundled` feature | SQLite checkpoint store |
| `tokio-util` | latest | CancellationToken |
| `tracing` | latest | Structured observability |

---

## Design Decisions Summary

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Mailbox overflow policy | Reject sender | Every message is user-initiated work; silent drops = invisible data loss |
| Backoff strategy | Exponential with cap | Prevents restart storms while allowing fast recovery from transient failures |
| Checkpoint backing store | SQLite (rusqlite bundled) | Single-file DB, no external deps, queryable, handles concurrent reads |
| Dead-letter storage | In-memory ring buffer (256) | Simple, no persistence needed; metrics + WARN logs sufficient for debugging |
| Fault injection scope | Test traits + runtime feature flag | CI coverage via test traits; staging validation via feature flag; zero prod overhead |
| IO timeout default | 120s AgentChat, 30s HttpRequest | Generous but bounded; prevents indefinite hangs |
