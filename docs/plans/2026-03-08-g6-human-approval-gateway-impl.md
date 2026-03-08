# G6: Human Approval Gateway — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement human approval gateway so agents can pause for human sign-off or free-form input before executing risky effects.

**Architecture:** `ApprovalStore` (shared `Arc<Mutex<HashMap>>`) holds pending requests. Reactor inserts on `HumanApproval`/`HumanInput` effects, spawns timeout tasks. HTTP endpoints (axum) let humans list/resolve pending approvals. Resolution sends `EffectResult` through existing `completion_tx` channel. Approval effects return `EffectStatus::Deferred` so the dispatch loop skips sending an immediate completion.

**Tech Stack:** Rust, tokio, axum (new dep), serde_json, crossbeam-channel (existing)

---

### Task 1: Add `EffectStatus::Deferred` variant

The reactor dispatch loop always sends a `Completion` after `execute_effect_once_configured` returns. Approval effects need to defer their completion (sent later by resolve/timeout). Adding a `Deferred` status variant lets the dispatch loop skip the completion send.

**Files:**
- Modify: `zeptovm/src/core/effect.rs` — add `Deferred` to `EffectStatus` enum
- Modify: `zeptovm/src/kernel/reactor.rs` — skip completion send when status is `Deferred`

**Step 1: Add `Deferred` variant to `EffectStatus`**

In `zeptovm/src/core/effect.rs`, add `Deferred` after `Streaming` in the enum:

```rust
pub enum EffectStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Streaming,
    Deferred,  // <-- add this
}
```

**Step 2: Update dispatch loop to skip deferred completions**

In `zeptovm/src/kernel/reactor.rs`, the dispatch loop at lines 179-196 currently always sends a `Completion`. Wrap the send in a check:

```rust
tokio::spawn(
  async move {
    let result =
      execute_effect_with_retry_configured(
        &dispatch.request,
        &cfg,
        &tx,
        pid,
      )
      .await;
    // Deferred effects handle their own completion
    // (e.g., human approval resolved later).
    if result.status != EffectStatus::Deferred {
      let _ = tx.send(
        ReactorMessage::Completion(
          EffectCompletion { pid, result },
        ),
      );
    }
  }
  .instrument(span),
);
```

Add `use crate::core::effect::EffectStatus;` to the imports at the top of reactor.rs if not already present.

**Step 3: Run tests**

Run: `cargo test --manifest-path zeptovm/Cargo.toml 2>&1 | tail -5`
Expected: All 420 tests pass (no behavioral change yet — nothing returns `Deferred`).

**Step 4: Commit**

```bash
git add zeptovm/src/core/effect.rs zeptovm/src/kernel/reactor.rs
git commit -m "feat(effect): add Deferred status for async effect resolution"
```

---

### Task 2: Create `ApprovalStore`

A thread-safe store for pending human approval/input requests. Shared between the reactor (insert, timeout) and HTTP handlers (list, resolve).

**Files:**
- Create: `zeptovm/src/kernel/approval_store.rs`
- Modify: `zeptovm/src/kernel/mod.rs` — add `pub mod approval_store;`

**Step 1: Write failing tests**

Create `zeptovm/src/kernel/approval_store.rs` with the struct, methods, and tests. Tests first:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::core::effect::{EffectId, EffectKind};
use crate::pid::Pid;

/// A pending human approval or input request.
#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
  pub effect_id: EffectId,
  pub pid: Pid,
  pub kind: EffectKind,
  pub description: String,
  pub input: serde_json::Value,
  pub created_at_ms: u64,
  pub expires_at_ms: u64,
}

/// Thread-safe store for pending approval requests.
#[derive(Clone)]
pub struct ApprovalStore {
  inner: Arc<Mutex<HashMap<u64, PendingApproval>>>,
}

