use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, Request, Response, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use tokio::sync::oneshot;

use crate::agent_rt::{
    approval_gate::{ApprovalDecision, SharedApprovalRegistry},
    observability::{RuntimeMetrics, RuntimeMetricsSnapshot},
};

#[derive(Clone)]
pub struct ServerState {
    pub metrics: Arc<RuntimeMetrics>,
    pub process_count: Arc<std::sync::atomic::AtomicUsize>,
    pub approval_registry: Option<SharedApprovalRegistry>,
}

/// Extended server state that includes MCP configuration.
#[derive(Clone)]
pub struct McpServerStateExt {
    pub base: ServerState,
    /// MCP auth token (if None, no auth required).
    pub mcp_auth_token: Option<String>,
    /// Whether MCP HTTP server is enabled.
    pub mcp_enabled: bool,
}

impl McpServerStateExt {
    /// Create a new extended server state.
    pub fn new(base: ServerState, mcp_auth_token: Option<String>, mcp_enabled: bool) -> Self {
        Self {
            base,
            mcp_auth_token,
            mcp_enabled,
        }
    }
}

pub struct HealthServer {
    shutdown_tx: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl HealthServer {
    /// Start a basic health server without MCP.
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

    /// Start a health server with MCP routes enabled.
    pub async fn start_with_mcp(
        bind: &str,
        state: McpServerStateExt,
    ) -> Result<Self, std::io::Error> {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Build the router
        let app = Self::build_router(state);

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

    /// Build the router with MCP routes.
    fn build_router(state: McpServerStateExt) -> Router {
        // Build base routes with extended state
        let base_router = Router::new()
            .route("/health", get(health_handler_ext))
            .route("/metrics", get(metrics_handler_ext))
            .route("/approvals", get(list_approvals_handler_ext))
            .route("/approve/:orch_id/:task_id", post(approve_handler_ext))
            .with_state(state.clone());

        // Add MCP routes if enabled
        if state.mcp_enabled {
            let mcp_router = Router::new()
                .route("/mcp", post(mcp_post_handler))
                .route("/mcp", get(mcp_get_handler))
                .route("/mcp", delete(mcp_delete_handler))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    mcp_auth_middleware,
                ))
                .with_state(state);

            base_router.merge(mcp_router)
        } else {
            base_router
        }
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.handle.await;
    }
}

// Basic handlers (work with ServerState)

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

// Extended handlers (work with McpServerStateExt)

async fn health_handler_ext(State(state): State<McpServerStateExt>) -> Json<serde_json::Value> {
    let uptime = state
        .base
        .metrics
        .snapshot(
            state
                .base
                .process_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .uptime_secs;
    Json(serde_json::json!({
        "status": "ok",
        "uptime_secs": uptime,
    }))
}

async fn metrics_handler_ext(
    State(state): State<McpServerStateExt>,
) -> Json<RuntimeMetricsSnapshot> {
    let snap = state.base.metrics.snapshot(
        state
            .base
            .process_count
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    Json(snap)
}

async fn list_approvals_handler_ext(
    State(state): State<McpServerStateExt>,
) -> Json<serde_json::Value> {
    let registry = match &state.base.approval_registry {
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

async fn approve_handler_ext(
    State(state): State<McpServerStateExt>,
    Path((orch_id, task_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let registry = match &state.base.approval_registry {
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

/// MCP authentication middleware.
async fn mcp_auth_middleware(
    State(state): State<McpServerStateExt>,
    request: Request<Body>,
    next: Next,
) -> Result<Response<Body>, StatusCode> {
    // Check if auth is required
    if let Some(expected_token) = &state.mcp_auth_token {
        // Extract the Authorization header
        let auth_header = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok());

        let provided_token = auth_header.and_then(|header| {
            header
                .strip_prefix("Bearer ")
                .or_else(|| header.strip_prefix("bearer "))
        });

        match provided_token {
            Some(token) if token == expected_token => {
                // Auth successful, continue
            }
            _ => {
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","error":{"code":-32003,"message":"Unauthorized"}}"#,
                    ))
                    .unwrap());
            }
        }
    }

    Ok(next.run(request).await)
}

/// MCP POST handler (main JSON-RPC endpoint).
async fn mcp_post_handler(
    State(_state): State<McpServerStateExt>,
    Json(_body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Note: In the full implementation, this would:
    // 1. Parse the JSON-RPC request
    // 2. Get or create a session from cookies/headers
    // 3. Call the McpProtocolHandler
    // 4. Return the JSON-RPC response

    // For now, return a placeholder response
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": -32603,
                "message": "MCP server handler not yet fully implemented. Use stdio transport for now."
            }
        })),
    )
}

/// MCP GET handler (SSE endpoint for notifications).
async fn mcp_get_handler(State(_state): State<McpServerStateExt>) -> impl IntoResponse {
    // SSE endpoint for server-to-client notifications
    // This is a placeholder - full implementation would stream events
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32603,
                "message": "SSE endpoint not yet implemented"
            }
        })),
    )
}

/// MCP DELETE handler (session termination).
async fn mcp_delete_handler(State(_state): State<McpServerStateExt>) -> impl IntoResponse {
    // Terminate the session
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "result": {
                "status": "session_terminated"
            }
        })),
    )
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

    #[tokio::test]
    async fn test_mcp_server_with_no_auth() {
        let base_state = ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            approval_registry: None,
        };
        let state = McpServerStateExt::new(base_state, None, true);
        let server = HealthServer::start_with_mcp("127.0.0.1:0", state).await.unwrap();

        // Server should start without error
        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_mcp_server_with_auth() {
        let base_state = ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            approval_registry: None,
        };
        let state = McpServerStateExt::new(base_state, Some("test-token".to_string()), true);
        let server = HealthServer::start_with_mcp("127.0.0.1:0", state).await.unwrap();

        // Server should start without error
        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_mcp_server_disabled() {
        let base_state = ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            approval_registry: None,
        };
        let state = McpServerStateExt::new(base_state, None, false);
        let server = HealthServer::start_with_mcp("127.0.0.1:0", state).await.unwrap();

        // Server should start without error
        server.shutdown().await;
    }
}
