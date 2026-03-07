# A1/G2: ObjectRef + Artifact Plane — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add durable artifact storage with typed ObjectRef handles, enabling processes to store/retrieve/delete large artifacts via explicit effects.

**Architecture:** `ObjectRef` metadata handle in `core/object.rs`. `ArtifactBackend` trait + `SqliteArtifactStore` in `durability/artifact_store.rs`. Reactor handles `ObjectPut`/`ObjectFetch`/`ObjectDelete` effects. Runtime wires store + TTL sweep.

**Tech Stack:** Rust, ZeptoVM (core/object.rs, durability/artifact_store.rs, kernel/reactor.rs, kernel/runtime.rs, kernel/metrics.rs), sha2 crate, base64 crate, rusqlite

**Key design decisions (fixes from plan review):**
1. **ObjectId is DB-assigned** — SQLite `INTEGER PRIMARY KEY` auto-assigns IDs via `last_insert_rowid()`. No in-process atomic counter, so IDs survive restarts.
2. **Mutex\<Connection\>** — `SqliteArtifactStore` wraps `Mutex<Connection>` for `Send + Sync` safety. The reactor spawns tokio tasks that share `Arc<dyn ArtifactBackend>` concurrently.
3. **Reactor API wiring** — `Reactor::start_with_config` gains a second parameter `artifact_store: Option<Arc<dyn ArtifactBackend>>`. `SchedulerRuntime` stores it and passes it through to the reactor.
4. **Base64 transport** — `ObjectPut` accepts `data_base64`, `ObjectFetch` returns `data_base64`, preserving arbitrary binary support.

---

### Task 1: Add `sha2` and `base64` dependencies, create `ObjectId` + `ObjectRef` types

**Files:**
- Modify: `zeptovm/Cargo.toml` (dependencies section, ~line 20)
- Create: `zeptovm/src/core/object.rs`
- Modify: `zeptovm/src/core/mod.rs:1-7` (add `pub mod object;`)

**Step 1: Add sha2 and base64 to Cargo.toml**

In `zeptovm/Cargo.toml`, add after the existing dependencies:

```toml
sha2 = "0.10"
base64 = "0.22"
```

**Step 2: Create `core/object.rs` with ObjectId and ObjectRef**

ObjectId is a wrapper around u64. IDs are assigned by the database (SQLite `INTEGER PRIMARY KEY`), NOT by an in-process counter. This ensures IDs survive process restarts without collision.

```rust
use serde::{Deserialize, Serialize};

/// Unique identifier for a stored artifact.
/// Values are assigned by the artifact store's database,
/// not by an in-process counter — this ensures durability
/// across restarts.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, Hash,
  Serialize, Deserialize,
)]
pub struct ObjectId(u64);

impl ObjectId {
  /// Wrap a raw database-assigned ID.
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
  fn test_object_id_from_raw() {
    let id = ObjectId::from_raw(42);
    assert_eq!(id.raw(), 42);
  }

  #[test]
  fn test_object_id_equality() {
    let a = ObjectId::from_raw(1);
    let b = ObjectId::from_raw(1);
    let c = ObjectId::from_raw(2);
    assert_eq!(a, b);
    assert_ne!(a, c);
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
git commit -m "feat(core): add ObjectId and ObjectRef types with sha2/base64 dependencies"
```

---

### Task 2: Create `ArtifactBackend` trait and `SqliteArtifactStore`

**Files:**
- Create: `zeptovm/src/durability/artifact_store.rs`
- Modify: `zeptovm/src/durability/mod.rs:1-7` (add `pub mod artifact_store;`)

**Step 1: Create `durability/artifact_store.rs`**

Critical design:
- `SqliteArtifactStore` wraps `Mutex<Connection>` (not bare `Connection`) for `Send + Sync` safety.
- `store()` inserts with `NULL` for `object_id` and reads back `last_insert_rowid()` — the DB assigns the ID.
- No `ObjectId::new()` exists; IDs come only from the database.

