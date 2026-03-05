//! MCP Server implementation.
//!
//! This module provides the MCP protocol handler and session management.
//! It handles MCP methods like initialize, tools/list, tools/call, and ping.

use crate::agent_rt::{
  mcp_types::*,
  observability::{RuntimeMetrics, RuntimeMetricsSnapshot},
  scheduler::AgentScheduler,
  types::{AgentPid, Message, Reason},
};
use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

/// MCP protocol version supported by this server.
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2024-11-05";

/// An active MCP session.
#[derive(Debug, Clone)]
pub struct McpSession {
  /// Session ID (UUID).
  pub id: String,
  /// When the session was created.
  pub created_at: Instant,
  /// Last activity timestamp.
  pub last_active: Instant,
  /// Whether the session has been initialized.
  pub initialized: bool,
}

impl McpSession {
  /// Create a new session with the given ID.
  pub fn new(id: impl Into<String>) -> Self {
    let now = Instant::now();
    Self {
      id: id.into(),
      created_at: now,
      last_active: now,
      initialized: false,
    }
  }

  /// Mark the session as active (update last_active timestamp).
  pub fn touch(&mut self) {
    self.last_active = Instant::now();
  }

  /// Mark the session as initialized.
  pub fn mark_initialized(&mut self) {
    self.initialized = true;
  }

  /// Check if the session has expired based on the given timeout.
  pub fn is_expired(&self, timeout: Duration) -> bool {
    self.last_active.elapsed() > timeout
  }
}

/// Handle to the runtime for MCP server operations.
#[derive(Clone)]
pub struct RuntimeHandle {
  /// Access to the scheduler for process management.
  pub scheduler: Arc<Mutex<AgentScheduler>>,
  /// Runtime metrics.
  pub metrics: Arc<RuntimeMetrics>,
}

impl RuntimeHandle {
  /// Create a new runtime handle.
  pub fn new(
    scheduler: Arc<Mutex<AgentScheduler>>,
    metrics: Arc<RuntimeMetrics>,
  ) -> Self {
    Self { scheduler, metrics }
  }

  /// Get a metrics snapshot.
  pub fn metrics_snapshot(&self) -> RuntimeMetricsSnapshot {
    let scheduler = self.scheduler.lock().unwrap();
    let process_count = scheduler.registry.count();
    self.metrics.snapshot(process_count)
  }

  /// List all processes.
  pub fn list_processes(&self) -> Vec<ProcessInfo> {
    let scheduler = self.scheduler.lock().unwrap();
    scheduler
      .list_processes()
      .into_iter()
      .map(|snap| ProcessInfo {
        pid: snap.pid,
        status: format!("{:?}", snap.status),
        priority: format!("{:?}", snap.priority),
        mailbox_depth: snap.mailbox_depth,
      })
      .collect()
  }

  /// Send a message to a process.
  pub fn send_message(&self, pid: u64, message: Message) -> Result<(), String> {
    let mut scheduler = self.scheduler.lock().unwrap();
    let pid = AgentPid::from_raw(pid);
    scheduler.send(pid, message)
  }

  /// Terminate a process.
  pub fn terminate_process(&self, pid: u64, reason: Reason) {
    let mut scheduler = self.scheduler.lock().unwrap();
    let pid = AgentPid::from_raw(pid);
    scheduler.terminate_process(pid, reason);
  }

  /// Get process count.
  pub fn process_count(&self) -> usize {
    let scheduler = self.scheduler.lock().unwrap();
    scheduler.registry.count()
  }
}

/// MCP server state shared across all sessions.
pub struct McpServerState {
  /// Active sessions mapped by session ID.
  pub sessions: Mutex<HashMap<String, McpSession>>,
  /// Optional bearer token for authentication.
  pub auth_token: Option<String>,
  /// Session timeout duration.
  pub session_timeout: Duration,
  /// Handle to the runtime.
  pub runtime_handle: RuntimeHandle,
}

