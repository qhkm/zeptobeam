# ZeptoVM Runtime Specification v0.2

> A durable distributed runtime for AI agents where each agent is an isolated supervised process with a mailbox, executed in bounded turns by a BEAM-inspired async-aware scheduler, suspended through explicit journaled effects, recovered through snapshots and logs, and governed by budgets, policy, timers, and cluster placement.

**Last updated:** 2026-03-06
**Implementation status:** Phase 1 + Phase 2 complete (single-node durable runtime with effect hardening)

---

## Table of Contents

1. [Thesis](#1-thesis)
2. [Design Goals](#2-design-goals)
3. [Core Primitives](#3-core-primitives)
4. [Process Model](#4-process-model)
5. [Message Model](#5-message-model)
6. [Mailbox Engine](#6-mailbox-engine)
7. [Turn Model](#7-turn-model)
8. [Deterministic Shell / Nondeterministic Effects](#8-deterministic-shell--nondeterministic-effects)
9. [Behavior Trait](#9-behavior-trait)
10. [Effect System](#10-effect-system)
11. [Journal Model](#11-journal-model)
12. [Snapshot Model](#12-snapshot-model)
13. [Turn Commit Protocol](#13-turn-commit-protocol)
14. [Recovery Model](#14-recovery-model)
15. [Scheduler Design](#15-scheduler-design)
16. [Reactor Bridge](#16-reactor-bridge)
17. [Timer Subsystem](#17-timer-subsystem)
18. [Supervision Model](#18-supervision-model)
19. [Links and Monitors](#19-links-and-monitors)
20. [Budget Model](#20-budget-model)
21. [Policy Engine](#21-policy-engine)
22. [Observability](#22-observability)
23. [Distributed Model](#23-distributed-model)
24. [Object Plane](#24-object-plane)
25. [Human-in-the-Loop](#25-human-in-the-loop)
26. [Behavior Versioning](#26-behavior-versioning)
27. [Strong Invariants](#27-strong-invariants)
28. [Module Map](#28-module-map)
29. [Implementation Status](#29-implementation-status)
30. [Phased Roadmap](#30-phased-roadmap)

---

## 1. Thesis

ZeptoVM is a **durable distributed agent runtime** where:

- every agent is a **process**
- every process communicates via **messages**
- every execution step is a bounded **turn**
- every side effect is an explicit **effect**
- every wait is **durable**
- every failure is governed by **supervision**
- every process is **budgeted**, **policy-checked**, and **inspectable**
- every process can be **placed**, **resumed**, and eventually **migrated** across a cluster

ZeptoVM fuses:

| Inspiration | What we take |
|-------------|-------------|
| **BEAM** | Processes, mailboxes, supervision, fair scheduling, links/monitors |
| **Temporal** | Durable waits, replay, timers, snapshot+journal recovery |
| **Ray** | Distributed placement, resource-aware execution, cluster routing |

---

## 2. Design Goals

ZeptoVM is designed for AI agents that are long-lived, mostly I/O-bound, stateful, failure-prone, partially nondeterministic, expensive to run, sensitive to policy/security/budget, and often composed of sub-agents, tools, and human approvals.

The runtime optimizes for:

1. **Cheap concurrency** — millions of logical processes, not OS threads
2. **Fair execution** — no process starves others
3. **Durable waiting** — effects, timers, human approvals survive restarts
4. **Failure containment** — crashes are isolated and supervised
5. **Explicit side effects** — all I/O goes through the effect system
6. **Replayability** — deterministic shell + recorded effect results
7. **Budget awareness** — token/cost/quota limits checked before dispatch
8. **Policy enforcement** — capabilities gated before execution
9. **Distributed placement** — processes placed by affinity, load, locality
10. **Deep observability** — every turn, effect, timer, budget debit is traceable

---

## 3. Core Primitives

ZeptoVM has 7 first-class runtime primitives:

| Primitive | Purpose |
|-----------|---------|
| **Process** | Durable isolated agent state machine |
| **Message** | Envelope-wrapped communication between processes |
| **Turn** | Bounded execution step with atomic commit |
| **Effect** | Explicit description of a side effect (LLM, HTTP, etc.) |
| **Journal** | Append-only log of process lifecycle events |
| **Supervisor** | Fault owner that restarts/escalates on child failure |
| **ObjectRef** | Reference to large out-of-band artifact (future) |

Everything else is built from these.

---

## 4. Process Model

A process is the fundamental unit of execution. It is not an OS thread, not a Tokio task, not a container. It is a **durable isolated agent state machine**.

### 4.1 Process Identity

```rust
// Current implementation (v0.2 — single-node)
pub struct Pid(u64);

// Target (cluster-aware)
pub struct Pid {
    tenant_id: TenantId,
    namespace: Namespace,
    process_id: u64,
    incarnation: u64,
}
```

`incarnation` will distinguish lifecycle epochs after restart or recovery.

**Current status:** Simple `u64` Pid. Cluster-aware Pid deferred to Phase 3.

### 4.2 Process Kinds

```rust
enum ProcessKind {
    Agent,
    Workflow,
    Supervisor,
    Adapter,
    System,
    HumanWait,
    TaskWorker,
}
```

At runtime, "agent" and "workflow" are the same core primitive. The difference is behavior, policy, and composition.

**Current status:** Not yet as an enum in code. All processes use the same `ProcessEntry` type. `SupervisorBehavior` is a behavior impl, not a process kind marker.

### 4.3 Process Runtime State

```rust
// Implemented in kernel/process_table.rs
pub enum ProcessRuntimeState {
    Ready,
    Running,
    WaitingMessage,
    WaitingEffect(u64),   // EffectId raw
    WaitingTimer,
    Checkpointing,
    Suspended,
    Failed,
    Done,
}
```

**Current status:** Fully implemented. State transitions enforced by `SchedulerEngine`.

### 4.4 Process State Layout

A process has multiple layers of state:

| Layer | Contents |
|-------|----------|
| **Metadata** | Pid, parent, trap_exit, priority |
| **Working state** | Behavior-owned state (opaque `Vec<u8>` snapshot) |
| **Mailbox state** | Multi-lane queue with cursor |
| **Durable execution state** | Journal offset, snapshot version |
| **Resource state** | Budget, active timers, pending effects |

---

## 5. Message Model

Processes communicate exclusively through messages.

### 5.1 Message Envelope

```rust
// Implemented in core/message.rs
pub struct Envelope {
    pub msg_id: MsgId,
    pub correlation_id: Option<String>,
    pub from: Option<Pid>,
    pub to: Pid,
    pub class: MessageClass,
    pub priority: Priority,
    pub dedup_key: Option<String>,
    pub payload: EnvelopePayload,
}
```

### 5.2 Message Classes

```rust
// Implemented
pub enum MessageClass {
    Control,        // Kill, Shutdown, Suspend, Resume
    Supervisor,     // ChildExited
    EffectResult,   // Completed effect results
    User,           // Application messages
    Background,     // Low-priority bulk
}
```

### 5.3 Envelope Payload

```rust
// Implemented
pub enum EnvelopePayload {
    User(Payload),          // Text, Bytes, Json
    Effect(EffectResult),   // Completed effect
    Signal(Signal),         // System signals
}

pub enum Signal {
    Kill,
    Shutdown,
    ExitLinked(Pid, Reason),
    MonitorDown(Pid, Reason),
    Suspend,
    Resume,
    TimerFired(TimerId),
    ChildExited { child_pid: Pid, reason: Reason },
}
```

### 5.4 Priority

```rust
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
}
```

**Target spec** adds `Max` priority. Not yet needed.

---

## 6. Mailbox Engine

Each process has a logical mailbox implemented as multiple internal lanes.

### 6.1 Lane Structure

```text
control_q > supervisor_q > effect_q > user_q > background_q
```

Default pull order: control first, then supervisor, then effect results, then user, then background.

**Current status:** Implemented as `MultiLaneMailbox` in `kernel/mailbox.rs`. Supports `push()` with automatic lane routing by `MessageClass`, and `pop()` with priority ordering.

### 6.2 Mailbox Features

| Feature | Status |
|---------|--------|
| Multi-lane priority | Implemented |
| Durable enqueue | Via journal |
| Per-message TTL | Spec only |
| Dedup keys | Field on Envelope, enforcement not yet |
| Selective receive | Spec only — bounded matching by type/correlation/child |
| Bounded thresholds | Spec only |

---

## 7. Turn Model

A process runs in **turns**, not as an unbounded async loop.

A turn is:

> A bounded execution step where one process consumes one eligible event, mutates state, emits intents, commits durably, then yields.

### 7.1 Turn Lifecycle

```text
dequeue next eligible message
  -> execute behavior.handle(msg, ctx)
  -> collect TurnIntents from ctx
  -> run policy + budget checks
  -> persist journal entries + state delta
  -> publish outbound messages/effects/timers
  -> yield
```

### 7.2 TurnIntent

Handlers emit intents, not direct mutations. The runtime collects, validates, and commits them atomically.

```rust
// Implemented in core/turn_context.rs
pub enum TurnIntent {
    SendMessage(Envelope),
    RequestEffect(EffectRequest),
    PatchState(Vec<u8>),
    ScheduleTimer(TimerSpec),
    CancelTimer(TimerId),
    Rollback,
    Link(Pid),
    Unlink(Pid),
    Monitor(Pid),
    Demonitor(MonitorRef),
    // Future: SpawnProcess, DebitBudget
}
```

### 7.3 TurnContext

```rust
// Implemented
pub struct TurnContext {
    pub pid: Pid,
    pub turn_id: TurnId,
    intents: Vec<TurnIntent>,
}
```

Convenience methods: `send()`, `send_text()`, `request_effect()`, `set_state()`, `schedule_timer()`, `cancel_timer()`, `rollback()`, `link()`, `unlink()`, `monitor()`, `demonitor()`.

### 7.4 Why Turns Matter

Turns give: fairness, durability boundaries, replayability, inspectability, easier recovery, easier supervision, easier budget accounting.

---

## 8. Deterministic Shell / Nondeterministic Effects

This is the central design principle.

### 8.1 Deterministic Orchestration Layer (replayable)

Handles: mailbox progression, state transitions, timers, retries, spawn decisions, supervisor semantics, budget counters, policy checks, commit boundaries.

### 8.2 Nondeterministic Effect Layer (recorded, not replayed)

Handles: LLM calls, HTTP requests, DB operations, browser tasks, sandboxed code, human tasks, object store I/O.

Execution flow:

```text
message arrives
  -> deterministic turn executes
  -> emits EffectRequest
  -> effect runs externally on reactor
  -> result recorded durably
  -> process resumes from recorded EffectResult
```

---

## 9. Behavior Trait

### 9.1 StepBehavior (turn-based, kernel-level)

```rust
// Implemented in core/behavior.rs
pub trait StepBehavior: Send + 'static {
    fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult;
    fn handle(&mut self, msg: Envelope, ctx: &mut TurnContext) -> StepResult;
    fn terminate(&mut self, reason: &Reason);
    fn snapshot(&self) -> Option<Vec<u8>> { None }
    fn restore(&mut self, _state: &[u8]) -> Result<(), String> { Ok(()) }
}
```

### 9.2 Behavior (async, legacy/convenience)

```rust
// Implemented in behavior.rs (top-level)
#[async_trait]
pub trait Behavior: Send + 'static {
    async fn init(&mut self, checkpoint: Option<Vec<u8>>) -> Result<(), Error>;
    async fn handle(&mut self, msg: Message) -> Action;
    async fn terminate(&mut self, reason: &Reason);
    fn checkpoint(&self) -> Option<Vec<u8>> { None }
    fn should_checkpoint(&self) -> bool { false }
}
```

The `StepBehavior` trait is the canonical kernel interface. `Behavior` is the higher-level async convenience for simple use cases.

### 9.3 StepResult

```rust
// Implemented in core/step_result.rs
pub enum StepResult {
    Continue,                   // Handled, may have more
    Wait,                       // Mailbox empty, park
    Suspend(EffectRequest),     // Request side effect, durable wait
    Done(Reason),               // Terminal normal
    Fail(Reason),               // Terminal abnormal
}
```

**Critical design:** `Suspend` takes `EffectRequest` (intent), not `Future` (opaque). This preserves journaling, replay, policy checks, budget interception, retries, and idempotency.

---

## 10. Effect System

Effects are explicit descriptions of side effects.

### 10.1 EffectRequest

```rust
// Implemented in core/effect.rs
pub struct EffectRequest {
    pub effect_id: EffectId,
    pub kind: EffectKind,
    pub input: serde_json::Value,
    pub timeout: Duration,
    pub retry: RetryPolicy,
    pub idempotency_key: Option<String>,
    pub compensation: Option<CompensationSpec>,
}
```

### 10.2 EffectKind

```rust
// Implemented
pub enum EffectKind {
    LlmCall,
    Http,
    DbQuery,
    DbWrite,
    CliExec,
    SandboxExec,
    BrowserAutomation,
    PublishEvent,
    HumanApproval,
    HumanInput,
    SleepUntil,
    Spawn,
    ObjectFetch,
    ObjectPut,
    VectorSearch,
    Custom(String),
}
```

### 10.3 EffectResult

```rust
// Implemented
pub struct EffectResult {
    pub effect_id: EffectId,
    pub status: EffectStatus,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}

pub enum EffectStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}
```

### 10.4 RetryPolicy

```rust
// Implemented
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: BackoffKind,
    pub retry_on: Vec<String>,    // error category filter
}

pub enum BackoffKind {
    Fixed(u64),
    Exponential { base_ms: u64, max_ms: u64 },
    ExponentialJitter { base_ms: u64, max_ms: u64 },
}
```

### 10.5 CompensationSpec

```rust
// Implemented
pub struct CompensationSpec {
    pub undo_kind: EffectKind,
    pub undo_input: serde_json::Value,
}
```

### 10.6 Effect Lifecycle

```text
Requested -> Dispatched -> Running -> Succeeded | Failed | TimedOut | Cancelled
```

**Target spec** adds: `Memoized`, `Compensated`.

### 10.7 LLM as First-Class Effect (target)

```text
LlmCallSpec {
    provider, model, prompt_ref, tools_allowed,
    temperature, max_tokens, schema,
    fallback_chain, cache_policy, budget_limit,
    deterministic_mode
}
```

**Current status:** LLM calls use generic `serde_json::Value` input. Typed `LlmCallSpec` is a future enhancement.

---

## 11. Journal Model

Every process has a durable journal.

### 11.1 Journal Entry Types

```rust
// Implemented in durability/journal.rs
pub enum JournalEntryType {
    ProcessSpawned,
    MessageReceived,
    TurnStarted,
    StatePatched,
    EffectRequested,
    EffectResultRecorded,
    MessageSent,
    TimerScheduled,
    TimerFired,
    ProcessSuspended,
    ProcessResumed,
    ProcessFailed,
    ProcessExited,
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
}
```

### 11.2 Journal Storage

SQLite-backed. Schema:

```sql
CREATE TABLE journal (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    pid INTEGER NOT NULL,
    turn_id INTEGER,
    entry_type TEXT NOT NULL,
    data BLOB,
    created_at INTEGER NOT NULL
);
```

### 11.3 Journal Purposes

Recovery, replay, audit, observability, debugging, cost attribution, lineage analysis.

**Target spec** adds: `CheckpointCreated`, `BudgetDebited`, `PolicyDecisionRecorded`, `ProcessMigrated`.

---

## 12. Snapshot Model

Replaying full history forever is too expensive. Each process gets periodic snapshots.

### 12.1 Snapshot Structure

```sql
CREATE TABLE snapshots (
    pid INTEGER NOT NULL,
    version INTEGER NOT NULL,
    state_blob BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (pid, version)
);
```

### 12.2 Snapshot Triggers

- Every N turns
- State delta threshold
- Before migration (future)
- Before long suspension
- On supervisor request

**Current status:** `SnapshotStore` implemented (SQLite). `StepBehavior::snapshot()` returns `Option<Vec<u8>>`.

---

## 13. Turn Commit Protocol

At the end of a successful turn, the runtime atomically commits:

- Consumed message ack
- State patch
- Journal entries
- Outbound messages
- Effect requests
- Timer registrations
- Budget debits (future)
- Policy decisions (future)

```rust
// Implemented in kernel/turn_executor.rs
pub struct TurnExecutor {
    journal: Journal,
    snapshot_store: SnapshotStore,
}
```

The turn executor writes journal entries and snapshots. Outbound messages and effects are routed by `SchedulerEngine`.

### 13.1 Turn Commit Invariant

A turn either fully commits or does not happen. If commit fails, the turn does not count as progressed.

**Current status:** Journal + snapshot writes are implemented. Full atomic commit (single transaction across all stores) is aspirational — currently best-effort sequential.

---

## 14. Recovery Model

On process crash, node loss, or restart:

```text
load latest snapshot
  -> replay journal tail after snapshot
  -> restore mailbox cursor
  -> restore active timers
  -> filter completed vs pending effects
  -> resume scheduling
```

### 14.1 Recovery Coordinator

```rust
// Implemented in kernel/recovery.rs
pub struct RecoveryCoordinator<'a> {
    journal: &'a Journal,
    snapshots: &'a SnapshotStore,
    timer_store: Option<&'a TimerStore>,
}
```

Methods: `recover_process(pid, behavior) -> Result<RecoveredProcess>`.

### 14.2 Recovery Invariant

Recovery is idempotent. Effects must never be blindly re-run unless explicitly allowed by policy and idempotency semantics.

---

## 15. Scheduler Design

The node-local scheduler replaces tokio-per-agent execution.

### 15.1 Why Not `tokio::spawn` Per Process

- Cooperative scheduling only, no fairness guarantees
- Weak runtime control, no reduction model
- No protection from mailbox starvation or CPU-heavy agents

### 15.2 Scheduler Model

```rust
// Implemented in kernel/scheduler.rs
pub struct SchedulerEngine {
    processes: HashMap<Pid, ProcessEntry>,
    run_queue: RunQueue,
    timer_wheel: TimerWheel,
    link_table: LinkTable,
    outbound_messages: Vec<Envelope>,
    outbound_effects: Vec<(Pid, EffectRequest)>,
    completed: Vec<(Pid, Reason)>,
    max_reductions: u32,
    clock_ms: u64,
    tick_count: u64,
}
```

### 15.3 Tick Cycle

```text
drain completions
  -> advance timer wheel, deliver fired timers
  -> pick next ready process from run queue
  -> mark Running
  -> execute one bounded turn (process.step())
  -> collect intents, route messages/effects
  -> handle exit propagation (links, monitors, parent)
  -> requeue if Continue, park if Wait/Suspend
  -> repeat
```

### 15.4 Reductions and Fairness

Each turn increments a reduction counter. After `max_reductions` (default 200), process is requeued. This is preemptive at turn granularity, not instruction-level.

The reduction budget protects against: mailbox flooding, tight Continue loops, pathological handlers, starvation.

**Caveat:** If a handler does heavy CPU inside a single `handle()` call, the scheduler cannot interrupt it. Heavy compute belongs in the effect worker plane.

### 15.5 Process State Transitions

```text
Ready -> Running -> Ready          (Continue, more messages)
Ready -> Running -> WaitingMessage (Wait, mailbox empty)
Ready -> Running -> WaitingEffect  (Suspend, effect dispatched)
Ready -> Running -> Done           (terminal)
Ready -> Running -> Failed         (terminal)
WaitingEffect -> Ready             (effect result delivered)
WaitingMessage -> Ready            (message delivered)
Suspended -> Ready                 (Resume signal)
```

### 15.6 Work Stealing (target)

When a scheduler queue is empty: drain local completions, check timer wakeups, steal work from another scheduler queue. Currently single-threaded scheduler; multi-thread + work stealing is Phase 3+.

---

## 16. Reactor Bridge

ZeptoVM uses a shared async reactor for I/O.

### 16.1 Architecture

```rust
// Implemented in kernel/reactor.rs
pub struct Reactor {
    dispatch_tx: Sender<EffectDispatch>,
    completion_rx: Receiver<EffectCompletion>,
    _handle: thread::JoinHandle<()>,
}
```

Background thread runs a Tokio multi-thread runtime. Effects are dispatched as Tokio tasks.

### 16.2 Process-to-Reactor Handoff

```text
process.handle(msg) -> Suspend(EffectRequest)
  -> turn commit
  -> effect dispatch to reactor
  -> reactor runs async work
  -> result returns via completion channel
  -> EffectResult message enqueued to process
  -> process Ready
  -> process.handle(EffectResult)
```

### 16.3 Retry Loop

```rust
// Implemented in reactor.rs
async fn execute_effect_with_retry(request: &EffectRequest) -> EffectResult {
    // Respects RetryPolicy: max_attempts, BackoffKind, retry_on filter
    // Succeeded -> return immediately
    // Failed + retryable -> backoff delay -> retry
    // TimedOut/Cancelled -> no retry
}
```

### 16.4 Reactor Benefits

Connection pooling, HTTP/2 reuse, TLS session reuse, large numbers of concurrent I/O ops.

---

## 17. Timer Subsystem

Durable timers are native runtime primitives.

### 17.1 Timer Types

```rust
// Implemented in core/timer.rs
pub struct TimerSpec {
    pub id: TimerId,
    pub owner: Pid,
    pub kind: TimerKind,
    pub deadline_ms: u64,
}

pub enum TimerKind {
    SleepUntil,
    Timeout,
    RetryBackoff,
}
```

**Target spec** adds: `Cron`, `LeaseRenewal`, `HumanReminder`.

### 17.2 Timer Wheel

```rust
// Implemented in kernel/timer_wheel.rs
pub struct TimerWheel {
    timers: BTreeMap<u64, Vec<TimerSpec>>,
}
```

`schedule()`, `cancel()`, `tick(now_ms)` -> Vec of fired timers. Wired into `SchedulerEngine::advance_clock()`.

### 17.3 Timer Persistence

```rust
// Implemented in durability/timer_store.rs
pub struct TimerStore { conn: Connection }
```

SQLite-backed. Used by `RecoveryCoordinator` to rehydrate timers after crash.

---

## 18. Supervision Model

Supervision is first-class, not an app-level convention.

### 18.1 Supervisor Types

```rust
// Implemented in core/supervisor.rs
pub struct SupervisorSpec {
    pub max_restarts: u32,
    pub restart_window_ms: u64,
    pub backoff: BackoffPolicy,
}

pub enum RestartStrategy {
    Permanent,      // Always restart
    Transient,      // Restart on abnormal exit only
    Temporary,      // Never restart
}

pub enum BackoffPolicy {
    Immediate,
    Fixed(u64),
    Exponential { base_ms: u64, max_ms: u64 },
}
```

**Target spec** adds: `OneForAll`, `RestForOne`, `EscalationPolicy`.

### 18.2 SupervisorBehavior

```rust
// Implemented in kernel/supervisor_behavior.rs
pub struct SupervisorBehavior {
    spec: SupervisorSpec,
    children: Vec<ChildEntry>,
    restart_log: VecDeque<u64>,
}
```

Handles `Signal::ChildExited` — restarts child or escalates (Fail with Shutdown) when `max_restarts` exceeded within window.

### 18.3 Exit Reasons

```rust
// Implemented in error.rs
pub enum Reason {
    Normal,
    Shutdown,
    Kill,
    Custom(String),
}
```

**Target spec** expands to: `BudgetExceeded`, `PolicyDenied`, `Timeout`, `Cancelled`, `NodeLost`, `DependencyFailed`, `Panic`.

---

## 19. Links and Monitors

### 19.1 Links (bidirectional failure coupling)

When a linked process exits abnormally, all linked processes receive `Signal::ExitLinked(pid, reason)`. Non-trapping processes die. Trapping processes (`trap_exit = true`) receive it as a message to handle.

### 19.2 Monitors (unidirectional observation)

When a monitored process exits, watchers receive `Signal::MonitorDown(pid, reason)`. Watchers are not killed.

```rust
// Implemented in link.rs
pub struct LinkTable {
    links: HashMap<Pid, HashSet<Pid>>,
    monitors: HashMap<Pid, Vec<MonitorEntry>>,
}
```

### 19.3 Exit Propagation

Wired into `SchedulerEngine`. On process exit:

1. Send `ExitLinked` to all linked processes
2. Send `MonitorDown` to all monitoring processes
3. Send `ChildExited` to parent (if any)

---

## 20. Budget Model

ZeptoVM is budget-aware by default.

### 20.1 Current Implementation

```rust
// kernel/runtime.rs
pub struct BudgetState {
    pub usd_remaining: f64,
    pub token_remaining: u64,
}

// control/budget.rs
pub struct BudgetGate {
    token_limit: Option<u64>,
    cost_limit_microdollars: Option<u64>,
    tokens_used: AtomicU64,
    cost_used: AtomicU64,
}
```

Budget is checked before LLM effect dispatch. Blocked effects get `EffectResult::failure("budget exceeded")`.

### 20.2 Target Budget State

```text
BudgetState {
    token_budget_remaining
    usd_micros_remaining
    api_quota_remaining
    max_parallel_children
    wall_clock_deadline_ms
    cpu_turn_quota
}
```

### 20.3 Budget Reactions (target)

When budget pressure rises, ZeptoVM can: block new effects, downgrade model tier, reduce parallelism, require human approval, pause subtree, escalate to supervisor.

---

## 21. Policy Engine

Every capability request passes through policy.

### 21.1 Policy Hook Points (target)

- Before effect dispatch
- Before child spawn
- Before external message
- Before object read/write
- Before network access
- Before model/provider selection

### 21.2 Policy Decisions (target)

```text
enum PolicyDecision {
    Allow,
    Deny,
    RequireApproval,
    Rewrite(Value),
    Downgrade(String),
    Sandbox,
}
```

**Current status:** `ProviderGate` and `AdmissionController` exist in `control/`. Full policy engine is Phase 5.

---

## 22. Observability

### 22.1 Metrics

```rust
// Implemented in kernel/metrics.rs
pub struct Metrics {
    counters: HashMap<&'static str, AtomicU64>,
    gauges: HashMap<&'static str, AtomicI64>,
}
```

Pre-registered counters: `processes.spawned`, `processes.exited`, `effects.dispatched`, `effects.completed`, `effects.failed`, `effects.retries`, `effects.timed_out`, `turns.committed`, `timers.scheduled`, `timers.fired`, `supervisor.restarts`, `compensation.triggered`, `budget.blocked`, `scheduler.ticks`.

Pre-registered gauges: `processes.active`, `mailbox.depth_total`.

### 22.2 Tracing

`tracing` crate spans on: scheduler tick, process step, effect dispatch, turn commit, runtime tick. Budget violations logged as warnings.

### 22.3 Target Observability (timeline)

For every process, the system should show: mailbox depth by lane, current state, supervisor tree, lineage, active timers, pending effects, restart count, token/dollar spend, provider choices, policy blocks, migration history, object refs.

---

## 23. Distributed Model

### 23.1 Cluster Architecture (target — Phase 3)

| Plane | Purpose |
|-------|---------|
| **Local runtime kernel** | Schedulers, run queues, process registry, mailbox, timer wheel, turn executor, reactor |
| **Durable state layer** | Journal, snapshots, mailbox cursor, timer persistence, recovery metadata |
| **Cluster control plane** | Membership (SWIM gossip), leases, shard ownership, rebalancing, failover |
| **Effect worker plane** | LLM pool, browser pool, sandbox pool, human task gateway |
| **Object/artifact plane** | Files, traces, outputs, refs |
| **Observability plane** | Metrics, traces, lineage, logs, spend analysis, replay UI |

### 23.2 Shards and Leases

- Each process belongs to a shard
- Each shard has an owner node
- Only the owner schedules those processes
- On node loss, lease expires, another node acquires and recovers

### 23.3 Routing

Messages addressed by PID, not by node. Router resolves owning shard -> active lease holder -> target node.

Delivery model: at-least-once with recipient-side dedup.

### 23.4 Placement

```text
PlacementInfo {
    current_node, shard_key, affinity, anti_affinity,
    data_locality_refs, gpu_required, compliance_zone,
    tenant_class, sticky_hint
}
```

Placement considers: node load, tenant affinity, data locality, GPU/sandbox proximity, compliance region.

### 23.5 Process Migration (target — Phase 4)

Restartable migration: checkpoint -> transfer state -> redirect routing -> resume on destination. Use cases: node drain, data locality shift, GPU affinity, maintenance.

**Current status:** Design exists at `docs/plans/2026-03-06-multi-node-clustering-design.md`. No implementation yet.

---

## 24. Object Plane

Mailboxes should carry references, not giant blobs.

### 24.1 ObjectRef (target — Phase 4)

```text
ObjectRef {
    object_id, content_hash, media_type, size_bytes,
    storage_class, encryption_scope, created_at, ttl
}
```

Large artifacts (screenshots, PDFs, browser traces, embeddings, long transcripts) live in an object/artifact plane. Messages pass `ObjectRef`, not huge payloads.

**Current status:** Not implemented. Deferred to Phase 4.

---

## 25. Human-in-the-Loop

Human interaction is native, not bolted on.

```text
HumanApprovalSpec {
    task_id, assigned_to, title, description,
    context_refs, deadline, reminder_policy
}
```

These are effects (`EffectKind::HumanApproval`, `EffectKind::HumanInput`). The process waits durably until the human result arrives.

**Current status:** EffectKind variants exist. No UI or gateway implementation yet. Phase 5.

---

## 26. Behavior Versioning

Long-lived processes require code/version management.

```text
BehaviorRef {
    module, version, checksum, migration_fn
}
```

When restoring older processes: either run the old behavior version, or migrate checkpointed state via migration function.

**Current status:** Not implemented. Phase 5.

---

## 27. Strong Invariants

These invariants govern ZeptoVM correctness.

### Invariant 1: Turn Atomicity
A process turn either fully commits or does not happen.

### Invariant 2: Effect Journaling
No side effect is durable unless recorded as an effect request/result in the journal.

### Invariant 3: Nondeterminism Boundary
A process observes nondeterministic input only through recorded effect results or runtime signals.

### Invariant 4: Recovery Completeness
Every waiting state is recoverable from snapshot + journal + timers + pending effects.

### Invariant 5: Fault Ownership
Every process has a fault owner: supervisor or system root.

### Invariant 6: Payload Size (target)
Large payloads flow through ObjectRef, not mailbox blobs.

### Invariant 7: Pre-dispatch Checks
Budget and policy checks happen before expensive or risky effects are dispatched.

### Invariant 8: Single Owner (target)
Only the current shard lease owner schedules a process.

---

## 28. Module Map

### Current crate layout (`zeptovm/src/`)

```text
lib.rs                          # Crate root, re-exports

# Top-level modules (v0 async API)
behavior.rs                     # async Behavior trait
error.rs                        # Reason, Action, Message, SystemMsg
link.rs                         # LinkTable (bidirectional links + monitors)
mailbox.rs                      # Tokio channel-based mailbox (v0 async)
pid.rs                          # Pid(u64)
process.rs                      # spawn_process (tokio::spawn based, v0)
registry.rs                     # Process registry
supervisor.rs                   # Async supervisor (v0)

# core/ — turn-based kernel types
core/behavior.rs                # StepBehavior trait
core/effect.rs                  # EffectId, EffectKind, EffectRequest, EffectResult,
                                # RetryPolicy, BackoffKind, CompensationSpec
core/message.rs                 # Envelope, MessageClass, Signal, Priority
core/step_result.rs             # StepResult enum
core/supervisor.rs              # SupervisorSpec, ChildSpec, RestartStrategy, BackoffPolicy
core/timer.rs                   # TimerId, TimerSpec, TimerKind
core/turn_context.rs            # TurnContext, TurnIntent, TurnId

# kernel/ — scheduler and runtime engine
kernel/compensation.rs          # CompensationLog (saga-style rollback)
kernel/mailbox.rs               # MultiLaneMailbox
kernel/metrics.rs               # Metrics (atomic counters + gauges)
kernel/process_table.rs         # ProcessEntry, ProcessRuntimeState
kernel/reactor.rs               # Reactor (tokio bridge), execute_effect_with_retry
kernel/recovery.rs              # RecoveryCoordinator
kernel/run_queue.rs             # RunQueue (VecDeque)
kernel/runtime.rs               # SchedulerRuntime (top-level orchestrator)
kernel/scheduler.rs             # SchedulerEngine (tick loop, intent routing,
                                #   timer wheel, link table, exit propagation)
kernel/supervisor_behavior.rs   # SupervisorBehavior (OneForOne)
kernel/timer_wheel.rs           # TimerWheel (BTreeMap-based)
kernel/turn_executor.rs         # TurnExecutor (journal + snapshot persistence)

# durability/ — persistence layer
durability/checkpoint.rs        # CheckpointStore trait, SqliteCheckpointStore
durability/idempotency.rs       # IdempotencyStore (SQLite)
durability/journal.rs           # Journal, JournalEntryType
durability/recovery.rs          # encode/decode_checkpoint, build_recovery_plan
durability/snapshot.rs          # SnapshotStore (SQLite)
durability/timer_store.rs       # TimerStore (SQLite)
durability/wal.rs               # SqliteWalStore, WalWriter

# control/ — admission, budget, provider gating
control/admission.rs            # AdmissionController
control/budget.rs               # BudgetGate (atomic token/cost limits)
control/provider_gate.rs        # ProviderGate
```

### Target layout (future phases)

```text
cluster/                        # Phase 3
  membership.rs
  lease.rs
  shard.rs
  routing.rs
  placement.rs
  migration.rs

workers/                        # Phase 2b/3
  dispatcher.rs
  llm.rs
  http.rs
  browser.rs
  sandbox.rs
  human.rs

objects/                        # Phase 4
  store.rs
  cache.rs
  refs.rs

observability/                  # Phase 5
  lineage.rs
  replay.rs
  audit.rs
```

### Integration test files (`zeptovm/tests/`)

```text
v0_gate.rs                      # v0 async process tests
v1_gate.rs                      # Supervision gate tests
v1_exit_signals.rs              # Link/monitor exit signal tests
v2_gate.rs                      # Recovery/checkpoint gate tests
phase1_gate.rs                  # Phase 1 integration gate
phase2_gate.rs                  # Phase 2 integration gate (timers, supervision,
                                #   compensation, metrics, idempotency)
```

---

## 29. Implementation Status

### Phase 1 — Single-node durable runtime (COMPLETE)

| Component | Files | Tests |
|-----------|-------|-------|
| Pid, Reason, Message types | `pid.rs`, `error.rs`, `core/message.rs` | ~15 |
| StepBehavior + StepResult | `core/behavior.rs`, `core/step_result.rs` | ~8 |
| TurnContext + TurnIntent | `core/turn_context.rs` | ~6 |
| EffectRequest/Result | `core/effect.rs` | ~12 |
| MultiLaneMailbox | `kernel/mailbox.rs` | ~8 |
| ProcessEntry | `kernel/process_table.rs` | ~12 |
| RunQueue | `kernel/run_queue.rs` | ~5 |
| SchedulerEngine | `kernel/scheduler.rs` | ~20 |
| Reactor | `kernel/reactor.rs` | ~7 |
| TurnExecutor | `kernel/turn_executor.rs` | ~5 |
| SchedulerRuntime | `kernel/runtime.rs` | ~15 |
| Journal (SQLite) | `durability/journal.rs` | ~6 |
| SnapshotStore (SQLite) | `durability/snapshot.rs` | ~5 |
| WAL | `durability/wal.rs` | ~5 |
| CheckpointStore | `durability/checkpoint.rs` | ~5 |
| LinkTable | `link.rs` | ~8 |
| BudgetGate | `control/budget.rs` | ~7 |
| AdmissionController | `control/admission.rs` | ~5 |
| ProviderGate | `control/provider_gate.rs` | ~5 |

### Phase 2 — Effect plane hardening (COMPLETE)

| Component | Files | Tests |
|-----------|-------|-------|
| Timer types | `core/timer.rs` | ~5 |
| TimerWheel | `kernel/timer_wheel.rs` | ~7 |
| TimerStore (SQLite) | `durability/timer_store.rs` | ~6 |
| Timers wired into scheduler | `kernel/scheduler.rs` | ~2 |
| SupervisorSpec types | `core/supervisor.rs` | ~6 |
| ProcessEntry parent/trap_exit | `kernel/process_table.rs` | ~4 |
| SupervisorBehavior | `kernel/supervisor_behavior.rs` | ~7 |
| LinkTable wired into scheduler | `kernel/scheduler.rs` | ~3 |
| Journal entry types expanded | `durability/journal.rs` | ~0 |
| RecoveryCoordinator | `kernel/recovery.rs` | ~4 |
| RetryPolicy + BackoffKind | `core/effect.rs` | ~5 |
| Reactor retry loop | `kernel/reactor.rs` | ~2 |
| IdempotencyStore | `durability/idempotency.rs` | ~5 |
| CompensationLog | `kernel/compensation.rs` | ~6 |
| Metrics | `kernel/metrics.rs` | ~7 |
| Tracing instrumentation | scheduler, reactor, turn_executor, runtime | ~0 |
| Metrics wired into runtime | `kernel/runtime.rs` | ~4 |

**Total tests:** 264 (243 unit + 21 integration), all passing.

---

## 30. Phased Roadmap

### Phase 1 — Single-node durable runtime (DONE)

Processes, multi-lane mailbox, turn executor, async-aware scheduler, effect requests/results, snapshots + journal, budget basics.

### Phase 2 — Effect plane hardening (DONE)

Timers (types, wheel, SQLite store, scheduler wiring), supervision (types, behavior, OneForOne), links/monitors (wired into scheduler with exit propagation), recovery coordinator, retries with BackoffKind, idempotency store, compensation log, metrics, tracing.

### Phase 3 — Multi-node cluster (NEXT)

Membership (SWIM gossip), shard leases, remote PID routing, node failover recovery, placement metadata. Design exists at `docs/plans/2026-03-06-multi-node-clustering-design.md`.

### Phase 4 — Artifact and migration

Object plane (ObjectRef, store, cache), restartable process migration, richer placement, locality-aware scheduling.

### Phase 5 — Enterprise / runtime intelligence

Advanced policy engine, human approval UI/gateway, behavior versioning + migration, cost optimizer, model/provider routing, replay and lineage UI.

---

## Appendix A: The 7 Laws of ZeptoVM

1. **Every process is isolated.**
2. **Every step is bounded.**
3. **Every side effect is explicit.**
4. **Every wait is durable.**
5. **Every failure has a supervisor.**
6. **Every expensive action is budgeted and policy-checked.**
7. **Every run is inspectable and replayable.**

---

## Appendix B: What ZeptoVM Is Not

- A general-purpose shared-memory runtime
- A transparent distributed transaction engine
- A VM for arbitrary heavy compute
- A replacement for GPU inference engines
- A magic exactly-once-everywhere fabric

Heavy compute belongs in worker/effect planes. The runtime's job is **orchestration, durability, supervision, and controlled execution**.

---

## Appendix C: Ecosystem Split

| Layer | Role |
|-------|------|
| **ZeptoVM** | Runtime kernel and execution substrate |
| **ZeptoClaw** | Reasoning and agent behavior layer on top of ZeptoVM |
| **r8r** | Workflow/product orchestration from ZeptoVM primitives |
| **SEA-MCP** | Capability and integration surface |
| **Worker pools** | Execution plane for LLM, browser, sandbox, CLI, human |