```rust
use std::sync::Mutex;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::core::object::{ObjectId, ObjectRef};

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
///
/// Uses `Mutex<Connection>` to satisfy `Send + Sync`.
/// The reactor shares `Arc<dyn ArtifactBackend>` across
/// concurrent tokio tasks — the mutex serializes DB access.
pub struct SqliteArtifactStore {
  conn: Mutex<Connection>,
}

impl SqliteArtifactStore {
  pub fn open(path: &str) -> Result<Self, String> {
    let conn = Connection::open(path)
      .map_err(|e| format!("artifact store open: {e}"))?;
    let store = Self {
      conn: Mutex::new(conn),
    };
    store.init_schema()?;
    Ok(store)
  }

  pub fn open_in_memory() -> Result<Self, String> {
    let conn = Connection::open_in_memory()
      .map_err(|e| format!("artifact store open: {e}"))?;
    let store = Self {
      conn: Mutex::new(conn),
    };
    store.init_schema()?;
    Ok(store)
  }

  fn init_schema(&self) -> Result<(), String> {
    let conn = self.conn.lock().map_err(|e| {
      format!("lock error: {e}")
    })?;
    conn
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
    let hash = format!("{:x}", Sha256::digest(data));
    let now = Self::now_ms();
    let expires_at = ttl_ms.map(|ttl| now + ttl);

    let conn = self.conn.lock().map_err(|e| {
      format!("lock error: {e}")
    })?;

    // Insert with NULL object_id — SQLite assigns it.
    conn
      .execute(
        "INSERT INTO artifacts
         (content_hash, media_type,
          size_bytes, data, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
          hash,
          media_type,
          data.len() as i64,
          data,
          now as i64,
          expires_at.map(|e| e as i64),
        ],
      )
      .map_err(|e| format!("store: {e}"))?;

    let object_id =
      ObjectId::from_raw(conn.last_insert_rowid() as u64);

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
    let conn = self.conn.lock().map_err(|e| {
      format!("lock error: {e}")
    })?;

    let mut stmt = conn
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
    let conn = self.conn.lock().map_err(|e| {
      format!("lock error: {e}")
    })?;
    let rows = conn
      .execute(
        "DELETE FROM artifacts WHERE object_id = ?1",
        params![object_id.raw() as i64],
      )
      .map_err(|e| format!("delete: {e}"))?;
    Ok(rows > 0)
  }

  fn cleanup_expired(&self) -> Result<u64, String> {
    let now = Self::now_ms() as i64;
    let conn = self.conn.lock().map_err(|e| {
      format!("lock error: {e}")
    })?;
    let rows = conn
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
    assert_eq!(
      retrieved_ref.content_hash,
      obj_ref.content_hash
    );
  }

  #[test]
  fn test_store_assigns_unique_ids() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let a =
      store.store(b"one", "text/plain", None).unwrap();
    let b =
      store.store(b"two", "text/plain", None).unwrap();
    assert_ne!(
      a.object_id.raw(),
      b.object_id.raw()
    );
    // IDs are DB-assigned (1, 2, ...)
    assert_eq!(a.object_id.raw(), 1);
    assert_eq!(b.object_id.raw(), 2);
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

    let result = store.retrieve(&obj_ref.object_id);
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
    let obj_ref = store
      .store(b"temp", "text/plain", Some(0))
      .unwrap();

    // Small sleep to ensure expiry
    std::thread::sleep(
      std::time::Duration::from_millis(10),
    );

    let removed = store.cleanup_expired().unwrap();
    assert_eq!(removed, 1);

    let result = store.retrieve(&obj_ref.object_id);
    assert!(result.is_err());
  }

  #[test]
  fn test_no_ttl_survives_cleanup() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref = store
      .store(b"permanent", "text/plain", None)
      .unwrap();

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
    let expected =
      format!("{:x}", Sha256::digest(b"test"));
    assert_eq!(obj_ref.content_hash, expected);
  }

  #[test]
  fn test_store_binary_data() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    // Store non-UTF8 binary data (like a PNG header)
    let binary: Vec<u8> =
      vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A];
    let obj_ref = store
      .store(&binary, "image/png", None)
      .unwrap();

    let (_, retrieved) =
      store.retrieve(&obj_ref.object_id).unwrap();
    assert_eq!(retrieved, binary);
  }

  #[test]
  fn test_concurrent_access() {
    use std::sync::Arc;
    let store = Arc::new(
      SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let handles: Vec<_> = (0..4)
      .map(|i| {
        let s = Arc::clone(&store);
        std::thread::spawn(move || {
          s.store(
            format!("data-{i}").as_bytes(),
            "text/plain",
            None,
          )
          .unwrap()
        })
      })
      .collect();
    let refs: Vec<_> =
      handles.into_iter().map(|h| h.join().unwrap()).collect();
    // All got unique IDs
    let ids: std::collections::HashSet<_> =
      refs.iter().map(|r| r.object_id.raw()).collect();
    assert_eq!(ids.len(), 4);
  }
}
```

