# Phase 5: Production Readiness Design

**Goal:** Make zeptoclaw-rt deployable as a standalone service with config loading, CLI, health endpoints, structured logging, error types, and checkpoint pruning.

**Working directory:** `~/ios/zeptoclaw-rt`

**Prerequisite:** Phase 4 (Reliability Hardening) — mailbox bounds, supervision backoff, timeouts, DLQ, SQLite checkpoints, chaos testing all complete.

---

## Architecture

New binary crate `zeptoclaw-rtd` (daemon) loads TOML config, initializes tracing, starts the agent runtime (scheduler + bridge), launches an axum health server, and handles graceful shutdown on SIGTERM/SIGINT. The `lib-erlangrt` agent_rt module gets structured error types via thiserror, FileCheckpointStore tests, and checkpoint TTL pruning.

```
zeptoclaw-rt/
├── lib-erlangrt/src/agent_rt/
│   ├── error.rs              (NEW - AgentRtError enum with thiserror)
│   ├── config.rs             (NEW - AppConfig, parsed from TOML)
│   ├── server.rs             (NEW - axum health/metrics/status endpoints)
│   ├── checkpoint.rs         (MODIFY - add list_keys, prune_before, TTL support)
│   ├── checkpoint_sqlite.rs  (MODIFY - add prune_before, list_keys)
│   └── ...existing...
├── zeptoclaw-rtd/            (NEW binary crate)
│   ├── Cargo.toml
│   └── src/main.rs           (clap CLI, config loading, tracing init, signal handling)
└── Cargo.toml                (workspace: add zeptoclaw-rtd member)
```

---

## 1. Structured Error Types (`error.rs`)

**Current state:** Agent_rt uses `Result<T, String>` throughout — CheckpointStore trait, bridge errors, orchestration errors.

**Changes:**

New `AgentRtError` enum replacing String errors:

```rust
#[derive(Debug, thiserror::Error)]
pub enum AgentRtError {
    #[error("config: {0}")]
    Config(String),
    #[error("checkpoint: {0}")]
    Checkpoint(String),
    #[error("checkpoint IO: {0}")]
    CheckpointIo(#[from] std::io::Error),
    #[error("checkpoint SQL: {0}")]
    CheckpointSql(#[from] rusqlite::Error),
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("bridge: {0}")]
    Bridge(String),
    #[error("server: {0}")]
    Server(String),
    #[error("shutdown: {0}")]
    Shutdown(String),
}
```

`CheckpointStore` trait changes from `Result<T, String>` to `Result<T, AgentRtError>`. All existing implementations updated.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/error.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add error module)
- Modify: `lib-erlangrt/src/agent_rt/checkpoint.rs` (update trait + InMemory + File impls)
- Modify: `lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs` (update SqliteCheckpointStore)
- Modify: callers in orchestration.rs, scheduler.rs
- Add dep: `thiserror = "2"` to lib-erlangrt/Cargo.toml

---

## 2. Configuration (`config.rs`)

**Current state:** No config file loading. Hardcoded paths in binary entry points.

**Config file format:** TOML

```toml
[runtime]
worker_count = 4          # bridge worker pool size
mailbox_capacity = 1024
max_reductions = 200

[checkpoint]
store = "sqlite"          # "memory" | "file" | "sqlite"
path = "./zeptoclaw-rt.db"
ttl_hours = 24            # auto-prune checkpoints older than this
prune_interval_secs = 3600

[server]
enabled = true
bind = "127.0.0.1:9090"

[logging]
level = "info"            # trace | debug | info | warn | error
format = "pretty"         # "pretty" | "json" | "compact"
```

Rust struct `AppConfig` with `#[derive(Debug, Deserialize, Default)]` and `serde(default)` for all fields. Subsections: `RuntimeConfig`, `CheckpointConfig`, `ServerConfig`, `LogConfig`.

**Defaults:** All fields have sensible defaults. Config file is optional — if not found, all defaults used. CLI args override config file values.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/config.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`
- Add dep: `toml = "0.8"` to lib-erlangrt/Cargo.toml

---

## 3. CLI Entry Point (`zeptoclaw-rtd`)

**Current state:** No production-oriented binary. erlexec is for BEAM emulation.

**New binary crate** in workspace:

```
zeptoclaw-rtd [OPTIONS]

Options:
  -c, --config <PATH>    Config file path [default: zeptoclaw-rt.toml]
  -l, --log-level <LVL>  Override log level (trace|debug|info|warn|error)
  -w, --workers <N>      Override worker count
  -b, --bind <ADDR>      Override server bind address
  -V, --version          Print version
  -h, --help             Print help
