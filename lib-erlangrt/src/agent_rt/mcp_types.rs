//! MCP (Model Context Protocol) types.
//!
//! This module defines the protocol-specific types for MCP
//! initialization, tools listing, and tool calls.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Re-export JSON-RPC types from mcp_jsonrpc for convenience
pub use crate::agent_rt::mcp_jsonrpc::{
  JsonRpcError, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
};

/// MCP protocol version.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Error codes for MCP-specific errors.
pub mod error_codes {
  /// Session not found or expired.
  pub const SESSION_NOT_INITIALIZED: i64 = -32001;
  /// Session expired.
  pub const SESSION_EXPIRED: i64 = -32002;
  /// Tool not found.
  pub const TOOL_NOT_FOUND: i64 = -32004;
  /// Parse error (from JSON-RPC).
  pub const PARSE_ERROR: i64 = -32700;
  /// Invalid request.
  pub const INVALID_REQUEST: i64 = -32600;
  /// Method not found.
  pub const METHOD_NOT_FOUND: i64 = -32601;
  /// Invalid params.
  pub const INVALID_PARAMS: i64 = -32602;
  /// Internal error.
  pub const INTERNAL_ERROR: i64 = -32603;
}

/// Server capabilities for MCP initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
  /// Tool support capabilities.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tools: Option<ToolsCapability>,
}

impl ServerCapabilities {
  /// Create default server capabilities with tools support.
  pub fn with_tools() -> Self {
    Self {
      tools: Some(ToolsCapability {
        list_changed: Some(false),
      }),
    }
  }
}

impl Default for ServerCapabilities {
  fn default() -> Self {
    Self::with_tools()
  }
}

/// Tools capability definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCapability {
  /// Whether the server supports list changed notifications.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub list_changed: Option<bool>,
}

/// Implementation information for MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
  pub name: String,
  pub version: String,
}

impl Implementation {
  /// Create implementation info for zeptoclaw.
  pub fn zeptoclaw() -> Self {
    Self {
      name: "zeptoclaw".to_string(),
      version: env!("CARGO_PKG_VERSION").to_string(),
    }
  }
}

/// Initialize request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
  pub protocol_version: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub capabilities: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub client_info: Option<Implementation>,
}

/// Initialize response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
  pub protocol_version: String,
  pub capabilities: ServerCapabilities,
  pub server_info: Implementation,
}

impl InitializeResult {
  /// Create a default initialize result.
  pub fn default_result() -> Self {
    Self {
      protocol_version: MCP_PROTOCOL_VERSION.to_string(),
      capabilities: ServerCapabilities::default(),
      server_info: Implementation::zeptoclaw(),
    }
  }
}

/// Tool definition for tools/list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub input_schema: Option<Value>,
}

impl Tool {
  /// Create a new tool definition.
  pub fn new(
    name: impl Into<String>,
    description: impl Into<String>,
    schema: Value,
  ) -> Self {
    Self {
      name: name.into(),
      description: Some(description.into()),
      input_schema: Some(schema),
    }
  }
}

/// Extended tool info for internal use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub input_schema: Option<Value>,
}

impl ToolInfo {
  /// Create a new tool info with just name and schema.
  pub fn new(name: impl Into<String>, schema: Value) -> Result<Self, &'static str> {
    Ok(Self {
      name: name.into(),
      description: None,
      input_schema: Some(schema),
    })
  }

  /// Add a description to the tool info.
  pub fn with_description(mut self, description: impl Into<String>) -> Self {
    self.description = Some(description.into());
    self
  }
}

/// Tools/list request parameters.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsListParams {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cursor: Option<String>,
}

/// Tools/list response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
  pub tools: Vec<ToolInfo>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub next_cursor: Option<String>,
}

/// Tool call request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCallParams {
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub arguments: Option<Value>,
}

/// Tool content for tool call responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolContent {
  #[serde(rename = "text")]
  Text { text: String },
}

impl ToolContent {
  /// Create a text content item.
  pub fn text(text: impl Into<String>) -> Self {
    Self::Text { text: text.into() }
  }
}

/// Tool call response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCallResult {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub content: Option<Vec<ToolContent>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub is_error: Option<bool>,
}

/// Tool output content item (alias for compatibility).
pub type ToolOutputContent = ToolContent;

/// Embedded resource reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedResource {
  pub uri: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub mime_type: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub text: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub blob: Option<String>,
}

/// Ping request parameters (empty).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingParams {}

/// Ping response result (empty).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingResult {}

/// Process information for list_processes tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
  pub pid: u64,
  pub status: String,
  pub priority: String,
  pub mailbox_depth: usize,
}

/// Result for spawn_orchestration tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnOrchestrationResult {
  pub orchestration_id: String,
  pub status: String,
}

/// Result for get_orchestration_status tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestrationStatusResult {
  pub orchestration_id: String,
  pub status: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub results: Option<Vec<Value>>,
}

/// Result for list_processes tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListProcessesResult {
  pub processes: Vec<ProcessInfo>,
}

/// Result for send_message tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResult {
  pub success: bool,
}

/// Result for get_metrics tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetMetricsResult {
  pub uptime_secs: u64,
  pub active_processes: usize,
  pub messages_sent_total: u64,
  pub messages_processed_total: u64,
}

