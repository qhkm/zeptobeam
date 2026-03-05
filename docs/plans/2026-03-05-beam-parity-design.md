# Phase 9: BEAM Parity Design

**Goal:** Add five Erlang/OTP-inspired features to make the zeptobeam agent runtime uniquely powerful: ETS/DETS tables, persistent message queues, a behaviors framework, hot code upgrades, and manifest-based release handling.

**Architecture:** Layered independence. Five modules built bottom-up, each independently useful and testable. Lower layers have zero dependency on upper layers. All features are opt-in.

---

## Decisions Summary

| Decision | Choice |
|----------|--------|
| Scope | All 5 features |
| Architecture | Layered independence (ETS -> Queues -> Behaviors -> Hot Code -> Releases) |
| Behaviors naming | Adapted for AI agents (GenAgent/StateMachine/EventManager) |
| Hot code granularity | Both per-process and per-type |
| Durable mailbox | Opt-in per process, WAL-based, at-least-once delivery |
| ETS queries | Key-value + scan(prefix) + filter(predicate) |
| Release handling | JSON manifest-based with transactional apply + compensating rollback |

---

## Layer 1: ETS/DETS Tables

### ETS (In-Memory)

```rust
pub struct EtsTable {
    name: String,
    access: AccessType,        // Public | Protected | Private
    owner: AgentPid,
    data: HashMap<String, serde_json::Value>,
}

pub enum AccessType {
    Public,     // Any process can read/write
    Protected,  // Any process reads, only owner writes
    Private,    // Only owner reads/writes
}
```

**API:**
- `EtsRegistry::create(name, access, owner)` -> `TableId`
- `EtsRegistry::destroy(table_id, caller_pid)` (owner-only)
- `get(table_id, key)` -> `Option<Value>`
- `put(table_id, key, value, caller_pid)` (access-checked)
- `delete(table_id, key, caller_pid)`
- `scan(table_id, prefix)` -> `Vec<(String, Value)>`
- `filter(table_id, predicate: &dyn Fn(&str, &Value) -> bool)` -> `Vec<(String, Value)>`
- `count(table_id)`, `keys(table_id)`

**Ownership:** When owner process dies, table is destroyed. Optional `heir` PID can inherit.

### DETS (Disk-Backed)

Same API as ETS, backed by append-log + compaction.

```rust
pub struct DetsTable {
    name: String,
    path: PathBuf,
    cache: HashMap<String, serde_json::Value>,  // in-memory cache
    access: AccessType,
    owner: AgentPid,
    max_entries: usize,
    dirty_count: usize,
    compact_threshold: usize,
}
```

**Storage strategy:**
- On open: load `.dat` file into cache, replay `.log` on top
- Mutations: append to `.log` (fast sequential write), update cache
- Compaction: when `dirty_count >= compact_threshold`, snapshot cache, release lock, write `.dat.tmp` from snapshot, reacquire lock, rename `.dat.tmp` -> `.dat`, truncate `.log`
- Default `max_entries`: 100,000. `put` returns error when exceeded
- Expected workload: tables under 100K entries, values under 1MB each

**Locking:** Registry lock only for create/destroy. Individual table lock for operations. No global lock during disk IO — compaction runs outside table lock using a cache snapshot.

### Unified Registry

`EtsRegistry` holds both ETS and DETS tables with `TableId` enum (`Ets(u64)` / `Dets(u64)`).

---

## Layer 2: Persistent Message Queues

### Opt-In Durable Mailbox

```rust
pub struct ProcessConfig {
    pub priority: Priority,
    pub mailbox_capacity: usize,
    pub durable_mailbox: bool,       // default false
    pub stable_id: Option<String>,   // required if durable_mailbox is true
}
```

### Delivery Guarantee

**At-least-once with idempotent dedup.**

- Each WAL entry has a `seq` number
- Process tracks `last_acked_seq`, persisted to `.ack` file
- On replay, only `seq > last_acked_seq` are re-delivered
- Consumers needing effectively-once must be idempotent or check `seq`

### Write-Ahead Log

```rust
pub struct DurableMailbox {
    stable_id: String,
    wal_path: PathBuf,
    ack_path: PathBuf,
    writer: BufWriter<File>,
    sequence: u64,
    last_acked_seq: u64,
}
```

