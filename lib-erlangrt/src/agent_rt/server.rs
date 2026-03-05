use std::{
  collections::HashMap,
  convert::Infallible,
  sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
  },
  time::{Duration, Instant},
};

use axum::{
  body::Body,
  extract::{Path, State},
  http::{header, HeaderMap, Request, Response, StatusCode},
  middleware::{self, Next},
  response::{
    sse::{Event, KeepAlive, Sse},
    IntoResponse,
  },
  routing::{delete, get, post},
  Json, Router,
};
use tokio::sync::oneshot;
use tokio_stream::{wrappers::IntervalStream, StreamExt};

use crate::agent_rt::{
  approval_gate::{ApprovalDecision, SharedApprovalRegistry},
  mcp_types::{
    cancel_process_schema, error_codes, get_metrics_schema,
    get_orchestration_status_schema, list_processes_schema, send_message_schema,
    spawn_orchestration_schema, CancelProcessResult, GetMetricsResult, InitializeParams,
    InitializeResult, JsonRpcRequest, JsonRpcResponse, ListProcessesResult,
    OrchestrationStatusResult, PingResult, ProcessInfo, SendMessageResult,
    SpawnOrchestrationResult, ToolContent, ToolInfo, ToolsCallParams, ToolsCallResult,
    ToolsListResult, MCP_PROTOCOL_VERSION,
  },
  observability::{RuntimeMetrics, RuntimeMetricsSnapshot},
  types::{Message, Reason},
};

#[derive(Clone)]
pub struct ServerState {
  pub metrics: Arc<RuntimeMetrics>,
  pub process_count: Arc<std::sync::atomic::AtomicUsize>,
  pub approval_registry: Option<SharedApprovalRegistry>,
}

/// Runtime callbacks used by HTTP MCP tool handlers.
pub trait McpRuntimeOps: Send + Sync {
  fn list_processes(&self) -> Result<Vec<ProcessInfo>, String>;
  fn send_message(&self, pid: u64, message: Message) -> Result<(), String>;
  fn cancel_process(&self, pid: u64, reason: Reason) -> Result<(), String>;
}

/// Default runtime callbacks when MCP HTTP is enabled without runtime wiring.
#[derive(Default)]
pub struct NoopMcpRuntimeOps;

impl McpRuntimeOps for NoopMcpRuntimeOps {
  fn list_processes(&self) -> Result<Vec<ProcessInfo>, String> {
    Err("MCP runtime operations are not configured".to_string())
  }

  fn send_message(&self, _pid: u64, _message: Message) -> Result<(), String> {
    Err("MCP runtime operations are not configured".to_string())
  }

  fn cancel_process(&self, _pid: u64, _reason: Reason) -> Result<(), String> {
    Err("MCP runtime operations are not configured".to_string())
  }
}

/// Extended server state that includes MCP configuration.
#[derive(Clone)]
pub struct McpServerStateExt {
  pub base: ServerState,
  /// MCP auth token (if None, no auth required).
  pub mcp_auth_token: Option<String>,
  /// Whether MCP HTTP server is enabled.
  pub mcp_enabled: bool,
  /// Counter for generating HTTP MCP session IDs.
  pub mcp_session_counter: Arc<AtomicU64>,
  /// Session timeout for HTTP MCP connections.
  pub mcp_session_timeout: Duration,
  /// Last-seen activity for active HTTP MCP sessions.
  pub mcp_sessions: Arc<Mutex<HashMap<String, Instant>>>,
  /// Runtime callbacks used by HTTP MCP tools.
  pub mcp_runtime_ops: Arc<dyn McpRuntimeOps>,
}

impl McpServerStateExt {
  /// Create a new extended server state.
  pub fn new(
    base: ServerState,
    mcp_auth_token: Option<String>,
    mcp_enabled: bool,
  ) -> Self {
    Self::with_timeout(base, mcp_auth_token, mcp_enabled, 3600)
  }

