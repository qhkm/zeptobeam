# A1/G2: ObjectRef + Artifact Plane — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add durable artifact storage with typed ObjectRef handles, enabling processes to store/retrieve/delete large artifacts via explicit effects.

**Architecture:** `ObjectRef` metadata handle in `core/object.rs`. `ArtifactBackend` trait + `SqliteArtifactStore` in `durability/artifact_store.rs`. Reactor handles `ObjectPut`/`ObjectFetch`/`ObjectDelete` effects. Runtime wires store + TTL sweep.

**Tech Stack:** Rust, ZeptoVM (core/object.rs, durability/artifact_store.rs, kernel/reactor.rs, kernel/runtime.rs, kernel/metrics.rs), sha2 crate, rusqlite

---

### Task 1: Add `sha2` dependency and create `ObjectId` + `ObjectRef` types

**Files:**
- Modify: `zeptovm/Cargo.toml` (dependencies section, ~line 20)
- Create: `zeptovm/src/core/object.rs`
- Modify: `zeptovm/src/core/mod.rs:1-7` (add `pub mod object;`)

**Step 1: Add sha2 to Cargo.toml**

In `zeptovm/Cargo.toml`, add after the existing dependencies:

```toml
sha2 = "0.10"
```

**Step 2: Create `core/object.rs` with ObjectId and ObjectRef**

```rust
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

/// Unique identifier for a stored artifact.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, Hash,
  Serialize, Deserialize,
)]
pub struct ObjectId(u64);

impl ObjectId {
  pub fn new() -> Self {
    Self(
      NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
    )
  }

  pub fn from_raw(id: u64) -> Self {
    Self(id)
  }

  pub fn raw(&self) -> u64 {
    self.0
  }
}

/// Metadata handle for a stored artifact.
/// Lightweight — passes in messages without the payload.
#[derive(
  Debug, Clone, PartialEq, Serialize, Deserialize,
)]
pub struct ObjectRef {
  pub object_id: ObjectId,
  pub content_hash: String,
  pub media_type: String,
  pub size_bytes: u64,
  pub created_at_ms: u64,
  pub ttl_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_object_id_monotonic() {
    let a = ObjectId::new();
    let b = ObjectId::new();
    assert!(b.raw() > a.raw());
  }

  #[test]
  fn test_object_id_from_raw() {
    let id = ObjectId::from_raw(42);
    assert_eq!(id.raw(), 42);
  }

  #[test]
  fn test_object_ref_serializable() {
    let r = ObjectRef {
      object_id: ObjectId::from_raw(1),
      content_hash: "abc123".into(),
      media_type: "text/plain".into(),
      size_bytes: 100,
      created_at_ms: 1000,
      ttl_ms: Some(5000),
    };
    let json = serde_json::to_string(&r).unwrap();
    let deser: ObjectRef =
      serde_json::from_str(&json).unwrap();
    assert_eq!(deser, r);
  }

  #[test]
  fn test_object_ref_no_ttl() {
    let r = ObjectRef {
      object_id: ObjectId::from_raw(1),
      content_hash: "abc".into(),
      media_type: "application/octet-stream".into(),
      size_bytes: 0,
      created_at_ms: 0,
      ttl_ms: None,
    };
    assert!(r.ttl_ms.is_none());
  }
}
```

**Step 3: Add module to core/mod.rs**

Add `pub mod object;` to `zeptovm/src/core/mod.rs`.

**Step 4: Run tests**

Run: `cargo test -p zeptovm --lib -- object::tests`
Expected: All 4 tests pass

**Step 5: Commit**

```bash
git add zeptovm/Cargo.toml zeptovm/src/core/object.rs zeptovm/src/core/mod.rs
git commit -m "feat(core): add ObjectId and ObjectRef types with sha2 dependency"
```

---

### Task 2: Create `ArtifactBackend` trait and `SqliteArtifactStore`

**Files:**
- Create: `zeptovm/src/durability/artifact_store.rs`
- Modify: `zeptovm/src/durability/mod.rs:1-7` (add `pub mod artifact_store;`)

**Step 1: Create `durability/artifact_store.rs`**

