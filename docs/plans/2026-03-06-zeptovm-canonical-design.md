# ZeptoVM — Canonical Design Document

## A durable distributed runtime for AI agents

> **ZeptoVM is a distributed actor-workflow runtime where AI agents execute as durable supervised processes, stepped by a BEAM-inspired async-aware scheduler, around explicit journaled effects.**

This is the fusion of:
- **BEAM** — processes, mailboxes, supervision, fair scheduling
- **Temporal** — durable waits, replay, timers, recovery
- **Ray** — distributed placement, resource-aware execution, cluster routing

---

## Design Goals

ZeptoVM is designed for AI agents that are long-lived, mostly I/O-bound, stateful, failure-prone, partially nondeterministic, expensive to run, sensitive to policy/security/budget, and often composed of sub-agents, tools, and human approvals.

The runtime optimizes for:

1. Cheap concurrency
2. Fair execution
3. Durable waiting
4. Failure containment
5. Explicit side effects
6. Replayability
7. Budget awareness
8. Policy enforcement
9. Distributed placement
10. Deep observability

---

## Core Primitives

7 first-class runtime primitives:

1. **Process** — durable isolated agent state machine
2. **Message** — typed envelope with class, priority, correlation
3. **Turn** — bounded execution step with atomic commit
4. **Effect** — explicit description of a side effect
5. **Journal** — append-only log of turn events
6. **Supervisor** — fault owner governing restart/escalation
7. **ObjectRef** — reference to large artifacts in object plane

---

## Process Model

### Identity

```
Pid { tenant_id, namespace, process_id, incarnation }
```

### Kinds

```
enum ProcessKind {
  Agent, Workflow, Supervisor, Adapter, System, HumanWait, TaskWorker
}
```

### Runtime States

```
enum ProcessRuntimeState {
    Ready,
    Running,
    WaitingMessage,
    WaitingEffect(EffectId),
    WaitingTimer(TimerId),
    Checkpointing,
    Suspended,
    Failed(Reason),
    Done(Reason),
}
```

---

## Message Model

### Envelope

```
MessageEnvelope {
  msg_id, correlation_id, causation_id,
  from, to, class, priority,
  dedup_key, ttl, created_at, payload
}
```

### Classes

```
enum MessageClass {
  Control, Supervisor, User, EffectResult, Timer, System, Signal
}
```

---

## Multi-Lane Mailbox

Each process has a logical mailbox with multiple internal lanes:

```
control > supervisor > effect > user > background
```

Features: durable enqueue, dequeue cursor, per-message TTL, dedup keys, coalescing, bounded thresholds, selective receive.

---

## Turn Model

A turn is a bounded execution step:

```
load snapshot
-> replay journal tail
-> dequeue next eligible message
-> execute behavior step
-> collect intents
-> run policy + budget checks
-> persist journal entries + state delta
-> publish outbound messages/effects/timers
-> ack consumed message
-> yield
```

### Turn Commit Protocol (atomic)

```
BEGIN TURN COMMIT
  persist StateDelta
  persist JournalEntries
  persist OutboundMessages
  persist EffectRequests
  persist TimerEntries
  persist BudgetChanges
  persist AckCursor
COMMIT
publish async work externally
```

---

## Deterministic Shell, Nondeterministic Effects

### Deterministic orchestration layer (replayable)

Mailbox progression, state transitions, timers, retries, spawn decisions, supervisor semantics, budget counters, policy checks, commit boundaries.

### Nondeterministic effect layer (journaled)

LLM calls, HTTP, DB, browser tasks, sandboxed code, human tasks, object store, provider calls.

```
message arrives
-> deterministic turn executes
-> emits EffectRequest
-> effect runs externally
-> result recorded durably
-> process resumes from recorded EffectResult
```

---

## Programming Model

### StepResult

```rust
enum StepResult {
    Continue,
    Wait,
    Suspend(EffectRequest),
    Done(Reason),
    Fail(Reason),
}
```

Important: `Suspend(EffectRequest)`, not `Suspend(Future)`. The process emits effect intent, not opaque futures. This preserves journaling, replay, policy checks, budget interception, idempotency, retries.

### Behavior Trait

```rust
pub trait Behavior: Send + 'static {
    fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult;
    fn handle(&mut self, msg: Message) -> StepResult;
    fn terminate(&mut self, reason: &Reason);
}
```

---

## Effect System

### EffectRequest

```
EffectRequest {
  effect_id, process_id, kind, input,
  policy, timeout, retry, compensation,
  idempotency_key, created_at
}
```

### EffectKind

```
enum EffectKind {
  LlmCall, Http, DbQuery, DbWrite, CliExec, SandboxExec,
  BrowserAutomation, PublishEvent, HumanApproval, HumanInput,
  SleepUntil, Spawn, ObjectFetch, ObjectPut, VectorSearch, Custom(String)
}
```

### Effect Lifecycle

