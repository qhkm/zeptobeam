# ZeptoVM Runtime Spec v0.1

> Formal type definitions and semantics for core runtime primitives.
> Source: codex design review. Companion to canonical-design.md.

## 1. Core type system

```rust
pub type TenantId = String;
pub type Namespace = String;
pub type ProcessId = u64;
pub type Incarnation = u64;
pub type MsgId = u128;
pub type EffectId = u128;
pub type TimerId = u128;
pub type TurnId = u128;
pub type ObjectId = u128;
pub type NodeId = String;
pub type ShardId = u64;
pub type TimestampMs = i64;
```

## 2. Pid

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pid {
    pub tenant_id: TenantId,
    pub namespace: Namespace,
    pub process_id: ProcessId,
    pub incarnation: Incarnation,
}
```

## 3. ProcessStatus

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    Ready,
    Running,
    WaitingMessage,
    WaitingEffect(EffectId),
    WaitingTimer(TimerId),
    Checkpointing,
    Suspended,
    Failed(ExitReason),
    Done(ExitReason),
}
```

Valid transitions:
```
Ready -> Running
Running -> Ready | WaitingMessage | WaitingEffect | WaitingTimer | Done | Failed
WaitingEffect -> Ready
WaitingTimer -> Ready
WaitingMessage -> Ready
Checkpointing -> Ready
Suspended -> Ready
```

Invalid: Done -> Running, Failed -> Running, Running -> Running on another scheduler.

## 4. ExitReason

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ExitReason {
    Normal,
    Cancelled,
    Timeout,
    BudgetExceeded,
    PolicyDenied(String),
    DependencyFailed(String),
    Panic(String),
    Error(String),
    Killed,
    NodeLost,
}
```

## 5. MessageEnvelope

```rust
#[derive(Debug, Clone)]
pub struct MessageEnvelope {
    pub msg_id: MsgId,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub from: Option<Pid>,
    pub to: Pid,
    pub class: MessageClass,
    pub priority: Priority,
    pub dedup_key: Option<String>,
    pub ttl_ms: Option<u64>,
    pub created_at_ms: TimestampMs,
    pub payload: MessagePayload,
}
```

## 6. MessagePayload

```rust
#[derive(Debug, Clone)]
pub enum MessagePayload {
    Start,
    Stop,
    Kill,
    Wake,
    UserEvent(serde_json::Value),
    EffectCompleted(EffectResult),
    TimerFired(TimerFired),
    ChildExited(ChildExit),
    MonitorDown(MonitorDown),
    BudgetExceeded(BudgetExceeded),
    PolicyDenied(PolicyDenied),
    SystemEvent(SystemEvent),
}
```

## 7. StepResult

```rust
#[derive(Debug, Clone)]
pub enum StepResult {
    Continue,
    Wait,
    Suspend(EffectRequest),
    Done(ExitReason),
    Fail(ExitReason),
}
```

## 8. Behavior trait

```rust
pub trait Behavior: Send + 'static {
    type State: StateCodec;

    fn init(&mut self, checkpoint: Option<Self::State>) -> StepResult;
    fn handle(&mut self, msg: MessageEnvelope, ctx: &mut TurnContext<Self::State>) -> StepResult;
    fn terminate(&mut self, reason: &ExitReason, ctx: &mut TerminationContext);
}
```

## 9. TurnContext

```rust
pub struct TurnContext<S> {
    pub pid: Pid,
    pub state: S,
    pub turn_id: TurnId,
    pub now_ms: TimestampMs,
    pub intents: Vec<TurnIntent>,
    pub reductions_used: u32,
}