  /// Create a new extended server state with explicit MCP session timeout.
  pub fn with_timeout(
    base: ServerState,
    mcp_auth_token: Option<String>,
    mcp_enabled: bool,
    session_timeout_secs: u64,
  ) -> Self {
    Self {
      base,
      mcp_auth_token,
      mcp_enabled,
      mcp_session_counter: Arc::new(AtomicU64::new(1)),
      mcp_session_timeout: Duration::from_secs(session_timeout_secs.max(1)),
      mcp_sessions: Arc::new(Mutex::new(HashMap::new())),
      mcp_runtime_ops: Arc::new(NoopMcpRuntimeOps),
    }
  }

  pub fn with_runtime_ops(mut self, runtime_ops: Arc<dyn McpRuntimeOps>) -> Self {
    self.mcp_runtime_ops = runtime_ops;
    self
  }

  fn prune_expired_http_sessions_locked(&self, sessions: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    let timeout = self.mcp_session_timeout;
    sessions.retain(|_, last_seen| now.duration_since(*last_seen) <= timeout);
  }

  pub fn create_or_refresh_http_session(&self, preferred: Option<&str>) -> String {
    let session_id = preferred
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(str::to_string)
      .unwrap_or_else(|| {
        format!(
          "mcp-{:016x}",
          self.mcp_session_counter.fetch_add(1, Ordering::Relaxed)
        )
      });

    let mut sessions = self.mcp_sessions.lock().unwrap();
    self.prune_expired_http_sessions_locked(&mut sessions);
    sessions.insert(session_id.clone(), Instant::now());
    session_id
  }

  pub fn touch_http_session(&self, session_id: &str) -> bool {
    let mut sessions = self.mcp_sessions.lock().unwrap();
    self.prune_expired_http_sessions_locked(&mut sessions);
    if let Some(last_seen) = sessions.get_mut(session_id) {
      *last_seen = Instant::now();
      true
    } else {
      false
    }
  }

  pub fn remove_http_session(&self, session_id: &str) -> bool {
    let mut sessions = self.mcp_sessions.lock().unwrap();
    self.prune_expired_http_sessions_locked(&mut sessions);
    sessions.remove(session_id).is_some()
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

    Ok(Self {
      shutdown_tx,
      handle,
    })
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

    Ok(Self {
      shutdown_tx,
      handle,
    })
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

async fn metrics_handler(
  State(state): State<ServerState>,
) -> Json<RuntimeMetricsSnapshot> {
  let snap = state.metrics.snapshot(
    state
      .process_count
      .load(std::sync::atomic::Ordering::Relaxed),
  );
  Json(snap)
}

async fn list_approvals_handler(
  State(state): State<ServerState>,
) -> Json<serde_json::Value> {
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
      return Json(serde_json::json!({"error": "approval registry not configured"}))
    }
  };
  let approved = body
    .get("approved")
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
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

async fn health_handler_ext(
  State(state): State<McpServerStateExt>,
) -> Json<serde_json::Value> {
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
      return Json(serde_json::json!({"error": "approval registry not configured"}))
    }
  };
  let approved = body
    .get("approved")
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
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
        return Ok(
          Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
              r#"{"jsonrpc":"2.0","error":{"code":-32003,"message":"Unauthorized"}}"#,
            ))
            .unwrap(),
        );
      }
    }
  }

  Ok(next.run(request).await)
}