**Step 2: Add module to durability/mod.rs**

Add `pub mod artifact_store;` to `zeptovm/src/durability/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p zeptovm --lib -- artifact_store::tests`
Expected: All 10 tests pass

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

In the counters array (lines 17-34), add these four entries:

```rust
"artifacts.stored",
"artifacts.fetched",
"artifacts.deleted",
"artifacts.expired",
```

**Step 2: Add test**

In the `#[cfg(test)] mod tests` block, add:

```rust
#[test]
fn test_metrics_artifact_counters() {
  let m = Metrics::new();
  m.inc("artifacts.stored");
  m.inc("artifacts.fetched");
  m.inc("artifacts.deleted");
  m.inc("artifacts.expired");
  assert_eq!(m.counter("artifacts.stored"), 1);
  assert_eq!(m.counter("artifacts.fetched"), 1);
  assert_eq!(m.counter("artifacts.deleted"), 1);
  assert_eq!(m.counter("artifacts.expired"), 1);
}
```

Note: The method is `counter()` (per `metrics.rs:67`), NOT `get()`.

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
- Modify: `zeptovm/src/kernel/reactor.rs:1-16` (imports)
- Modify: `zeptovm/src/kernel/reactor.rs:23-73` (ReactorConfig struct + constructors)
- Modify: `zeptovm/src/kernel/reactor.rs:118-121` (Reactor::start)
- Modify: `zeptovm/src/kernel/reactor.rs:129-137` (Reactor::start_with_config signature)
- Modify: `zeptovm/src/kernel/reactor.rs:560-570` (execute_effect_once_configured catch-all)

This is the highest-risk task. Read the actual code before making changes.

**Step 1: Add imports to reactor.rs**

At the top, add:

```rust
use crate::durability::artifact_store::ArtifactBackend;
```

(`std::sync::Arc` is already imported at line 1.)

**Step 2: Add artifact_store field to ReactorConfig**

In `ReactorConfig` struct (line 23-32), add after `use_real_http`:

```rust
artifact_store: Option<Arc<dyn ArtifactBackend>>,
```

Initialize to `None` in both `from_provider_config()` (after line 58) and `placeholder()` (after line 70).

**Step 3: Change `Reactor::start_with_config` signature**

Current signature (line 129-131):
```rust
pub fn start_with_config(
    config: Option<ProviderConfig>,
) -> Self {
```

Change to:
```rust
pub fn start_with_config(
    config: Option<ProviderConfig>,
    artifact_store: Option<Arc<dyn ArtifactBackend>>,
) -> Self {
```

Inside, after `ReactorConfig` construction (line 132-137), set:
```rust
let mut reactor_config = match config {
    Some(ref c) if c.has_any_provider() => {
        ReactorConfig::from_provider_config(c)
    }
    _ => ReactorConfig::placeholder(),
};
reactor_config.artifact_store = artifact_store;
let reactor_config = Arc::new(reactor_config);
```

(Note: the existing code does `let reactor_config = Arc::new(match config {...})`. Restructure to a `let mut` + `Arc::new()` to set the artifact_store before wrapping.)

**Step 4: Update `Reactor::start` to pass None**

Change line 119:
```rust
pub fn start() -> Self {
    Self::start_with_config(None, None)
}
```

**Step 5: Add ObjectPut/ObjectFetch/ObjectDelete handlers**

In `execute_effect_once_configured` (line 428-570), replace the catch-all `other =>` arm (lines 560-570) with explicit artifact handlers + a new catch-all:

```rust
EffectKind::ObjectPut => {
  if let Some(ref store) = config.artifact_store {
    let data_b64 = request
      .input
      .get("data_base64")
      .and_then(|v| v.as_str())
      .unwrap_or("");
    let media_type = request
      .input
      .get("media_type")
      .and_then(|v| v.as_str())
      .unwrap_or("application/octet-stream");
    let ttl_ms = request
      .input
      .get("ttl_ms")
      .and_then(|v| v.as_u64());

    use base64::Engine;
    let data =
      base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| format!("base64 decode: {e}"));

    match data {
      Ok(bytes) => {
        match store.store(&bytes, media_type, ttl_ms)
        {
          Ok(obj_ref) => EffectResult::success(
            request.effect_id,
            serde_json::to_value(&obj_ref)
              .unwrap_or_default(),
          ),
          Err(e) => EffectResult::failure(
            request.effect_id,
            e,
          ),
        }
      }
      Err(e) => {
        EffectResult::failure(request.effect_id, e)
      }
    }
  } else {
    EffectResult::failure(
      request.effect_id,
      "no artifact store configured",
    )
  }
}

EffectKind::ObjectFetch => {
  if let Some(ref store) = config.artifact_store {
    let obj_id = request
      .input
      .get("object_id")
      .and_then(|v| v.as_u64())
      .unwrap_or(0);

    use crate::core::object::ObjectId;
    match store
      .retrieve(&ObjectId::from_raw(obj_id))
    {
      Ok((obj_ref, data)) => {
        use base64::Engine;
        let data_b64 =
          base64::engine::general_purpose::STANDARD
            .encode(&data);
        EffectResult::success(
          request.effect_id,
          serde_json::json!({
            "ref": obj_ref,
            "data_base64": data_b64,
          }),
        )
      }
      Err(e) => EffectResult::failure(
        request.effect_id,
        e,
      ),
    }
  } else {
    EffectResult::failure(
      request.effect_id,
      "no artifact store configured",
    )
  }
}

EffectKind::ObjectDelete => {
  if let Some(ref store) = config.artifact_store {
    let obj_id = request
      .input
      .get("object_id")
      .and_then(|v| v.as_u64())
      .unwrap_or(0);

    use crate::core::object::ObjectId;
    match store.delete(&ObjectId::from_raw(obj_id))
    {
      Ok(existed) => EffectResult::success(
        request.effect_id,
        serde_json::json!({ "deleted": existed }),
      ),
      Err(e) => EffectResult::failure(
        request.effect_id,
        e,
      ),
    }
  } else {
    EffectResult::failure(
      request.effect_id,
      "no artifact store configured",
    )
  }
}

other => {
  // Placeholder for other effect kinds.
  EffectResult::success(
    request.effect_id,
    serde_json::json!({
      "status": "completed",
      "kind": format!("{:?}", other)
    }),
  )
}
```

Note: Uses `EffectResult::failure()` (not `failed()`), matching `effect.rs:213`.

**Step 6: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass (existing tests use `Reactor::start()` which passes `None, None`)

**Step 7: Commit**

```bash
git add zeptovm/Cargo.toml zeptovm/src/kernel/reactor.rs
git commit -m "feat(reactor): implement ObjectPut/ObjectFetch/ObjectDelete handlers with base64 transport"
```

---