impl<S> TurnContext<S> {
    pub fn send(&mut self, msg: MessageEnvelope);
    pub fn send_user(&mut self, to: Pid, payload: serde_json::Value);
    pub fn spawn(&mut self, spec: SpawnSpec) -> SpawnHandle;
    pub fn link(&mut self, pid: Pid);
    pub fn monitor(&mut self, pid: Pid);
    pub fn schedule_timer(&mut self, timer: TimerSpec);
    pub fn request_effect(&mut self, effect: EffectRequest);
    pub fn debit_budget(&mut self, delta: BudgetDelta);
    pub fn set_state(&mut self, new_state: S);
    pub fn now(&self) -> TimestampMs;
}
```

## 10. TurnIntent

```rust
#[derive(Debug, Clone)]
pub enum TurnIntent {
    SendMessage(MessageEnvelope),
    SpawnProcess(SpawnSpec),
    Link(Pid),
    Monitor(Pid),
    ScheduleTimer(TimerSpec),
    RequestEffect(EffectRequest),
    DebitBudget(BudgetDelta),
    PatchState(Vec<u8>),
    EmitSignal(RuntimeSignal),
}
```

## 11. TurnCommit

```rust
#[derive(Debug, Clone)]
pub struct TurnCommit {
    pub turn_id: TurnId,
    pub pid: Pid,
    pub consumed_msg_id: MsgId,
    pub state_patch: Option<Vec<u8>>,
    pub outbound_messages: Vec<MessageEnvelope>,
    pub effect_requests: Vec<EffectRequest>,
    pub timer_registrations: Vec<TimerSpec>,
    pub budget_changes: Vec<BudgetDelta>,
    pub journal_entries: Vec<JournalEntry>,
    pub next_status: ProcessStatus,
    pub committed_at_ms: TimestampMs,
}
```

## 12. TurnAbort

```rust
#[derive(Debug)]
pub enum TurnAbort {
    Panic(String),
    CommitFailed(String),
    PolicyViolation(String),
    BudgetViolation(String),
    InvalidState(String),
}
```

## 13. EffectRequest

```rust
#[derive(Debug, Clone)]
pub struct EffectRequest {
    pub effect_id: EffectId,
    pub process_id: Pid,
    pub kind: EffectKind,
    pub input: EffectInput,
    pub timeout_ms: u64,
    pub retry: RetryPolicy,
    pub compensation: Option<CompensationSpec>,
    pub idempotency_key: Option<String>,
    pub cost_hint: Option<CostHint>,
    pub policy_hint: Option<PolicyHint>,
    pub created_at_ms: TimestampMs,
}
```

## 14. EffectInput

```rust
#[derive(Debug, Clone)]
pub enum EffectInput {
    Llm(LlmCallSpec),
    Http(HttpSpec),
    DbQuery(DbQuerySpec),
    DbWrite(DbWriteSpec),
    Browser(BrowserSpec),
    Cli(CliSpec),
    Sandbox(SandboxSpec),
    HumanApproval(HumanApprovalSpec),
    HumanInput(HumanInputSpec),
    SleepUntil(SleepUntilSpec),
    Publish(PublishSpec),
    ObjectFetch(ObjectFetchSpec),
    ObjectPut(ObjectPutSpec),
    VectorSearch(VectorSearchSpec),
    Json(serde_json::Value),
}
```

## 15. EffectResult

```rust
#[derive(Debug, Clone)]
pub struct EffectResult {
    pub effect_id: EffectId,
    pub process_id: Pid,
    pub status: EffectStatus,
    pub output: Option<EffectOutput>,
    pub error: Option<EffectError>,
    pub cost: Option<CostRecord>,
    pub started_at_ms: TimestampMs,
    pub finished_at_ms: TimestampMs,
    pub worker_id: Option<String>,
}
```

## 16. JournalEntry

```rust
#[derive(Debug, Clone)]
pub enum JournalEntry {
    ProcessSpawned { pid: Pid, parent: Option<Pid>, kind: ProcessKind },
    MessageReceived { pid: Pid, msg_id: MsgId, class: MessageClass },
    TurnStarted { pid: Pid, turn_id: TurnId },
    StatePatched { pid: Pid, snapshot_version: u64 },
    EffectRequested { pid: Pid, effect_id: EffectId, kind: EffectKind },
    EffectResultRecorded { pid: Pid, effect_id: EffectId, status: EffectStatus },
    MessageSent { from: Pid, to: Pid, msg_id: MsgId },
    TimerScheduled { pid: Pid, timer_id: TimerId },
    TimerFired { pid: Pid, timer_id: TimerId },
    ChildSpawned { parent: Pid, child: Pid },
    LinkCreated { from: Pid, to: Pid },
    MonitorCreated { from: Pid, to: Pid },
    BudgetDebited { pid: Pid, delta: BudgetDelta },
    PolicyDecisionRecorded { pid: Pid, decision: PolicyDecision },
    CheckpointCreated { pid: Pid, snapshot_version: u64 },
    ProcessSuspended { pid: Pid },
    ProcessResumed { pid: Pid },
    ProcessFailed { pid: Pid, reason: ExitReason },
    ProcessExited { pid: Pid, reason: ExitReason },
    ProcessMigrated { pid: Pid, from_node: NodeId, to_node: NodeId },
}
```

## 17. SupervisorStrategy

```rust
#[derive(Debug, Clone)]
pub struct SupervisorStrategy {
    pub restart: RestartStrategy,
    pub max_restarts: u32,
    pub window_ms: u64,
    pub backoff: BackoffPolicy,
    pub escalation: EscalationPolicy,
}

