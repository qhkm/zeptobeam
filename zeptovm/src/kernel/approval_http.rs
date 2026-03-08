use axum::{
  extract::{Path, State},
  http::StatusCode,
  response::IntoResponse,
  routing::{get, post},
  Json, Router,
};
use crossbeam_channel::Sender;

use crate::core::effect::EffectResult;
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
  if !valid_actions.contains(&body.action.as_str())
  {
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

  use crate::core::effect::{
    EffectId, EffectKind, EffectStatus,
  };
  use crate::kernel::approval_store::PendingApproval;
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
      resp.into_body(),
      usize::MAX,
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
          .header(
            "content-type",
            "application/json",
          )
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
          .header(
            "content-type",
            "application/json",
          )
          .body(Body::from(
            r#"{"action":"approve"}"#,
          ))
          .unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(
      resp.status(),
      StatusCode::NOT_FOUND
    );
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
          .header(
            "content-type",
            "application/json",
          )
          .body(Body::from(
            r#"{"action":"yolo"}"#,
          ))
          .unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(
      resp.status(),
      StatusCode::BAD_REQUEST
    );
  }
}