/// Result for cancel_process tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelProcessResult {
  pub success: bool,
}

/// Server info for initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
  pub name: String,
  pub version: String,
}

impl Default for ServerInfo {
  fn default() -> Self {
    Self {
      name: "zeptoclaw".to_string(),
      version: env!("CARGO_PKG_VERSION").to_string(),
    }
  }
}

/// Input schema for spawn_orchestration tool.
pub fn spawn_orchestration_schema() -> Value {
  serde_json::json!({
      "type": "object",
      "properties": {
          "goal": {
              "type": "string",
              "description": "The goal or task description for the orchestration"
          },
          "max_concurrency": {
              "type": "integer",
              "description": "Maximum concurrent workers",
              "minimum": 1,
              "default": 3
          }
      },
      "required": ["goal"]
  })
}

/// Input schema for get_orchestration_status tool.
pub fn get_orchestration_status_schema() -> Value {
  serde_json::json!({
      "type": "object",
      "properties": {
          "orchestration_id": {
              "type": "string",
              "description": "The orchestration ID (PID) to check"
          }
      },
      "required": ["orchestration_id"]
  })
}

/// Input schema for list_processes tool.
pub fn list_processes_schema() -> Value {
  serde_json::json!({
      "type": "object",
      "properties": {}
  })
}

/// Input schema for send_message tool.
pub fn send_message_schema() -> Value {
  serde_json::json!({
      "type": "object",
      "properties": {
          "pid": {
              "type": "string",
              "description": "Target process ID"
          },
          "message": {
              "type": "string",
              "description": "Message content to send"
          }
      },
      "required": ["pid", "message"]
  })
}

/// Input schema for get_metrics tool.
pub fn get_metrics_schema() -> Value {
  serde_json::json!({
      "type": "object",
      "properties": {}
  })
}

/// Input schema for cancel_process tool.
pub fn cancel_process_schema() -> Value {
  serde_json::json!({
      "type": "object",
      "properties": {
          "pid": {
              "type": "string",
              "description": "Process ID to cancel"
          },
          "reason": {
              "type": "string",
              "description": "Reason for cancellation",
              "default": "cancelled via MCP"
          }
      },
      "required": ["pid"]
  })
}

/// Build the list of available MCP tools.
pub fn build_tools_list() -> Vec<Tool> {
  vec![
    Tool::new(
      "spawn_orchestration",
      "Start a new orchestration with a goal",
      spawn_orchestration_schema(),
    ),
    Tool::new(
      "get_orchestration_status",
      "Check status/results of an orchestration by ID",
      get_orchestration_status_schema(),
    ),
    Tool::new(
      "list_processes",
      "List active agent processes",
      list_processes_schema(),
    ),
    Tool::new(
      "send_message",
      "Send a message to a process by PID",
      send_message_schema(),
    ),
    Tool::new(
      "get_metrics",
      "Get runtime metrics snapshot",
      get_metrics_schema(),
    ),
    Tool::new(
      "cancel_process",
      "Terminate a process by PID",
      cancel_process_schema(),
    ),
  ]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_initialize_result() {
    let result = InitializeResult::default_result();
    assert_eq!(result.protocol_version, MCP_PROTOCOL_VERSION);
    assert_eq!(result.server_info.name, "zeptoclaw");
  }

  #[test]
  fn test_tool_creation() {
    let tool = Tool::new("test_tool", "A test tool", serde_json::json!({}));
    assert_eq!(tool.name, "test_tool");
    assert_eq!(tool.description, Some("A test tool".to_string()));
  }

  #[test]
  fn test_tools_list() {
    let tools = build_tools_list();
    assert_eq!(tools.len(), 6);

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"spawn_orchestration"));
    assert!(names.contains(&"get_orchestration_status"));
    assert!(names.contains(&"list_processes"));
    assert!(names.contains(&"send_message"));
    assert!(names.contains(&"get_metrics"));
    assert!(names.contains(&"cancel_process"));
  }

  #[test]
  fn test_tool_output_content() {
    let content = ToolContent::text("Hello, world!");
    match content {
      ToolContent::Text { text } => assert_eq!(text, "Hello, world!"),
      _ => panic!("Expected Text content"),
    }
  }

  #[test]
  fn test_initialize_params_deserialization() {
    let json = r#"{
            "protocol_version": "2024-11-05",
            "capabilities": {},
            "client_info": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }"#;
    let params: InitializeParams = serde_json::from_str(json).unwrap();
    assert_eq!(params.protocol_version, "2024-11-05");
    assert!(params.client_info.is_some());
  }

  #[test]
  fn test_tools_call_params_deserialization() {
    let json = r#"{
            "name": "test_tool",
            "arguments": {"key": "value"}
        }"#;
    let params: ToolsCallParams = serde_json::from_str(json).unwrap();
    assert_eq!(params.name, "test_tool");
    assert!(params.arguments.is_some());
  }

  #[test]
  fn test_error_codes() {
    assert_eq!(error_codes::SESSION_NOT_INITIALIZED, -32001);
    assert_eq!(error_codes::PARSE_ERROR, -32700);
    assert_eq!(error_codes::METHOD_NOT_FOUND, -32601);
  }
}