impl McpServerState {
  /// Create a new MCP server state.
  pub fn new(
    auth_token: Option<String>,
    session_timeout: Duration,
    runtime_handle: RuntimeHandle,
  ) -> Self {
    Self {
      sessions: Mutex::new(HashMap::new()),
      auth_token,
      session_timeout,
      runtime_handle,
    }
  }

  /// Create a new session and return its ID.
  pub fn create_session(&self) -> String {
    let session_id = uuid::generate_uuid();
    let session = McpSession::new(&session_id);
    let mut sessions = self.sessions.lock().unwrap();
    sessions.insert(session_id.clone(), session);
    session_id
  }

  /// Get a session by ID, returning None if not found or expired.
  pub fn get_session(&self, session_id: &str) -> Option<McpSession> {
    let mut sessions = self.sessions.lock().unwrap();

    // Clean up expired sessions first
    let expired_ids: Vec<String> = sessions
      .values()
      .filter(|s| s.is_expired(self.session_timeout))
      .map(|s| s.id.clone())
      .collect();
    for id in expired_ids {
      sessions.remove(&id);
    }

    // Get the requested session
    sessions.get(session_id).cloned()
  }

  /// Update a session (touch it and save changes).
  pub fn update_session(&self, mut session: McpSession) {
    session.touch();
    let mut sessions = self.sessions.lock().unwrap();
    sessions.insert(session.id.clone(), session);
  }

  /// Remove a session.
  pub fn remove_session(&self, session_id: &str) -> bool {
    let mut sessions = self.sessions.lock().unwrap();
    sessions.remove(session_id).is_some()
  }

  /// Return the most recently created session ID, if any.
  pub fn most_recent_session_id(&self) -> Option<String> {
    let sessions = self.sessions.lock().unwrap();
    sessions
      .values()
      .max_by_key(|s| s.created_at)
      .map(|s| s.id.clone())
  }

  /// Verify authentication token.
  pub fn verify_auth(&self, token: Option<&str>) -> bool {
    match &self.auth_token {
      None => true, // No auth required
      Some(expected) => token.map(|t| t == expected).unwrap_or(false),
    }
  }

  /// Get all available tools.
  pub fn get_available_tools(&self) -> Vec<ToolInfo> {
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
}

/// MCP protocol handler.
pub struct McpProtocolHandler {
  state: Arc<McpServerState>,
}

impl McpProtocolHandler {
  /// Create a new protocol handler.
  pub fn new(state: Arc<McpServerState>) -> Self {
    Self { state }
  }

  /// Handle a JSON-RPC request and return a response.
  pub fn handle_request(
    &self,
    request: JsonRpcRequest,
    session_id: Option<&str>,
  ) -> JsonRpcResponse {
    // Handle initialize specially (no session required)
    if request.method == "initialize" {
      return self.handle_initialize(request, session_id);
    }

    // For all other methods, require a valid session
    let session_id = match session_id {
      Some(id) => id,
      None => {
        return JsonRpcResponse::error(
          request.id,
          error_codes::SESSION_NOT_INITIALIZED,
          "Session not initialized. Call initialize first.",
        );
      }
    };

    let session = match self.state.get_session(session_id) {
      Some(s) => s,
      None => {
        return JsonRpcResponse::error(
          request.id,
          error_codes::SESSION_EXPIRED,
          "Session not found or expired",
        );
      }
    };

    if !session.initialized {
      return JsonRpcResponse::error(
        request.id,
        error_codes::SESSION_NOT_INITIALIZED,
        "Session not initialized. Call initialize first.",
      );
    }

    // Dispatch to the appropriate handler
    let response = match request.method.as_str() {
      "tools/list" => self.handle_tools_list(&request, &session),
      "tools/call" => self.handle_tools_call(&request, &session),
      "ping" => self.handle_ping(&request, &session),
      _ => Err(McpError::MethodNotFound(request.method.clone())),
    };

    // Update session activity
    self.state.update_session(session);

    match response {
      Ok(response) => response,
      Err(e) => e.to_response(request.id),
    }
  }