```rust
use crate::core::object::{ObjectId, ObjectRef};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

/// Backend trait for artifact storage.
pub trait ArtifactBackend: Send + Sync {
  /// Store bytes, return an ObjectRef handle.
  fn store(
    &self,
    data: &[u8],
    media_type: &str,
    ttl_ms: Option<u64>,
  ) -> Result<ObjectRef, String>;

  /// Retrieve artifact bytes and metadata by ObjectId.
  fn retrieve(
    &self,
    object_id: &ObjectId,
  ) -> Result<(ObjectRef, Vec<u8>), String>;

  /// Delete an artifact. Returns true if it existed.
  fn delete(
    &self,
    object_id: &ObjectId,
  ) -> Result<bool, String>;

  /// Remove expired artifacts. Returns count removed.
  fn cleanup_expired(&self) -> Result<u64, String>;
}

/// SQLite-backed artifact store.
pub struct SqliteArtifactStore {
  conn: Connection,
}

impl SqliteArtifactStore {
  pub fn open(path: &str) -> Result<Self, String> {
    let conn = Connection::open(path)
      .map_err(|e| format!("artifact store open: {e}"))?;
    let store = Self { conn };
    store.init_schema()?;
    Ok(store)
  }

  pub fn open_in_memory() -> Result<Self, String> {
    let conn = Connection::open_in_memory()
      .map_err(|e| format!("artifact store open: {e}"))?;
    let store = Self { conn };
    store.init_schema()?;
    Ok(store)
  }

  fn init_schema(&self) -> Result<(), String> {
    self.conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS artifacts (
          object_id INTEGER PRIMARY KEY,
          content_hash TEXT NOT NULL,
          media_type TEXT NOT NULL,
          size_bytes INTEGER NOT NULL,
          data BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          expires_at INTEGER
        );",
      )
      .map_err(|e| format!("schema init: {e}"))?;
    Ok(())
  }

  fn now_ms() -> u64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_millis() as u64
  }
}

impl ArtifactBackend for SqliteArtifactStore {
  fn store(
    &self,
    data: &[u8],
    media_type: &str,
    ttl_ms: Option<u64>,
  ) -> Result<ObjectRef, String> {
    let object_id = ObjectId::new();
    let hash = format!("{:x}", Sha256::digest(data));
    let now = Self::now_ms();
    let expires_at =
      ttl_ms.map(|ttl| now + ttl);

    self.conn
      .execute(
        "INSERT INTO artifacts
         (object_id, content_hash, media_type,
          size_bytes, data, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
          object_id.raw() as i64,
          hash,
          media_type,
          data.len() as i64,
          data,
          now as i64,
          expires_at.map(|e| e as i64),
        ],
      )
      .map_err(|e| format!("store: {e}"))?;

    Ok(ObjectRef {
      object_id,
      content_hash: hash,
      media_type: media_type.to_string(),
      size_bytes: data.len() as u64,
      created_at_ms: now,
      ttl_ms,
    })
  }

  fn retrieve(
    &self,
    object_id: &ObjectId,
  ) -> Result<(ObjectRef, Vec<u8>), String> {
    let mut stmt = self
      .conn
      .prepare(
        "SELECT content_hash, media_type, size_bytes,
                data, created_at, expires_at
         FROM artifacts WHERE object_id = ?1",
      )
      .map_err(|e| format!("retrieve prepare: {e}"))?;

    let result = stmt
      .query_row(
        params![object_id.raw() as i64],
        |row| {
          let hash: String = row.get(0)?;
          let media: String = row.get(1)?;
          let size: i64 = row.get(2)?;
          let data: Vec<u8> = row.get(3)?;
          let created: i64 = row.get(4)?;
          let expires: Option<i64> = row.get(5)?;
          Ok((hash, media, size, data, created, expires))
        },
      )
      .map_err(|_| {
        format!(
          "object not found: {}",
          object_id.raw()
        )
      })?;

    let (hash, media, size, data, created, expires) =
      result;
    let ttl_ms = expires
      .map(|e| (e - created).max(0) as u64);

    let obj_ref = ObjectRef {
      object_id: *object_id,
      content_hash: hash,
      media_type: media,
      size_bytes: size as u64,
      created_at_ms: created as u64,
      ttl_ms,
    };
    Ok((obj_ref, data))
  }

  fn delete(
    &self,
    object_id: &ObjectId,
  ) -> Result<bool, String> {
    let rows = self
      .conn
      .execute(
        "DELETE FROM artifacts WHERE object_id = ?1",
        params![object_id.raw() as i64],
      )
      .map_err(|e| format!("delete: {e}"))?;
    Ok(rows > 0)
  }

  fn cleanup_expired(&self) -> Result<u64, String> {
    let now = Self::now_ms() as i64;
    let rows = self
      .conn
      .execute(
        "DELETE FROM artifacts
         WHERE expires_at IS NOT NULL
           AND expires_at < ?1",
        params![now],
      )
      .map_err(|e| format!("cleanup: {e}"))?;
    Ok(rows as u64)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_store_and_retrieve() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let data = b"hello world";
    let obj_ref =
      store.store(data, "text/plain", None).unwrap();

    assert_eq!(obj_ref.media_type, "text/plain");
    assert_eq!(obj_ref.size_bytes, 11);
    assert!(obj_ref.ttl_ms.is_none());
    assert!(!obj_ref.content_hash.is_empty());

    let (retrieved_ref, retrieved_data) =
      store.retrieve(&obj_ref.object_id).unwrap();
    assert_eq!(retrieved_data, data);
    assert_eq!(retrieved_ref.content_hash, obj_ref.content_hash);
  }

  #[test]
  fn test_retrieve_nonexistent() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let result =
      store.retrieve(&ObjectId::from_raw(9999));
    assert!(result.is_err());
    assert!(
      result.unwrap_err().contains("object not found")
    );
  }

  #[test]
  fn test_delete_existing() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref =
      store.store(b"data", "text/plain", None).unwrap();
    let existed =
      store.delete(&obj_ref.object_id).unwrap();
    assert!(existed);

    let result =
      store.retrieve(&obj_ref.object_id);
    assert!(result.is_err());
  }

  #[test]
  fn test_delete_nonexistent_is_ok() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let existed =
      store.delete(&ObjectId::from_raw(9999)).unwrap();
    assert!(!existed);
  }

  #[test]
  fn test_cleanup_expired() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    // Store with ttl_ms=0 (expires immediately)
    let obj_ref =
      store.store(b"temp", "text/plain", Some(0)).unwrap();

    // Small sleep to ensure expiry
    std::thread::sleep(
      std::time::Duration::from_millis(10),
    );

    let removed = store.cleanup_expired().unwrap();
    assert_eq!(removed, 1);

    let result =
      store.retrieve(&obj_ref.object_id);
    assert!(result.is_err());
  }

  #[test]
  fn test_no_ttl_survives_cleanup() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref =
      store.store(b"permanent", "text/plain", None).unwrap();

    let removed = store.cleanup_expired().unwrap();
    assert_eq!(removed, 0);

    let (_, data) =
      store.retrieve(&obj_ref.object_id).unwrap();
    assert_eq!(data, b"permanent");
  }

  #[test]
  fn test_content_hash_is_sha256() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref =
      store.store(b"test", "text/plain", None).unwrap();
    // SHA-256 of "test" is well-known
    let expected = format!(
      "{:x}",
      Sha256::digest(b"test")
    );
    assert_eq!(obj_ref.content_hash, expected);
  }
}
```