impl ApprovalStore {
  pub fn new() -> Self {
    Self {
      inner: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// Insert a pending approval. Keyed by effect_id.raw().
  pub fn insert(&self, approval: PendingApproval) {
    let key = approval.effect_id.raw();
    let mut map =
      self.inner.lock().expect("approval store lock");
    map.insert(key, approval);
  }

  /// Remove and return a pending approval (resolve it).
  pub fn resolve(
    &self,
    effect_id: u64,
  ) -> Option<PendingApproval> {
    let mut map =
      self.inner.lock().expect("approval store lock");
    map.remove(&effect_id)
  }

  /// List all pending approvals (snapshot).
  pub fn list(&self) -> Vec<PendingApproval> {
    let map =
      self.inner.lock().expect("approval store lock");
    map.values().cloned().collect()
  }

  /// Check if an approval is still pending.
  pub fn is_pending(&self, effect_id: u64) -> bool {
    let map =
      self.inner.lock().expect("approval store lock");
    map.contains_key(&effect_id)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_approval(
    effect_id: u64,
    kind: EffectKind,
  ) -> PendingApproval {
    PendingApproval {
      effect_id: EffectId::from_raw(effect_id),
      pid: Pid::from_raw(1),
      kind,
      description: "test approval".into(),
      input: serde_json::json!({}),
      created_at_ms: 1000,
      expires_at_ms: 2000,
    }
  }

  #[test]
  fn test_approval_store_insert_and_resolve() {
    let store = ApprovalStore::new();
    let approval =
      make_approval(42, EffectKind::HumanApproval);
    store.insert(approval);

    assert!(store.is_pending(42));
    assert_eq!(store.list().len(), 1);

    let resolved = store.resolve(42);
    assert!(resolved.is_some());
    assert_eq!(resolved.unwrap().effect_id.raw(), 42);

    assert!(!store.is_pending(42));
    assert_eq!(store.list().len(), 0);
  }

  #[test]
  fn test_approval_store_double_resolve() {
    let store = ApprovalStore::new();
    store.insert(
      make_approval(42, EffectKind::HumanApproval),
    );

    let first = store.resolve(42);
    assert!(first.is_some());

    let second = store.resolve(42);
    assert!(second.is_none());
  }

  #[test]
  fn test_approval_store_list_multiple() {
    let store = ApprovalStore::new();
    store.insert(
      make_approval(1, EffectKind::HumanApproval),
    );
    store.insert(
      make_approval(2, EffectKind::HumanInput),
    );
    store.insert(
      make_approval(3, EffectKind::HumanApproval),
    );

    assert_eq!(store.list().len(), 3);

    store.resolve(2);
    assert_eq!(store.list().len(), 2);
  }

  #[test]
  fn test_approval_store_resolve_nonexistent() {
    let store = ApprovalStore::new();
    assert!(store.resolve(999).is_none());
    assert!(!store.is_pending(999));
  }
}
```

**Step 2: Add module to kernel/mod.rs**

Add `pub mod approval_store;` to `zeptovm/src/kernel/mod.rs`.

**Step 3: Run tests**

Run: `cargo test --manifest-path zeptovm/Cargo.toml -- approval_store 2>&1 | tail -10`
Expected: 4 tests pass.

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/approval_store.rs zeptovm/src/kernel/mod.rs
git commit -m "feat(kernel): add ApprovalStore for pending human approval requests"
```

---

### Task 3: Wire approval handling into reactor

Add `HumanApproval` and `HumanInput` match arms to `execute_effect_once_configured`. These insert into `ApprovalStore`, spawn a timeout task, and return `EffectStatus::Deferred`.

**Files:**
- Modify: `zeptovm/src/kernel/reactor.rs` — add `ApprovalStore` field to `ReactorConfig`, add handler match arms, add `start_with_approval_store` constructor

**Step 1: Add `ApprovalStore` to `ReactorConfig`**

In `zeptovm/src/kernel/reactor.rs`, add to the `ReactorConfig` struct:

```rust
use crate::kernel::approval_store::ApprovalStore;

pub struct ReactorConfig {
  // ... existing fields ...
  approval_store: Option<ApprovalStore>,
}
```

Initialize to `None` in both `from_provider_config` and `placeholder()`.

**Step 2: Add `approval_store` parameter to `start_with_config`**

Update the `start_with_config` signature:

```rust
pub fn start_with_config(
  config: Option<ProviderConfig>,
  artifact_store: Option<Arc<dyn ArtifactBackend>>,
  approval_store: Option<ApprovalStore>,
) -> Self {
```

Set `reactor_config.approval_store = approval_store;` alongside the existing `artifact_store` assignment.

Update `Reactor::start()` to pass `None`:

```rust
pub fn start() -> Self {
  Self::start_with_config(None, None, None)
}
```

**Step 3: Fix all existing callers**

Search for `start_with_config(` calls and add the third `None` argument. Key locations:
- `zeptovm/src/kernel/runtime.rs` — the `with_provider_config` method
- Test functions in `reactor.rs` that call `start_with_config(None, Some(store))`  — change to `start_with_config(None, Some(store), None)`

**Step 4: Add `HumanApproval`/`HumanInput` match arms**

In `execute_effect_once_configured`, before the catch-all `other =>` arm, add:

```rust
EffectKind::HumanApproval | EffectKind::HumanInput => {
  if let Some(ref store) = config.approval_store {
    let description = match request
      .input
      .get("description")
      .and_then(|v| v.as_str())
    {
      Some(s) => s.to_string(),
      None => {
        return EffectResult::failure(
          request.effect_id,
          "missing required field: description",
        );
      }
    };

    let now_ms = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_millis() as u64;
    let expires_at_ms =
      now_ms.saturating_add(request.timeout.as_millis() as u64);

    let approval = crate::kernel::approval_store::PendingApproval {
      effect_id: request.effect_id,
      pid,
      kind: request.kind.clone(),
      description,
      input: request.input.clone(),
      created_at_ms: now_ms,
      expires_at_ms,
    };

    store.insert(approval);

    // Spawn timeout task
    let timeout = request.timeout;
    let effect_id_raw = request.effect_id.raw();
    let effect_id = request.effect_id;
    let timeout_store = store.clone();
    let timeout_tx = completion_tx.clone();
    tokio::spawn(async move {
      tokio::time::sleep(timeout).await;
      if timeout_store.is_pending(effect_id_raw) {
        if let Some(_) = timeout_store.resolve(effect_id_raw) {
          let _ = timeout_tx.send(
            ReactorMessage::Completion(EffectCompletion {
              pid,
              result: EffectResult::failure(
                effect_id,
                "approval timed out",
              ),
            }),
          );
        }
      }
    });

    // Return Deferred — dispatch loop will skip
    // sending a completion.
    EffectResult {
      effect_id: request.effect_id,
      status: EffectStatus::Deferred,
      output: None,
      error: None,
    }
  } else {
    EffectResult::failure(
      request.effect_id,
      "no approval store configured",
    )
  }
}
```

**Step 5: Add `approval_store()` accessor to `Reactor`**

Add a method so HTTP endpoints can access the store:

```rust
impl Reactor {
  // ... existing methods ...

  /// Get the approval store, if configured.
  pub fn approval_store(&self) -> Option<ApprovalStore> {
    // The store is inside ReactorConfig which is inside the thread.
    // We need to share it externally.
    // See Step 6.
  }
}
```

Actually, the `ApprovalStore` is `Clone` (it wraps `Arc<Mutex<...>>`), so we should store a clone on the `Reactor` struct itself:

Add to `Reactor` struct:

```rust
pub struct Reactor {
  dispatch_tx: Sender<EffectDispatch>,
  completion_rx: Receiver<ReactorMessage>,
  approval_store: Option<ApprovalStore>,
  _handle: thread::JoinHandle<()>,
}
```

In `start_with_config`, clone the store before moving into the thread:

```rust
let external_approval_store = approval_store.clone();
// ... existing thread::spawn code ...
// In the Self { ... } return:
Self {
  dispatch_tx,
  completion_rx,
  approval_store: external_approval_store,
  _handle: handle,
}
```

Add accessor:

```rust
pub fn approval_store(&self) -> Option<&ApprovalStore> {
  self.approval_store.as_ref()
}
```

**Step 6: Run tests**

Run: `cargo test --manifest-path zeptovm/Cargo.toml 2>&1 | tail -5`
Expected: All existing tests pass (no behavioral change for non-approval effects).

**Step 7: Commit**

```bash
git add zeptovm/src/kernel/reactor.rs
git commit -m "feat(reactor): wire HumanApproval/HumanInput handlers with ApprovalStore and timeout"
```

---

### Task 4: Add reactor-level approval tests

Test the full approval flow through the reactor: dispatch, resolve via store, verify completion. Also test timeout and missing description.

**Files:**
- Modify: `zeptovm/src/kernel/reactor.rs` — add tests in the `#[cfg(test)] mod tests` block

**Step 1: Write approval flow test**

Add to the test module at the end of `reactor.rs`:

```rust
#[test]
fn test_reactor_human_approval_flow() {
  use crate::kernel::approval_store::ApprovalStore;

  let store = ApprovalStore::new();
  let reactor = Reactor::start_with_config(
    None,
    None,
    Some(store.clone()),
  );
  let pid = Pid::from_raw(1);

  let req = EffectRequest::new(
    EffectKind::HumanApproval,
    serde_json::json!({
      "description": "Delete production database?",
    }),
  );
  reactor.dispatch(pid, req);

  // Wait for it to appear in the store
  std::thread::sleep(
    std::time::Duration::from_millis(50),
  );

  let pending = store.list();
  assert_eq!(pending.len(), 1);
  assert_eq!(
    pending[0].description,
    "Delete production database?"
  );

  // Resolve it (simulate human approving)
  let effect_id_raw = pending[0].effect_id.raw();
  let resolved = store.resolve(effect_id_raw).unwrap();

  // Send the completion manually (as HTTP handler would)
  reactor.dispatch_sender().send(/* ... */);
  // Actually, we need completion_tx. The HTTP handler
  // would use the approval_store + completion_tx.
  // For testing, we resolve via store and send completion
  // through the reactor's internal channel.
  //
  // Better approach: add a resolve_and_complete method
  // that takes the action and sends the completion.
}
```

Wait — the test needs to send a completion after resolving. The HTTP handler will need `completion_tx`. Let's add a `resolve_approval` method on `Reactor` that resolves from the store and sends the completion:

**Step 2: Add `resolve_approval` method to `Reactor`**

This method is the programmatic API for resolving approvals (used by both HTTP handlers and tests):

```rust
impl Reactor {
  /// Resolve a pending approval and send the completion.
  /// Returns `true` if the approval was found and resolved.
  pub fn resolve_approval(
    &self,
    effect_id: u64,
    action: &str,
    payload: serde_json::Value,
  ) -> bool {
    let store = match &self.approval_store {
      Some(s) => s,
      None => return false,
    };

    let approval = match store.resolve(effect_id) {
      Some(a) => a,
      None => return false,
    };

    let result = match action {
      "approve" => EffectResult::success(
        approval.effect_id,
        serde_json::json!({"approved": true}),
      ),
      "deny" => {
        let reason = payload
          .get("reason")
          .and_then(|v| v.as_str())
          .unwrap_or("");
        EffectResult::success(
          approval.effect_id,
          serde_json::json!({
            "approved": false,
            "reason": reason,
          }),
        )
      }
      "respond" => {
        let response = payload
          .get("response")
          .cloned()
          .unwrap_or(serde_json::Value::Null);
        EffectResult::success(
          approval.effect_id,
          serde_json::json!({"response": response}),
        )
      }
      _ => EffectResult::failure(
        approval.effect_id,
        format!("unknown action: {action}"),
      ),
    };

    let _ = self.dispatch_tx.send(EffectDispatch {
      pid: approval.pid,
      request: EffectRequest::new(
        approval.kind,
        serde_json::json!({}),
      ),
    });
    // Actually, we need completion_tx here. But completion_tx
    // is inside the thread. We need to expose it.
    // Let's store a clone of completion_tx on Reactor.
    true
  }
}
```

Actually, the reactor needs a `completion_tx` clone stored externally. Let's add that to `Reactor`:

```rust
pub struct Reactor {
  dispatch_tx: Sender<EffectDispatch>,
  completion_rx: Receiver<ReactorMessage>,
  completion_tx: Sender<ReactorMessage>,  // <-- add this
  approval_store: Option<ApprovalStore>,
  _handle: thread::JoinHandle<()>,
}
```

Clone `completion_tx` before moving into thread, store it on `Reactor`. Then `resolve_approval` can use `self.completion_tx.send(...)`.

**Step 3: Write the actual tests**

```rust
#[test]
fn test_reactor_human_approval_flow() {
  use crate::kernel::approval_store::ApprovalStore;

  let store = ApprovalStore::new();
  let reactor = Reactor::start_with_config(
    None, None, Some(store.clone()),
  );
  let pid = Pid::from_raw(1);

  let req = EffectRequest::new(
    EffectKind::HumanApproval,
    serde_json::json!({
      "description": "Delete production DB?",
    }),
  );
  reactor.dispatch(pid, req);

  std::thread::sleep(
    std::time::Duration::from_millis(50),
  );

  // Should be in pending store
  let pending = store.list();
  assert_eq!(pending.len(), 1);
  let effect_id = pending[0].effect_id.raw();

  // Resolve: approve
  let resolved = reactor.resolve_approval(
    effect_id,
    "approve",
    serde_json::json!({}),
  );
  assert!(resolved);

  // Should receive completion
  let completion =
    poll_for_completion(&reactor)
      .expect("should get approval completion");
  assert_eq!(
    completion.result.status,
    EffectStatus::Succeeded,
  );
  let output = completion.result.output.unwrap();
  assert_eq!(output["approved"], true);
}

#[test]
fn test_reactor_human_input_flow() {
  use crate::kernel::approval_store::ApprovalStore;

  let store = ApprovalStore::new();
  let reactor = Reactor::start_with_config(
    None, None, Some(store.clone()),
  );
  let pid = Pid::from_raw(1);

  let req = EffectRequest::new(
    EffectKind::HumanInput,
    serde_json::json!({
      "description": "Which environment?",
    }),
  );
  reactor.dispatch(pid, req);

  std::thread::sleep(
    std::time::Duration::from_millis(50),
  );

  let pending = store.list();
  assert_eq!(pending.len(), 1);
  let effect_id = pending[0].effect_id.raw();

  let resolved = reactor.resolve_approval(
    effect_id,
    "respond",
    serde_json::json!({"response": "staging"}),
  );
  assert!(resolved);

  let completion =
    poll_for_completion(&reactor)
      .expect("should get input completion");
  assert_eq!(
    completion.result.status,
    EffectStatus::Succeeded,
  );
  let output = completion.result.output.unwrap();
  assert_eq!(output["response"], "staging");
}

#[test]
fn test_reactor_approval_timeout() {
  use crate::kernel::approval_store::ApprovalStore;

  let store = ApprovalStore::new();
  let reactor = Reactor::start_with_config(
    None, None, Some(store.clone()),
  );
  let pid = Pid::from_raw(1);

  // Short timeout: 50ms
  let mut req = EffectRequest::new(
    EffectKind::HumanApproval,
    serde_json::json!({
      "description": "Auto-deny test",
    }),
  );
  req.timeout =
    std::time::Duration::from_millis(50);
  reactor.dispatch(pid, req);

  // Wait for timeout to fire
  std::thread::sleep(
    std::time::Duration::from_millis(150),
  );

  // Should be removed from store
  assert!(!store.is_pending(
    store.list().first().map(|a| a.effect_id.raw())
      .unwrap_or(0)
  ));

  // Should receive failure completion
  let completion =
    poll_for_completion(&reactor)
      .expect("should get timeout completion");
  assert_eq!(
    completion.result.status,
    EffectStatus::Failed,
  );
  assert!(
    completion.result.error.as_ref().unwrap()
      .contains("timed out"),
  );
}

#[test]
fn test_reactor_approval_missing_description() {
  use crate::kernel::approval_store::ApprovalStore;

  let store = ApprovalStore::new();
  let reactor = Reactor::start_with_config(
    None, None, Some(store.clone()),
  );
  let pid = Pid::from_raw(1);

  // No description field
  let req = EffectRequest::new(
    EffectKind::HumanApproval,
    serde_json::json!({}),
  );
  reactor.dispatch(pid, req);

  let completion =
    poll_for_completion(&reactor)
      .expect("should get failure completion");
  assert_eq!(
    completion.result.status,
    EffectStatus::Failed,
  );
  assert!(
    completion.result.error.as_ref().unwrap()
      .contains("description"),
  );
}

#[test]
fn test_reactor_approval_no_store_configured() {
  let reactor = Reactor::start();
  let pid = Pid::from_raw(1);

  let req = EffectRequest::new(
    EffectKind::HumanApproval,
    serde_json::json!({
      "description": "test",
    }),
  );
  reactor.dispatch(pid, req);

  let completion =
    poll_for_completion(&reactor)
      .expect("should get failure completion");
  assert_eq!(
    completion.result.status,
    EffectStatus::Failed,
  );
  assert!(
    completion.result.error.as_ref().unwrap()
      .contains("no approval store"),
  );
}

#[test]
fn test_reactor_approval_deny_with_reason() {
  use crate::kernel::approval_store::ApprovalStore;

  let store = ApprovalStore::new();
  let reactor = Reactor::start_with_config(
    None, None, Some(store.clone()),
  );
  let pid = Pid::from_raw(1);

  let req = EffectRequest::new(
    EffectKind::HumanApproval,
    serde_json::json!({
      "description": "Risky operation",
    }),
  );
  reactor.dispatch(pid, req);

  std::thread::sleep(
    std::time::Duration::from_millis(50),
  );

  let pending = store.list();
  let effect_id = pending[0].effect_id.raw();

  let resolved = reactor.resolve_approval(
    effect_id,
    "deny",
    serde_json::json!({"reason": "too risky"}),
  );
  assert!(resolved);

  let completion =
    poll_for_completion(&reactor)
      .expect("should get deny completion");
  let output = completion.result.output.unwrap();
  assert_eq!(output["approved"], false);
  assert_eq!(output["reason"], "too risky");
}
```

**Step 4: Run tests**

Run: `cargo test --manifest-path zeptovm/Cargo.toml -- test_reactor_human 2>&1 | tail -15`
Expected: 6 new tests pass.

Run: `cargo test --manifest-path zeptovm/Cargo.toml 2>&1 | grep "test result:"`
Expected: All tests pass (420 existing + 6 new = 426+).

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/reactor.rs
git commit -m "test(reactor): add human approval/input flow, timeout, and validation tests"
```

---

### Task 5: Add approval metrics

Pre-register counters for approval operations.

**Files:**
- Modify: `zeptovm/src/kernel/metrics.rs` — add approval counters

**Step 1: Add counters**

Add these to the pre-registered counters in `MetricsRegistry::new()`:

```rust
"approvals.requested",
"approvals.approved",
"approvals.denied",
"approvals.timed_out",
"approvals.input_received",
```

**Step 2: Wire metrics into resolve logic**

In `Reactor::resolve_approval`, increment the appropriate counter:
- `"approve"` → increment `"approvals.approved"`
- `"deny"` → increment `"approvals.denied"`
- `"respond"` → increment `"approvals.input_received"`

In the timeout task, increment `"approvals.timed_out"`.

On initial insertion, increment `"approvals.requested"`.

Note: The reactor doesn't currently have access to `MetricsRegistry`. For now, add the counter pre-registrations only. The actual metric increments will be wired in `runtime.rs` when processing completions (same pattern as artifact metrics — using `pending_artifact_kinds` HashMap). This avoids threading `MetricsRegistry` into the reactor.

**Step 3: Run tests**

Run: `cargo test --manifest-path zeptovm/Cargo.toml 2>&1 | tail -5`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add zeptovm/src/kernel/metrics.rs
git commit -m "feat(metrics): add approval counter pre-registrations"
```

---

### Task 6: Add HTTP endpoints for approval management

Add axum dependency and create HTTP routes for listing and resolving pending approvals.

**Files:**
- Modify: `zeptovm/Cargo.toml` — add `axum` dependency
- Create: `zeptovm/src/kernel/approval_http.rs` — HTTP handlers
- Modify: `zeptovm/src/kernel/mod.rs` — add module

**Step 1: Add axum dependency**

In `zeptovm/Cargo.toml`, add:

```toml
axum = "0.8"
```

**Step 2: Create HTTP handler module**

Create `zeptovm/src/kernel/approval_http.rs`:

```rust
use std::sync::Arc;

use axum::{
  extract::{Path, State},
  http::StatusCode,
  response::IntoResponse,
  routing::{get, post},
  Json, Router,
};
use crossbeam_channel::Sender;

use crate::core::effect::{
  EffectResult, EffectStatus,
};
use crate::kernel::approval_store::ApprovalStore;
use crate::kernel::reactor::{
  EffectCompletion, ReactorMessage,
};

/// Shared state for the approval HTTP handlers.
#[derive(Clone)]
pub struct ApprovalHttpState {
  pub store: ApprovalStore,
  pub completion_tx: Sender<ReactorMessage>,
}

/// Build the approval router.
pub fn approval_router(
  state: ApprovalHttpState,
) -> Router {
  Router::new()
    .route("/approvals", get(list_approvals))
    .route(
      "/approvals/{id}/resolve",
      post(resolve_approval),
    )
    .with_state(state)
}

async fn list_approvals(
  State(state): State<ApprovalHttpState>,
) -> impl IntoResponse {
  let pending = state.store.list();
  Json(pending)
}

#[derive(serde::Deserialize)]
struct ResolveRequest {
  action: String,
  #[serde(default)]
  reason: Option<String>,
  #[serde(default)]
  response: Option<serde_json::Value>,
}

async fn resolve_approval(
  State(state): State<ApprovalHttpState>,
  Path(id): Path<u64>,
  Json(body): Json<ResolveRequest>,
) -> impl IntoResponse {
  let approval = match state.store.resolve(id) {
    Some(a) => a,
    None => {
      return (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
          "error": "approval not found or already resolved"
        })),
      );
    }
  };

  let valid_actions =
    ["approve", "deny", "respond"];
  if !valid_actions.contains(&body.action.as_str()) {
    // Re-insert since we already removed it
    state.store.insert(approval);
    return (
      StatusCode::BAD_REQUEST,
      Json(serde_json::json!({
        "error": format!(
          "invalid action: {}. Must be one of: approve, deny, respond",
          body.action
        )
      })),
    );
  }

  let result = match body.action.as_str() {
    "approve" => EffectResult::success(
      approval.effect_id,
      serde_json::json!({"approved": true}),
    ),
    "deny" => {
      let reason =
        body.reason.as_deref().unwrap_or("");
      EffectResult::success(
        approval.effect_id,
        serde_json::json!({
          "approved": false,
          "reason": reason,
        }),
      )
    }
    "respond" => {
      let response = body
        .response
        .unwrap_or(serde_json::Value::Null);
      EffectResult::success(
        approval.effect_id,
        serde_json::json!({"response": response}),
      )
    }
    _ => unreachable!(),
  };

  let _ = state.completion_tx.send(
    ReactorMessage::Completion(EffectCompletion {
      pid: approval.pid,
      result,
    }),
  );

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "status": "resolved",
      "effect_id": id,
      "action": body.action,
    })),
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::Body;
  use axum::http::Request;
  use tower::ServiceExt;

  use crate::core::effect::{EffectId, EffectKind};
  use crate::pid::Pid;

  fn make_state() -> (
    ApprovalHttpState,
    crossbeam_channel::Receiver<ReactorMessage>,
  ) {
    let (tx, rx) =
      crossbeam_channel::unbounded();
    let state = ApprovalHttpState {
      store: ApprovalStore::new(),
      completion_tx: tx,
    };
    (state, rx)
  }

  fn insert_test_approval(
    state: &ApprovalHttpState,
    id: u64,
  ) {
    use crate::kernel::approval_store::PendingApproval;
    state.store.insert(PendingApproval {
      effect_id: EffectId::from_raw(id),
      pid: Pid::from_raw(1),
      kind: EffectKind::HumanApproval,
      description: "test".into(),
      input: serde_json::json!({}),
      created_at_ms: 1000,
      expires_at_ms: 60000,
    });
  }

  #[tokio::test]
  async fn test_http_list_approvals() {
    let (state, _rx) = make_state();
    insert_test_approval(&state, 42);
    insert_test_approval(&state, 43);

    let app = approval_router(state);
    let resp = app
      .oneshot(
        Request::builder()
          .uri("/approvals")
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(
      resp.into_body(), usize::MAX,
    )
    .await
    .unwrap();
    let list: Vec<serde_json::Value> =
      serde_json::from_slice(&body).unwrap();
    assert_eq!(list.len(), 2);
  }

  #[tokio::test]
  async fn test_http_resolve_approval() {
    let (state, rx) = make_state();
    insert_test_approval(&state, 42);

    let app = approval_router(state);
    let resp = app
      .oneshot(
        Request::builder()
          .method("POST")
          .uri("/approvals/42/resolve")
          .header("content-type", "application/json")
          .body(Body::from(
            r#"{"action":"approve"}"#,
          ))
          .unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Check completion was sent
    let msg = rx.try_recv().unwrap();
    match msg {
      ReactorMessage::Completion(c) => {
        assert_eq!(
          c.result.status,
          EffectStatus::Succeeded,
        );
      }
      _ => panic!("expected completion"),
    }
  }

  #[tokio::test]
  async fn test_http_resolve_not_found() {
    let (state, _rx) = make_state();

    let app = approval_router(state);
    let resp = app
      .oneshot(
        Request::builder()
          .method("POST")
          .uri("/approvals/999/resolve")
          .header("content-type", "application/json")
          .body(Body::from(
            r#"{"action":"approve"}"#,
          ))
          .unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn test_http_resolve_invalid_action() {
    let (state, _rx) = make_state();
    insert_test_approval(&state, 42);

    let app = approval_router(state);
    let resp = app
      .oneshot(
        Request::builder()
          .method("POST")
          .uri("/approvals/42/resolve")
          .header("content-type", "application/json")
          .body(Body::from(
            r#"{"action":"yolo"}"#,
          ))
          .unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
  }
}
```

**Step 3: Add module to kernel/mod.rs**

Add `pub mod approval_http;` to `zeptovm/src/kernel/mod.rs`.

**Step 4: Run tests**

Run: `cargo test --manifest-path zeptovm/Cargo.toml -- approval_http 2>&1 | tail -10`
Expected: 4 HTTP tests pass.

Run: `cargo test --manifest-path zeptovm/Cargo.toml 2>&1 | grep "test result:"`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add zeptovm/Cargo.toml zeptovm/src/kernel/approval_http.rs zeptovm/src/kernel/mod.rs
git commit -m "feat(http): add approval list and resolve HTTP endpoints with axum"
```

---

### Task 7: Update gap analysis

Mark G6 as DONE in the gap analysis document.

**Files:**
- Modify: `docs/plans/2026-03-06-spec-v03-gap-analysis.md`

**Step 1: Update G6 status**

Change G6 line from:

```
| G6 | Human approval UI/gateway | NOT DONE | EffectKind exists, no handler |
```

to:

```
| G6 | Human approval UI/gateway | DONE | ApprovalStore + reactor handlers + HTTP endpoints + timeout enforcement |
```

**Step 2: Commit**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark G6 (human approval gateway) as DONE in gap analysis"
```

---

## Summary

| Task | What | Tests Added |
|------|------|-------------|
| 1 | `EffectStatus::Deferred` variant | 0 (no new behavior) |
| 2 | `ApprovalStore` struct + methods | 4 unit tests |
| 3 | Reactor handler wiring | 0 (tests in Task 4) |
| 4 | Reactor approval tests + `resolve_approval` method | 6 tests |
| 5 | Approval metrics pre-registrations | 0 |
| 6 | HTTP endpoints (axum) | 4 integration tests |
| 7 | Gap analysis update | 0 |

**Total new tests: ~14**
**New dependency: `axum = "0.8"`**
**New files: `approval_store.rs`, `approval_http.rs`**