  /// Handle initialize request.
  fn handle_initialize(
    &self,
    request: JsonRpcRequest,
    session_id: Option<&str>,
  ) -> JsonRpcResponse {
    // Parse params
    let params: InitializeParams = match request.params {
      Some(p) => match serde_json::from_value(p) {
        Ok(p) => p,
        Err(e) => {
          return JsonRpcResponse::error(
            request.id,
            error_codes::INVALID_PARAMS,
            format!("Invalid initialize params: {}", e),
          );
        }
      },
      None => {
        return JsonRpcResponse::error(
          request.id,
          error_codes::INVALID_PARAMS,
          "Missing initialize params",
        );
      }
    };

    // Check protocol version compatibility
    if params.protocol_version != SUPPORTED_PROTOCOL_VERSION {
      return JsonRpcResponse::error(
        request.id,
        error_codes::INVALID_PARAMS,
        format!(
          "Unsupported protocol version: {}. Supported: {}",
          params.protocol_version, SUPPORTED_PROTOCOL_VERSION
        ),
      );
    }

    // Create or update session
    let session_id = match session_id {
      Some(id) => id.to_string(),
      None => self.state.create_session(),
    };

    let mut session = McpSession::new(&session_id);
    session.mark_initialized();
    self.state.update_session(session);

    // Return initialize result
    let result = InitializeResult {
      protocol_version: SUPPORTED_PROTOCOL_VERSION.to_string(),
      capabilities: ServerCapabilities::default(),
      server_info: Implementation::zeptoclaw(),
    };

    match JsonRpcResponse::success(request.id.clone(), result) {
      Ok(response) => response,
      Err(e) => JsonRpcResponse::error(
        request.id.clone(),
        error_codes::INTERNAL_ERROR,
        format!("Serialization error: {}", e),
      ),
    }
  }

  /// Handle tools/list request.
  fn handle_tools_list(
    &self,
    request: &JsonRpcRequest,
    _session: &McpSession,
  ) -> Result<JsonRpcResponse, McpError> {
    let _params: ToolsListParams = match &request.params {
      Some(p) => serde_json::from_value(p.clone())?,
      None => ToolsListParams::default(),
    };

    let tools = self.state.get_available_tools();
    let result = ToolsListResult {
      tools,
      next_cursor: None,
    };

    Ok(JsonRpcResponse::success(request.id.clone(), result)?)
  }

  /// Handle tools/call request.
  fn handle_tools_call(
    &self,
    request: &JsonRpcRequest,
    _session: &McpSession,
  ) -> Result<JsonRpcResponse, McpError> {
    let params: ToolsCallParams = match &request.params {
      Some(p) => serde_json::from_value(p.clone())?,
      None => {
        return Err(McpError::InvalidParams(
          "Missing tools/call params".to_string(),
        ))
      }
    };

    let result = match params.name.as_str() {
      "spawn_orchestration" => self.handle_spawn_orchestration(params.arguments),
      "get_orchestration_status" => {
        self.handle_get_orchestration_status(params.arguments)
      }
      "list_processes" => self.handle_list_processes(),
      "send_message" => self.handle_send_message(params.arguments),
      "get_metrics" => self.handle_get_metrics(),
      "cancel_process" => self.handle_cancel_process(params.arguments),
      _ => Err(McpError::ToolNotFound(params.name)),
    }?;

    let tool_result = ToolsCallResult {
      content: Some(vec![ToolContent::text(result)]),
      is_error: Some(false),
    };

    Ok(JsonRpcResponse::success(request.id.clone(), tool_result)?)
  }

  /// Handle ping request.
  fn handle_ping(
    &self,
    request: &JsonRpcRequest,
    _session: &McpSession,
  ) -> Result<JsonRpcResponse, McpError> {
    Ok(JsonRpcResponse::success(request.id.clone(), PingResult {})?)
  }

