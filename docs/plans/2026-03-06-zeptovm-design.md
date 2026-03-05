# ZeptoVM Design Document

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:writing-plans to create the implementation plan from this design.

**Goal:** Build an async-native process runtime purpose-built for AI agents — combining Erlang/OTP supervision trees, Temporal-style durable execution, and native LLM orchestration. This is the execution engine for the ZeptoClaw AI agent framework.

**Philosophy:** Erlang's "let it crash" applied to AI agents — processes don't handle errors defensively, they crash and supervisors restart them with clean state. Three pillars: (1) no defensive coding inside agent logic, (2) failure isolation via process boundaries, (3) hierarchical recovery via supervision trees.

**Deployment Priority:** (A) Single-node daemon first, (C) Embeddable library "PM2 for AI agents" second, (B) Multi-node cluster later.

**Trust Domain:** Single-tenant. All processes in one ZeptoVM instance share trust. Multi-tenant deferred.

---

## 1. Core Architecture

### 1.1 Async-Native Process Model

Each agent is a tokio task. No sync scheduler thread. No bridge pattern.

```rust
use async_trait::async_trait;

#[async_trait]
pub trait Behavior: Send + 'static {
    /// Called once at spawn. checkpoint is Some if recovering from a prior run.
    async fn init(&mut self, checkpoint: Option<Vec<u8>>) -> Result<()>;

    /// Called for each user message. Returns an Action (continue, stop, checkpoint).
    async fn handle(&mut self, msg: Message) -> Action;

    /// Called when the process is terminating. Cleanup hook.
    async fn terminate(&mut self, reason: &Reason);

    /// Opt-in: return true if the runtime should call checkpoint() after this handle().
    fn should_checkpoint(&self) -> bool { false }

    /// Opt-in: serialize process state for durable recovery.
    fn checkpoint(&self) -> Option<Vec<u8>> { None }
}
```

`async_trait` crate is used for object-safe `dyn Behavior`.

### 1.2 Resource Vector (replaces CPU reductions)

CPU reductions are irrelevant for I/O-bound AI agents. The real scheduling resources are:

| Resource | Unit | Scope | Default |
|----------|------|-------|---------|
| Token budget | tokens | per-agent | unlimited |
| Cost budget | microdollars | per-agent | unlimited |
| In-flight LLM slots | count | global | 32 |
| Provider RPM/TPM | requests/tokens per min | per-provider | provider default |
| Mailbox depth | messages | per-agent | 1024 |
| Wall-clock timeout | ms | per-handle() | 300_000 |

If no budget is configured, the agent runs unconstrained (precheck returns true, commit is a no-op).

### 1.3 Two-Channel Mailbox + Out-of-Band Kill

```rust
struct ProcessMailbox {
    user_rx: mpsc::Receiver<Message>,        // bounded, configurable (default 1024)
    control_rx: mpsc::Receiver<SystemMsg>,    // bounded, capacity 32
}

/// Out-of-band kill signal — never queue-based.
/// CancellationToken is checked in the select! loop and cannot be blocked
/// by a stalled process, full queue, or slow async operation.
struct ProcessHandle {
    kill_token: CancellationToken,            // from tokio_util
    user_tx: mpsc::Sender<Message>,
    control_tx: mpsc::Sender<SystemMsg>,
}
```

**Why out-of-band Kill:** Even a dedicated critical channel with capacity 4 can fail if the producer can't enqueue while the process is stalled. `CancellationToken` is cooperative cancellation — it sets an atomic flag that `tokio::select!` polls every iteration, guaranteeing delivery regardless of queue state.

```rust
enum SystemMsg {
    ExitLinked(Pid, Reason),   // linked process died
    MonitorDown(Pid, Reason),  // monitored process died
    Suspend,
    Resume,
    GetState(oneshot::Sender<ProcessInfo>),
}
```

### 1.4 Process Run Loop

