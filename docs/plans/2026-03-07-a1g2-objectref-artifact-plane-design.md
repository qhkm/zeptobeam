# A1/G2: ObjectRef + Artifact Plane — Design

## Goal

Add a durable artifact storage plane with typed `ObjectRef` handles, enabling processes to store, retrieve, and delete large artifacts (LLM outputs, files, screenshots) without bloating mailbox messages or journals.

## Architecture

`ObjectRef` is a metadata-rich handle passed in messages. Actual bytes live in an `ArtifactBackend` (trait-based, SQLite impl first). Behaviors explicitly store/fetch via `ObjectPut`/`ObjectFetch`/`ObjectDelete` effects. Runtime runs periodic TTL sweep.

## Tech Stack

Rust, ZeptoVM kernel (core/object.rs, durability/artifact_store.rs, kernel/reactor.rs, kernel/runtime.rs)

---

## ObjectRef Type

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectId(u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub object_id: ObjectId,
    pub content_hash: String,      // SHA-256 hex
    pub media_type: String,        // MIME type
    pub size_bytes: u64,
    pub created_at_ms: u64,
    pub ttl_ms: Option<u64>,       // None = lives forever
}
```

Lives in `core/object.rs`. ObjectId uses a monotonic counter (like EffectId). ObjectRef is lightweight enough to pass in mailbox messages — content lives in the store.

`storage_class` and `encryption_scope` from the spec are deferred until multi-node/encryption features.

---

## ArtifactBackend Trait + SQLite Implementation

```rust
pub trait ArtifactBackend: Send + Sync {
    fn store(&self, data: &[u8], media_type: &str, ttl_ms: Option<u64>) -> Result<ObjectRef, String>;
    fn retrieve(&self, object_id: &ObjectId) -> Result<(ObjectRef, Vec<u8>), String>;
    fn delete(&self, object_id: &ObjectId) -> Result<bool, String>;
    fn cleanup_expired(&self) -> Result<u64, String>;
}
```

Lives in `durability/artifact_store.rs`. SQLite implementation (`SqliteArtifactStore`) uses:

```sql
CREATE TABLE artifacts (
    object_id INTEGER PRIMARY KEY,
    content_hash TEXT NOT NULL,
    media_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    data BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER  -- NULL = no expiry
);
```

- `store()`: SHA-256 hash, monotonic ObjectId, insert row, return ObjectRef
- `retrieve()`: lookup by object_id, return metadata + bytes
- `delete()`: remove row, return whether it existed
- `cleanup_expired()`: `DELETE WHERE expires_at IS NOT NULL AND expires_at < now_ms`

Follows IdempotencyStore/SnapshotStore patterns — SQLite connection, `open()` and `open_in_memory()` constructors.

---

## Reactor Handlers

Existing `ObjectFetch`/`ObjectPut` EffectKind variants get real handlers. New `ObjectDelete` variant added.

**ObjectPut** request payload:
```json
{ "data_base64": "...", "media_type": "application/json", "ttl_ms": 3600000 }
```
Handler decodes base64, calls `store()`, returns ObjectRef in `EffectResult.output`.

**ObjectFetch** request payload:
```json
{ "object_id": 42 }
```
Handler calls `retrieve()`, returns `{ "ref": <ObjectRef>, "data_base64": "..." }`. Not found returns `EffectStatus::Failed`.

**ObjectDelete** request payload:
```json
{ "object_id": 42 }
```
Handler calls `delete()`, returns success. Idempotent (deleting missing object succeeds).

Artifact store passed to reactor as `Arc<dyn ArtifactBackend>`.

---

## Runtime Wiring + TTL Sweep

Runtime gains `artifact_store: Option<Arc<dyn ArtifactBackend>>`, passed to reactor on construction.

TTL sweep in `tick()`, throttled to every 100 ticks:

```rust
self.artifact_sweep_counter += 1;
if self.artifact_sweep_counter >= 100 {
    self.artifact_sweep_counter = 0;
    if let Some(ref store) = self.artifact_store {
        let removed = store.cleanup_expired().unwrap_or(0);
        if removed > 0 {
            self.metrics.inc_by("artifacts.expired", removed);
        }
    }
}
```

Metrics: `artifacts.stored`, `artifacts.fetched`, `artifacts.deleted`, `artifacts.expired`.

---

## Error Handling

- `store()` SQLite failure -> `EffectStatus::Failed` with error string
- `retrieve()` missing object -> `EffectStatus::Failed`, "object not found"
- `delete()` missing object -> success (idempotent)
- `cleanup_expired()` failure -> `warn!` + continue (non-fatal)

---

## Testing

- ObjectRef serialization round-trip
- ArtifactBackend: store/retrieve/delete/cleanup_expired (SQLite in-memory)
- Store returns correct metadata (size, hash, media_type)
- Retrieve non-existent object_id -> error
- TTL expiry: store with ttl_ms=0, cleanup removes it
- Reactor integration: ObjectPut -> ObjectRef, ObjectFetch -> data back
- ObjectDelete -> gone, subsequent fetch fails

---

## Out of Scope

- `storage_class` and `encryption_scope` fields (multi-node/encryption)
- Implicit artifact storage (auto-storing large effect outputs)
- Deduplication by content_hash (same data = two objects)
- Cross-process access control (any process can fetch any ObjectRef)
- Filesystem or S3 backend implementations (trait ready, impl deferred)