**WAL format** (newline-delimited JSON with schema header):
```json
{"schema_version": 1, "type": "wal", "stable_id": "orchestrator-main"}
{"seq": 1, "ts": 1709654400, "msg": {"Text": "hello"}}
{"seq": 2, "ts": 1709654401, "msg": {"IoResponse": "result"}}
```

### Strict Fsync Protocol

**Message delivery:**
1. Serialize message to WAL entry
2. Write to WAL file
3. fsync WAL fd
4. Only THEN enqueue to in-memory mailbox

**Ack commit:**
1. Write `last_acked_seq` to `{stable_id}.ack.tmp`
2. fsync `.ack.tmp` fd
3. rename `.ack.tmp` -> `.ack` (atomic on POSIX)
4. Only THEN eligible for truncation

**Truncation:**
1. Read `.ack` file to get confirmed seq
2. Rewrite WAL excluding entries <= confirmed seq
3. fsync new WAL
4. rename into place

### Stable ID Lifecycle

**Collision policy:** Spawn fails with `AgentRtError::DurableMailbox("stable_id already active")` if a live process holds the same `stable_id`.

**Orphan cleanup:**
```rust
pub struct WalRetentionPolicy {
    pub orphan_ttl_secs: u64,       // default 86400 (24h)
    pub cleanup_interval_secs: u64,  // default 3600 (1h)
}
```
- Background task scans WAL directory periodically
- Orphaned files (no active process) older than TTL are deleted
- Deletions logged at `info` level

### Recovery Flow

1. Scan WAL directory for `{stable_id}.wal` files
2. Read corresponding `.ack` file for `last_acked_seq`
3. Replay WAL entries with `seq > last_acked_seq`
4. Re-deliver to respawned process's mailbox in order
5. Supervisor restarts the process, messages already waiting

### Configuration

```toml
[durable_mailbox]
enabled = true
wal_dir = "./data/wal"
flush_strategy = "every_message"   # or "every_n:10" or "every_ms:100"
orphan_ttl_secs = 86400
cleanup_interval_secs = 3600
```

---

## Layer 3: Behaviors Framework

Three patterns adapted for AI agents, each wrapping `AgentBehavior`.

### GenAgent (gen_server equivalent)

```rust
pub trait GenAgent: Send + Sync {
    type State: Send;

    fn init(&self, args: serde_json::Value) -> Result<Self::State, String>;

    fn handle_request(
        &self, state: &mut Self::State, from: AgentPid, request: serde_json::Value,
    ) -> Reply;

    fn handle_notify(
        &self, state: &mut Self::State, notification: serde_json::Value,
    ) -> Noreply;

    fn handle_info(
        &self, state: &mut Self::State, msg: Message,
    ) -> Noreply;

    fn code_change(
        &self, state: &mut Self::State, old_vsn: &str, extra: serde_json::Value,
    ) -> Result<(), String> {
        let _ = (old_vsn, extra);
        Ok(())
    }

    fn terminate(&self, state: &mut Self::State, reason: &str) {
        let _ = reason;
    }
}

pub enum Reply { Ok(serde_json::Value), Error(String), Stop(String) }
pub enum Noreply { Ok, Stop(String) }
```

`GenAgentProcess<T: GenAgent>` implements `AgentBehavior`. Messages tagged `{"$call": ..., "$from": pid}` dispatch to `handle_request`, `{"$cast": ...}` to `handle_notify`, everything else to `handle_info`.

### StateMachine (gen_fsm equivalent)

```rust
pub trait StateMachine: Send + Sync {
    type Data: Send;

    fn init(&self, args: serde_json::Value) -> Result<(String, Self::Data), String>;

    fn handle_event(
        &self, state_name: &str, data: &mut Self::Data, event: serde_json::Value,
    ) -> Transition;

    fn code_change(
        &self, state_name: &str, data: &mut Self::Data, old_vsn: &str,
    ) -> Result<(), String> {
        let _ = (state_name, old_vsn);
        Ok(())
    }
}

pub enum Transition { Next { state: String }, Same, Stop(String) }
```

### EventManager (gen_event equivalent)

```rust
pub trait EventHandler: Send + Sync {
    fn handle_event(&self, event: &serde_json::Value) -> EventAction;
    fn id(&self) -> &str;
}

pub enum EventAction { Ok, Remove }

pub struct EventManager {
    handlers: Vec<Box<dyn EventHandler>>,
}
```

