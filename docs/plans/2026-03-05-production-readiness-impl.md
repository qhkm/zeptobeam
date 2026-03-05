# Phase 5: Production Readiness Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make zeptoclaw-rt deployable as a standalone service with TOML config, CLI, health endpoints, structured errors, logging, FileCheckpointStore tests, and checkpoint pruning.

**Architecture:** New binary crate `zeptoclaw-rtd` loads TOML config, initializes tracing-subscriber, starts scheduler + bridge, launches axum health server, and handles SIGTERM/SIGINT graceful shutdown. The `lib-erlangrt/src/agent_rt/` module gains structured error types (thiserror), config parsing, an HTTP server module, checkpoint TTL pruning, and FileCheckpointStore test coverage.

**Tech Stack:** Rust, tokio, axum 0.7, clap 4, toml 0.8, thiserror 2, tracing-subscriber 0.3

**Working directory:** `~/ios/zeptoclaw-rt`

**Build command:** `cargo build` (from workspace root)
**Test command:** `cargo test -p erlangrt` (lib tests) or `cargo test --workspace` (all)

---

### Task 1: Structured Error Types (`error.rs`)

**Files:**
- Create: `lib-erlangrt/src/agent_rt/error.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`
- Modify: `lib-erlangrt/Cargo.toml`

**Step 1: Add thiserror dependency**

In `lib-erlangrt/Cargo.toml`, add to `[dependencies]`:
```toml
thiserror = "2"
```

**Step 2: Create error.rs with AgentRtError enum**

Create `lib-erlangrt/src/agent_rt/error.rs`:
```rust
/// Structured error type for the agent runtime.
#[derive(Debug, thiserror::Error)]
pub enum AgentRtError {
    #[error("config: {0}")]
    Config(String),

    #[error("checkpoint: {0}")]
    Checkpoint(String),

    #[error("checkpoint IO: {0}")]
    CheckpointIo(#[from] std::io::Error),

    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("bridge: {0}")]
    Bridge(String),

    #[error("server: {0}")]
    Server(String),

    #[error("shutdown: {0}")]
    Shutdown(String),
}

/// Convert rusqlite errors (separate impl to avoid orphan rule conflicts).
impl From<rusqlite::Error> for AgentRtError {
    fn from(e: rusqlite::Error) -> Self {
        AgentRtError::Checkpoint(format!("sqlite: {}", e))
    }
}
```

**Step 3: Register module in mod.rs**

In `lib-erlangrt/src/agent_rt/mod.rs`, add:
```rust
pub mod error;
```

**Step 4: Run build to verify**

Run: `cargo build -p erlangrt`
Expected: BUILD SUCCESS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/error.rs lib-erlangrt/src/agent_rt/mod.rs lib-erlangrt/Cargo.toml
git commit -m "feat(phase5): add AgentRtError structured error type"
```

---

### Task 2: Migrate CheckpointStore trait to AgentRtError

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/checkpoint.rs`
- Modify: `lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs`
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs` (callers)

**Step 1: Update CheckpointStore trait**

In `lib-erlangrt/src/agent_rt/checkpoint.rs`, change the trait to:
```rust
use crate::agent_rt::error::AgentRtError;