```rust
loop {
    tokio::select! {
        biased;

        // Out-of-band kill: always wins, never blocked
        _ = kill_token.cancelled() => {
            behavior.terminate(&Reason::Kill).await;
            break;
        }

        // Control messages: exit signals, suspend, monitor-down
        Some(sys) = control_rx.recv() => {
            match sys {
                SystemMsg::ExitLinked(from, reason) => {
                    if !trap_exit {
                        exit_reason = reason;
                        break;
                    }
                    // Convert to user message if trapping
                    let _ = user_tx_internal.try_send(
                        Message::system_exit(from, reason)
                    );
                }
                SystemMsg::MonitorDown(from, reason) => {
                    let _ = user_tx_internal.try_send(
                        Message::monitor_down(from, reason)
                    );
                }
                SystemMsg::Suspend => { /* pause user_rx polling */ }
                SystemMsg::Resume => { /* resume user_rx polling */ }
                SystemMsg::GetState(tx) => { let _ = tx.send(get_info()); }
            }
        }

        // User messages: normal agent work
        Some(msg) = user_rx.recv(), if !suspended => {
            match behavior.handle(msg).await {
                Action::Continue => {}
                Action::Stop(reason) => {
                    exit_reason = reason;
                    break;
                }
                Action::Checkpoint => {
                    if let Some(data) = behavior.checkpoint() {
                        checkpoint_store.save(pid, &data).await;
                    }
                }
            }
        }

        // All senders dropped
        else => break,
    }
}
```

### 1.5 Control Channel Overflow Policy