/// MCP POST handler (main JSON-RPC endpoint).
async fn mcp_post_handler(
  State(state): State<McpServerStateExt>,
  headers: HeaderMap,
  Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
  let request: JsonRpcRequest = match serde_json::from_value(body) {
    Ok(req) => req,
    Err(e) => {
      let response = JsonRpcResponse::error(
        None,
        error_codes::PARSE_ERROR,
        format!("Parse error: {}", e),
      );
      return (StatusCode::BAD_REQUEST, Json(response)).into_response();
    }
  };

  let request_id = request.id.clone();
  let method = request.method.clone();
  let session_id = headers
    .get("mcp-session-id")
    .and_then(|h| h.to_str().ok())
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(str::to_string);

  let (response, response_session_id): (JsonRpcResponse, Option<String>) =
    if method == "initialize" {
      let params: InitializeParams = match request.params {
        Some(p) => match serde_json::from_value(p) {
          Ok(p) => p,
          Err(e) => {
            let response = JsonRpcResponse::error(
              request_id,
              error_codes::INVALID_PARAMS,
              format!("Invalid initialize params: {}", e),
            );
            return (StatusCode::BAD_REQUEST, Json(response)).into_response();
          }
        },
        None => {
          let response = JsonRpcResponse::error(
            request_id,
            error_codes::INVALID_PARAMS,
            "Missing initialize params",
          );
          return (StatusCode::BAD_REQUEST, Json(response)).into_response();
        }
      };

      if params.protocol_version != MCP_PROTOCOL_VERSION {
        (
          JsonRpcResponse::error(
            request_id,
            error_codes::INVALID_PARAMS,
            format!(
              "Unsupported protocol version: {}. Supported: {}",
              params.protocol_version, MCP_PROTOCOL_VERSION
            ),
          ),
          None,
        )
      } else {
        let sid = state.create_or_refresh_http_session(session_id.as_deref());
        let result = InitializeResult::default_result();
        match JsonRpcResponse::success(request_id, result) {
          Ok(resp) => (resp, Some(sid)),
          Err(e) => (
            JsonRpcResponse::error(
              None,
              error_codes::INTERNAL_ERROR,
              format!("Serialization error: {}", e),
            ),
            None,
          ),
        }
      }
    } else {
      let Some(session_id) = session_id else {
        let response = JsonRpcResponse::error(
          request_id,
          error_codes::SESSION_NOT_INITIALIZED,
          "Session not initialized. Call initialize first.",
        );
        return (StatusCode::BAD_REQUEST, Json(response)).into_response();
      };

      if !state.touch_http_session(&session_id) {
        let response = JsonRpcResponse::error(
          request_id,
          error_codes::SESSION_EXPIRED,
          "Session not found or expired. Call initialize again.",
        );
        return (StatusCode::BAD_REQUEST, Json(response)).into_response();
      }

      match method.as_str() {
        "tools/list" => {
          let result = ToolsListResult {
            tools: mcp_http_tools(),
            next_cursor: None,
          };
          match JsonRpcResponse::success(request_id, result) {
            Ok(resp) => (resp, None),
            Err(e) => (
              JsonRpcResponse::error(
                None,
                error_codes::INTERNAL_ERROR,
                format!("Serialization error: {}", e),
              ),
              None,
            ),
          }
        }
        "tools/call" => {
          let params: ToolsCallParams = match request.params {
            Some(p) => match serde_json::from_value(p) {
              Ok(p) => p,
              Err(e) => {
                let response = JsonRpcResponse::error(
                  request_id,
                  error_codes::INVALID_PARAMS,
                  format!("Invalid tools/call params: {}", e),
                );
                return (StatusCode::BAD_REQUEST, Json(response)).into_response();
              }
            },
            None => {
              let response = JsonRpcResponse::error(
                request_id,
                error_codes::INVALID_PARAMS,
                "Missing tools/call params",
              );
              return (StatusCode::BAD_REQUEST, Json(response)).into_response();
            }
          };

          match mcp_http_call_tool(&state, params) {
            Ok(result) => match JsonRpcResponse::success(request_id, result) {
              Ok(resp) => (resp, None),
              Err(e) => (
                JsonRpcResponse::error(
                  None,
                  error_codes::INTERNAL_ERROR,
                  format!("Serialization error: {}", e),
                ),
                None,
              ),
            },
            Err(msg) => (
              JsonRpcResponse::error(request_id, error_codes::TOOL_NOT_FOUND, msg),
              None,
            ),
          }
        }
        "ping" => match JsonRpcResponse::success(request_id, PingResult {}) {
          Ok(resp) => (resp, None),
          Err(e) => (
            JsonRpcResponse::error(
              None,
              error_codes::INTERNAL_ERROR,
              format!("Serialization error: {}", e),
            ),
            None,
          ),
        },
        _ => (
          JsonRpcResponse::error(
            request_id,
            error_codes::METHOD_NOT_FOUND,
            "Method not found",
          ),
          None,
        ),
      }
    };

  let mut builder = Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/json");

  if let Some(sid) = response_session_id {
    builder = builder.header("mcp-session-id", sid);
  }

  let body = serde_json::to_vec(&response).unwrap_or_else(|_| {
        br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"Serialization error"}}"#
            .to_vec()
    });

  match builder.body(Body::from(body)) {
    Ok(resp) => resp,
    Err(_) => (
      StatusCode::INTERNAL_SERVER_ERROR,
      Json(serde_json::json!({
          "jsonrpc": "2.0",
          "id": null,
          "error": {
              "code": error_codes::INTERNAL_ERROR,
              "message": "Failed to build response"
          }
      })),
    )
      .into_response(),
  }
}

