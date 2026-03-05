//! MCP stdio transport implementation.
//!
//! This module provides a stdio-based transport for MCP communication,
//! reading JSON-RPC requests from stdin and writing responses to stdout.

use crate::agent_rt::{
    mcp_server::{McpProtocolHandler, McpServerState},
    mcp_types::{error_codes, JsonRpcRequest, JsonRpcResponse},
};
use std::{
    io::{self, BufRead, Write},
    sync::Arc,
};

/// MCP stdio listener that handles JSON-RPC communication over stdin/stdout.
pub struct McpStdioListener {
    handler: Arc<McpProtocolHandler>,
    state: Arc<McpServerState>,
}

impl McpStdioListener {
    /// Create a new stdio listener.
    pub fn new(handler: Arc<McpProtocolHandler>, state: Arc<McpServerState>) -> Self {
        Self { handler, state }
    }

    /// Run the stdio listener synchronously (blocking).
    /// This reads lines from stdin and writes responses to stdout.
    pub fn run_blocking(&self) -> io::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdout_lock = stdout.lock();
        let mut session_id: Option<String> = None;

        for line in stdin.lock().lines() {
            let line = line?;
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            // Parse the request
            let request: JsonRpcRequest = match serde_json::from_str(line) {
                Ok(req) => req,
                Err(e) => {
                    let response = JsonRpcResponse::error(
                        None,
                        error_codes::PARSE_ERROR,
                        format!("Parse error: {}", e),
                    );
                    self.write_response_sync(&mut stdout_lock, &response)?;
                    continue;
                }
            };

            // Track session ID from initialize responses
            let is_initialize = request.method == "initialize";
            
            // Handle the request
            let response = self.handler.handle_request(request, session_id.as_deref());

            // If this was a successful initialize, capture the session ID
            if is_initialize && response.error.is_none() {
                // Generate a new session for stdio after successful initialize
                let new_session_id = self.state.create_session();
                if let Some(ref mut sess) = self.state.get_session(&new_session_id) {
                    let mut updated = sess.clone();
                    updated.mark_initialized();
                    self.state.update_session(updated);
                }
                session_id = Some(new_session_id);
            }

            self.write_response_sync(&mut stdout_lock, &response)?;
        }

        Ok(())
    }

    /// Write a response to stdout synchronously.
    fn write_response_sync(
        &self,
        stdout: &mut io::StdoutLock,
        response: &JsonRpcResponse,
    ) -> io::Result<()> {
        let json = match serde_json::to_string(response) {
            Ok(s) => s,
            Err(e) => {
                // Fallback error response
                format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":{},"message":"Serialization error: {}"}}}}"#,
                    error_codes::INTERNAL_ERROR,
                    e
                )
            }
        };

        stdout.write_all(json.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::{
        mcp_server::{RuntimeHandle, McpServerState},
        observability::RuntimeMetrics,
        scheduler::AgentScheduler,
    };
    use std::time::Duration;

    fn create_test_state() -> Arc<McpServerState> {
        let scheduler = Arc::new(std::sync::Mutex::new(AgentScheduler::new()));
        let metrics = Arc::new(RuntimeMetrics::new());
        let runtime_handle = RuntimeHandle::new(scheduler, metrics);

        Arc::new(McpServerState::new(
            None,
            Duration::from_secs(3600),
            runtime_handle,
        ))
    }

    #[test]
    fn test_stdio_listener_creation() {
        let state = create_test_state();
        let handler = Arc::new(McpProtocolHandler::new(state.clone()));
        let listener = McpStdioListener::new(handler, state);
        // Just verify it can be created
        drop(listener);
    }

    #[test]
    fn test_jsonrpc_request_parsing() {
        // Create a simple request
        let request = JsonRpcRequest::new(Some(serde_json::json!(1)), "initialize")
            .with_params(crate::agent_rt::mcp_types::InitializeParams {
                protocol_version: crate::agent_rt::mcp_server::SUPPORTED_PROTOCOL_VERSION.to_string(),
                capabilities: None,
                client_info: None,
            })
            .unwrap();

        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.is_empty());

        // Parse it back
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "initialize");
    }

    #[test]
    fn test_jsonrpc_response_serialization() {
        let response = JsonRpcResponse::success(
            Some(serde_json::json!(1)),
            serde_json::json!({"result": "ok"}),
        ).unwrap();

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"result\""));
    }

    #[test]
    fn test_error_response_formatting() {
        let response = JsonRpcResponse::error(
            Some(serde_json::json!(1)),
            error_codes::PARSE_ERROR,
            "Test error",
        );

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"code\":-32700"));
        assert!(json.contains("Test error"));
    }
}