When `control_tx` is full (32 capacity):
- `ExitLinked` with `reason = normal`: drop silently (benign)
- `ExitLinked` with `reason != normal`: escalate to supervisor immediately (don't wait for delivery)
- `MonitorDown`: drop, supervisor already knows
- `Suspend/Resume/GetState`: return `Err` to caller, they retry or escalate

---

## 2. Process Lifecycle

### 2.1 States

```
Spawning -> Running -> (Suspended) -> Exiting -> Dead
                |                        ^
                +--- crash --------------+
```

### 2.2 Spawn Sequence

1. Allocate `Pid` (atomic u64 counter)
2. Create channels (user_rx/tx, control_rx/tx, CancellationToken)
3. Register in `ProcessRegistry` (DashMap<Pid, ProcessHandle>)
4. Spawn tokio task: `init()` -> run loop
5. If `auto_start` and checkpoint exists: pass `Some(checkpoint)` to `init()`
6. Notify supervisor of successful start

### 2.3 Link/Monitor Semantics

**Links** are bidirectional. When process A dies, all linked processes receive `ExitLinked(A, reason)`.

**Monitors** are unidirectional. When monitored process dies, monitor receives `MonitorDown`.

**Exit Signal Truth Table (Links):**

| Dying reason | Receiver trap_exit? | Result |
|-------------|-------------------|--------|
| normal | false | ignore (link removed) |
| normal | true | deliver as message |
| abnormal | false | receiver dies with same reason |
| abnormal | true | deliver as message |
| kill (CancellationToken) | either | receiver dies with `killed` (unstoppable) |

### 2.4 Supervision Trees

```rust
enum RestartStrategy {
    OneForOne,    // restart only the crashed child
    OneForAll,    // restart all children if one crashes
    RestForOne,   // restart crashed child and all children started after it
}

struct SupervisorSpec {
    strategy: RestartStrategy,
    max_restarts: u32,          // e.g., 5
    restart_window: Duration,   // e.g., 60s
    children: Vec<ChildSpec>,
}

struct ChildSpec {
    id: String,
    behavior_factory: Box<dyn Fn() -> Box<dyn Behavior> + Send>,
    restart: RestartPolicy,     // Always | OnFailure | Never
    shutdown: ShutdownPolicy,   // Timeout(Duration) | Brutal
}
```

When `max_restarts` exceeded within `restart_window`, supervisor itself crashes (escalates to its parent).

---

## 3. Control Plane

### 3.1 Gate Pipeline

```
User message arrives
    |
    v
[Budget Precheck] -- fail --> backpressure / drop
    |
    pass
    v
[Admission Gate] -- reject --> queue or shed
    |
    admit
    v
[Provider Gate] -- wait --> RPM/TPM semaphore
    |
    acquire slot
    v
[LLM Call] (async, with wall-clock timeout)
    |
    response
    v
[Budget Commit] -- record actual tokens + cost
    |
    v
Return to process
```

### 3.2 Budget Gate (Two-Phase)

```rust
struct BudgetGate {
    token_limit: Option<u64>,
    cost_limit_microdollars: Option<u64>,
    tokens_used: AtomicU64,
    cost_used: AtomicU64,
}

impl BudgetGate {
    /// Phase 1: cheap check before queueing at provider gate.
    /// Prevents exhausted agents from occupying gate queues.
    fn precheck(&self, estimated_tokens: u64) -> bool {
        match self.token_limit {
            None => true,
            Some(limit) => self.tokens_used.load(Relaxed) + estimated_tokens <= limit,
        }
    }

    /// Phase 2: record actual usage after LLM response.
    fn commit(&self, actual_tokens: u64, cost_microdollars: u64) {
        self.tokens_used.fetch_add(actual_tokens, Relaxed);
        self.cost_used.fetch_add(cost_microdollars, Relaxed);
    }
}
```

### 3.3 Provider Gate

Per-provider semaphore enforcing RPM/TPM limits:

```rust
struct ProviderGate {
    rpm_semaphore: Semaphore,        // permits = max RPM
    tpm_budget: AtomicU64,           // refilled every minute
    in_flight: AtomicU32,            // current in-flight requests
    max_in_flight: u32,              // global cap (default 32)
}
```

### 3.4 Admission Control

Weighted fair queuing for priority:

| Priority | Weight | Meaning |
|----------|--------|---------|
| High | 4 | 4x scheduling share |
| Normal | 2 | default |
| Low | 1 | background/batch |

Starvation prevention: Low priority promoted to Normal after 60s waiting.

### 3.5 Wall-Clock Timeout

Each `handle()` call wrapped in `tokio::time::timeout`. On timeout:
1. Cancel the in-flight LLM request
2. Return `Action::Stop(Reason::Timeout)` to the process
3. Supervisor decides whether to restart

---

## 4. Durability

### 4.1 Four Levels

| Level | Name | Mechanism | Use Case |
|-------|------|-----------|----------|
| 0 | None | In-memory only | Dev, stateless agents |
| 1 | Checkpoint | Periodic state snapshot | Conversational agents |
| 2 | Durable Mailbox | WAL for messages | Exactly-once processing |
| 3 | Swarm Checkpoint | DAG step state + WAL | Multi-agent workflows |

Default is Level 0. Levels are opt-in per agent via config.

### 4.2 Checkpoint Store

```rust
#[async_trait]
trait CheckpointStore: Send + Sync {
    async fn save(&self, pid: Pid, data: &[u8]) -> Result<()>;
    async fn load(&self, pid: Pid) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, pid: Pid) -> Result<()>;
}
```

Implementations: `SqliteCheckpointStore` (single-node), future: S3, Postgres.

### 4.3 Write-Ahead Log (Level 2+)

```rust
struct WalEntry {
    wal_seq: u64,           // global monotonic, assigned by WAL writer
                            // used as replay cursor and ack position
    message_id: u128,       // globally unique (UUID v7), for consumer idempotency
    sender: Pid,
    sender_seq: u64,        // per-sender monotonic, diagnostic only (debugging, ordering verification)
    recipient: Pid,
    payload: Vec<u8>,
    timestamp: u64,
}
```

**Key invariant:** `wal_seq` is the single source of truth for replay and acknowledgment. Recovery replays from `last_acked_wal_seq + 1`. `sender_seq` is never used for cursor positioning.

WAL is append-only. Compaction removes entries below the minimum `last_acked_wal_seq` across all live processes.

### 4.4 Recovery Sequence (10 Steps)

On daemon startup or process restart:

1. Open WAL, read high watermark (`max_wal_seq`)
2. Load checkpoint store index
3. For each managed agent with `auto_start = true`:
   a. Load latest checkpoint (if exists)
   b. Read `last_acked_wal_seq` from checkpoint metadata
   c. Replay WAL entries where `wal_seq > last_acked_wal_seq` AND `recipient = this_pid`
   d. Deduplicate by `message_id` (idempotency set)
4. Spawn process with `init(Some(checkpoint))`
5. Deliver replayed messages in `wal_seq` order
6. Resume normal message delivery
7. Re-establish links and monitors from supervisor spec
8. Run supervisor restart-count validation (don't count recovery as restarts)
9. Emit recovery metrics (messages replayed, time elapsed)
10. Mark process as `Running`

**Precedence:** Checkpoint state wins over WAL replay for process state. WAL replay only re-delivers messages that arrived *after* the checkpoint.

---

## 5. Config Contract

### 5.1 Ownership Boundary

- **ZeptoClaw** defines WHAT an agent is: model, system prompt, tools, provider, channels, safety policy
- **ZeptoBeam** defines HOW agents run: supervision, restart policy, mailbox, scheduling, orchestration

ZeptoBeam never redefines model/prompt/tools. It uses `agent_ref` to point into ZeptoClaw config, with optional `overrides` for deltas.

### 5.2 Merge Algorithm

For each `managed_agents[i]`:

1. Resolve `agent_ref` in ZeptoClaw config
2. Build base config from referenced agent
3. Apply overrides in fixed order:
   - Scalar overrides (model, system_prompt, numeric timeouts)
   - Tool delta (tools_add, then tools_remove)
4. Apply runtime metadata (restart, priority, mailbox, links)
5. Normalize and sort tool list for deterministic hash
6. Compute `effective_config_hash = sha256(canonical_json(effective_config))`
7. Store hash in process metadata/checkpoint

### 5.3 Startup Validation (Fail Fast)

Must fail on:
1. Config version mismatch
2. Duplicate managed_agents.name
3. Unknown agent_ref or channel_ref
4. Unknown tool in tools_add/tools_remove
5. Missing provider for resolved agent
6. Invalid enum values
7. Invalid restart limits
8. Swarm step references unknown agent/depends_on
9. Cycles in DAG swarms
10. Schema version unsupported

Full contract: `docs/reference/zeptoclaw-zeptobeam-config-contract.md`

---

## 6. Module Structure

### 6.1 Crate Layout

```
zeptovm/                          # Agent-agnostic process runtime
  src/
    lib.rs                        # Public API surface
    pid.rs                        # Pid type (atomic u64 counter)
    process.rs                    # Process struct, run loop, Behavior trait
    mailbox.rs                    # Two-channel mailbox + CancellationToken
    registry.rs                   # ProcessRegistry (DashMap<Pid, ProcessHandle>)
    supervisor.rs                 # SupervisorSpec, restart strategies, child specs
    link.rs                       # Link/monitor tables, exit signal propagation
    error.rs                      # Reason, Action, RtResult
    ets.rs                        # In-memory term storage (later)
    config.rs                     # RuntimeConfig (worker_count, mailbox defaults)
    metrics.rs                    # Prometheus metrics (process count, restarts, etc.)
    control/
      provider_gate.rs            # Per-provider RPM/TPM semaphore
      budget.rs                   # Two-phase budget gate
      admission.rs                # Weighted fair queuing, priority, starvation prevention
    durability/
      checkpoint.rs               # CheckpointStore trait + SqliteCheckpointStore
      wal.rs                      # WAL writer/reader, WalEntry, compaction
      recovery.rs                 # 10-step recovery sequence

zeptobeam/                        # ZeptoClaw-specific daemon
  src/
    main.rs                       # CLI entry, daemon startup
    agent_adapter.rs              # Implements Behavior for ZeptoClaw agents
    swarm.rs                      # DAG/sequential/parallel swarm orchestration
    config_loader.rs              # TOML parsing, merge algorithm, validation
    server.rs                     # HTTP/gRPC management API
    mcp.rs                        # MCP server for tool serving
```

**Boundary rule:** `zeptovm` knows nothing about LLMs, ZeptoClaw, or AI agents. `zeptobeam` is the only crate that imports ZeptoClaw types and implements `Behavior` for agents.

### 6.2 Key Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `tokio-util` | CancellationToken |
| `async-trait` | Object-safe async Behavior trait |
| `dashmap` | Concurrent ProcessRegistry |
| `rusqlite` | Checkpoint store, WAL storage |
| `serde` + `toml` | Config parsing |
| `sha2` | Config hash |
| `prometheus` | Metrics export |
| `tracing` | Structured logging |

---

## 7. Migration (Strangler Pattern)

### 7.1 Stages

| Stage | Scope | Cutover Gate |
|-------|-------|-------------|
| v0 | Pid, Process, Mailbox, Registry, Behavior trait | Can spawn 100 processes, deliver 10k messages, clean shutdown |
| v1 | Supervisor, Link/Monitor, restart strategies | OneForOne restart works, max_restarts escalation, exit truth table passes |
| v2 | Checkpoint, WAL, recovery sequence | Process recovers from checkpoint after kill, WAL replay correct |
| v3 | Budget gate, provider gate, admission, priority | Budget precheck blocks exhausted agent, provider gate enforces RPM |
| v4 | agent_adapter, config_loader, swarm, server | ZeptoClaw agent runs via ZeptoBeam config, swarm DAG executes |

### 7.2 Cutover from agent_rt

The existing `agent_rt` crate (41 modules) continues running production. ZeptoBeam develops in parallel. At each stage gate:

1. Run parity tests: same agent config produces same behavior on both runtimes
2. Shadow mode: route 10% traffic to ZeptoBeam, compare results
3. Cutover: switch config to point at ZeptoBeam
4. Deprecate corresponding agent_rt module

### 7.3 SLO Targets

**Gate 1 (Baseline):**
- Process spawn: < 1ms
- Message delivery (in-memory): < 1ms p99
- Memory per idle process: < 16KB
- Supervisor restart: < 10ms

**Gate 3 (Aspirational):**
- Process spawn: < 500us
- Message delivery: < 500us p99
- Memory per idle process: < 8KB
- Checkpoint save: < 5ms for < 1MB state
- WAL append: < 1ms

---

## 8. Versioning

- `zeptovm` follows semver. Pre-1.0 during v0-v2.
- `zeptobeam` follows semver independently.
- Config versions: `zeptobeam_config_version = "1"`, `required_zeptoclaw_config_version = "1"`
- Breaking changes to Behavior trait require major version bump on zeptovm.

---

## 9. Security Considerations

- **Single-tenant trust domain:** No process isolation beyond Rust memory safety. All processes share the tokio runtime.
- **Tool execution sandboxing:** Owned by ZeptoClaw, not ZeptoVM. ZeptoVM trusts the Behavior implementation.
- **Budget enforcement:** Defense in depth — precheck + commit. But a malicious Behavior impl can bypass by not calling the LLM through the gate. This is acceptable in single-tenant.
- **WAL encryption:** Not in v0-v2. Add AES-256-GCM envelope encryption in v3 if needed.
- **API authentication:** Management server (zeptobeam) uses mTLS or API key. Not in v0.

---

## Appendix A: ErlangRT/BEAM Parity (20% effort)

Separate from ZeptoVM. The existing ErlangRT codebase continues receiving OTP 28 opcode/BIF parity work as documented in `docs/plans/2026-03-06-otp28-parity-impl.md`. This feeds into ZeptoVM only as reference for supervision semantics and term representation.
