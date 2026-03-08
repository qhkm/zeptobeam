# G6: Human Approval Gateway — Design Document

**Date:** 2026-03-08
**Status:** Approved
**Goal:** Agents pause and wait for human sign-off (or free-form input) before executing risky effects.

**Architecture:** HTTP endpoints as external interface, standard effect completion as internal mechanism. Pending requests stored in a shared map inside the reactor. Timeouts are required — no infinite waits.

---

## Scope

Two effect kinds handled by this feature:

- **`HumanApproval`** — binary approve/deny with optional reason
- **`HumanInput`** — free-form text/JSON response from a human

Both share the same pending-request store, HTTP routes, and timeout logic.

---

## Architecture

```
Agent handler                    Reactor                         HTTP API
     |                              |                               |
     |-- Suspend(HumanApproval) --> |                               |
     |                              |-- store in pending map        |
     |                              |-- start timeout timer         |
     |                              |                               |
     |                              |          GET /approvals <---- |
     |                              |          POST /approvals/:id  |
     |                              |                               |
     |                              |<- resolve (approve/deny/input)|
     |                              |-- EffectResult to mailbox     |
     | <-- effect lane delivers --- |                               |
```

- Process stays in `WaitingEffect(effect_id)` — no new process states needed
- Timeout uses existing `EffectRequest.timeout` field — required for all approval/input requests
- Journal records the effect request and resolution for crash recovery

---

## Components

### 1. PendingApproval struct

```rust
pub struct PendingApproval {
    pub effect_id: EffectId,
    pub pid: Pid,
    pub kind: EffectKind,          // HumanApproval or HumanInput
    pub description: String,       // what the agent is asking for
    pub input: serde_json::Value,  // full effect input for context
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}
```

### 2. ApprovalStore

`Arc<Mutex<HashMap<u64, PendingApproval>>>` shared between reactor and HTTP handler. Keyed by `effect_id.raw()`.

Methods:
- `insert(approval: PendingApproval)` — add pending request
- `resolve(effect_id: u64) -> Option<PendingApproval>` — remove and return
- `list() -> Vec<PendingApproval>` — snapshot of all pending
- `is_pending(effect_id: u64) -> bool` — check without removing

### 3. Reactor handler

When `HumanApproval` or `HumanInput` arrives in `execute_effect_once_configured`:

1. Extract `description` from `request.input` (required — fail if missing)
2. Compute `expires_at_ms` from `now + request.timeout`
3. Insert `PendingApproval` into `ApprovalStore`
4. Spawn timeout task: `tokio::spawn` with `sleep(request.timeout)`, then check if still pending and auto-deny
5. **Do not return an EffectResult** — the completion is deferred until resolved or timed out

This requires a change to the reactor's dispatch flow: instead of always awaiting a result from `execute_effect_once_configured`, approval effects are "fire and forget" with deferred completion via `completion_tx`.

### 4. HTTP routes

Three endpoints:

- **`GET /approvals`** — list all pending approvals
  - Response: `[{ "effect_id": 42, "pid": 1, "kind": "HumanApproval", "description": "...", "created_at_ms": ..., "expires_at_ms": ... }]`

- **`POST /approvals/:id/resolve`** — resolve a pending approval
  - Request body for HumanApproval: `{"action": "approve"}` or `{"action": "deny", "reason": "too risky"}`
  - Request body for HumanInput: `{"action": "respond", "response": "Use staging environment"}`
  - Response: 200 on success, 404 if not found/already resolved, 400 if invalid action

- Resolution sends `EffectResult` through the existing `completion_tx` channel:
  - Approve: `EffectResult::success(effect_id, json!({"approved": true}))`
  - Deny: `EffectResult::success(effect_id, json!({"approved": false, "reason": "..."}))`
  - Respond: `EffectResult::success(effect_id, json!({"response": "..."}))`

### 5. Timeout task

```rust
tokio::spawn(async move {
    tokio::time::sleep(timeout).await;
    if store.is_pending(effect_id) {
        if let Some(_approval) = store.resolve(effect_id) {
            let _ = completion_tx.send(
                ReactorMessage::Completion(EffectCompletion {
                    pid,
                    result: EffectResult::failure(effect_id, "approval timed out"),
                })
            );
        }
    }
});
```

---

## Data Flow

### Happy path (approval)

1. Agent returns `StepResult::Suspend(EffectRequest { kind: HumanApproval, input: json!({ "description": "Delete production database?" }), timeout: 30min })`
2. Reactor inserts `PendingApproval` into store, spawns timeout task
3. Human calls `GET /approvals` — sees pending request
4. Human calls `POST /approvals/{id}/resolve` with `{"action": "approve"}`
5. Reactor sends `EffectResult::success(effect_id, json!({"approved": true}))` via `completion_tx`
6. Process wakes, handler receives result in effect lane

### Happy path (input)

Same flow, but resolve payload is `{"action": "respond", "response": "Use the staging environment instead"}`. EffectResult contains `json!({"response": "Use the staging environment instead"})`.

### Timeout

Timeout task fires, checks `ApprovalStore` — if still pending, removes it and sends `EffectResult::failure(effect_id, "approval timed out")`. Process gets failure result, handler decides what to do.

### Double-resolve

Second `POST /approvals/:id/resolve` returns HTTP 404 — already resolved. No duplicate effect results.

### Crash recovery

Pending approvals are not persisted to SQLite in this iteration. On crash, the effect was journaled as dispatched but no result was recorded. Recovery re-dispatches the effect — reactor re-inserts into `ApprovalStore` with remaining TTL. Human re-approves.

Future enhancement: persist pending approvals to SQLite for survival across restarts without re-dispatch.

### Invalid input

- Missing `description` field → `EffectResult::failure` with `"missing required field: description"`
- Resolve with unknown action → HTTP 400

---

## Testing Strategy

### Unit tests (reactor)

1. `test_approval_store_insert_and_resolve` — insert pending, resolve, verify removed
2. `test_approval_store_timeout` — insert with short TTL, wait, verify auto-denied
3. `test_approval_store_double_resolve` — resolve twice, second returns None
4. `test_reactor_human_approval_flow` — dispatch HumanApproval effect, resolve via store, verify process gets success result
5. `test_reactor_human_input_flow` — same flow with HumanInput and free-form response
6. `test_reactor_approval_timeout` — dispatch with short timeout, verify failure result
7. `test_reactor_approval_missing_description` — missing description field returns failure

### Integration tests (HTTP)

8. `test_http_list_approvals` — GET returns pending approvals
9. `test_http_resolve_approval` — POST resolves and returns 200
10. `test_http_resolve_not_found` — POST on resolved/unknown ID returns 404
11. `test_http_resolve_invalid_action` — POST with bad action returns 400

---

## Existing Integration Points

- `EffectKind::HumanApproval` and `EffectKind::HumanInput` — already defined in `zeptovm/src/core/effect.rs`
- `ProcessRuntimeState::WaitingEffect(effect_id)` — reused, no new states needed
- `ReactorMessage::Completion` — reused for deferred delivery
- `EffectRequest.timeout` — reused for required timeout
- Reactor catch-all arm in `execute_effect_once_configured` — replaced with real handler

---

## Non-Goals (this iteration)

- SQLite persistence of pending approvals (future)
- Notification system (email/Slack when approval needed)
- Role-based approval (any human with HTTP access can approve)
- Approval chains (multi-person sign-off)
