use std::sync::Arc;

use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use tokio::sync::oneshot;

use crate::agent_rt::approval_gate::{ApprovalDecision, SharedApprovalRegistry};
use crate::agent_rt::observability::{RuntimeMetrics, RuntimeMetricsSnapshot};

#[derive(Clone)]
pub struct ServerState {
    pub metrics: Arc<RuntimeMetrics>,
    pub process_count: Arc<std::sync::atomic::AtomicUsize>,
    pub approval_registry: Option<SharedApprovalRegistry>,
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
            .route("/approvals", get(list_approvals_handler))
            .route("/approve/:orch_id/:task_id", post(approve_handler))
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

async fn list_approvals_handler(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let registry = match &state.approval_registry {
        Some(r) => r.lock().unwrap(),
        None => return Json(serde_json::json!({"pending": []})),
    };
    let pending: Vec<serde_json::Value> = registry
        .pending_requests()
        .iter()
        .map(|(orch_id, task_id, req)| {
            serde_json::json!({
                "orch_id": orch_id,
                "task_id": task_id,
                "message": req.message,
                "task": req.task,
            })
        })
        .collect();
    Json(serde_json::json!({"pending": pending}))
}

async fn approve_handler(
    State(state): State<ServerState>,
    Path((orch_id, task_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let registry = match &state.approval_registry {
        Some(r) => r,
        None => {
            return Json(
                serde_json::json!({"error": "approval registry not configured"}),
            )
        }
    };
    let approved = body.get("approved").and_then(|v| v.as_bool()).unwrap_or(false);
    let decision = if approved {
        ApprovalDecision::Approved
    } else {
        let reason = body
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("rejected")
            .to_string();
        ApprovalDecision::Rejected { reason }
    };
    let mut reg = registry.lock().unwrap();
    let ok = reg.submit_decision(&orch_id, &task_id, decision);
    Json(serde_json::json!({"ok": ok}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(5)),
            approval_registry: None,
        };
        let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();

        // Server should start without error
        // (Full HTTP client testing is integration-level)

        server.shutdown().await;
    }
}