```

**Override precedence:** defaults < config file < CLI args

**Startup sequence:**
1. Parse CLI args (clap)
2. Load config file (TOML, optional)
3. Apply CLI overrides
4. Initialize tracing
5. Create checkpoint store (per config)
6. Start bridge worker pool
7. Start scheduler
8. Start health server (if enabled)
9. Register signal handlers
10. Block on shutdown signal

**Files:**
- Create: `zeptoclaw-rtd/Cargo.toml`
- Create: `zeptoclaw-rtd/src/main.rs`
- Modify: root `Cargo.toml` (add workspace member)
- Deps: `clap = { version = "4", features = ["derive"] }`, `tokio`, `tracing`, lib-erlangrt

---

## 4. Logging Setup

**Current state:** `tracing` macros used throughout agent_rt, but no subscriber initialization. No `tracing-subscriber` dependency.

**Changes:**

Initialize `tracing-subscriber` in `zeptoclaw-rtd` main:
- **Pretty format** (default, development): colored human-readable output
- **JSON format** (production): structured JSON lines to stdout
- **Compact format**: single-line logs without spans
- Level filter from config or CLI override
- `EnvFilter` support for fine-grained per-module control via `RUST_LOG`

**Files:**
- Modify: `zeptoclaw-rtd/src/main.rs` (subscriber init)
- Add dep: `tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }` to zeptoclaw-rtd/Cargo.toml

---

## 5. Health Server (`server.rs`)

**Current state:** No HTTP server. RuntimeMetrics and ProcessSnapshot structs exist with comprehensive data.

**Endpoints:**

| Endpoint | Method | Response |
|----------|--------|----------|
| `GET /health` | Liveness check | `200 {"status": "ok", "uptime_secs": N}` |
| `GET /metrics` | Runtime metrics | `200` JSON `RuntimeMetricsSnapshot` |
| `GET /status` | Full status | `200` JSON with process count, bridge status, checkpoint info |

**Implementation:**
- Axum router with shared `Arc<RuntimeMetrics>` state
- Runs on separate tokio task
- `HealthServer::start()` returns `JoinHandle` + `shutdown_tx` (oneshot channel)
- On shutdown signal, sends to `shutdown_tx`, server drains connections with 5s grace period

**Files:**
- Create: `lib-erlangrt/src/agent_rt/server.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`
- Add deps: `axum = "0.7"`, `tower = "0.4"` to lib-erlangrt/Cargo.toml

---

## 6. Signal Handling & Graceful Shutdown

**Current state:** No signal handling. RuntimeMetrics has `shutdown()` for bounded drain.

**Implementation (in zeptoclaw-rtd main):**

1. Register handlers: `tokio::signal::ctrl_c()` + `tokio::signal::unix::signal(SignalKind::terminate())`
2. On first signal:
   - Log "Shutting down gracefully..."
   - Set shutdown flag
   - Drain scheduler (bounded, 30s timeout)
   - Checkpoint all in-flight state
   - Stop bridge workers
   - Stop health server
   - Exit 0
3. On second signal during shutdown:
   - Log "Force shutting down"
   - Exit 1

**Key invariant:** No data loss on graceful shutdown — all in-flight tasks checkpointed before exit.

---

## 7. FileCheckpointStore Tests

**Current state:** FileCheckpointStore implemented in checkpoint.rs but has ZERO test coverage. InMemory has 1 test, SQLite has 3 tests.

**Tests to add:**
- `test_file_checkpoint_roundtrip` — save, load, verify data matches
- `test_file_checkpoint_overwrite` — save twice with same key, load returns latest
- `test_file_checkpoint_delete` — save then delete, load returns None
- `test_file_checkpoint_load_missing` — load nonexistent key returns None
- `test_file_checkpoint_key_sanitization` — keys with special chars get sanitized
- `test_file_checkpoint_atomic_write` — verify .tmp + rename pattern (no partial writes)

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/checkpoint.rs` (add tests in `#[cfg(test)]` block)

---

## 8. Checkpoint TTL & Pruning

**Current state:** No TTL concept. Checkpoints persist indefinitely.

**Changes:**

Extend `CheckpointStore` trait with optional methods (default impls that return empty/noop):

```rust
fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    Ok(vec![])
}

fn prune_before(&self, _timestamp_epoch_secs: i64) -> Result<u64, AgentRtError> {
    Ok(0)
}
```

**SqliteCheckpointStore::prune_before():**
```sql
DELETE FROM checkpoints WHERE updated_at < ?
```
Returns count of deleted rows.

**FileCheckpointStore::prune_before():**
Check file modification time, delete files older than threshold.

**Background pruning task (in daemon):**
- Spawns tokio task that runs every `prune_interval_secs`
- Calls `store.prune_before(now - ttl_hours * 3600)`
- Logs count of pruned checkpoints at INFO level

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/checkpoint.rs` (add trait methods, FileCheckpointStore impl)
- Modify: `lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs` (add SqliteCheckpointStore impl)
- Create: pruning task in `zeptoclaw-rtd/src/main.rs`

---

## New Dependencies

| Crate | Version | Target | Purpose |
|-------|---------|--------|---------|
| `thiserror` | 2 | lib-erlangrt | Structured error types |
| `toml` | 0.8 | lib-erlangrt | Config file parsing |
| `axum` | 0.7 | lib-erlangrt | Health/metrics HTTP server |
| `tower` | 0.4 | lib-erlangrt | Axum middleware |
| `clap` | 4, features=["derive"] | zeptoclaw-rtd | CLI argument parsing |
| `tracing-subscriber` | 0.3, features=["json","env-filter"] | zeptoclaw-rtd | Logging initialization |

---

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Binary crate | Separate `zeptoclaw-rtd` | Clean separation from BEAM emulator |
| Config format | TOML | Rust ecosystem standard, readable, supports sections |
| HTTP framework | axum | Tokio-native, lightweight, minimal API surface |
| Error handling | thiserror in agent_rt only | Don't touch working BEAM VM error system (RtErr) |
| Config override order | defaults < file < CLI args | Standard precedence, CLI wins |
| Health server scope | 3 endpoints | YAGNI — add Prometheus format later if needed |
| Checkpoint pruning | Background tokio task | Non-blocking, configurable interval |
| Signal handling | Two-signal pattern | First = graceful, second = force. Standard daemon behavior |
| Config file optional | All defaults work | Zero-config startup for development, file for production |