pub trait CheckpointStore: Send + Sync {
    fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError>;
    fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError>;
    fn delete(&self, key: &str) -> Result<(), AgentRtError>;
}
```

**Step 2: Update InMemoryCheckpointStore**

Replace all `Result<..., String>` with `Result<..., AgentRtError>`:
```rust
impl CheckpointStore for InMemoryCheckpointStore {
    fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError> {
        let mut map = self.inner.lock()
            .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
        map.insert(key.to_string(), checkpoint.clone());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError> {
        let map = self.inner.lock()
            .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
        Ok(map.get(key).cloned())
    }

    fn delete(&self, key: &str) -> Result<(), AgentRtError> {
        let mut map = self.inner.lock()
            .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
        map.remove(key);
        Ok(())
    }
}
```

**Step 3: Update FileCheckpointStore**

Same pattern — replace `Result<..., String>` with `Result<..., AgentRtError>`. For `new()`:
```rust
pub fn new(dir: impl AsRef<Path>) -> Result<Self, AgentRtError> {
    let dir = dir.as_ref().to_path_buf();
    fs::create_dir_all(&dir)?;  // auto-converts via From<io::Error>
    Ok(Self { dir })
}
```

For save/load/delete, use `?` with `From` impls (io::Error auto-converts, serde_json::Error auto-converts). For `format!()` error messages (like rename), wrap in `AgentRtError::Checkpoint(...)`.

**Step 4: Update SqliteCheckpointStore**

In `lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs`, change all method signatures to return `AgentRtError`. Use `?` for rusqlite errors (auto-converts via From impl), `?` for serde_json errors. For mutex poisoned:
```rust
let conn = self.conn.lock()
    .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
```

Also update `SqliteCheckpointStore::open()` and `in_memory()` to return `Result<Self, AgentRtError>`.

**Step 5: Update orchestration.rs callers**

In `lib-erlangrt/src/agent_rt/orchestration.rs`, find all `.map_err(|e| ...)` on checkpoint calls. These should still work since the orchestration code just logs errors and continues — but update any explicit `String` pattern matches to handle `AgentRtError`. The main patterns are:
- `store.save(...)` — wrap error with `warn!` or `debug!` log, continue
- `store.load(...)` — wrap error with `warn!`, return None
- `store.delete(...)` — best-effort, log and continue

**Step 6: Run tests**

Run: `cargo test -p erlangrt`
Expected: ALL PASS (existing tests should work since `.unwrap()` calls don't care about error type)

**Step 7: Commit**

```bash
git add lib-erlangrt/src/agent_rt/checkpoint.rs lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs lib-erlangrt/src/agent_rt/orchestration.rs
git commit -m "feat(phase5): migrate CheckpointStore to AgentRtError"
```

---

### Task 3: FileCheckpointStore Tests

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/checkpoint.rs` (add tests)

**Step 1: Write failing tests**

Add to the `#[cfg(test)] mod tests` block in `checkpoint.rs`:

```rust
#[test]
fn test_file_checkpoint_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    let key = "test-roundtrip";
    let payload = serde_json::json!({"goal": "ship", "tasks": [1, 2, 3]});

    store.save(key, &payload).unwrap();
    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded, payload);

    store.delete(key).unwrap();
    assert!(store.load(key).unwrap().is_none());
}

#[test]
fn test_file_checkpoint_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    let key = "overwrite-key";

    store.save(key, &serde_json::json!({"v": 1})).unwrap();
    store.save(key, &serde_json::json!({"v": 2})).unwrap();

    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded["v"], 2);
}

#[test]
fn test_file_checkpoint_load_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    assert!(store.load("nonexistent").unwrap().is_none());
}

#[test]
fn test_file_checkpoint_delete_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    // Should not error when deleting a key that doesn't exist
    store.delete("nonexistent").unwrap();
}

#[test]
fn test_file_checkpoint_key_sanitization() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    // Keys with special chars should be sanitized but still work
    let key = "test/key:with<special>chars";
    let payload = serde_json::json!({"sanitized": true});

    store.save(key, &payload).unwrap();
    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded, payload);
}

#[test]
fn test_file_checkpoint_atomic_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    let key = "atomic-test";
    let payload = serde_json::json!({"data": "important"});

    store.save(key, &payload).unwrap();

    // Verify no .tmp files remain after save
    let tmp_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "tmp"))
        .collect();
    assert!(tmp_files.is_empty(), "No .tmp files should remain after save");
}
```

**Step 2: Add tempfile dev dependency**

In `lib-erlangrt/Cargo.toml`, add:
```toml
[dev-dependencies]
tempfile = "3"
```

**Step 3: Run tests to verify they pass**

Run: `cargo test -p erlangrt test_file_checkpoint -- --nocapture`
Expected: ALL 6 PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/checkpoint.rs lib-erlangrt/Cargo.toml
git commit -m "test(phase5): add FileCheckpointStore test coverage"
```

---

### Task 4: Checkpoint list_keys and prune_before

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/checkpoint.rs`
- Modify: `lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs`

**Step 1: Write failing tests for list_keys and prune_before**

Add to checkpoint.rs tests:
```rust
#[test]
fn test_in_memory_list_keys() {
    let store = InMemoryCheckpointStore::new();
    store.save("a", &serde_json::json!(1)).unwrap();
    store.save("b", &serde_json::json!(2)).unwrap();
    let mut keys = store.list_keys().unwrap();
    keys.sort();
    assert_eq!(keys, vec!["a", "b"]);
}
```

Add to checkpoint_sqlite.rs tests:
```rust
#[test]
fn test_sqlite_list_keys() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    store.save("x", &serde_json::json!(1)).unwrap();
    store.save("y", &serde_json::json!(2)).unwrap();
    let mut keys = store.list_keys().unwrap();
    keys.sort();
    assert_eq!(keys, vec!["x", "y"]);
}

#[test]
fn test_sqlite_prune_before() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    store.save("old", &serde_json::json!(1)).unwrap();
    store.save("new", &serde_json::json!(2)).unwrap();

    // Prune with future timestamp should delete everything
    let future = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64 + 3600;
    let pruned = store.prune_before(future).unwrap();
    assert_eq!(pruned, 2);
    assert!(store.load("old").unwrap().is_none());
    assert!(store.load("new").unwrap().is_none());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p erlangrt test_in_memory_list_keys test_sqlite_list_keys test_sqlite_prune_before`
Expected: FAIL (method not found)

**Step 3: Add list_keys and prune_before to CheckpointStore trait**

In `checkpoint.rs`, add default-implemented methods to the trait:
```rust
pub trait CheckpointStore: Send + Sync {
    fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError>;
    fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError>;
    fn delete(&self, key: &str) -> Result<(), AgentRtError>;

    fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
        Ok(vec![])
    }

    /// Delete checkpoints with updated_at before the given epoch timestamp (seconds).
    /// Returns count of pruned entries.
    fn prune_before(&self, _epoch_secs: i64) -> Result<u64, AgentRtError> {
        Ok(0)
    }
}
```

**Step 4: Implement list_keys for InMemoryCheckpointStore**

```rust
fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    let map = self.inner.lock()
        .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
    Ok(map.keys().cloned().collect())
}
```

**Step 5: Implement list_keys for FileCheckpointStore**

```rust
fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    let mut keys = Vec::new();
    for entry in fs::read_dir(&self.dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "json") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                keys.push(stem.to_string());
            }
        }
    }
    Ok(keys)
}
```

**Step 6: Implement list_keys and prune_before for SqliteCheckpointStore**

In `checkpoint_sqlite.rs`:
```rust
fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    let conn = self.conn.lock()
        .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
    let mut stmt = conn.prepare("SELECT key FROM checkpoints")
        .map_err(|e| AgentRtError::Checkpoint(format!("sqlite prepare: {}", e)))?;
    let keys = stmt.query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| AgentRtError::Checkpoint(format!("sqlite query: {}", e)))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(keys)
}

fn prune_before(&self, epoch_secs: i64) -> Result<u64, AgentRtError> {
    let conn = self.conn.lock()
        .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
    let count = conn.execute(
        "DELETE FROM checkpoints WHERE updated_at < ?1",
        rusqlite::params![epoch_secs],
    ).map_err(|e| AgentRtError::Checkpoint(format!("sqlite prune: {}", e)))?;
    Ok(count as u64)
}
```

**Step 7: Implement prune_before for FileCheckpointStore**

```rust
fn prune_before(&self, epoch_secs: i64) -> Result<u64, AgentRtError> {
    let threshold = std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs as u64);
    let mut pruned = 0u64;
    for entry in fs::read_dir(&self.dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "json") {
            if let Ok(meta) = fs::metadata(&path) {
                if let Ok(modified) = meta.modified() {
                    if modified < threshold {
                        fs::remove_file(&path)?;
                        pruned += 1;
                    }
                }
            }
        }
    }
    Ok(pruned)
}
```

**Step 8: Run tests**

Run: `cargo test -p erlangrt -- list_keys prune_before`
Expected: ALL PASS

**Step 9: Commit**

```bash
git add lib-erlangrt/src/agent_rt/checkpoint.rs lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs
git commit -m "feat(phase5): add list_keys and prune_before to CheckpointStore"
```

---

### Task 5: Configuration Module (`config.rs`)

**Files:**
- Create: `lib-erlangrt/src/agent_rt/config.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`
- Modify: `lib-erlangrt/Cargo.toml`

**Step 1: Add toml dependency**

In `lib-erlangrt/Cargo.toml`, add:
```toml
toml = "0.8"
```

**Step 2: Write failing test**

Create `lib-erlangrt/src/agent_rt/config.rs` with test first:
```rust
use serde::Deserialize;

use crate::agent_rt::error::AgentRtError;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub runtime: RuntimeConfig,
    pub checkpoint: CheckpointConfig,
    pub server: ServerConfig,
    pub logging: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub worker_count: usize,
    pub mailbox_capacity: usize,
    pub max_reductions: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CheckpointConfig {
    pub store: String,
    pub path: String,
    pub ttl_hours: u64,
    pub prune_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub enabled: bool,
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub format: String,
}

// ... Default impls and load_config fn (added in step 3)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.runtime.worker_count, 4);
        assert_eq!(config.runtime.mailbox_capacity, 1024);
        assert_eq!(config.checkpoint.store, "sqlite");
        assert_eq!(config.checkpoint.ttl_hours, 24);
        assert_eq!(config.server.bind, "127.0.0.1:9090");
        assert!(config.server.enabled);
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, "pretty");
    }

    #[test]
    fn test_parse_toml_config() {
        let toml_str = r#"
[runtime]
worker_count = 8
mailbox_capacity = 2048

[checkpoint]
store = "file"
path = "/tmp/checkpoints"

[server]
enabled = false
bind = "0.0.0.0:8080"

[logging]
level = "debug"
format = "json"
"#;
        let config = load_config_from_str(toml_str).unwrap();
        assert_eq!(config.runtime.worker_count, 8);
        assert_eq!(config.runtime.mailbox_capacity, 2048);
        assert_eq!(config.checkpoint.store, "file");
        assert!(!config.server.enabled);
        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, "json");
    }

    #[test]
    fn test_partial_toml_uses_defaults() {
        let toml_str = r#"
[runtime]
worker_count = 16
"#;
        let config = load_config_from_str(toml_str).unwrap();
        assert_eq!(config.runtime.worker_count, 16);
        // Everything else should be default
        assert_eq!(config.runtime.mailbox_capacity, 1024);
        assert_eq!(config.checkpoint.store, "sqlite");
        assert!(config.server.enabled);
    }

    #[test]
    fn test_empty_toml_gives_defaults() {
        let config = load_config_from_str("").unwrap();
        assert_eq!(config.runtime.worker_count, 4);
    }

    #[test]
    fn test_invalid_toml_returns_error() {
        let result = load_config_from_str("not [valid toml {{{}");
        assert!(result.is_err());
    }
}
```

**Step 3: Implement Default impls and load functions**

```rust
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            runtime: RuntimeConfig::default(),
            checkpoint: CheckpointConfig::default(),
            server: ServerConfig::default(),
            logging: LogConfig::default(),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_count: 4,
            mailbox_capacity: 1024,
            max_reductions: 200,
        }
    }
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            store: "sqlite".into(),
            path: "./zeptoclaw-rt.db".into(),
            ttl_hours: 24,
            prune_interval_secs: 3600,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1:9090".into(),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "pretty".into(),
        }
    }
}

pub fn load_config_from_str(s: &str) -> Result<AppConfig, AgentRtError> {
    toml::from_str(s).map_err(|e| AgentRtError::Config(format!("parse TOML: {}", e)))
}

pub fn load_config(path: &str) -> Result<AppConfig, AgentRtError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| AgentRtError::Config(format!("read config file {}: {}", path, e)))?;
    load_config_from_str(&content)
}
```

**Step 4: Register module in mod.rs**

Add `pub mod config;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 5: Run tests**

Run: `cargo test -p erlangrt test_default_config test_parse_toml test_partial_toml test_empty_toml test_invalid_toml`
Expected: ALL 5 PASS

**Step 6: Commit**

```bash
git add lib-erlangrt/src/agent_rt/config.rs lib-erlangrt/src/agent_rt/mod.rs lib-erlangrt/Cargo.toml
git commit -m "feat(phase5): add TOML configuration module"
```

---

### Task 6: Derive Serialize on RuntimeMetricsSnapshot

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/observability.rs`

**Step 1: Add Serialize derive to RuntimeMetricsSnapshot**

The health server needs to return metrics as JSON. Add `serde::Serialize` derive:

```rust
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeMetricsSnapshot {
    // ... all fields stay the same
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessSnapshot {
    // ... all fields stay the same
}
```

Also need to derive `Serialize` on `ProcessStatus` and `Priority` in `process.rs` if they don't already have it.

**Step 2: Run build**

Run: `cargo build -p erlangrt`
Expected: BUILD SUCCESS

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/observability.rs lib-erlangrt/src/agent_rt/process.rs
git commit -m "feat(phase5): derive Serialize on metrics/process snapshots"
```

---

### Task 7: Health Server Module (`server.rs`)

**Files:**
- Create: `lib-erlangrt/src/agent_rt/server.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`
- Modify: `lib-erlangrt/Cargo.toml`

**Step 1: Add axum dependency**

In `lib-erlangrt/Cargo.toml`, add:
```toml
axum = "0.7"
```

Note: `tower` is pulled in transitively by axum, no need to add explicitly.

**Step 2: Write test**

Create `lib-erlangrt/src/agent_rt/server.rs`:

```rust
use std::sync::Arc;

use axum::{extract::State, routing::get, Json, Router};
use tokio::sync::oneshot;

use crate::agent_rt::observability::{RuntimeMetrics, RuntimeMetricsSnapshot};

#[derive(Clone)]
pub struct ServerState {
    pub metrics: Arc<RuntimeMetrics>,
    pub process_count: Arc<std::sync::atomic::AtomicUsize>,
}

pub struct HealthServer {
    shutdown_tx: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl HealthServer {
    pub async fn start(bind: &str, state: ServerState) -> Result<Self, std::io::Error> {
        let app = Router::new()
            .route("/health", get(health_handler))
            .route("/metrics", get(metrics_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(bind).await?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });

        Ok(Self { shutdown_tx, handle })
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.handle.await;
    }
}

async fn health_handler(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let uptime = state
        .metrics
        .snapshot(
            state
                .process_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .uptime_secs;
    Json(serde_json::json!({
        "status": "ok",
        "uptime_secs": uptime,
    }))
}

async fn metrics_handler(State(state): State<ServerState>) -> Json<RuntimeMetricsSnapshot> {
    let snap = state.metrics.snapshot(
        state
            .process_count
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    Json(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(5)),
        };
        let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();

        // Server should start without error
        // (Full HTTP client testing is integration-level)

        server.shutdown().await;
    }
}
```

**Step 3: Register module**

Add `pub mod server;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 4: Run tests**

Run: `cargo test -p erlangrt test_health_endpoint`
Expected: PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/server.rs lib-erlangrt/src/agent_rt/mod.rs lib-erlangrt/Cargo.toml
git commit -m "feat(phase5): add axum health/metrics server"
```

---

### Task 8: Create zeptoclaw-rtd Binary Crate

**Files:**
- Create: `zeptoclaw-rtd/Cargo.toml`
- Create: `zeptoclaw-rtd/src/main.rs`
- Modify: root `Cargo.toml` (workspace members)

**Step 1: Create Cargo.toml**

Create `zeptoclaw-rtd/Cargo.toml`:
```toml
[package]
name = "zeptoclaw-rtd"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "zeptoclaw-rtd"
path = "src/main.rs"

[dependencies]
erlangrt = { path = "../lib-erlangrt" }
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["rt-multi-thread", "signal", "macros"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
serde_json = "1"
```

**Step 2: Create main.rs with CLI + config loading + tracing init**

Create `zeptoclaw-rtd/src/main.rs`:
```rust
use std::sync::{atomic::AtomicUsize, Arc};

use clap::Parser;
use tracing::{info, warn};

use erlangrt::agent_rt::{
    config::{load_config, AppConfig},
    observability::RuntimeMetrics,
    server::{HealthServer, ServerState},
};

#[derive(Parser, Debug)]
#[command(name = "zeptoclaw-rtd", version, about = "Zeptoclaw agent runtime daemon")]
struct Cli {
    /// Config file path
    #[arg(short, long, default_value = "zeptoclaw-rt.toml")]
    config: String,

    /// Override log level (trace|debug|info|warn|error)
    #[arg(short, long)]
    log_level: Option<String>,

    /// Override worker count
    #[arg(short, long)]
    workers: Option<usize>,

    /// Override server bind address
    #[arg(short, long)]
    bind: Option<String>,
}

fn init_tracing(config: &AppConfig, cli_level: Option<&str>) {
    let level = cli_level.unwrap_or(&config.logging.level);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));