```
Requested -> Dispatched -> Running -> Succeeded/Failed/TimedOut/Cancelled/Compensated/Memoized
```

---

## LLM as First-Class Effect

```
LlmCallSpec {
  provider, model, prompt_ref, tools_allowed,
  temperature, max_tokens, schema, fallback_chain,
  cache_policy, budget_limit, deterministic_mode
}
```

---

## Journal Model

```
enum JournalEntry {
  ProcessSpawned, MessageReceived, TurnStarted, StatePatched,
  EffectRequested, EffectResultRecorded, MessageSent,
  TimerScheduled, TimerFired, ChildSpawned, LinkCreated,
  MonitorCreated, BudgetDebited, PolicyDecisionRecorded,
  CheckpointCreated, ProcessSuspended, ProcessResumed,
  ProcessFailed, ProcessExited, ProcessMigrated
}
```

---

## Snapshot Model

```
Snapshot {
  pid, snapshot_version, state_blob_ref,
  mailbox_cursor, active_timers, children,
  pending_effects, budget_state, taken_at
}
```

Triggers: every N turns, state delta threshold, before migration, before long suspension, on supervisor request, before node drain.

---

## Recovery

```
load latest snapshot
-> replay journal tail
-> restore mailbox cursor
-> restore active timers
-> restore pending effects
-> restore budget state
-> reacquire process/shard lease
-> resume scheduling
```

Recovery must be idempotent. Effects never blindly re-run.

---

## Scheduler

N scheduler threads, one per CPU core. Each owns a local run queue. Work stealing when idle. Scheduler collapse when few threads have work.

Reductions: one message-handling step = 1 reduction. After configurable limit (default 200), process is requeued. Honest claim: preemptive at process-turn granularity, not instruction-level.

---

## Supervision

Strategies: OneForOne, OneForAll, RestForOne, Never.

Exit reasons: Normal, Error, BudgetExceeded, PolicyDenied, Timeout, Cancelled, NodeLost, DependencyFailed, Panic.

Supervisors are themselves processes.

---

## Budget Model

```
BudgetState {
  token_budget_remaining, usd_budget_remaining,
  api_quota_remaining, max_parallel_children,
  wall_clock_deadline, cpu_turn_quota
}
```

Checked before: LLM calls, large fan-out, expensive browser tasks, spawning children, provider selection, retries.

---

## Policy Engine

Hook points: before effect dispatch, child spawn, external message, object read/write, network access, model/provider selection.

Decisions: Allow, Deny, RequireApproval, Rewrite, Downgrade, Sandbox.

---

## Strong Invariants

1. A process turn either fully commits or does not happen.
2. No side effect is durable unless recorded as effect request/result in journal.
3. A process observes nondeterministic input only through recorded effect results.
4. Every waiting state is recoverable from snapshot + journal + timers + pending effects.
5. Every process has a fault owner: supervisor or system root.
6. Large payloads flow through ObjectRef, not mailbox blobs.
7. Budget and policy checks happen before expensive effects are dispatched.
8. Only the current shard lease owner schedules a process.

---

## Phased Implementation

| Phase | Scope |
|-------|-------|
| 1 | Single-node durable runtime: process model, multi-lane mailbox, turn executor, scheduler, effects, journal, snapshots, timers, supervision, budget/policy basics |
| 2 | Effect plane hardening: LLM-native effects, HTTP/DB/browser workers, idempotency, retries, compensation, observability |
| 3 | Multi-node cluster: membership, shard leases, remote routing, failover |
| 4 | Artifact and migration: object plane, restartable migration, locality-aware scheduling |
| 5 | Enterprise: advanced policy, human approvals, behavior versioning, cost optimizer, replay UI |

---

## Module Structure

```
zeptovm/
  core/       pid, message, process, behavior, step_result, effect, timer, object_ref, budget, policy
  kernel/     scheduler, run_queue, mailbox, process_table, turn_executor, supervisor, watchdog, reactor
  durability/ journal, snapshot, wal, recovery, checkpoint
  cluster/    membership, lease, shard, routing, placement, migration
  workers/    dispatcher, llm, http, browser, sandbox, human, vector
  objects/    store, cache, refs
  observability/ metrics, tracing, lineage, replay, audit
  sdk/        rust, ts, python
```

---

## Ecosystem Split

- **ZeptoVM** — runtime kernel and execution substrate
- **ZeptoClaw** — reasoning and agent behavior layer on top
- **r8r** — workflow/product orchestration from ZeptoVM primitives
- **SEA-MCP** — capability and integration surface
- **worker pools** — execution plane for LLM, browser, sandbox, CLI, human

---

## The 7 Laws

1. Every process is isolated.
2. Every step is bounded.
3. Every side effect is explicit.
4. Every wait is durable.
5. Every failure has a supervisor.
6. Every expensive action is budgeted and policy-checked.
7. Every run is inspectable and replayable.
