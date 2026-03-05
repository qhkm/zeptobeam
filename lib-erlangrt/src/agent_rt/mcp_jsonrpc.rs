//! JSON-RPC 2.0 types for MCP protocol.
//!
//! This module defines the core JSON-RPC message structures
//! used by the Model Context Protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC version constant.
pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
  pub jsonrpc: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<Value>,
  pub method: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub params: Option<Value>,
}

impl JsonRpcRequest {
  /// Create a new JSON-RPC request.
  pub fn new(id: Option<Value>, method: impl Into<String>) -> Self {
    Self {
      jsonrpc: JSONRPC_VERSION.to_string(),
      id,
      method: method.into(),
      params: None,
    }
  }

  /// Add params to the request.
  pub fn with_params<T: Serialize>(
    mut self,
    params: T,
  ) -> Result<Self, serde_json::Error> {
    self.params = Some(serde_json::to_value(params)?);
    Ok(self)
  }

  /// Check if this is a notification (no id).
  pub fn is_notification(&self) -> bool {
    self.id.is_none()
  }
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
  pub jsonrpc: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub result: Option<Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
  /// Create a successful response.
  pub fn success<T: Serialize>(
    id: Option<Value>,
    result: T,
  ) -> Result<Self, serde_json::Error> {
    Ok(Self {
      jsonrpc: JSONRPC_VERSION.to_string(),
      id,
      result: Some(serde_json::to_value(result)?),
      error: None,
    })
  }

  /// Create an error response.
  pub fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
    Self {
      jsonrpc: JSONRPC_VERSION.to_string(),
      id,
      result: None,
      error: Some(JsonRpcError {
        code,
        message: message.into(),
        data: None,
      }),
    }
  }

  /// Create an error response from a JsonRpcError.
  pub fn from_error(id: Option<Value>, error: JsonRpcError) -> Self {
    Self {
      jsonrpc: JSONRPC_VERSION.to_string(),
      id,
      result: None,
      error: Some(error),
    }
  }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
  pub code: i64,
  pub message: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
}

impl JsonRpcError {
  /// Create a new JSON-RPC error.
  pub fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
    Self {
      code,
      message: message.into(),
      data,
    }
  }

  /// Standard error: Parse error (-32700).
  pub fn parse_error(message: impl Into<String>) -> Self {
    Self::new(-32700, message, None)
  }

  /// Standard error: Invalid Request (-32600).
  pub fn invalid_request(message: impl Into<String>) -> Self {
    Self::new(-32600, message, None)
  }

  /// Standard error: Method not found (-32601).
  pub fn method_not_found(method: &str) -> Self {
    Self::new(-32601, format!("Method not found: {}", method), None)
  }

  /// Standard error: Invalid params (-32602).
  pub fn invalid_params(message: impl Into<String>) -> Self {
    Self::new(-32602, message, None)
  }

  /// Standard error: Internal error (-32603).
  pub fn internal_error(message: impl Into<String>) -> Self {
    Self::new(-32603, message, None)
  }

  /// Server error: Session not found.
  pub fn session_not_found() -> Self {
    Self::new(-32001, "Session not found", None)
  }

  /// Server error: Not initialized.
  pub fn not_initialized() -> Self {
    Self::new(-32002, "Server not initialized", None)
  }

  /// Server error: Already initialized.
  pub fn already_initialized() -> Self {
    Self::new(-32003, "Server already initialized", None)
  }

  /// Server error: Tool not found.
  pub fn tool_not_found(name: &str) -> Self {
    Self::new(-32004, format!("Tool not found: {}", name), None)
  }
}

/// A JSON-RPC message (either request or response).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
  Request(JsonRpcRequest),
  Response(JsonRpcResponse),
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_request_serialization() {
    let request = JsonRpcRequest::new(Some(Value::String("1".to_string())), "initialize")
      .with_params(serde_json::json!({ "protocolVersion": "2024-11-05" }))
      .unwrap();
    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains("\"jsonrpc\":\"2.0\""));
    assert!(json.contains("\"id\":\"1\""));
    assert!(json.contains("\"method\":\"initialize\""));
  }

  #[test]
  fn test_request_deserialization() {
    let json = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":null}"#;
    let request: JsonRpcRequest = serde_json::from_str(json).unwrap();
    assert_eq!(request.jsonrpc, "2.0");
    assert_eq!(request.id, Some(Value::Number(1.into())));
    assert_eq!(request.method, "ping");
  }

  #[test]
  fn test_response_serialization() {
    let response = JsonRpcResponse::success(
      Some(Value::String("1".to_string())),
      serde_json::json!({ "protocolVersion": "2024-11-05" }),
    )
    .unwrap();
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"jsonrpc\":\"2.0\""));
    assert!(json.contains("\"id\":\"1\""));
    assert!(json.contains("\"result\""));
  }

  #[test]
  fn test_error_response() {
    let response = JsonRpcResponse::error(
      Some(Value::String("1".to_string())),
      -32601,
      "Method not found",
    );
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"code\":-32601"));
    assert!(json.contains("Method not found"));
  }

  #[test]
  fn test_notification_detection() {
    let notification = JsonRpcRequest::new(None, "ping");
    assert!(notification.is_notification());

    let request = JsonRpcRequest::new(Some(Value::Number(1.into())), "ping");
    assert!(!request.is_notification());
  }

  #[test]
  fn test_standard_errors() {
    assert_eq!(JsonRpcError::parse_error("test").code, -32700);
    assert_eq!(JsonRpcError::invalid_request("test").code, -32600);
    assert_eq!(JsonRpcError::method_not_found("test").code, -32601);
    assert_eq!(JsonRpcError::invalid_params("test").code, -32602);
    assert_eq!(JsonRpcError::internal_error("test").code, -32603);
    assert_eq!(JsonRpcError::session_not_found().code, -32001);
    assert_eq!(JsonRpcError::not_initialized().code, -32002);
    assert_eq!(JsonRpcError::already_initialized().code, -32003);
    assert_eq!(JsonRpcError::tool_not_found("test").code, -32004);
  }
}