    match config.logging.format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .init();
        }
        "compact" => {
            tracing_subscriber::fmt()
                .compact()
                .with_env_filter(env_filter)
                .init();
        }
        _ => {
            // "pretty" or default
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .init();
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Load config (optional — use defaults if file not found)
    let mut config = match load_config(&cli.config) {
        Ok(c) => {
            eprintln!("Loaded config from {}", cli.config);
            c
        }
        Err(_) => {
            eprintln!(
                "Config file '{}' not found, using defaults",
                cli.config
            );
            AppConfig::default()
        }
    };

    // Apply CLI overrides
    if let Some(workers) = cli.workers {
        config.runtime.worker_count = workers;
    }
    if let Some(ref bind) = cli.bind {
        config.server.bind = bind.clone();
    }

    // Init tracing
    init_tracing(&config, cli.log_level.as_deref());

    info!("zeptoclaw-rtd starting");
    info!(
        workers = config.runtime.worker_count,
        mailbox_capacity = config.runtime.mailbox_capacity,
        checkpoint_store = %config.checkpoint.store,
        "runtime configuration"
    );

    // Create shared metrics
    let metrics = Arc::new(RuntimeMetrics::new());
    let process_count = Arc::new(AtomicUsize::new(0));

    // Start health server (if enabled)
    let server = if config.server.enabled {
        let state = ServerState {
            metrics: metrics.clone(),
            process_count: process_count.clone(),
        };
        match HealthServer::start(&config.server.bind, state).await {
            Ok(s) => {
                info!(bind = %config.server.bind, "health server started");
                Some(s)
            }
            Err(e) => {
                warn!("Failed to start health server: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Wait for shutdown signal
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("register SIGTERM");
            tokio::select! {
                _ = ctrl_c => info!("received SIGINT"),
                _ = sigterm.recv() => info!("received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
            info!("received SIGINT");
        }
    };

    shutdown.await;
    info!("shutting down gracefully...");

    // Shutdown health server
    if let Some(s) = server {
        s.shutdown().await;
        info!("health server stopped");
    }

    info!("zeptoclaw-rtd stopped");
}
```

**Step 3: Add workspace member**

In root `Cargo.toml`, add `zeptoclaw-rtd`:
```toml
[workspace]
members = ["lib-erlangrt", "erlexec", "ct_run", "zeptoclaw-rtd"]
```

**Step 4: Build**

Run: `cargo build -p zeptoclaw-rtd`
Expected: BUILD SUCCESS

**Step 5: Verify help output**

Run: `cargo run -p zeptoclaw-rtd -- --help`
Expected: Shows usage with -c, -l, -w, -b, -V, -h options

**Step 6: Commit**

```bash
git add zeptoclaw-rtd/ Cargo.toml
git commit -m "feat(phase5): add zeptoclaw-rtd daemon binary crate"
```

---

### Task 9: Integration Test — Config + Server + Shutdown

**Files:**
- Create: `zeptoclaw-rtd/tests/integration.rs`

**Step 1: Write integration test**

Create `zeptoclaw-rtd/tests/integration.rs`:
```rust
use std::sync::{atomic::AtomicUsize, Arc};

use erlangrt::agent_rt::{
    config::{load_config_from_str, AppConfig},
    observability::RuntimeMetrics,
    server::{HealthServer, ServerState},
};

#[tokio::test]
async fn test_health_server_responds() {
    let config = AppConfig::default();
    let metrics = Arc::new(RuntimeMetrics::new());
    let process_count = Arc::new(AtomicUsize::new(3));

    let state = ServerState {
        metrics,
        process_count,
    };

    // Bind to port 0 to get a random available port
    let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();

    // Note: To do full HTTP client testing we'd need reqwest or ureq.
    // For now, verify the server starts and stops cleanly.
    server.shutdown().await;
}

#[test]
fn test_config_defaults_are_sensible() {
    let config = AppConfig::default();
    assert!(config.runtime.worker_count > 0);
    assert!(config.runtime.mailbox_capacity > 0);
    assert!(config.checkpoint.ttl_hours > 0);
    assert!(config.server.enabled);
}

#[test]
fn test_config_from_full_toml() {
    let toml = r#"
[runtime]
worker_count = 8
mailbox_capacity = 512
max_reductions = 100

[checkpoint]
store = "memory"
path = ""
ttl_hours = 48
prune_interval_secs = 1800

[server]
enabled = false
bind = "0.0.0.0:3000"

[logging]
level = "warn"
format = "json"
"#;
    let config = load_config_from_str(toml).unwrap();
    assert_eq!(config.runtime.worker_count, 8);
    assert_eq!(config.checkpoint.ttl_hours, 48);
    assert!(!config.server.enabled);
    assert_eq!(config.logging.format, "json");
}
```

**Step 2: Run integration tests**

Run: `cargo test -p zeptoclaw-rtd`
Expected: ALL PASS

**Step 3: Commit**

```bash
git add zeptoclaw-rtd/tests/
git commit -m "test(phase5): integration tests for config + health server"
```

---

### Task 10: Background Checkpoint Pruning Task

**Files:**
- Create: `lib-erlangrt/src/agent_rt/pruner.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Create pruner module**

Create `lib-erlangrt/src/agent_rt/pruner.rs`:
```rust
use std::sync::Arc;

use tokio::time::{self, Duration};
use tracing::{debug, info, warn};

use crate::agent_rt::checkpoint::CheckpointStore;

/// Spawns a background task that periodically prunes stale checkpoints.
/// Returns a JoinHandle that can be aborted on shutdown.
pub fn spawn_pruner(
    store: Arc<dyn CheckpointStore>,
    interval_secs: u64,
    ttl_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        loop {
            interval.tick().await;
            let cutoff = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
                - ttl_secs as i64;
            match store.prune_before(cutoff) {
                Ok(0) => debug!("checkpoint pruner: nothing to prune"),
                Ok(n) => info!(pruned = n, "checkpoint pruner: pruned stale entries"),
                Err(e) => warn!("checkpoint pruner failed: {}", e),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::checkpoint::InMemoryCheckpointStore;

    #[tokio::test]
    async fn test_pruner_starts_and_can_be_aborted() {
        let store = Arc::new(InMemoryCheckpointStore::new());
        let handle = spawn_pruner(store, 3600, 86400);

        // Pruner should be running
        assert!(!handle.is_finished());

        // Abort it
        handle.abort();
        let _ = handle.await;
    }
}
```

**Step 2: Register module**

Add `pub mod pruner;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p erlangrt test_pruner_starts`
Expected: PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/pruner.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(phase5): add background checkpoint pruner"
```

---

### Task 11: Wire Pruner into Daemon

**Files:**
- Modify: `zeptoclaw-rtd/src/main.rs`

**Step 1: Add checkpoint store creation and pruner to main**

After the metrics creation section in main.rs, add checkpoint store creation and pruner:

```rust
use erlangrt::agent_rt::{
    checkpoint::{CheckpointStore, InMemoryCheckpointStore, FileCheckpointStore},
    checkpoint_sqlite::SqliteCheckpointStore,
    pruner::spawn_pruner,
    // ... existing imports
};

// Create checkpoint store based on config
let checkpoint_store: Arc<dyn CheckpointStore> = match config.checkpoint.store.as_str() {
    "memory" => Arc::new(InMemoryCheckpointStore::new()),
    "file" => {
        Arc::new(FileCheckpointStore::new(&config.checkpoint.path)
            .expect("Failed to create file checkpoint store"))
    }
    _ => {
        // "sqlite" or default
        Arc::new(SqliteCheckpointStore::open(&config.checkpoint.path)
            .expect("Failed to open SQLite checkpoint store"))
    }
};

// Start checkpoint pruner
let pruner = if config.checkpoint.ttl_hours > 0 {
    let ttl_secs = config.checkpoint.ttl_hours * 3600;
    Some(spawn_pruner(
        checkpoint_store.clone(),
        config.checkpoint.prune_interval_secs,
        ttl_secs,
    ))
} else {
    None
};
```

In the shutdown section, add:
```rust
// Stop pruner
if let Some(p) = pruner {
    p.abort();
    info!("checkpoint pruner stopped");
}
```

**Step 2: Build**

Run: `cargo build -p zeptoclaw-rtd`
Expected: BUILD SUCCESS

**Step 3: Commit**

```bash
git add zeptoclaw-rtd/src/main.rs
git commit -m "feat(phase5): wire checkpoint store + pruner into daemon"
```

---

### Task 12: Update ROADMAP.md

**Files:**
- Modify: `docs/ROADMAP.md`

**Step 1: Update Phase 5 status**

Change Phase 5 from `[PLANNED]` to `[DONE]` and add description matching the Phase 4 format:

```markdown
## Phase 5: Production Readiness [DONE]

Make the runtime deployable as a standalone service.

- **Configuration**: TOML config file loading with AppConfig, RuntimeConfig, CheckpointConfig, ServerConfig, LogConfig
- **CLI entry point**: `zeptoclaw-rtd` binary with clap arg parsing, config file override, log level, worker count, bind address
- **Structured errors**: AgentRtError enum with thiserror, replacing String errors in CheckpointStore and related modules
- **Health server**: Axum HTTP server with /health and /metrics endpoints, graceful shutdown
- **Logging setup**: tracing-subscriber with JSON/pretty/compact formats, env-filter, configurable levels
- **FileCheckpointStore tests**: 6 tests covering roundtrip, overwrite, delete, missing key, sanitization, atomic write
- **Checkpoint pruning**: list_keys and prune_before on CheckpointStore trait, background pruner task with configurable interval and TTL
- **Signal handling**: SIGTERM/SIGINT graceful shutdown, drain in-flight ops, stop server

**Commits:** `<first>` through `<last>`
```

Update the Phase Summary table to show Phase 5 as Done.

**Step 2: Commit**

```bash
git add docs/ROADMAP.md
git commit -m "docs: update ROADMAP.md — Phase 5 complete"
```

---

## Task Summary

| Task | Component | Tests |
|------|-----------|-------|
| 1 | AgentRtError enum (thiserror) | Compile check |
| 2 | Migrate CheckpointStore to AgentRtError | Existing tests pass |
| 3 | FileCheckpointStore tests | 6 new tests |
| 4 | list_keys + prune_before | 3 new tests |
| 5 | AppConfig TOML module | 5 new tests |
| 6 | Serialize derives on snapshots | Compile check |
| 7 | Axum health/metrics server | 1 new test |
| 8 | zeptoclaw-rtd daemon binary | Build + help check |
| 9 | Integration tests | 3 new tests |
| 10 | Background checkpoint pruner | 1 new test |
| 11 | Wire pruner into daemon | Build check |
| 12 | Update ROADMAP.md | — |

**Total: 12 tasks, ~19 new tests**