**Step 2: Add module to durability/mod.rs**

Add `pub mod artifact_store;` to `zeptovm/src/durability/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm --lib -- artifact_store::tests`
Expected: All 7 tests pass

**Step 4: Commit**

```bash
git add zeptovm/src/durability/artifact_store.rs zeptovm/src/durability/mod.rs
git commit -m "feat(durability): add ArtifactBackend trait and SqliteArtifactStore"
```

---

### Task 3: Add `ObjectDelete` to EffectKind

**Files:**
- Modify: `zeptovm/src/core/effect.rs:34-51` (EffectKind enum)

**Step 1: Add ObjectDelete variant**

After `ObjectPut` (line 48), add:

```rust
ObjectDelete,
```

**Step 2: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass (ObjectDelete just adds an enum variant)

**Step 3: Commit**

```bash
git add zeptovm/src/core/effect.rs
git commit -m "feat(effect): add ObjectDelete EffectKind variant"
```

---

### Task 4: Add artifact metrics

**Files:**
- Modify: `zeptovm/src/kernel/metrics.rs:17-36` (pre-registered counters array)

**Step 1: Add artifact counter names**

In the counters array (lines 17-34), add:

```rust
"artifacts.stored",
"artifacts.fetched",
"artifacts.deleted",
"artifacts.expired",
```

**Step 2: Add test**