/// MCP GET handler (SSE endpoint for notifications).
async fn mcp_get_handler(
  State(state): State<McpServerStateExt>,
  headers: HeaderMap,
) -> Response<Body> {
  let Some(session_id) = headers
    .get("mcp-session-id")
    .and_then(|h| h.to_str().ok())
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(str::to_string)
  else {
    return (
      StatusCode::BAD_REQUEST,
      Json(serde_json::json!({
          "jsonrpc": "2.0",
          "id": null,
          "error": {
              "code": error_codes::INVALID_PARAMS,
              "message": "Missing mcp-session-id header"
          }
      })),
    )
      .into_response();
  };

  if !state.touch_http_session(&session_id) {
    return (
      StatusCode::BAD_REQUEST,
      Json(serde_json::json!({
          "jsonrpc": "2.0",
          "id": null,
          "error": {
              "code": error_codes::SESSION_EXPIRED,
              "message": "Session not found or expired. Call initialize again."
          }
      })),
    )
      .into_response();
  }

  let stream_state = state.clone();
  let stream_session_id = session_id.clone();
  let mut interval = tokio::time::interval(Duration::from_secs(15));
  interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
  let stream = IntervalStream::new(interval).map(move |_| {
    let active = stream_state.touch_http_session(&stream_session_id);
    let event = if active {
      Event::default()
        .event("heartbeat")
        .data(r#"{"type":"heartbeat"}"#)
    } else {
      Event::default()
        .event("session_expired")
        .data(r#"{"type":"session_expired"}"#)
    };
    Ok::<Event, Infallible>(event)
  });

  Sse::new(stream)
    .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
    .into_response()
}

/// MCP DELETE handler (session termination).
async fn mcp_delete_handler(
  State(state): State<McpServerStateExt>,
  headers: HeaderMap,
) -> impl IntoResponse {
  let Some(session_id) = headers
    .get("mcp-session-id")
    .and_then(|h| h.to_str().ok())
    .map(str::trim)
    .filter(|s| !s.is_empty())
  else {
    return (
      StatusCode::BAD_REQUEST,
      Json(serde_json::json!({
          "jsonrpc": "2.0",
          "id": null,
          "error": {
              "code": error_codes::INVALID_PARAMS,
              "message": "Missing mcp-session-id header"
          }
      })),
    );
  };

  if !state.remove_http_session(session_id) {
    return (
      StatusCode::BAD_REQUEST,
      Json(serde_json::json!({
          "jsonrpc": "2.0",
          "id": null,
          "error": {
              "code": error_codes::SESSION_EXPIRED,
              "message": "Session not found or already expired"
          }
      })),
    );
  }

  (
    StatusCode::OK,
    Json(serde_json::json!({
        "jsonrpc": "2.0",
        "result": {
            "status": "session_terminated",
            "session_id": session_id
        }
    })),
  )
}

fn mcp_http_tools() -> Vec<ToolInfo> {
  vec![
    ToolInfo::new("spawn_orchestration", spawn_orchestration_schema())
      .unwrap()
      .with_description("Start a new orchestration with a goal"),
    ToolInfo::new(
      "get_orchestration_status",
      get_orchestration_status_schema(),
    )
    .unwrap()
    .with_description("Check status/results of an orchestration"),
    ToolInfo::new("list_processes", list_processes_schema())
      .unwrap()
      .with_description("List active agent processes"),
    ToolInfo::new("send_message", send_message_schema())
      .unwrap()
      .with_description("Send a message to a process by PID"),
    ToolInfo::new("get_metrics", get_metrics_schema())
      .unwrap()
      .with_description("Get runtime metrics snapshot"),
    ToolInfo::new("cancel_process", cancel_process_schema())
      .unwrap()
      .with_description("Terminate a process by PID"),
  ]
}

fn parse_pid_arg(args: &serde_json::Value) -> Result<u64, String> {
  let Some(pid_value) = args.get("pid") else {
    return Err("Missing 'pid' parameter".to_string());
  };

  if let Some(pid) = pid_value.as_u64() {
    return Ok(pid);
  }

  pid_value
    .as_str()
    .and_then(|s| s.parse::<u64>().ok())
    .ok_or_else(|| "Missing or invalid 'pid' parameter".to_string())
}

fn parse_reason_arg(args: &serde_json::Value) -> Reason {
  let reason_str = args
    .get("reason")
    .and_then(|v| v.as_str())
    .unwrap_or("cancelled");
  match reason_str {
    "normal" => Reason::Normal,
    "shutdown" => Reason::Shutdown,
    _ => Reason::Custom(reason_str.to_string()),
  }
}

fn mcp_http_call_tool(
  state: &McpServerStateExt,
  params: ToolsCallParams,
) -> Result<ToolsCallResult, String> {
  let (text, is_error) = match params.name.as_str() {
    "spawn_orchestration" => {
      let args = params.arguments.unwrap_or_default();
      let goal_ok = args
        .get("goal")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .is_some();
      if !goal_ok {
        return Err("Missing 'goal' parameter".to_string());
      }
      let result = SpawnOrchestrationResult {
        orchestration_id: format!(
          "orch-http-{:016x}",
          state.mcp_session_counter.fetch_add(1, Ordering::Relaxed)
        ),
        status: "pending".to_string(),
      };
      (
        serde_json::to_string(&result).map_err(|e| e.to_string())?,
        false,
      )
    }
    "get_orchestration_status" => {
      let args = params.arguments.unwrap_or_default();
      let orchestration_id = args
        .get("orchestration_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing 'orchestration_id' parameter".to_string())?;
      let result = OrchestrationStatusResult {
        orchestration_id: orchestration_id.to_string(),
        status: "unknown".to_string(),
        results: None,
      };
      (
        serde_json::to_string(&result).map_err(|e| e.to_string())?,
        false,
      )
    }
    "list_processes" => match state.mcp_runtime_ops.list_processes() {
      Ok(processes) => {
        let result = ListProcessesResult { processes };
        (
          serde_json::to_string(&result).map_err(|e| e.to_string())?,
          false,
        )
      }
      Err(e) => (serde_json::json!({ "error": e }).to_string(), true),
    },
    "send_message" => {
      let args = params.arguments.unwrap_or_default();
      let pid = parse_pid_arg(&args)?;
      let message_data = args
        .get("message")
        .cloned()
        .ok_or_else(|| "Missing 'message' parameter".to_string())?;
      let message = match message_data {
        serde_json::Value::String(text) => Message::Text(text),
        value => Message::Json(value),
      };
      match state.mcp_runtime_ops.send_message(pid, message) {
        Ok(_) => (
          serde_json::to_string(&SendMessageResult { success: true })
            .map_err(|e| e.to_string())?,
          false,
        ),
        Err(e) => (
          serde_json::json!({
              "result": SendMessageResult { success: false },
              "error": e
          })
          .to_string(),
          true,
        ),
      }
    }
    "get_metrics" => {
      let snapshot = state.base.metrics.snapshot(
        state
          .base
          .process_count
          .load(std::sync::atomic::Ordering::Relaxed),
      );
      let result = GetMetricsResult {
        uptime_secs: snapshot.uptime_secs,
        active_processes: snapshot.active_processes as usize,
        messages_sent_total: snapshot.messages_sent_total,
        messages_processed_total: snapshot.messages_processed_total,
      };
      (
        serde_json::to_string(&result).map_err(|e| e.to_string())?,
        false,
      )
    }
    "cancel_process" => {
      let args = params.arguments.unwrap_or_default();
      let pid = parse_pid_arg(&args)?;
      let reason = parse_reason_arg(&args);
      match state.mcp_runtime_ops.cancel_process(pid, reason) {
        Ok(_) => (
          serde_json::to_string(&CancelProcessResult { success: true })
            .map_err(|e| e.to_string())?,
          false,
        ),
        Err(e) => (
          serde_json::json!({
              "result": CancelProcessResult { success: false },
              "error": e
          })
          .to_string(),
          true,
        ),
      }
    }
    _ => return Err(format!("Tool not found: {}", params.name)),
  };

  Ok(ToolsCallResult {
    content: Some(vec![ToolContent::text(text)]),
    is_error: Some(is_error),
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::http::HeaderValue;

  struct TestRuntimeOps {
    sent: Mutex<Vec<(u64, Message)>>,
    cancelled: Mutex<Vec<(u64, Reason)>>,
  }

  impl TestRuntimeOps {
    fn new() -> Self {
      Self {
        sent: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
      }
    }
  }

  impl McpRuntimeOps for TestRuntimeOps {
    fn list_processes(&self) -> Result<Vec<ProcessInfo>, String> {
      Ok(vec![ProcessInfo {
        pid: 42,
        status: "Running".to_string(),
        priority: "Normal".to_string(),
        mailbox_depth: 3,
      }])
    }

    fn send_message(&self, pid: u64, message: Message) -> Result<(), String> {
      self.sent.lock().unwrap().push((pid, message));
      Ok(())
    }

    fn cancel_process(&self, pid: u64, reason: Reason) -> Result<(), String> {
      self.cancelled.lock().unwrap().push((pid, reason));
      Ok(())
    }
  }

  fn mcp_test_state() -> ServerState {
    ServerState {
      metrics: Arc::new(RuntimeMetrics::new()),
      process_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
      approval_registry: None,
    }
  }

  fn first_text(result: &ToolsCallResult) -> String {
    match result.content.as_ref().and_then(|items| items.first()) {
      Some(ToolContent::Text { text }) => text.clone(),
      _ => panic!("expected text content"),
    }
  }

  #[tokio::test]
  async fn test_health_endpoint() {
    let mut state = mcp_test_state();
    state.process_count = Arc::new(std::sync::atomic::AtomicUsize::new(5));
    let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();

    // Server should start without error
    // (Full HTTP client testing is integration-level)

    server.shutdown().await;
  }

  #[tokio::test]
  async fn test_mcp_server_with_no_auth() {
    let base_state = mcp_test_state();
    let state = McpServerStateExt::new(base_state, None, true);
    let server = HealthServer::start_with_mcp("127.0.0.1:0", state)
      .await
      .unwrap();

    // Server should start without error
    server.shutdown().await;
  }

  #[tokio::test]
  async fn test_mcp_server_with_auth() {
    let base_state = mcp_test_state();
    let state = McpServerStateExt::new(base_state, Some("test-token".to_string()), true);
    let server = HealthServer::start_with_mcp("127.0.0.1:0", state)
      .await
      .unwrap();

    // Server should start without error
    server.shutdown().await;
  }

  #[tokio::test]
  async fn test_mcp_server_disabled() {
    let base_state = mcp_test_state();
    let state = McpServerStateExt::new(base_state, None, false);
    let server = HealthServer::start_with_mcp("127.0.0.1:0", state)
      .await
      .unwrap();

    // Server should start without error
    server.shutdown().await;
  }

  #[test]
  fn test_http_mcp_session_lifecycle() {
    let base_state = mcp_test_state();
    let state = McpServerStateExt::with_timeout(base_state, None, true, 60);

    let sid = state.create_or_refresh_http_session(None);
    assert!(state.touch_http_session(&sid));
    assert!(state.remove_http_session(&sid));
    assert!(!state.touch_http_session(&sid));
  }

  #[test]
  fn test_http_mcp_session_expires() {
    let base_state = mcp_test_state();
    let state = McpServerStateExt::with_timeout(base_state, None, true, 1);
    let sid = state.create_or_refresh_http_session(None);

    std::thread::sleep(Duration::from_millis(1100));

    assert!(!state.touch_http_session(&sid));
    assert!(!state.remove_http_session(&sid));
  }

  #[tokio::test]
  async fn test_mcp_get_requires_active_session_header() {
    let state = McpServerStateExt::new(mcp_test_state(), None, true);
    let response = mcp_get_handler(State(state), HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  }

  #[tokio::test]
  async fn test_mcp_get_returns_sse_for_active_session() {
    let state = McpServerStateExt::new(mcp_test_state(), None, true);
    let sid = state.create_or_refresh_http_session(None);
    let mut headers = HeaderMap::new();
    headers.insert(
      "mcp-session-id",
      HeaderValue::from_str(&sid).expect("valid header"),
    );

    let response = mcp_get_handler(State(state), headers).await;
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
      .headers()
      .get(header::CONTENT_TYPE)
      .and_then(|v| v.to_str().ok())
      .unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"));
  }

  #[test]
  fn test_mcp_http_tools_use_runtime_ops() {
    let runtime_ops = Arc::new(TestRuntimeOps::new());
    let state = McpServerStateExt::new(mcp_test_state(), None, true)
      .with_runtime_ops(runtime_ops.clone());

    let list = mcp_http_call_tool(
      &state,
      ToolsCallParams {
        name: "list_processes".to_string(),
        arguments: None,
      },
    )
    .expect("list_processes succeeds");
    assert_eq!(list.is_error, Some(false));
    let list_payload: ListProcessesResult =
      serde_json::from_str(&first_text(&list)).expect("valid list payload");
    assert_eq!(list_payload.processes.len(), 1);
    assert_eq!(list_payload.processes[0].pid, 42);

    let send = mcp_http_call_tool(
      &state,
      ToolsCallParams {
        name: "send_message".to_string(),
        arguments: Some(serde_json::json!({
            "pid": "42",
            "message": { "kind": "ping" }
        })),
      },
    )
    .expect("send_message succeeds");
    assert_eq!(send.is_error, Some(false));
    let sent = runtime_ops.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, 42);
    match &sent[0].1 {
      Message::Json(value) => assert_eq!(value["kind"], "ping"),
      _ => panic!("expected JSON message"),
    }
    drop(sent);

    let cancel = mcp_http_call_tool(
      &state,
      ToolsCallParams {
        name: "cancel_process".to_string(),
        arguments: Some(serde_json::json!({
            "pid": 42,
            "reason": "shutdown"
        })),
      },
    )
    .expect("cancel_process succeeds");
    assert_eq!(cancel.is_error, Some(false));
    let cancelled = runtime_ops.cancelled.lock().unwrap();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].0, 42);
    assert!(matches!(cancelled[0].1, Reason::Shutdown));
  }
}