### Task 6: Wire artifact store into Runtime + TTL sweep + artifact metrics

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs:1-33` (imports)
- Modify: `zeptovm/src/kernel/runtime.rs:61-77` (SchedulerRuntime struct)
- Modify: `zeptovm/src/kernel/runtime.rs:81-127` (constructors)
- Modify: `zeptovm/src/kernel/runtime.rs:133-146` (with_provider_config)
- Modify: `zeptovm/src/kernel/runtime.rs:218-624` (tick method)

**Step 1: Add imports**

At the top of runtime.rs, add:

```rust
use crate::durability::artifact_store::ArtifactBackend;
```

(`std::sync::Arc` needs to be added if not present — check the import list.)

**Step 2: Add fields to SchedulerRuntime struct**

After `idempotency_store` field (~line 68), add:

```rust
artifact_store: Option<Arc<dyn ArtifactBackend>>,
artifact_sweep_counter: u64,
pending_artifact_kinds: HashMap<u64, EffectKind>,
```

**Step 3: Initialize in constructors**

In `new()` (line 81-98), add:
```rust
artifact_store: None,
artifact_sweep_counter: 0,
pending_artifact_kinds: HashMap::new(),
```

In `with_durability()` (line 101-127), add the same three fields.

**Step 4: Add builder method**

After `with_provider_config` (~line 146), add:

```rust
/// Configure the artifact store for ObjectPut/Fetch/Delete.
///
/// If a reactor already exists, it is restarted with the
/// artifact store so handlers can access it.
pub fn with_artifact_store(
  mut self,
  store: Arc<dyn ArtifactBackend>,
) -> Self {
  self.artifact_store = Some(store.clone());
  // Restart reactor with the artifact store
  if self.reactor.is_some() {
    self.reactor = Some(
      Reactor::start_with_config(
        self.provider_config.clone(),
        Some(store),
      ),
    );
  }
  self
}
```

**Step 5: Update `with_provider_config` to pass artifact_store**

In `with_provider_config` (~line 133-146), change the `Reactor::start_with_config` call to:

```rust
self.reactor = Some(
  Reactor::start_with_config(
    Some(config.clone()),
    self.artifact_store.clone(),
  ),
);
```

**Step 6: Track artifact effect kinds at dispatch time**

In `tick()`, step 7 (lines 478-584), where effects are dispatched to reactor, add tracking BEFORE `reactor.dispatch(pid, req)` (around line 580-581):

```rust
// Track artifact effect kinds for metric
// correlation on completion
match &req.kind {
  EffectKind::ObjectPut
  | EffectKind::ObjectFetch
  | EffectKind::ObjectDelete => {
    self.pending_artifact_kinds.insert(
      req.effect_id.raw(),
      req.kind.clone(),
    );
  }
  _ => {}
}
self.metrics.inc("effects.dispatched");
if let Some(ref reactor) = self.reactor {
  reactor.dispatch(pid, req);
}
```

(Replace the existing `self.metrics.inc("effects.dispatched");` and reactor dispatch at lines 579-582.)

**Step 7: Increment artifact metrics on completion**

In `tick()`, step 1 (lines 218-366), in the `ReactorMessage::Completion` arm, AFTER the effect status metrics (line 344-359), add:

```rust
// Track artifact-specific metrics
if let Some(kind) = self
  .pending_artifact_kinds
  .remove(&completion.result.effect_id.raw())
{
  if completion.result.status
    == EffectStatus::Succeeded
  {
    match kind {
      EffectKind::ObjectPut => {
        self.metrics.inc("artifacts.stored");
      }
      EffectKind::ObjectFetch => {
        self.metrics.inc("artifacts.fetched");
      }
      EffectKind::ObjectDelete => {
        self.metrics.inc("artifacts.deleted");
      }
      _ => {}
    }
  }
}
```

**Step 8: Add TTL sweep to tick()**

At the END of `tick()`, before `stepped` is returned (~line 623), add:

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

**Step 9: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 10: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire artifact store with TTL sweep and artifact metrics"
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
  use crate::durability::artifact_store::{
    ArtifactBackend, SqliteArtifactStore,
  };

  let store: Arc<dyn ArtifactBackend> = Arc::new(
    SqliteArtifactStore::open_in_memory().unwrap(),
  );

  // Pass artifact store via start_with_config
  let reactor = Reactor::start_with_config(
    None,
    Some(store),
  );

  // ObjectPut — store base64-encoded data
  use base64::Engine;
  let data_b64 =
    base64::engine::general_purpose::STANDARD
      .encode(b"hello artifact");
  let put_req = EffectRequest::new(
    EffectKind::ObjectPut,
    serde_json::json!({
      "data_base64": data_b64,
      "media_type": "text/plain",
    }),
  );
  let pid = Pid::from_raw(1);
  reactor.dispatch(pid, put_req);
  std::thread::sleep(Duration::from_millis(200));

  let messages = reactor.drain_messages();
  let put_result = messages
    .iter()
    .find_map(|m| {
      if let ReactorMessage::Completion(c) = m {
        if c.result.status == EffectStatus::Succeeded
        {
          Some(&c.result)
        } else {
          None
        }
      } else {
        None
      }
    })
    .expect("should get put completion");
  assert_eq!(
    put_result.status,
    EffectStatus::Succeeded
  );

  let obj_id = put_result
    .output
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
  std::thread::sleep(Duration::from_millis(200));

  let messages = reactor.drain_messages();
  let fetch_result = messages
    .iter()
    .find_map(|m| {
      if let ReactorMessage::Completion(c) = m {
        Some(&c.result)
      } else {
        None
      }
    })
    .expect("should get fetch completion");
  assert_eq!(
    fetch_result.status,
    EffectStatus::Succeeded
  );

  let fetched_b64 = fetch_result
    .output
    .as_ref()
    .and_then(|v| v.get("data_base64"))
    .and_then(|v| v.as_str())
    .expect("should have data_base64");
  let decoded =
    base64::engine::general_purpose::STANDARD
      .decode(fetched_b64)
      .unwrap();
  assert_eq!(decoded, b"hello artifact");

  // Verify ref metadata is present
  assert!(
    fetch_result
      .output
      .as_ref()
      .and_then(|v| v.get("ref"))
      .is_some()
  );
}

#[test]
fn test_reactor_object_delete() {
  use crate::durability::artifact_store::{
    ArtifactBackend, SqliteArtifactStore,
  };

  let store: Arc<dyn ArtifactBackend> = Arc::new(
    SqliteArtifactStore::open_in_memory().unwrap(),
  );
  let reactor = Reactor::start_with_config(
    None,
    Some(store),
  );

  // Store an artifact first
  use base64::Engine;
  let data_b64 =
    base64::engine::general_purpose::STANDARD
      .encode(b"to-delete");
  let put_req = EffectRequest::new(
    EffectKind::ObjectPut,
    serde_json::json!({
      "data_base64": data_b64,
      "media_type": "text/plain",
    }),
  );
  let pid = Pid::from_raw(1);
  reactor.dispatch(pid, put_req);
  std::thread::sleep(Duration::from_millis(200));

  let messages = reactor.drain_messages();
  let obj_id = messages
    .iter()
    .find_map(|m| {
      if let ReactorMessage::Completion(c) = m {
        c.result
          .output
          .as_ref()
          .and_then(|v| v.get("object_id"))
          .and_then(|v| v.as_u64())
      } else {
        None
      }
    })
    .expect("should get object_id from put");

  // ObjectDelete
  let del_req = EffectRequest::new(
    EffectKind::ObjectDelete,
    serde_json::json!({ "object_id": obj_id }),
  );
  reactor.dispatch(pid, del_req);
  std::thread::sleep(Duration::from_millis(200));

  let messages = reactor.drain_messages();
  let del_result = messages
    .iter()
    .find_map(|m| {
      if let ReactorMessage::Completion(c) = m {
        Some(&c.result)
      } else {
        None
      }
    })
    .expect("should get delete completion");
  assert_eq!(
    del_result.status,
    EffectStatus::Succeeded
  );

  // Verify fetch now fails
  let fetch_req = EffectRequest::new(
    EffectKind::ObjectFetch,
    serde_json::json!({ "object_id": obj_id }),
  );
  reactor.dispatch(pid, fetch_req);
  std::thread::sleep(Duration::from_millis(200));

  let messages = reactor.drain_messages();
  let fetch_result = messages
    .iter()
    .find_map(|m| {
      if let ReactorMessage::Completion(c) = m {
        Some(&c.result)
      } else {
        None
      }
    })
    .expect("should get fetch completion");
  assert_eq!(
    fetch_result.status,
    EffectStatus::Failed
  );
}

#[test]
fn test_reactor_object_no_store_configured() {
  // Reactor without artifact store — ObjectPut fails
  let reactor = Reactor::start();
  let pid = Pid::from_raw(1);

  use base64::Engine;
  let data_b64 =
    base64::engine::general_purpose::STANDARD
      .encode(b"test");
  let req = EffectRequest::new(
    EffectKind::ObjectPut,
    serde_json::json!({
      "data_base64": data_b64,
      "media_type": "text/plain",
    }),
  );
  reactor.dispatch(pid, req);
  std::thread::sleep(Duration::from_millis(200));

  let messages = reactor.drain_messages();
  let result = messages
    .iter()
    .find_map(|m| {
      if let ReactorMessage::Completion(c) = m {
        Some(&c.result)
      } else {
        None
      }
    })
    .expect("should get completion");
  assert_eq!(result.status, EffectStatus::Failed);
  assert!(
    result
      .error
      .as_ref()
      .unwrap()
      .contains("no artifact store")
  );
}
```

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