```rust
#[test]
fn test_metrics_artifact_counters() {
    let m = Metrics::new();
    m.inc("artifacts.stored");
    m.inc("artifacts.fetched");
    m.inc("artifacts.deleted");
    m.inc("artifacts.expired");
    assert_eq!(m.get("artifacts.stored"), 1);
    assert_eq!(m.get("artifacts.fetched"), 1);
    assert_eq!(m.get("artifacts.deleted"), 1);
    assert_eq!(m.get("artifacts.expired"), 1);
}
```

**Step 3: Run tests**

Run: `cargo test -p zeptovm --lib -- metrics`
Expected: All tests pass

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/metrics.rs
git commit -m "feat(metrics): add artifact counter pre-registrations"
```

---

### Task 5: Wire artifact store into Reactor

**Files:**
- Modify: `zeptovm/src/kernel/reactor.rs:23-73` (ReactorConfig)
- Modify: `zeptovm/src/kernel/reactor.rs:428-570` (execute_effect_once_configured)

**Step 1: Add artifact_store to ReactorConfig**

Add field to `ReactorConfig` struct:

```rust
pub artifact_store: Option<Arc<dyn ArtifactBackend>>,
```

Add import:
```rust
use crate::durability::artifact_store::ArtifactBackend;
use std::sync::Arc;
```

Initialize to `None` in both `from_provider_config()` and `placeholder()`.

**Step 2: Pass artifact_store through to execute functions**

Change `execute_effect_once_configured` signature to include config (it already receives `&ReactorConfig`). No signature change needed — just use `config.artifact_store` inside the match.

**Step 3: Add ObjectPut handler**

In `execute_effect_once_configured`, before the `other =>` fallback arm, add:

```rust
EffectKind::ObjectPut => {
  if let Some(ref store) = config.artifact_store {
    let data_b64 = request.input
      .get("data_base64")
      .and_then(|v| v.as_str())
      .unwrap_or("");
    let media_type = request.input
      .get("media_type")
      .and_then(|v| v.as_str())
      .unwrap_or("application/octet-stream");
    let ttl_ms = request.input
      .get("ttl_ms")
      .and_then(|v| v.as_u64());

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
      .decode(data_b64)
      .unwrap_or_default();

    match store.store(&data, media_type, ttl_ms) {
      Ok(obj_ref) => EffectResult::success(
        request.effect_id,
        serde_json::to_value(&obj_ref)
          .unwrap_or_default(),
      ),
      Err(e) => EffectResult::failed(
        request.effect_id,
        e,
      ),
    }
  } else {
    EffectResult::failed(
      request.effect_id,
      "no artifact store configured".into(),
    )
  }
}
```

Note: Check if `base64` crate is available. If not, add it to Cargo.toml. Alternatively, accept raw JSON bytes instead of base64. Check what makes sense for the existing codebase.

Actually, simpler approach — accept `data` as a JSON array of bytes or a JSON string, avoiding base64 dependency:

```rust
EffectKind::ObjectPut => {
  if let Some(ref store) = config.artifact_store {
    let media_type = request.input
      .get("media_type")
      .and_then(|v| v.as_str())
      .unwrap_or("application/octet-stream");
    let ttl_ms = request.input
      .get("ttl_ms")
      .and_then(|v| v.as_u64());
    // Store the entire input JSON as the artifact payload
    let data = serde_json::to_vec(&request.input)
      .unwrap_or_default();

    match store.store(&data, media_type, ttl_ms) {
      Ok(obj_ref) => EffectResult::success(
        request.effect_id,
        serde_json::to_value(&obj_ref)
          .unwrap_or_default(),
      ),
      Err(e) => EffectResult::failed(
        request.effect_id,
        e,
      ),
    }
  } else {
    EffectResult::failed(
      request.effect_id,
      "no artifact store configured".into(),
    )
  }
}
```

**Step 4: Add ObjectFetch handler**

```rust
EffectKind::ObjectFetch => {
  if let Some(ref store) = config.artifact_store {
    let obj_id = request.input
      .get("object_id")
      .and_then(|v| v.as_u64())
      .unwrap_or(0);

    match store.retrieve(
      &crate::core::object::ObjectId::from_raw(obj_id),
    ) {
      Ok((obj_ref, data)) => {
        let data_json: serde_json::Value =
          serde_json::from_slice(&data)
            .unwrap_or(serde_json::Value::Null);
        EffectResult::success(
          request.effect_id,
          serde_json::json!({
            "ref": obj_ref,
            "data": data_json,
          }),
        )
      }
      Err(e) => EffectResult::failed(
        request.effect_id,
        e,
      ),
    }
  } else {
    EffectResult::failed(
      request.effect_id,
      "no artifact store configured".into(),
    )
  }
}
```

**Step 5: Add ObjectDelete handler**

```rust
EffectKind::ObjectDelete => {
  if let Some(ref store) = config.artifact_store {
    let obj_id = request.input
      .get("object_id")
      .and_then(|v| v.as_u64())
      .unwrap_or(0);

    match store.delete(
      &crate::core::object::ObjectId::from_raw(obj_id),
    ) {
      Ok(existed) => EffectResult::success(
        request.effect_id,
        serde_json::json!({ "deleted": existed }),
      ),
      Err(e) => EffectResult::failed(
        request.effect_id,
        e,
      ),
    }
  } else {
    EffectResult::failed(
      request.effect_id,
      "no artifact store configured".into(),
    )
  }
}
```

**Step 6: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 7: Commit**

```bash
git add zeptovm/Cargo.toml zeptovm/src/kernel/reactor.rs
git commit -m "feat(reactor): implement ObjectPut/ObjectFetch/ObjectDelete handlers"
```

---

### Task 6: Wire artifact store into Runtime + TTL sweep

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs:61-77` (SchedulerRuntime struct)
- Modify: `zeptovm/src/kernel/runtime.rs:81-127` (constructors)
- Modify: `zeptovm/src/kernel/runtime.rs` (tick method)