pub enum RestartStrategy { OneForOne, OneForAll, RestForOne, Never }
pub enum BackoffPolicy { None, Fixed { delay_ms: u64 }, Exponential { base_ms: u64, factor: f32, max_ms: u64 } }
pub enum EscalationPolicy { Ignore, NotifyParent, NotifyRoot, KillSubtree, SuspendSubtree }
```

## 18. BudgetState and BudgetDelta

```rust
#[derive(Debug, Clone, Default)]
pub struct BudgetState {
    pub token_budget_remaining: Option<u64>,
    pub usd_micros_remaining: Option<u64>,
    pub api_quota_remaining: Option<u64>,
    pub max_parallel_children: Option<u32>,
    pub wall_clock_deadline_ms: Option<TimestampMs>,
    pub cpu_turn_quota: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BudgetDelta {
    pub token_delta: i64,
    pub usd_micros_delta: i64,
    pub api_quota_delta: i64,
    pub parallel_children_delta: i32,
    pub cpu_turn_delta: i64,
    pub reason: String,
}
```

## 19. PolicyDecision

```rust
#[derive(Debug, Clone)]
pub enum PolicyDecision {
    Allow,
    Deny { reason: String },
    RequireApproval { queue: String, reason: String },
    Rewrite { note: String },
    Downgrade { note: String },
    Sandbox { note: String },
}
```

## 20. TimerSpec

```rust
#[derive(Debug, Clone)]
pub struct TimerSpec {
    pub timer_id: TimerId,
    pub pid: Pid,
    pub kind: TimerKind,
    pub deadline_ms: TimestampMs,
    pub payload: Option<serde_json::Value>,
    pub durable: bool,
    pub dedup_key: Option<String>,
}

pub enum TimerKind { SleepUntil, Timeout, RetryBackoff, Cron, LeaseRenewal, HumanReminder }
```

## 21. ObjectRef

```rust
#[derive(Debug, Clone)]
pub struct ObjectRef {
    pub object_id: ObjectId,
    pub content_hash: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub storage_class: String,
    pub encryption_scope: Option<String>,
    pub created_at_ms: TimestampMs,
    pub ttl_ms: Option<u64>,
}
```

## 22. Persistence schema (v0.1)

```sql
-- processes
CREATE TABLE processes (
    pid INTEGER PRIMARY KEY,
    tenant_id TEXT, namespace TEXT, incarnation INTEGER,
    kind TEXT, status TEXT,
    behavior_module TEXT, behavior_version TEXT,
    snapshot_version INTEGER, journal_offset INTEGER,
    supervisor_pid INTEGER, priority TEXT,
    created_at INTEGER, updated_at INTEGER
);

-- snapshots
CREATE TABLE snapshots (
    pid INTEGER, snapshot_version INTEGER,
    state_blob BLOB, mailbox_cursor INTEGER,
    budget_blob BLOB, timers_blob BLOB,
    pending_effects_blob BLOB, created_at INTEGER,
    PRIMARY KEY (pid, snapshot_version)
);

-- journal
CREATE TABLE journal (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    pid INTEGER, turn_id INTEGER,
    entry_type TEXT, entry_blob BLOB,
    created_at INTEGER
);

-- mailbox
CREATE TABLE mailbox (
    pid INTEGER, lane TEXT, msg_id INTEGER,
    envelope_blob BLOB, visible_at INTEGER,
    expires_at INTEGER, dedup_key TEXT
);

-- effects
CREATE TABLE effects (
    effect_id INTEGER PRIMARY KEY,
    pid INTEGER, kind TEXT,
    request_blob BLOB, status TEXT,
    result_blob BLOB, idempotency_key TEXT,
    started_at INTEGER, finished_at INTEGER
);

-- timers
CREATE TABLE timers (
    timer_id INTEGER PRIMARY KEY,
    pid INTEGER, kind TEXT,
    deadline_ms INTEGER, payload_blob BLOB,
    durable INTEGER, fired INTEGER
);
```

## 23. Recommended implementation order

v0: Pid, MessageEnvelope, ProcessStatus, StepResult, Behavior, TurnContext, in-memory scheduler, in-memory mailbox lanes
v1: EffectRequest, effect dispatcher, turn commit, journal, snapshotting, timers
v2: Supervisor tree, links/monitors, watchdog, policy hooks, budget hooks
v3: Persistent mailbox, recovery, strict status transitions
v4: Shard leases, remote routing, multi-node recovery

## 24. The three pillars

1. `StepResult` must emit effect intents, not futures
2. `TurnCommit` must be the atomic durable boundary
3. `ProcessStatus` transitions must be strict and recoverable