  /// Handle spawn_orchestration tool.
  fn handle_spawn_orchestration(
    &self,
    args: Option<serde_json::Value>,
  ) -> Result<String, McpError> {
    let args =
      args.ok_or_else(|| McpError::InvalidParams("Missing arguments".to_string()))?;
    let _goal = args
      .get("goal")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError::InvalidParams("Missing 'goal' parameter".to_string()))?;

    // For now, return a placeholder - actual implementation would integrate
    // with the orchestration system
    let result = SpawnOrchestrationResult {
      orchestration_id: format!("orch-{}", uuid::generate_uuid()),
      status: "pending".to_string(),
    };

    Ok(serde_json::to_string(&result)?)
  }

  /// Handle get_orchestration_status tool.
  fn handle_get_orchestration_status(
    &self,
    args: Option<serde_json::Value>,
  ) -> Result<String, McpError> {
    let args =
      args.ok_or_else(|| McpError::InvalidParams("Missing arguments".to_string()))?;
    let orchestration_id = args
      .get("orchestration_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| {
        McpError::InvalidParams("Missing 'orchestration_id' parameter".to_string())
      })?;

    // For now, return a placeholder
    let result = OrchestrationStatusResult {
      orchestration_id: orchestration_id.to_string(),
      status: "unknown".to_string(),
      results: None,
    };

    Ok(serde_json::to_string(&result)?)
  }

  /// Handle list_processes tool.
  fn handle_list_processes(&self) -> Result<String, McpError> {
    let processes = self.state.runtime_handle.list_processes();
    let result = ListProcessesResult { processes };
    Ok(serde_json::to_string(&result)?)
  }

  /// Handle send_message tool.
  fn handle_send_message(
    &self,
    args: Option<serde_json::Value>,
  ) -> Result<String, McpError> {
    let args =
      args.ok_or_else(|| McpError::InvalidParams("Missing arguments".to_string()))?;
    let pid = args
      .get("pid")
      .and_then(|v| v.as_str())
      .and_then(|s| s.parse::<u64>().ok())
      .ok_or_else(|| {
        McpError::InvalidParams("Missing or invalid 'pid' parameter".to_string())
      })?;
    let message_data = args.get("message").cloned().ok_or_else(|| {
      McpError::InvalidParams("Missing 'message' parameter".to_string())
    })?;

    let message = Message::Json(message_data);
    let result = match self.state.runtime_handle.send_message(pid, message) {
      Ok(_) => SendMessageResult { success: true },
      Err(e) => {
        return Ok(format!("{{\"success\": false, \"error\": \"{}\"}}", e));
      }
    };

    Ok(serde_json::to_string(&result)?)
  }

  /// Handle get_metrics tool.
  fn handle_get_metrics(&self) -> Result<String, McpError> {
    let snapshot = self.state.runtime_handle.metrics_snapshot();
    let result = GetMetricsResult {
      uptime_secs: snapshot.uptime_secs,
      active_processes: snapshot.active_processes as usize,
      messages_sent_total: snapshot.messages_sent_total,
      messages_processed_total: snapshot.messages_processed_total,
    };
    Ok(serde_json::to_string(&result)?)
  }

  /// Handle cancel_process tool.
  fn handle_cancel_process(
    &self,
    args: Option<serde_json::Value>,
  ) -> Result<String, McpError> {
    let args =
      args.ok_or_else(|| McpError::InvalidParams("Missing arguments".to_string()))?;
    let pid = args
      .get("pid")
      .and_then(|v| v.as_str())
      .and_then(|s| s.parse::<u64>().ok())
      .ok_or_else(|| {
        McpError::InvalidParams("Missing or invalid 'pid' parameter".to_string())
      })?;
    let reason_str = args
      .get("reason")
      .and_then(|v| v.as_str())
      .unwrap_or("cancelled");

    let reason = match reason_str {
      "normal" => Reason::Normal,
      "shutdown" => Reason::Shutdown,
      _ => Reason::Custom(reason_str.to_string()),
    };

    self.state.runtime_handle.terminate_process(pid, reason);

    let result = CancelProcessResult { success: true };
    Ok(serde_json::to_string(&result)?)
  }
}