**Step 1: Add artifact_store field to SchedulerRuntime**

After `idempotency_store` field (~line 68), add:

```rust
artifact_store: Option<Arc<dyn ArtifactBackend>>,
artifact_sweep_counter: u64,
```

Add imports:
```rust
use crate::durability::artifact_store::ArtifactBackend;
use std::sync::Arc;
```

Initialize both to `None` / `0` in `new()` and `with_durability()`.

**Step 2: Add setter method**

```rust
pub fn with_artifact_store(
    mut self,
    store: Arc<dyn ArtifactBackend>,
) -> Self {
    self.artifact_store = Some(store);
    self
}
```

**Step 3: Pass artifact_store to ReactorConfig**

In the method that constructs the reactor (find where `ReactorConfig::from_provider_config` is called), set:

```rust
config.artifact_store = self.artifact_store.clone();
```

If the reactor is constructed in `start_reactor()` or similar, add the wiring there.

**Step 4: Add TTL sweep to tick()**

After the existing metrics drain section in `tick()`, add:

```rust
// Artifact TTL sweep (every 100 ticks)
self.artifact_sweep_counter += 1;
if self.artifact_sweep_counter >= 100 {
    self.artifact_sweep_counter = 0;
    if let Some(ref store) = self.artifact_store {
        let removed =
            store.cleanup_expired().unwrap_or(0);
        if removed > 0 {
            self.metrics.inc_by(
                "artifacts.expired",
                removed,
            );
        }
    }
}
```

**Step 5: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 6: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire artifact store with TTL sweep in tick()"
```

---

### Task 7: Integration test — full ObjectPut/Fetch/Delete round-trip

**Files:**
- Modify: `zeptovm/src/kernel/reactor.rs` (test module)

**Step 1: Add integration tests**

In the reactor's `#[cfg(test)] mod tests` block, add:

```rust
#[test]
fn test_reactor_object_put_and_fetch() {
    use crate::durability::artifact_store::SqliteArtifactStore;

    let store = Arc::new(
        SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let mut config = ReactorConfig::placeholder();
    config.artifact_store = Some(store);

    let reactor = Reactor::start_with_config(
        Some(config),
    );

    // ObjectPut
    let put_req = EffectRequest::new(
        EffectKind::ObjectPut,
        serde_json::json!({
            "media_type": "application/json",
            "data": {"key": "value"},
        }),
    );
    let pid = Pid::from_raw(1);
    reactor.dispatch(pid, put_req);
    std::thread::sleep(
        std::time::Duration::from_millis(100),
    );

    let messages = reactor.drain_messages();
    let put_completion = messages.iter().find_map(|m| {
        if let ReactorMessage::Completion(c) = m {
            Some(c)
        } else {
            None
        }
    });
    assert!(put_completion.is_some());
    let put_result = &put_completion.unwrap().result;
    assert_eq!(put_result.status, EffectStatus::Succeeded);

    let obj_id = put_result.output
        .as_ref()
        .and_then(|v| v.get("object_id"))
        .and_then(|v| v.as_u64())
        .expect("should have object_id");

    // ObjectFetch
    let fetch_req = EffectRequest::new(
        EffectKind::ObjectFetch,
        serde_json::json!({ "object_id": obj_id }),
    );
    reactor.dispatch(pid, fetch_req);
    std::thread::sleep(
        std::time::Duration::from_millis(100),
    );

    let messages = reactor.drain_messages();
    let fetch_completion = messages.iter().find_map(|m| {
        if let ReactorMessage::Completion(c) = m {
            Some(c)
        } else {
            None
        }
    });
    assert!(fetch_completion.is_some());
    let fetch_result = &fetch_completion.unwrap().result;
    assert_eq!(
        fetch_result.status,
        EffectStatus::Succeeded
    );
    assert!(
        fetch_result.output
            .as_ref()
            .and_then(|v| v.get("ref"))
            .is_some()
    );
}

#[test]
fn test_reactor_object_delete() {
    use crate::durability::artifact_store::SqliteArtifactStore;

    let store = Arc::new(
        SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let mut config = ReactorConfig::placeholder();
    config.artifact_store = Some(store.clone());

    let reactor = Reactor::start_with_config(
        Some(config),
    );

    // Store an artifact first
    let put_req = EffectRequest::new(
        EffectKind::ObjectPut,
        serde_json::json!({
            "media_type": "text/plain",
        }),
    );
    let pid = Pid::from_raw(1);
    reactor.dispatch(pid, put_req);
    std::thread::sleep(
        std::time::Duration::from_millis(100),
    );

    let messages = reactor.drain_messages();
    let obj_id = messages.iter().find_map(|m| {
        if let ReactorMessage::Completion(c) = m {
            c.result.output.as_ref()
                .and_then(|v| v.get("object_id"))
                .and_then(|v| v.as_u64())
        } else {
            None
        }
    }).expect("should get object_id from put");

    // ObjectDelete
    let del_req = EffectRequest::new(
        EffectKind::ObjectDelete,
        serde_json::json!({ "object_id": obj_id }),
    );
    reactor.dispatch(pid, del_req);
    std::thread::sleep(
        std::time::Duration::from_millis(100),
    );

    let messages = reactor.drain_messages();
    let del_completion = messages.iter().find_map(|m| {
        if let ReactorMessage::Completion(c) = m {
            Some(c)
        } else {
            None
        }
    });
    assert!(del_completion.is_some());
    let del_result = &del_completion.unwrap().result;
    assert_eq!(
        del_result.status,
        EffectStatus::Succeeded
    );

    // Verify fetch fails
    let fetch_req = EffectRequest::new(
        EffectKind::ObjectFetch,
        serde_json::json!({ "object_id": obj_id }),
    );
    reactor.dispatch(pid, fetch_req);
    std::thread::sleep(
        std::time::Duration::from_millis(100),
    );

    let messages = reactor.drain_messages();
    let fetch_result = messages.iter().find_map(|m| {
        if let ReactorMessage::Completion(c) = m {
            Some(&c.result)
        } else {
            None
        }
    });
    assert!(fetch_result.is_some());
    assert_eq!(
        fetch_result.unwrap().status,
        EffectStatus::Failed
    );
}
```

IMPORTANT: These tests depend on `Reactor::start_with_config` being public and accepting the artifact store in the config. Adapt to the actual API — read the reactor code first.

**Step 2: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 3: Commit**

```bash
git add zeptovm/src/kernel/reactor.rs
git commit -m "test(reactor): add ObjectPut/ObjectFetch/ObjectDelete integration tests"
```

---

### Task 8: Update gap analysis

**Files:**
- Modify: `docs/plans/2026-03-06-spec-v03-gap-analysis.md`

**Step 1: Update A1 and G2 status**

In the "Things to Add Now" table, update A1:
- Status -> `DONE`
- Details -> `ObjectRef type + ArtifactBackend trait + SqliteArtifactStore + ObjectPut/ObjectFetch/ObjectDelete reactor handlers + TTL sweep`

In the "Acknowledged Gaps" table, update G2:
- Status -> `DONE`
- Notes -> `ObjectRef + artifact plane implemented with trait-based backend (SQLite first)`

In the Tier 4 list, mark A1/G2 as DONE.

**Step 2: Commit**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark A1/G2 (ObjectRef + artifact plane) as DONE in gap analysis"
```