API: `add_handler`, `remove_handler(id)`, `notify(event)`. Runs as an `AgentBehavior` process dispatching to all handlers.

---

## Layer 4: Hot Code Upgrades

### Behavior Version Registry

```rust
pub struct BehaviorVersion {
    pub name: String,
    pub version: String,
    pub factory: Box<dyn Fn() -> Box<dyn AgentBehavior> + Send + Sync>,
}

pub struct HotCodeRegistry {
    behaviors: HashMap<String, Vec<BehaviorVersion>>,
    process_versions: HashMap<AgentPid, (String, String)>,
}
```

**API:**
- `register(name, version, factory)`
- `upgrade_process(pid, target_version)` -> `Result<(), AgentRtError>`
- `upgrade_all(name, target_version)` -> `Result<UpgradeReport, AgentRtError>`
- `rollback_process(pid)` -> `Result<(), AgentRtError>`
- `current_version(pid)` -> `Option<(String, String)>`
- `list_versions(name)` -> `Vec<String>`

### Upgrade Flow (per process)

```
1. Suspend process (remove from scheduler run queue)
2. Quiesce — wait for in-flight handler to complete (up to quiesce_timeout_ms)
3. Call new_behavior.code_change(&mut state, old_vsn, extra)
4. If code_change succeeds -> swap behavior, update version map, resume
5. If code_change fails -> leave old behavior, resume unchanged, return error
```

**No terminate during upgrade.** `terminate` only called on actual process death.

### Quiesce Timeout

```rust
pub struct HotCodeConfig {
    pub quiesce_timeout_ms: u64,                    // default 30000
    pub quiesce_timeout_policy: QuiesceTimeoutPolicy,
}

pub enum QuiesceTimeoutPolicy {
    Abort,      // cancel upgrade, resume on old behavior (default)
    ForceSwap,  // swap anyway, discard in-flight result (risky)
}
```

### Two-Version Coexistence

At most two versions active simultaneously. When a third is registered:

```rust
pub enum StalePolicy {
    ForceUpgrade,   // auto-upgrade processes on oldest version
    Terminate,      // kill processes on oldest version
}
```

---

## Layer 5: Release Handling

### Release Manifest

```json
{
  "schema_version": 1,
  "version": "1.2.0",
  "previous": "1.1.0",
  "behaviors": [
    {
      "name": "worker",
      "version": "v2",
      "upgrade_from": "v1",
      "extra": { "new_model": "gpt-4o" }
    }
  ],
  "config_changes": {
    "runtime.worker_count": 16
  }
}
```

### Transactional Apply

```rust
pub struct ReleasePlan {
    steps: Vec<ReleaseStep>,
}

pub enum ReleaseStep {
    UpgradeBehavior { name: String, from: String, to: String, extra: Value },
    ApplyConfig { key: String, old_value: Value, new_value: Value },
}
```

**Apply flow:**
1. Build plan from manifest (captures old values for each step)
2. Write `.in_progress.json` with `completed_steps: 0`
3. Execute steps sequentially, update `completed_steps` + fsync after each
4. If step N fails: compensate steps N-1..0 in reverse, delete `.in_progress.json`
5. If all succeed: delete `.in_progress.json`, push to history

**Idempotency:** Each step checks current state. Upgrading already-at-target is a no-op.

### Crash Recovery

On restart:
1. Check for `.in_progress.json`
2. If found: compensating rollback from `completed_steps - 1` down to 0
3. Delete `.in_progress.json`
4. Log at `warn` level

### ReleaseManager

```rust
pub struct ReleaseManager {
    current: Option<ReleaseManifest>,
    history: Vec<ReleaseManifest>,   // capped at max_history
    hot_code: Arc<Mutex<HotCodeRegistry>>,
}
```

API: `load_manifest(path)`, `apply(manifest)`, `rollback()`, `current()`, `history()`

---

## Configuration

```toml
[ets]
max_tables = 256
max_entries_per_table = 100000

[durable_mailbox]
enabled = true
wal_dir = "./data/wal"
flush_strategy = "every_message"
orphan_ttl_secs = 86400
cleanup_interval_secs = 3600

[hot_code]
quiesce_timeout_ms = 30000
quiesce_timeout_policy = "abort"
stale_policy = "force_upgrade"

[release]
manifest_dir = "./releases"
max_history = 10
```

---

## Error Handling

New `AgentRtError` variants:

```rust
Ets(String),              // table not found, access denied, capacity exceeded
DurableMailbox(String),   // WAL write failure, replay error, stable_id collision
HotCode(String),          // version not found, code_change failed, quiesce timeout
Release(String),          // manifest invalid, apply failed, rollback failed
```

---

## Concurrency Model

### Lock Ordering (acquire in this order)

1. `EtsRegistry` lock
2. Individual `EtsTable`/`DetsTable` lock
3. `HotCodeRegistry` lock
4. `ReleaseManager` lock
5. Process-level locks (mailbox, state)

### Rules

- No global lock held during disk IO
- DETS compaction runs outside table lock using cache snapshot
- WAL writes are per-process, no global lock
- Hot code suspend/resume goes through scheduler run queue
- Release steps acquire only the lock needed for that step, never two simultaneously

---

## Schema Evolution

### Compatibility Rules

- Version N reader MUST support schemas [N-2 .. N]
- Version N writer ALWAYS writes schema version N
- Unknown fields are preserved (round-tripped) but ignored
- If reader encounters `schema_version > supported`, returns error: `"schema version X not supported, max supported: Y"`

### Per-File Rules

| File | Schema | Required Fields (forever) |
|------|--------|--------------------------|
| WAL entry | v1 | `seq`, `msg` |
| ACK | v1 | Single integer |
| DETS .dat/.log | v1 | `key`, `value` per entry |
| Release manifest | v1 | `schema_version`, `version`, `behaviors` |
| .in_progress | v1 | `schema_version`, `plan`, `completed_steps` |

New step types in manifests are ignored with a warning (forward compat).

---

## Testing Strategy

| Layer | Tests | Key Scenarios |
|-------|-------|---------------|
| ETS | ~12 | CRUD, access control, scan/filter, owner death, heir transfer |
| DETS | ~8 | Roundtrip, persistence, atomic compaction, recovery from .dat+.log, capacity limit |
| Durable Mailbox | ~10 | Append/replay, ack protocol, truncation, stable_id collision, orphan cleanup |
| Behaviors | ~15 | GenAgent call/cast/info, StateMachine transitions, EventManager pub/sub, code_change callbacks |
| Hot Code | ~10 | Upgrade single/all, rollback, code_change failure, quiesce timeout, stale policy |
| Release | ~8 | Load manifest, apply, rollback, partial failure compensation, idempotency |
| Failure Injection | ~7 | Crash between WAL append/enqueue, crash between handle/ack, crash during upgrade, crash during apply step N, DETS crash between log/cache |
| Concurrency | ~5 | Lock ordering, no deadlock under contention, compaction + concurrent reads |
| **Total** | **~75** | |

---

## Future Improvements

| Feature | Description | Deferred To |
|---------|-------------|-------------|
| TOML manifests | Support TOML alongside JSON for releases | Post-Phase 9 |
| Distributed releases | Coordinate releases across cluster nodes | Phase 8 integration |
| Approval gates for releases | Require approval before applying | Wire into Phase 6 ApprovalRegistry |
| Canary releases | Upgrade subset first, monitor, proceed | Post-Phase 9 |
| Manifest signing | Cryptographic verification | Enterprise track |
| Full match specifications | Erlang-style pattern matching on ETS | Post-Phase 9 |
| ETS table linking | Auto-destroy tables when linked process dies | Post-Phase 9 |
| WAL compression | Compress old WAL segments | Post-Phase 9 |
| Shared session pool for DETS | Multiple processes share one DETS handle | Post-Phase 9 |

---

## New Files

- `lib-erlangrt/src/agent_rt/ets.rs`
- `lib-erlangrt/src/agent_rt/dets.rs`
- `lib-erlangrt/src/agent_rt/durable_mailbox.rs`
- `lib-erlangrt/src/agent_rt/behaviors.rs`
- `lib-erlangrt/src/agent_rt/hot_code.rs`
- `lib-erlangrt/src/agent_rt/release.rs`

## Modified Files

- `lib-erlangrt/src/agent_rt/mod.rs` — module declarations
- `lib-erlangrt/src/agent_rt/types.rs` — ProcessConfig additions
- `lib-erlangrt/src/agent_rt/config.rs` — EtsConfig, DurableMailboxConfig, HotCodeConfig, ReleaseConfig
- `lib-erlangrt/src/agent_rt/error.rs` — new error variants