/// MCP error type for internal handling.
#[derive(Debug)]
enum McpError {
  MethodNotFound(String),
  InvalidParams(String),
  ToolNotFound(String),
  Serialization(serde_json::Error),
  Runtime(String),
}

impl From<serde_json::Error> for McpError {
  fn from(e: serde_json::Error) -> Self {
    McpError::Serialization(e)
  }
}

impl McpError {
  /// Convert to a JSON-RPC response.
  fn to_response(&self, id: Option<serde_json::Value>) -> JsonRpcResponse {
    match self {
      McpError::MethodNotFound(m) => {
        JsonRpcResponse::error(id, error_codes::METHOD_NOT_FOUND, m.clone())
      }
      McpError::InvalidParams(m) => {
        JsonRpcResponse::error(id, error_codes::INVALID_PARAMS, m.clone())
      }
      McpError::ToolNotFound(m) => JsonRpcResponse::error(
        id,
        error_codes::TOOL_NOT_FOUND,
        format!("Tool not found: {}", m),
      ),
      McpError::Serialization(e) => JsonRpcResponse::error(
        id,
        error_codes::INTERNAL_ERROR,
        format!("Serialization error: {}", e),
      ),
      McpError::Runtime(m) => {
        JsonRpcResponse::error(id, error_codes::INTERNAL_ERROR, m.clone())
      }
    }
  }
}

/// Simple UUID generation (placeholder implementation).
/// In production, use a proper UUID crate.
mod uuid {
  use std::sync::atomic::{AtomicU64, Ordering};

  static COUNTER: AtomicU64 = AtomicU64::new(1);

  pub fn generate_uuid() -> String {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
      "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
      id >> 32,
      (id >> 16) & 0xFFFF,
      id & 0xFFFF,
      0x8000 | (id & 0x3FFF),
      id
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::scheduler::AgentScheduler;

  fn create_test_state() -> Arc<McpServerState> {
    let scheduler = Arc::new(Mutex::new(AgentScheduler::new()));
    let metrics = Arc::new(RuntimeMetrics::new());
    let runtime_handle = RuntimeHandle::new(scheduler, metrics);

    Arc::new(McpServerState::new(
      None, // no auth
      Duration::from_secs(3600),
      runtime_handle,
    ))
  }

  #[test]
  fn test_create_session() {
    let state = create_test_state();
    let session_id = state.create_session();
    assert!(!session_id.is_empty());

    let session = state.get_session(&session_id);
    assert!(session.is_some());
  }

  #[test]
  fn test_session_expiration() {
    let scheduler = Arc::new(Mutex::new(AgentScheduler::new()));
    let metrics = Arc::new(RuntimeMetrics::new());
    let runtime_handle = RuntimeHandle::new(scheduler, metrics);

    let state = Arc::new(McpServerState::new(
      None,
      Duration::from_millis(10), // very short timeout
      runtime_handle,
    ));

    let session_id = state.create_session();
    std::thread::sleep(Duration::from_millis(50));

    let session = state.get_session(&session_id);
    assert!(session.is_none()); // Should be expired and removed
  }

  #[test]
  fn test_verify_auth_no_token_required() {
    let state = create_test_state();
    assert!(state.verify_auth(None));
    assert!(state.verify_auth(Some("any-token")));
  }

  #[test]
  fn test_verify_auth_with_token() {
    let scheduler = Arc::new(Mutex::new(AgentScheduler::new()));
    let metrics = Arc::new(RuntimeMetrics::new());
    let runtime_handle = RuntimeHandle::new(scheduler, metrics);

    let state = Arc::new(McpServerState::new(
      Some("secret-token".to_string()),
      Duration::from_secs(3600),
      runtime_handle,
    ));

    assert!(!state.verify_auth(None));
    assert!(!state.verify_auth(Some("wrong-token")));
    assert!(state.verify_auth(Some("secret-token")));
  }

  #[test]
  fn test_handle_initialize() {
    let state = create_test_state();
    let handler = McpProtocolHandler::new(state);

    let request = JsonRpcRequest::new(Some(serde_json::json!(1)), "initialize")
      .with_params(InitializeParams {
        protocol_version: SUPPORTED_PROTOCOL_VERSION.to_string(),
        capabilities: None,
        client_info: Some(Implementation {
          name: "test".to_string(),
          version: env!("CARGO_PKG_VERSION").to_string(),
        }),
      })
      .unwrap();

    let response = handler.handle_request(request, None);
    assert!(response.result.is_some());
    assert!(response.error.is_none());
  }

  #[test]
  fn test_handle_initialize_wrong_version() {
    let state = create_test_state();
    let handler = McpProtocolHandler::new(state);

    let request = JsonRpcRequest::new(Some(serde_json::json!(1)), "initialize")
      .with_params(InitializeParams {
        protocol_version: "1.0.0".to_string(),
        capabilities: None,
        client_info: None,
      })
      .unwrap();

    let response = handler.handle_request(request, None);
    assert!(response.result.is_none());
    assert!(response.error.is_some());
  }

  #[test]
  fn test_handle_tools_list_requires_session() {
    let state = create_test_state();
    let handler = McpProtocolHandler::new(state);

    let request = JsonRpcRequest::new(Some(serde_json::json!(1)), "tools/list")
      .with_params(ToolsListParams::default())
      .unwrap();

    let response = handler.handle_request(request, None);
    assert!(response.result.is_none());
    assert!(response.error.is_some());
    let error = response.error.unwrap();
    assert_eq!(error.code, error_codes::SESSION_NOT_INITIALIZED);
  }

  #[test]
  fn test_handle_tools_list_with_session() {
    let state = create_test_state();
    let handler = McpProtocolHandler::new(state.clone());

    // Create and initialize a session
    let session_id = state.create_session();
    let mut session = McpSession::new(&session_id);
    session.mark_initialized();
    state.update_session(session);

    let request = JsonRpcRequest::new(Some(serde_json::json!(1)), "tools/list")
      .with_params(ToolsListParams::default())
      .unwrap();

    let response = handler.handle_request(request, Some(&session_id));
    assert!(response.result.is_some());
    assert!(response.error.is_none());
  }

  #[test]
  fn test_handle_ping() {
    let state = create_test_state();
    let handler = McpProtocolHandler::new(state.clone());

    // Create and initialize a session
    let session_id = state.create_session();
    let mut session = McpSession::new(&session_id);
    session.mark_initialized();
    state.update_session(session);

    let request = JsonRpcRequest::new(Some(serde_json::json!(1)), "ping");

    let response = handler.handle_request(request, Some(&session_id));
    assert!(response.result.is_some());
    assert!(response.error.is_none());
  }

  #[test]
  fn test_handle_unknown_method() {
    let state = create_test_state();
    let handler = McpProtocolHandler::new(state.clone());

    // Create and initialize a session
    let session_id = state.create_session();
    let mut session = McpSession::new(&session_id);
    session.mark_initialized();
    state.update_session(session);

    let request = JsonRpcRequest::new(Some(serde_json::json!(1)), "unknown/method");

    let response = handler.handle_request(request, Some(&session_id));
    assert!(response.result.is_none());
    assert!(response.error.is_some());
    let error = response.error.unwrap();
    assert_eq!(error.code, error_codes::METHOD_NOT_FOUND);
  }

  #[test]
  fn test_get_available_tools() {
    let state = create_test_state();
    let tools = state.get_available_tools();
    assert_eq!(tools.len(), 6);

    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"spawn_orchestration"));
    assert!(tool_names.contains(&"list_processes"));
    assert!(tool_names.contains(&"get_metrics"));
  }
}
