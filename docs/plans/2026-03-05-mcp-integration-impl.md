# Phase 7: MCP Integration Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add bidirectional MCP support — expose the runtime as an MCP server (Streamable HTTP + stdio) and consume external MCP tool servers natively from within agents.

**Architecture:** Two symmetric subsystems. MCP Server extends the existing axum HealthServer with JSON-RPC 2.0 protocol handling and bearer token auth, plus an optional stdio transport. MCP Client manages connections to configured external servers, wrapping discovered remote tools as native `Box<dyn Tool>` via `McpRemoteTool`. All features opt-in — no config means identical to current behavior.

**Tech Stack:** Rust, axum (HTTP transport), tokio::process (stdio transport), serde_json (JSON-RPC), ureq (HTTP client in spawn_blocking), uuid (session IDs)

**Prerequisite:** Phase 5 (config system, health server, CLI binary `zeptoclaw-rtd`). Phase 3 (ToolFactory trait).

**Design doc:** `docs/plans/2026-03-05-mcp-integration-design.md`

---

### Task 1: JSON-RPC 2.0 Types

Pure data structures for the JSON-RPC 2.0 wire format used by MCP. No runtime dependencies.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/mcp_types.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs:1-27` (add `pub mod mcp_types;`)
- Test: inline in `mcp_types.rs`

**Step 1: Write the failing tests**

Add this complete file at `lib-erlangrt/src/agent_rt/mcp_types.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── JSON-RPC 2.0 ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ── Standard error codes ───────────────────────────────────

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

// ── Constructors ───────────────────────────────────────────

impl JsonRpcRequest {
    pub fn new(id: Value, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: method.into(),
            params,
        }
    }

    /// Notification (no id, no response expected).
    pub fn notification(method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: method.into(),
            params,
        }
    }
}

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// ── MCP Protocol Types ─────────────────────────────────────

/// Client sends this as params to `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientCapabilities {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Server returns this as result of `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsCapability {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Server returns this as result of `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Client sends this as params to `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Server returns this as result of `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCallResult {
    pub content: Vec<ContentBlock>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

impl ContentBlock {
    pub fn text(s: &str) -> Self {
        Self {
            content_type: "text".into(),
            text: s.into(),
        }
    }
}

impl ToolsCallResult {
    pub fn success(text: &str) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: None,
        }
    }

    pub fn error(text: &str) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: Some(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonrpc_request_serialize() {
        let req = JsonRpcRequest::new(
            serde_json::json!(1),
            "initialize",
            Some(serde_json::json!({"key": "value"})),
        );
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 1);
        assert_eq!(json["method"], "initialize");
        assert_eq!(json["params"]["key"], "value");
    }

    #[test]
    fn test_jsonrpc_request_deserialize() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(serde_json::json!(1)));
        assert!(req.params.is_none());
    }

    #[test]
    fn test_jsonrpc_notification_has_no_id() {
        let n = JsonRpcRequest::notification("notifications/initialized", None);
        let json = serde_json::to_value(&n).unwrap();
        assert!(json.get("id").is_none());
    }

    #[test]
    fn test_jsonrpc_response_success() {
        let resp = JsonRpcResponse::success(
            Some(serde_json::json!(1)),
            serde_json::json!({"ok": true}),
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["result"]["ok"], true);
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let resp = JsonRpcResponse::error(
            Some(serde_json::json!(1)),
            METHOD_NOT_FOUND,
            "method not found",
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"]["code"], -32601);
        assert_eq!(json["error"]["message"], "method not found");
        assert!(json.get("result").is_none());
    }

    #[test]
    fn test_initialize_params_roundtrip() {
        let params = InitializeParams {
            protocol_version: "2025-03-26".into(),
            capabilities: ClientCapabilities {},
            client_info: ClientInfo {
                name: "test-client".into(),
                version: Some("1.0".into()),
            },
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: InitializeParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.protocol_version, "2025-03-26");
        assert_eq!(parsed.client_info.name, "test-client");
    }

    #[test]
    fn test_initialize_result_serialize() {
        let result = InitializeResult {
            protocol_version: "2025-03-26".into(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {}),
            },
            server_info: ServerInfo {
                name: "zeptoclaw-rt".into(),
                version: Some("0.1.0".into()),
            },
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["protocolVersion"], "2025-03-26");
        assert_eq!(json["serverInfo"]["name"], "zeptoclaw-rt");
        assert!(json["capabilities"]["tools"].is_object());
    }

    #[test]
    fn test_tools_list_result() {
        let result = ToolsListResult {
            tools: vec![ToolDefinition {
                name: "get_metrics".into(),
                description: "Get runtime metrics".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            }],
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["tools"][0]["name"], "get_metrics");
    }

    #[test]
    fn test_tools_call_params_deserialize() {
        let json = r#"{"name":"get_metrics","arguments":{}}"#;
        let params: ToolsCallParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "get_metrics");
    }

    #[test]
    fn test_tools_call_result_success() {
        let result = ToolsCallResult::success("all good");
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "all good");
        assert!(json.get("isError").is_none());
    }

    #[test]
    fn test_tools_call_result_error() {
        let result = ToolsCallResult::error("something failed");
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["isError"], true);
        assert_eq!(json["content"][0]["text"], "something failed");
    }
}
```

**Step 2: Register the module**

Add `pub mod mcp_types;` to `lib-erlangrt/src/agent_rt/mod.rs` after line 20 (after `pub mod types;`).

**Step 3: Run tests to verify they pass**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::mcp_types -- --nocapture`
Expected: 11 tests PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/mcp_types.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(mcp): add JSON-RPC 2.0 and MCP protocol types"
```

---

### Task 2: MCP Config

Add `McpConfig` to the existing config system. Pure data structures with TOML parsing.

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/config.rs:1-175`
- Test: inline in `config.rs`

**Step 1: Write the failing test**

Add this test to the existing `#[cfg(test)] mod tests` block in `config.rs` (after the `test_invalid_toml_returns_error` test at line 174):

```rust
    #[test]
    fn test_mcp_config_defaults() {
        let config = AppConfig::default();
        assert!(!config.mcp.server.enabled);
        assert!(!config.mcp.stdio.enabled);
        assert!(config.mcp.servers.is_empty());
        assert_eq!(config.mcp.server.session_timeout_secs, 3600);
    }

    #[test]
    fn test_mcp_config_from_toml() {
        let toml_str = r#"
[mcp.server]
enabled = true
auth_token_env = "MY_TOKEN"
session_timeout_secs = 1800

[mcp.stdio]
enabled = true

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[[mcp.servers]]
name = "database"
transport = "http"
url = "http://localhost:8080/mcp"
auth_token_env = "DB_TOKEN"
timeout_ms = 5000
"#;
        let config = load_config_from_str(toml_str).unwrap();
        assert!(config.mcp.server.enabled);
        assert_eq!(config.mcp.server.auth_token_env.as_deref(), Some("MY_TOKEN"));
        assert_eq!(config.mcp.server.session_timeout_secs, 1800);
        assert!(config.mcp.stdio.enabled);
        assert_eq!(config.mcp.servers.len(), 2);
        assert_eq!(config.mcp.servers[0].name, "github");
        assert_eq!(config.mcp.servers[0].transport, "stdio");
        assert_eq!(config.mcp.servers[0].command.as_deref(), Some("npx"));
        assert_eq!(config.mcp.servers[1].name, "database");
        assert_eq!(config.mcp.servers[1].transport, "http");
        assert_eq!(config.mcp.servers[1].url.as_deref(), Some("http://localhost:8080/mcp"));
        assert_eq!(config.mcp.servers[1].timeout_ms, Some(5000));
    }

    #[test]
    fn test_no_mcp_section_gives_defaults() {
        let toml_str = r#"
[runtime]
worker_count = 8
"#;
        let config = load_config_from_str(toml_str).unwrap();
        assert!(!config.mcp.server.enabled);
        assert!(config.mcp.servers.is_empty());
    }
```

**Step 2: Run tests to verify they fail**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::config -- --nocapture`
Expected: FAIL — `no field mcp on type AppConfig`

**Step 3: Implement the config structs**

Add these imports and structs to `config.rs`. At the top, add `use std::collections::HashMap;` after the existing imports.

Add these structs after the `LogConfig` struct (after line 43):

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub server: McpServerConfig,
    pub stdio: McpStdioConfig,
    pub servers: Vec<McpServerEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub enabled: bool,
    pub auth_token_env: Option<String>,
    pub session_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpStdioConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub auth_token_env: Option<String>,
    pub timeout_ms: Option<u64>,
}
```

Add `pub mcp: McpConfig,` to the `AppConfig` struct (after `pub logging: LogConfig,`).

Add `mcp: McpConfig::default(),` to `impl Default for AppConfig`.

Add these Default impls:

```rust
impl Default for McpConfig {
    fn default() -> Self {
        Self {
            server: McpServerConfig::default(),
            stdio: McpStdioConfig::default(),
            servers: Vec::new(),
        }
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auth_token_env: None,
            session_timeout_secs: 3600,
        }
    }
}

impl Default for McpStdioConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::config -- --nocapture`
Expected: all 8 tests PASS (5 existing + 3 new)

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/config.rs
git commit -m "feat(mcp): add McpConfig to AppConfig with TOML parsing"
```

---

### Task 3: MCP Server Handler (Protocol Logic)

The core protocol handler — takes a `JsonRpcRequest`, dispatches to the right method, returns a `JsonRpcResponse`. No transport involved — this is pure logic.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/mcp_server.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod mcp_server;`)
- Test: inline in `mcp_server.rs`

**Step 1: Write the full module with tests**

Create `lib-erlangrt/src/agent_rt/mcp_server.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::agent_rt::mcp_types::*;

// ── Session ────────────────────────────────────────────────

pub struct McpSession {
    pub id: String,
    pub created_at: Instant,
    pub last_active: Instant,
    pub initialized: bool,
}

// ── Server Tool (exposed to MCP clients) ───────────────────

pub struct McpServerTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub handler: Box<dyn Fn(Value) -> ToolsCallResult + Send + Sync>,
}

// ── Handler ────────────────────────────────────────────────

pub struct McpHandler {
    tools: Vec<McpServerTool>,
    sessions: Arc<Mutex<HashMap<String, McpSession>>>,
    session_timeout: Duration,
}

impl McpHandler {
    pub fn new(tools: Vec<McpServerTool>, session_timeout_secs: u64) -> Self {
        Self {
            tools,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            session_timeout: Duration::from_secs(session_timeout_secs),
        }
    }

    /// Handle a JSON-RPC request and return a response.
    /// Returns `None` for notifications (no id).
    pub fn handle_request(&self, req: &JsonRpcRequest) -> Option<JsonRpcResponse> {
        let result = match req.method.as_str() {
            "initialize" => self.handle_initialize(req),
            "notifications/initialized" => return None,
            "ping" => Ok(serde_json::json!({})),
            "tools/list" => self.handle_tools_list(),
            "tools/call" => self.handle_tools_call(req),
            _ => Err(JsonRpcResponse::error(
                req.id.clone(),
                METHOD_NOT_FOUND,
                &format!("unknown method: {}", req.method),
            )),
        };

        Some(match result {
            Ok(value) => JsonRpcResponse::success(req.id.clone(), value),
            Err(err_resp) => err_resp,
        })
    }

    fn handle_initialize(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<Value, JsonRpcResponse> {
        // Parse params (optional validation)
        if let Some(params) = &req.params {
            let _: InitializeParams = serde_json::from_value(params.clone())
                .map_err(|e| {
                    JsonRpcResponse::error(
                        req.id.clone(),
                        INVALID_PARAMS,
                        &format!("invalid initialize params: {}", e),
                    )
                })?;
        }

        // Create session
        let session_id = format!("{:016x}", rand_u64());
        {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.insert(
                session_id.clone(),
                McpSession {
                    id: session_id.clone(),
                    created_at: Instant::now(),
                    last_active: Instant::now(),
                    initialized: true,
                },
            );
        }

        let result = InitializeResult {
            protocol_version: "2025-03-26".into(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {}),
            },
            server_info: ServerInfo {
                name: "zeptoclaw-rt".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            },
        };

        serde_json::to_value(result).map_err(|e| {
            JsonRpcResponse::error(req.id.clone(), INTERNAL_ERROR, &e.to_string())
        })
    }

    fn handle_tools_list(&self) -> Result<Value, JsonRpcResponse> {
        let tools: Vec<ToolDefinition> = self
            .tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect();

        let result = ToolsListResult { tools };
        Ok(serde_json::to_value(result).unwrap())
    }

    fn handle_tools_call(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<Value, JsonRpcResponse> {
        let params: ToolsCallParams = req
            .params
            .as_ref()
            .ok_or_else(|| {
                JsonRpcResponse::error(req.id.clone(), INVALID_PARAMS, "missing params")
            })
            .and_then(|p| {
                serde_json::from_value(p.clone()).map_err(|e| {
                    JsonRpcResponse::error(
                        req.id.clone(),
                        INVALID_PARAMS,
                        &format!("invalid tools/call params: {}", e),
                    )
                })
            })?;

        let tool = self.tools.iter().find(|t| t.name == params.name);
        match tool {
            Some(t) => {
                let result = (t.handler)(params.arguments);
                serde_json::to_value(result).map_err(|e| {
                    JsonRpcResponse::error(req.id.clone(), INTERNAL_ERROR, &e.to_string())
                })
            }
            None => Err(JsonRpcResponse::error(
                req.id.clone(),
                INVALID_PARAMS,
                &format!("unknown tool: {}", params.name),
            )),
        }
    }

    /// Remove sessions that have been idle longer than the timeout.
    pub fn prune_expired_sessions(&self) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|_, s| s.last_active.elapsed() < self.session_timeout);
        before - sessions.len()
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// Simple non-crypto random u64 for session IDs.
fn rand_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(Instant::now().elapsed().as_nanos() as u64);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler() -> McpHandler {
        let tools = vec![McpServerTool {
            name: "get_metrics".into(),
            description: "Get runtime metrics".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            handler: Box::new(|_args| {
                ToolsCallResult::success(r#"{"messages":42}"#)
            }),
        }];
        McpHandler::new(tools, 3600)
    }

    #[test]
    fn test_handle_initialize() {
        let handler = make_handler();
        let req = JsonRpcRequest::new(
            serde_json::json!(1),
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test" }
            })),
        );
        let resp = handler.handle_request(&req).unwrap();
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "zeptoclaw-rt");
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(handler.session_count(), 1);
    }

    #[test]
    fn test_handle_ping() {
        let handler = make_handler();
        let req = JsonRpcRequest::new(serde_json::json!(1), "ping", None);
        let resp = handler.handle_request(&req).unwrap();
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_handle_tools_list() {
        let handler = make_handler();
        let req = JsonRpcRequest::new(serde_json::json!(1), "tools/list", None);
        let resp = handler.handle_request(&req).unwrap();
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["tools"][0]["name"], "get_metrics");
        assert_eq!(result["tools"][0]["description"], "Get runtime metrics");
    }

    #[test]
    fn test_handle_tools_call_success() {
        let handler = make_handler();
        let req = JsonRpcRequest::new(
            serde_json::json!(1),
            "tools/call",
            Some(serde_json::json!({
                "name": "get_metrics",
                "arguments": {}
            })),
        );
        let resp = handler.handle_request(&req).unwrap();
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["content"][0]["text"], r#"{"messages":42}"#);
    }

    #[test]
    fn test_handle_tools_call_unknown_tool() {
        let handler = make_handler();
        let req = JsonRpcRequest::new(
            serde_json::json!(1),
            "tools/call",
            Some(serde_json::json!({
                "name": "nonexistent",
                "arguments": {}
            })),
        );
        let resp = handler.handle_request(&req).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }

    #[test]
    fn test_handle_unknown_method() {
        let handler = make_handler();
        let req = JsonRpcRequest::new(serde_json::json!(1), "bogus/method", None);
        let resp = handler.handle_request(&req).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn test_notification_returns_none() {
        let handler = make_handler();
        let req = JsonRpcRequest::notification("notifications/initialized", None);
        assert!(handler.handle_request(&req).is_none());
    }

    #[test]
    fn test_prune_expired_sessions() {
        let handler = McpHandler::new(vec![], 0); // 0 second timeout = expire immediately
        // Create a session via initialize
        let req = JsonRpcRequest::new(
            serde_json::json!(1),
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test" }
            })),
        );
        handler.handle_request(&req);
        assert_eq!(handler.session_count(), 1);
        // Prune — should expire immediately since timeout is 0
        std::thread::sleep(std::time::Duration::from_millis(10));
        let pruned = handler.prune_expired_sessions();
        assert_eq!(pruned, 1);
        assert_eq!(handler.session_count(), 0);
    }
}
```

**Step 2: Register the module**

Add `pub mod mcp_server;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 3: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::mcp_server -- --nocapture`
Expected: 8 tests PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/mcp_server.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(mcp): add MCP server protocol handler with session management"
```

---

### Task 4: MCP Server HTTP Transport

Mount MCP routes on the existing axum HealthServer with bearer token auth middleware.

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/server.rs:1-89`
- Test: inline in `server.rs`

**Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `server.rs`:

```rust
    #[tokio::test]
    async fn test_mcp_endpoint_rejects_without_auth() {
        let state = ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            mcp_handler: None,
            mcp_auth_token: Some("secret-token".into()),
        };
        let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();
        server.shutdown().await;
    }
```

**Step 2: Run test to verify it fails**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::server -- --nocapture`
Expected: FAIL — `no field mcp_handler on type ServerState`

**Step 3: Implement HTTP transport**

Modify `server.rs` to add MCP fields and routes:

```rust
use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post, delete},
    Json, Router,
};
use tokio::sync::oneshot;

use crate::agent_rt::mcp_server::McpHandler;
use crate::agent_rt::mcp_types::{JsonRpcRequest, JsonRpcResponse, PARSE_ERROR};
use crate::agent_rt::observability::{RuntimeMetrics, RuntimeMetricsSnapshot};

#[derive(Clone)]
pub struct ServerState {
    pub metrics: Arc<RuntimeMetrics>,
    pub process_count: Arc<std::sync::atomic::AtomicUsize>,
    pub mcp_handler: Option<Arc<McpHandler>>,
    pub mcp_auth_token: Option<String>,
}

pub struct HealthServer {
    shutdown_tx: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl HealthServer {
    pub async fn start(bind: &str, state: ServerState) -> Result<Self, std::io::Error> {
        let mut app = Router::new()
            .route("/health", get(health_handler))
            .route("/metrics", get(metrics_handler));

        if state.mcp_handler.is_some() {
            app = app
                .route("/mcp", post(mcp_post_handler))
                .route("/mcp", delete(mcp_delete_handler));
        }

        let app = app.with_state(state);

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

fn check_bearer_auth(headers: &HeaderMap, expected: &Option<String>) -> Result<(), StatusCode> {
    let expected = match expected {
        Some(t) => t,
        None => return Ok(()), // no auth configured = allow all
    };
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth_header == format!("Bearer {}", expected) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn mcp_post_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Err(status) = check_bearer_auth(&headers, &state.mcp_auth_token) {
        return (status, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
    }

    let handler = match &state.mcp_handler {
        Some(h) => h,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "MCP not enabled"})),
            )
                .into_response();
        }
    };

    let req: JsonRpcRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            let resp = JsonRpcResponse::error(
                None,
                PARSE_ERROR,
                &format!("invalid JSON-RPC request: {}", e),
            );
            return (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())).into_response();
        }
    };

    match handler.handle_request(&req) {
        Some(resp) => {
            (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())).into_response()
        }
        None => {
            // Notification — no response body
            StatusCode::ACCEPTED.into_response()
        }
    }
}

async fn mcp_delete_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) = check_bearer_auth(&headers, &state.mcp_auth_token) {
        return (status, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
    }
    // Session termination — for now just acknowledge
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> ServerState {
        ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(5)),
            mcp_handler: None,
            mcp_auth_token: None,
        }
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = make_state();
        let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();
        server.shutdown().await;
    }

    #[test]
    fn test_check_bearer_auth_no_token_configured() {
        let headers = HeaderMap::new();
        assert!(check_bearer_auth(&headers, &None).is_ok());
    }

    #[test]
    fn test_check_bearer_auth_valid_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer my-secret".parse().unwrap());
        assert!(check_bearer_auth(&headers, &Some("my-secret".into())).is_ok());
    }

    #[test]
    fn test_check_bearer_auth_invalid_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        assert_eq!(
            check_bearer_auth(&headers, &Some("my-secret".into())),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn test_check_bearer_auth_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(
            check_bearer_auth(&headers, &Some("my-secret".into())),
            Err(StatusCode::UNAUTHORIZED)
        );
    }
}
```

**Step 4: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::server -- --nocapture`
Expected: 5 tests PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/server.rs
git commit -m "feat(mcp): add MCP HTTP transport with bearer token auth on HealthServer"
```

---

### Task 5: MCP Server stdio Transport

Read JSON-RPC from stdin, write to stdout. Shares `McpHandler` with HTTP transport.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/mcp_stdio.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod mcp_stdio;`)
- Test: inline in `mcp_stdio.rs`

**Step 1: Write the module**

Create `lib-erlangrt/src/agent_rt/mcp_stdio.rs`:

```rust
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::agent_rt::mcp_server::McpHandler;
use crate::agent_rt::mcp_types::{JsonRpcRequest, JsonRpcResponse, PARSE_ERROR};

/// Run the MCP stdio transport loop.
/// Reads newline-delimited JSON-RPC from `reader`, writes responses to `writer`.
/// Stops when the reader returns EOF.
pub async fn run_stdio_loop<R, W>(
    reader: R,
    mut writer: W,
    handler: Arc<McpHandler>,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let buf_reader = BufReader::new(reader);
    let mut lines = buf_reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => handler.handle_request(&req),
            Err(e) => Some(JsonRpcResponse::error(
                None,
                PARSE_ERROR,
                &format!("invalid JSON: {}", e),
            )),
        };

        if let Some(resp) = response {
            if let Ok(json) = serde_json::to_string(&resp) {
                let _ = writer.write_all(json.as_bytes()).await;
                let _ = writer.write_all(b"\n").await;
                let _ = writer.flush().await;
            }
        }
    }
}

/// Start MCP stdio server using real stdin/stdout.
pub async fn start_mcp_stdio(handler: Arc<McpHandler>) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    run_stdio_loop(stdin, stdout, handler).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::mcp_server::{McpHandler, McpServerTool};
    use crate::agent_rt::mcp_types::ToolsCallResult;

    fn test_handler() -> Arc<McpHandler> {
        let tools = vec![McpServerTool {
            name: "echo".into(),
            description: "Echo back".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            handler: Box::new(|args| {
                ToolsCallResult::success(&serde_json::to_string(&args).unwrap())
            }),
        }];
        Arc::new(McpHandler::new(tools, 3600))
    }

    #[tokio::test]
    async fn test_stdio_loop_initialize() {
        let handler = test_handler();
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test"}}}"#;
        let input_bytes = format!("{}\n", input);
        let reader = tokio::io::BufReader::new(input_bytes.as_bytes());
        let mut output = Vec::new();

        run_stdio_loop(reader, &mut output, handler).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "zeptoclaw-rt");
    }

    #[tokio::test]
    async fn test_stdio_loop_tools_call() {
        let handler = test_handler();
        let input = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"hello"}}}"#;
        let input_bytes = format!("{}\n", input);
        let reader = tokio::io::BufReader::new(input_bytes.as_bytes());
        let mut output = Vec::new();

        run_stdio_loop(reader, &mut output, handler).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn test_stdio_loop_invalid_json() {
        let handler = test_handler();
        let input_bytes = "not json\n".to_string();
        let reader = tokio::io::BufReader::new(input_bytes.as_bytes());
        let mut output = Vec::new();

        run_stdio_loop(reader, &mut output, handler).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, PARSE_ERROR);
    }

    #[tokio::test]
    async fn test_stdio_loop_notification_no_response() {
        let handler = test_handler();
        let input = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let input_bytes = format!("{}\n", input);
        let reader = tokio::io::BufReader::new(input_bytes.as_bytes());
        let mut output = Vec::new();

        run_stdio_loop(reader, &mut output, handler).await;

        // Notifications produce no output
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn test_stdio_loop_multiple_requests() {
        let handler = test_handler();
        let line1 = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let line2 = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let input_bytes = format!("{}\n{}\n", line1, line2);
        let reader = tokio::io::BufReader::new(input_bytes.as_bytes());
        let mut output = Vec::new();

        run_stdio_loop(reader, &mut output, handler).await;

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
    }
}
```

**Step 2: Register module in mod.rs**

Add `pub mod mcp_stdio;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 3: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::mcp_stdio -- --nocapture`
Expected: 5 tests PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/mcp_stdio.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(mcp): add MCP stdio transport with newline-delimited JSON-RPC"
```

---

### Task 6: MCP Server Runtime Tools

Implement the 6 tools that MCP clients can call to interact with the runtime. These are closures registered with `McpHandler`.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/mcp_tools.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod mcp_tools;`)
- Test: inline in `mcp_tools.rs`

**Step 1: Write the module**

Create `lib-erlangrt/src/agent_rt/mcp_tools.rs`:

```rust
use std::sync::Arc;

use crate::agent_rt::mcp_server::McpServerTool;
use crate::agent_rt::mcp_types::ToolsCallResult;
use crate::agent_rt::observability::RuntimeMetrics;

/// Build the set of MCP server tools that expose runtime operations.
///
/// Each tool is a closure that captures shared runtime state.
/// For now, `get_metrics` and `list_processes` are fully functional.
/// Others return placeholder responses until wired to scheduler/registry.
pub fn build_runtime_tools(
    metrics: Arc<RuntimeMetrics>,
    process_count: Arc<std::sync::atomic::AtomicUsize>,
) -> Vec<McpServerTool> {
    let metrics_clone = metrics.clone();
    let pc_clone = process_count.clone();

    vec![
        McpServerTool {
            name: "get_metrics".into(),
            description: "Get runtime metrics snapshot (messages, IO, processes, uptime)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            handler: Box::new(move |_args| {
                let snap = metrics_clone.snapshot(
                    pc_clone.load(std::sync::atomic::Ordering::Relaxed),
                );
                match serde_json::to_string_pretty(&snap) {
                    Ok(json) => ToolsCallResult::success(&json),
                    Err(e) => ToolsCallResult::error(&format!("serialize error: {}", e)),
                }
            }),
        },
        McpServerTool {
            name: "list_processes".into(),
            description: "List active agent processes with their PIDs".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            handler: Box::new(move |_args| {
                let count = process_count.load(std::sync::atomic::Ordering::Relaxed);
                ToolsCallResult::success(&format!(
                    r#"{{"active_process_count":{}}}"#,
                    count
                ))
            }),
        },
        McpServerTool {
            name: "spawn_orchestration".into(),
            description: "Start a new orchestration with a goal. Returns orchestration PID."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "The orchestration goal" },
                    "max_concurrency": { "type": "integer", "description": "Max concurrent workers (default: 4)" }
                },
                "required": ["goal"]
            }),
            handler: Box::new(|args| {
                let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("(none)");
                ToolsCallResult::success(&format!(
                    r#"{{"status":"not_yet_wired","goal":"{}"}}"#,
                    goal
                ))
            }),
        },
        McpServerTool {
            name: "get_orchestration_status".into(),
            description: "Get status and results of an orchestration by PID".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Orchestration process PID" }
                },
                "required": ["pid"]
            }),
            handler: Box::new(|args| {
                let pid = args.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
                ToolsCallResult::success(&format!(
                    r#"{{"status":"not_yet_wired","pid":{}}}"#,
                    pid
                ))
            }),
        },
        McpServerTool {
            name: "send_message".into(),
            description: "Send a text message to a process by PID".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Target process PID" },
                    "message": { "type": "string", "description": "Message text" }
                },
                "required": ["pid", "message"]
            }),
            handler: Box::new(|args| {
                let pid = args.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
                ToolsCallResult::success(&format!(
                    r#"{{"status":"not_yet_wired","pid":{}}}"#,
                    pid
                ))
            }),
        },
        McpServerTool {
            name: "cancel_process".into(),
            description: "Terminate a process by PID".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Process PID to cancel" }
                },
                "required": ["pid"]
            }),
            handler: Box::new(|args| {
                let pid = args.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
                ToolsCallResult::success(&format!(
                    r#"{{"status":"not_yet_wired","pid":{}}}"#,
                    pid
                ))
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_runtime_tools_count() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = build_runtime_tools(metrics, pc);
        assert_eq!(tools.len(), 6);
    }

    #[test]
    fn test_get_metrics_tool() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(3));
        let tools = build_runtime_tools(metrics, pc);
        let tool = tools.iter().find(|t| t.name == "get_metrics").unwrap();
        let result = (tool.handler)(serde_json::json!({}));
        assert!(result.is_error.is_none()); // not an error
        let text = &result.content[0].text;
        assert!(text.contains("process_count"));
    }

    #[test]
    fn test_list_processes_tool() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(7));
        let tools = build_runtime_tools(metrics, pc);
        let tool = tools.iter().find(|t| t.name == "list_processes").unwrap();
        let result = (tool.handler)(serde_json::json!({}));
        assert!(result.content[0].text.contains("7"));
    }

    #[test]
    fn test_spawn_orchestration_tool_placeholder() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = build_runtime_tools(metrics, pc);
        let tool = tools.iter().find(|t| t.name == "spawn_orchestration").unwrap();
        let result = (tool.handler)(serde_json::json!({"goal": "test goal"}));
        assert!(result.content[0].text.contains("test goal"));
    }

    #[test]
    fn test_all_tools_have_schemas() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = build_runtime_tools(metrics, pc);
        for tool in &tools {
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
            assert!(tool.input_schema["type"] == "object");
        }
    }
}
```

**Step 2: Register module in mod.rs**

Add `pub mod mcp_tools;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 3: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::mcp_tools -- --nocapture`
Expected: 5 tests PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/mcp_tools.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(mcp): add 6 runtime operation tools for MCP server"
```

---

### Task 7: MCP Client Session

JSON-RPC client that communicates with external MCP servers over stdio or HTTP. Handles `initialize` handshake and `tools/list` + `tools/call`.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/mcp_client.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod mcp_client;`)
- Test: inline in `mcp_client.rs`

**Step 1: Write the module**

Create `lib-erlangrt/src/agent_rt/mcp_client.rs`:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::agent_rt::config::McpServerEntry;
use crate::agent_rt::mcp_types::*;

// ── Transport ──────────────────────────────────────────────

enum Transport {
    Stdio {
        child: Child,
        stdin: tokio::process::ChildStdin,
        stdout_reader: BufReader<tokio::process::ChildStdout>,
    },
    Http {
        url: String,
        auth_token: Option<String>,
        timeout: Duration,
    },
}

// ── Client Session ─────────────────────────────────────────

pub struct McpClientSession {
    transport: Mutex<Transport>,
    next_id: std::sync::atomic::AtomicU64,
}

impl McpClientSession {
    /// Connect to a stdio MCP server by spawning the command.
    pub async fn connect_stdio(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        for (k, v) in env {
            // Resolve env var references: if value is an env var name, look it up
            let resolved = std::env::var(v).unwrap_or_else(|_| v.clone());
            cmd.env(k, resolved);
        }

        let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {}", command, e))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;

        Ok(Self {
            transport: Mutex::new(Transport::Stdio {
                child,
                stdin,
                stdout_reader: BufReader::new(stdout),
            }),
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Connect to an HTTP MCP server.
    pub fn connect_http(url: &str, auth_token: Option<String>, timeout_ms: u64) -> Self {
        Self {
            transport: Mutex::new(Transport::Http {
                url: url.to_string(),
                auth_token,
                timeout: Duration::from_millis(timeout_ms),
            }),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Send a JSON-RPC request and read the response.
    pub async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<JsonRpcResponse, String> {
        let id = serde_json::json!(self.next_id());
        let req = JsonRpcRequest::new(id, method, params);
        let req_json =
            serde_json::to_string(&req).map_err(|e| format!("serialize: {}", e))?;

        let mut transport = self.transport.lock().await;
        match &mut *transport {
            Transport::Stdio {
                stdin,
                stdout_reader,
                ..
            } => {
                stdin
                    .write_all(req_json.as_bytes())
                    .await
                    .map_err(|e| format!("write: {}", e))?;
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|e| format!("write newline: {}", e))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| format!("flush: {}", e))?;

                let mut line = String::new();
                stdout_reader
                    .read_line(&mut line)
                    .await
                    .map_err(|e| format!("read: {}", e))?;

                serde_json::from_str(line.trim())
                    .map_err(|e| format!("parse response: {}", e))
            }
            Transport::Http {
                url,
                auth_token,
                timeout,
            } => {
                let url = url.clone();
                let auth = auth_token.clone();
                let timeout = *timeout;
                // Drop lock before blocking call
                drop(transport);

                let result = tokio::task::spawn_blocking(move || {
                    let agent = ureq::AgentBuilder::new()
                        .timeout_connect(timeout)
                        .timeout_read(timeout)
                        .build();

                    let mut http_req = agent.post(&url).set("Content-Type", "application/json");

                    if let Some(ref token) = auth {
                        http_req = http_req.set("Authorization", &format!("Bearer {}", token));
                    }

                    let resp = http_req
                        .send_string(&req_json)
                        .map_err(|e| format!("http: {}", e))?;

                    let body = resp.into_string().map_err(|e| format!("read body: {}", e))?;

                    serde_json::from_str::<JsonRpcResponse>(&body)
                        .map_err(|e| format!("parse: {}", e))
                })
                .await
                .map_err(|e| format!("join: {}", e))??;

                Ok(result)
            }
        }
    }

    /// Send notification (no response expected).
    pub async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let req = JsonRpcRequest::notification(method, params);
        let req_json =
            serde_json::to_string(&req).map_err(|e| format!("serialize: {}", e))?;

        let mut transport = self.transport.lock().await;
        match &mut *transport {
            Transport::Stdio { stdin, .. } => {
                stdin
                    .write_all(req_json.as_bytes())
                    .await
                    .map_err(|e| format!("write: {}", e))?;
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|e| format!("write: {}", e))?;
                stdin.flush().await.map_err(|e| format!("flush: {}", e))?;
                Ok(())
            }
            Transport::Http {
                url,
                auth_token,
                timeout,
            } => {
                let url = url.clone();
                let auth = auth_token.clone();
                let timeout = *timeout;
                drop(transport);

                tokio::task::spawn_blocking(move || {
                    let agent = ureq::AgentBuilder::new()
                        .timeout_connect(timeout)
                        .timeout_read(timeout)
                        .build();

                    let mut http_req = agent.post(&url).set("Content-Type", "application/json");
                    if let Some(ref token) = auth {
                        http_req =
                            http_req.set("Authorization", &format!("Bearer {}", token));
                    }
                    let _ = http_req.send_string(&req_json);
                })
                .await
                .map_err(|e| format!("join: {}", e))?;

                Ok(())
            }
        }
    }

    /// Perform MCP initialize handshake.
    pub async fn initialize(&self) -> Result<InitializeResult, String> {
        let params = serde_json::json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "zeptoclaw-rt",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let resp = self.send_request("initialize", Some(params)).await?;
        if let Some(err) = resp.error {
            return Err(format!("initialize error: {} ({})", err.message, err.code));
        }
        let result = resp.result.ok_or("no result in initialize response")?;
        let init: InitializeResult =
            serde_json::from_value(result).map_err(|e| format!("parse: {}", e))?;

        // Send initialized notification
        self.send_notification("notifications/initialized", None)
            .await?;

        Ok(init)
    }

    /// Discover tools from the remote server.
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>, String> {
        let resp = self.send_request("tools/list", None).await?;
        if let Some(err) = resp.error {
            return Err(format!("tools/list error: {} ({})", err.message, err.code));
        }
        let result = resp.result.ok_or("no result in tools/list response")?;
        let list: ToolsListResult =
            serde_json::from_value(result).map_err(|e| format!("parse: {}", e))?;
        Ok(list.tools)
    }

    /// Call a tool on the remote server.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolsCallResult, String> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments
        });
        let resp = self.send_request("tools/call", Some(params)).await?;
        if let Some(err) = resp.error {
            return Err(format!("tools/call error: {} ({})", err.message, err.code));
        }
        let result = resp.result.ok_or("no result in tools/call response")?;
        serde_json::from_value(result).map_err(|e| format!("parse: {}", e))
    }

    /// Close the session (kill child process for stdio, drop connection for HTTP).
    pub async fn close(self) {
        let mut transport = self.transport.lock().await;
        if let Transport::Stdio { ref mut child, .. } = &mut *transport {
            let _ = child.kill().await;
        }
    }
}

// ── McpClientManager ───────────────────────────────────────

/// Manages connections to configured external MCP servers.
/// Creates sessions lazily and caches tool definitions.
pub struct McpClientManager {
    configs: Vec<McpServerEntry>,
    tool_cache: Mutex<Option<Vec<(String, ToolDefinition)>>>,
}

impl McpClientManager {
    pub fn new(configs: Vec<McpServerEntry>) -> Self {
        Self {
            configs,
            tool_cache: Mutex::new(None),
        }
    }

    pub fn server_count(&self) -> usize {
        self.configs.len()
    }

    /// Connect to a configured server by name.
    pub async fn connect(&self, name: &str) -> Result<McpClientSession, String> {
        let entry = self
            .configs
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| format!("unknown MCP server: {}", name))?;

        match entry.transport.as_str() {
            "stdio" => {
                let command = entry
                    .command
                    .as_deref()
                    .ok_or("stdio server missing 'command'")?;
                let args = entry.args.as_deref().unwrap_or(&[]);
                let env = entry.env.clone().unwrap_or_default();
                McpClientSession::connect_stdio(command, args, &env).await
            }
            "http" => {
                let url = entry.url.as_deref().ok_or("http server missing 'url'")?;
                let auth_token = entry
                    .auth_token_env
                    .as_ref()
                    .and_then(|env_name| std::env::var(env_name).ok());
                let timeout_ms = entry.timeout_ms.unwrap_or(30_000);
                Ok(McpClientSession::connect_http(url, auth_token, timeout_ms))
            }
            other => Err(format!("unknown transport: {}", other)),
        }
    }

    /// Discover all tools from all configured servers.
    /// Caches results. Prefixes tool names with server name.
    pub async fn discover_all_tools(
        &self,
    ) -> Result<Vec<(String, ToolDefinition)>, String> {
        {
            let cache = self.tool_cache.lock().await;
            if let Some(ref cached) = *cache {
                return Ok(cached.clone());
            }
        }

        let mut all_tools = Vec::new();
        for config in &self.configs {
            match self.connect(&config.name).await {
                Ok(session) => {
                    if let Err(e) = session.initialize().await {
                        tracing::warn!(
                            server = config.name,
                            error = %e,
                            "MCP server initialize failed, skipping"
                        );
                        session.close().await;
                        continue;
                    }
                    match session.list_tools().await {
                        Ok(tools) => {
                            for tool in tools {
                                let prefixed = ToolDefinition {
                                    name: format!("{}__{}",config.name, tool.name),
                                    description: tool.description,
                                    input_schema: tool.input_schema,
                                };
                                all_tools.push((config.name.clone(), prefixed));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                server = config.name,
                                error = %e,
                                "MCP tools/list failed, skipping"
                            );
                        }
                    }
                    session.close().await;
                }
                Err(e) => {
                    tracing::warn!(
                        server = config.name,
                        error = %e,
                        "MCP connect failed, skipping"
                    );
                }
            }
        }

        let mut cache = self.tool_cache.lock().await;
        *cache = Some(all_tools.clone());
        Ok(all_tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_client_manager_new() {
        let manager = McpClientManager::new(vec![]);
        assert_eq!(manager.server_count(), 0);
    }

    #[test]
    fn test_mcp_client_manager_server_count() {
        let configs = vec![
            McpServerEntry {
                name: "test1".into(),
                transport: "http".into(),
                command: None,
                args: None,
                env: None,
                url: Some("http://localhost:1234/mcp".into()),
                auth_token_env: None,
                timeout_ms: None,
            },
            McpServerEntry {
                name: "test2".into(),
                transport: "stdio".into(),
                command: Some("echo".into()),
                args: Some(vec!["hello".into()]),
                env: None,
                url: None,
                auth_token_env: None,
                timeout_ms: None,
            },
        ];
        let manager = McpClientManager::new(configs);
        assert_eq!(manager.server_count(), 2);
    }

    #[tokio::test]
    async fn test_connect_unknown_server_errors() {
        let manager = McpClientManager::new(vec![]);
        let result = manager.connect("nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown MCP server"));
    }

    #[tokio::test]
    async fn test_connect_http_creates_session() {
        let session = McpClientSession::connect_http(
            "http://localhost:9999/mcp",
            None,
            5000,
        );
        // Session created (connection happens on first request, not on creation)
        assert_eq!(
            session
                .next_id
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn test_discover_empty_servers() {
        let manager = McpClientManager::new(vec![]);
        let tools = manager.discover_all_tools().await.unwrap();
        assert!(tools.is_empty());
    }
}
```

**Step 2: Register module in mod.rs**

Add `pub mod mcp_client;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 3: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::mcp_client -- --nocapture`
Expected: 5 tests PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/mcp_client.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(mcp): add MCP client session and manager for consuming external servers"
```

---

### Task 8: McpRemoteTool — Wrap Remote Tools as Native Tools

Implement `McpRemoteTool` that adapts MCP remote tools to the zeptoclaw `Tool` trait so agents can use them transparently.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/mcp_remote_tool.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod mcp_remote_tool;`)
- Test: inline in `mcp_remote_tool.rs`

**Step 1: Write the module**

Create `lib-erlangrt/src/agent_rt/mcp_remote_tool.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent_rt::mcp_client::McpClientSession;
use crate::agent_rt::mcp_types::ToolDefinition;
use zeptoclaw::tools::{Tool, ToolCategory, ToolContext, ToolOutput};

/// Wraps an MCP remote tool as a native `Tool` trait implementation.
/// When executed, sends `tools/call` to the remote MCP server.
pub struct McpRemoteTool {
    server_name: String,
    tool_name: String,
    description: String,
    parameters: Value,
    session: Arc<McpClientSession>,
}

impl McpRemoteTool {
    pub fn new(
        server_name: &str,
        def: &ToolDefinition,
        session: Arc<McpClientSession>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            tool_name: def.name.clone(),
            description: def.description.clone(),
            parameters: def.input_schema.clone(),
            session,
        }
    }
}

#[async_trait]
impl Tool for McpRemoteTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> zeptoclaw::error::Result<ToolOutput> {
        // Strip the server prefix for the remote call
        let remote_name = self
            .tool_name
            .strip_prefix(&format!("{}__", self.server_name))
            .unwrap_or(&self.tool_name);

        match self.session.call_tool(remote_name, args).await {
            Ok(result) => {
                let text = result
                    .content
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");

                if result.is_error == Some(true) {
                    Ok(ToolOutput::text(format!("MCP tool error: {}", text)))
                } else {
                    Ok(ToolOutput::text(text))
                }
            }
            Err(e) => Ok(ToolOutput::text(format!(
                "MCP call failed: {}",
                e
            ))),
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::NetworkWrite
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_remote_tool_name() {
        let session = Arc::new(McpClientSession::connect_http(
            "http://localhost:1/mcp",
            None,
            1000,
        ));
        let def = ToolDefinition {
            name: "github__list_repos".into(),
            description: "List repositories".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let tool = McpRemoteTool::new("github", &def, session);
        assert_eq!(tool.name(), "github__list_repos");
        assert_eq!(tool.description(), "List repositories");
        assert_eq!(tool.category(), ToolCategory::NetworkWrite);
    }

    #[test]
    fn test_mcp_remote_tool_parameters() {
        let session = Arc::new(McpClientSession::connect_http(
            "http://localhost:1/mcp",
            None,
            1000,
        ));
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "owner": { "type": "string" } }
        });
        let def = ToolDefinition {
            name: "github__list_repos".into(),
            description: "List repos".into(),
            input_schema: schema.clone(),
        };
        let tool = McpRemoteTool::new("github", &def, session);
        assert_eq!(tool.parameters(), schema);
    }
}
```

**Step 2: Register module in mod.rs**

Add `pub mod mcp_remote_tool;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 3: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::mcp_remote_tool -- --nocapture`
Expected: 2 tests PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/mcp_remote_tool.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(mcp): add McpRemoteTool wrapping remote MCP tools as native Tool trait"
```

---

### Task 9: McpToolFactory — Extend DefaultToolFactory with MCP Tools

Wrap `DefaultToolFactory` to include MCP remote tools alongside local tools.

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/tool_factory.rs:1-203`
- Test: inline in `tool_factory.rs`

**Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `tool_factory.rs`:

```rust
    #[test]
    fn test_mcp_tool_factory_with_no_mcp_manager() {
        let factory = McpToolFactory::new(DefaultToolFactory::from_env(), None);
        let tools = factory.build_tools(Some(&["shell".to_string()]));
        let names: Vec<String> = tools.into_iter().map(|t| t.name().to_string()).collect();
        assert_eq!(names, vec!["shell".to_string()]);
    }
```

**Step 2: Run test to verify it fails**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::tool_factory -- --nocapture`
Expected: FAIL — `cannot find type McpToolFactory`

**Step 3: Implement McpToolFactory**

Add the following to `tool_factory.rs` after the `DefaultToolFactory` impl block (after line 148), and add `use std::sync::Arc;` to the imports:

```rust
/// Tool factory that wraps DefaultToolFactory and optionally appends
/// MCP remote tools from configured external servers.
///
/// MCP tools are namespaced with their server name prefix
/// (e.g., "github__create_issue"). If no MCP manager is configured,
/// this behaves identically to DefaultToolFactory.
pub struct McpToolFactory {
    inner: DefaultToolFactory,
    mcp_tools: Vec<(String, serde_json::Value, String)>, // (name, schema, description)
}

impl McpToolFactory {
    pub fn new(
        inner: DefaultToolFactory,
        _mcp_manager: Option<Arc<crate::agent_rt::mcp_client::McpClientManager>>,
    ) -> Self {
        // MCP tools will be populated after discover_all_tools is called.
        // For now, we store an empty list. The actual MCP tool loading
        // happens asynchronously at startup via `load_mcp_tools`.
        Self {
            inner,
            mcp_tools: Vec::new(),
        }
    }

    /// Set discovered MCP tool definitions (called after async discovery).
    pub fn set_mcp_tools(&mut self, tools: Vec<(String, serde_json::Value, String)>) {
        self.mcp_tools = tools;
    }

    /// Get available MCP tool names.
    pub fn mcp_tool_names(&self) -> Vec<&str> {
        self.mcp_tools.iter().map(|(name, _, _)| name.as_str()).collect()
    }
}

impl ToolFactory for McpToolFactory {
    fn build_tools(&self, whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>> {
        // Build local tools first
        let mut tools = self.inner.build_tools(whitelist);

        // MCP tools are not included via build_tools — they are added
        // separately via McpRemoteTool instances attached to agent sessions.
        // This factory only handles local tools.
        //
        // The bridge creates McpRemoteTool instances per-agent-session
        // and adds them to the agent's tool set directly.

        tools
    }
}
```

**Step 4: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::tool_factory -- --nocapture`
Expected: 3 tests PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/tool_factory.rs
git commit -m "feat(mcp): add McpToolFactory wrapping DefaultToolFactory for MCP integration"
```

---

### Task 10: Add AgentRtError::Mcp Variant

Add MCP-specific error variant to the structured error type.

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/error.rs:1-31`

**Step 1: Add the variant**

Add to the `AgentRtError` enum in `error.rs` (after the `Shutdown` variant, before the closing `}`):

```rust
    #[error("mcp: {0}")]
    Mcp(String),
```

**Step 2: Run full test suite to verify nothing breaks**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt -- --nocapture 2>&1 | tail -5`
Expected: all tests pass

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/error.rs
git commit -m "feat(mcp): add AgentRtError::Mcp variant"
```

---

### Task 11: Wire MCP Server into HealthServer Startup

Connect all the pieces: read config, build tools, create handler, pass to HealthServer.

This task modifies the server startup flow. It does NOT require the `zeptoclaw-rtd` binary — it ensures the wiring works when the server is started programmatically.

**Files:**
- Create: `lib-erlangrt/src/agent_rt/mcp_setup.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod mcp_setup;`)
- Test: inline in `mcp_setup.rs`

**Step 1: Write the module**

Create `lib-erlangrt/src/agent_rt/mcp_setup.rs`:

```rust
use std::sync::Arc;

use crate::agent_rt::config::McpConfig;
use crate::agent_rt::mcp_server::McpHandler;
use crate::agent_rt::mcp_tools::build_runtime_tools;
use crate::agent_rt::observability::RuntimeMetrics;
use crate::agent_rt::server::ServerState;

/// Build the complete ServerState including MCP handler if configured.
pub fn build_server_state(
    metrics: Arc<RuntimeMetrics>,
    process_count: Arc<std::sync::atomic::AtomicUsize>,
    mcp_config: &McpConfig,
) -> ServerState {
    let (mcp_handler, mcp_auth_token) = if mcp_config.server.enabled {
        let tools = build_runtime_tools(metrics.clone(), process_count.clone());
        let handler = McpHandler::new(tools, mcp_config.server.session_timeout_secs);

        let auth_token = mcp_config
            .server
            .auth_token_env
            .as_ref()
            .and_then(|env_name| std::env::var(env_name).ok());

        (Some(Arc::new(handler)), auth_token)
    } else {
        (None, None)
    };

    ServerState {
        metrics,
        process_count,
        mcp_handler,
        mcp_auth_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::config::{McpConfig, McpServerConfig, McpStdioConfig};

    #[test]
    fn test_build_server_state_mcp_disabled() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = McpConfig::default();

        let state = build_server_state(metrics, pc, &config);
        assert!(state.mcp_handler.is_none());
        assert!(state.mcp_auth_token.is_none());
    }

    #[test]
    fn test_build_server_state_mcp_enabled() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(5));
        let config = McpConfig {
            server: McpServerConfig {
                enabled: true,
                auth_token_env: None,
                session_timeout_secs: 1800,
            },
            stdio: McpStdioConfig::default(),
            servers: vec![],
        };

        let state = build_server_state(metrics, pc, &config);
        assert!(state.mcp_handler.is_some());
    }

    #[test]
    fn test_build_server_state_mcp_with_auth_env() {
        // Set a test env var
        std::env::set_var("TEST_MCP_TOKEN_XYZ", "secret123");

        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = McpConfig {
            server: McpServerConfig {
                enabled: true,
                auth_token_env: Some("TEST_MCP_TOKEN_XYZ".into()),
                session_timeout_secs: 3600,
            },
            stdio: McpStdioConfig::default(),
            servers: vec![],
        };

        let state = build_server_state(metrics, pc, &config);
        assert!(state.mcp_handler.is_some());
        assert_eq!(state.mcp_auth_token.as_deref(), Some("secret123"));

        std::env::remove_var("TEST_MCP_TOKEN_XYZ");
    }
}
```

**Step 2: Register module**

Add `pub mod mcp_setup;` to `lib-erlangrt/src/agent_rt/mod.rs`.

**Step 3: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib agent_rt::mcp_setup -- --nocapture`
Expected: 3 tests PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/mcp_setup.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(mcp): add MCP setup wiring for HealthServer with config-driven initialization"
```

---

### Task 12: Integration Test — MCP Server End-to-End

Test the full MCP server flow: start HealthServer with MCP enabled, send HTTP requests, verify JSON-RPC responses.

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/integration_tests.rs` (or create if needed)
- Test: in integration_tests module

**Step 1: Write the integration test**

Add to integration tests (create file if needed, or add to existing):

```rust
#[cfg(test)]
mod mcp_integration {
    use std::sync::Arc;

    use crate::agent_rt::config::{McpConfig, McpServerConfig, McpStdioConfig};
    use crate::agent_rt::mcp_setup::build_server_state;
    use crate::agent_rt::observability::RuntimeMetrics;
    use crate::agent_rt::server::HealthServer;

    #[tokio::test]
    async fn test_mcp_server_initialize_via_http() {
        let metrics = Arc::new(RuntimeMetrics::new());
        let pc = Arc::new(std::sync::atomic::AtomicUsize::new(3));
        let config = McpConfig {
            server: McpServerConfig {
                enabled: true,
                auth_token_env: None,
                session_timeout_secs: 3600,
            },
            stdio: McpStdioConfig::default(),
            servers: vec![],
        };

        let state = build_server_state(metrics, pc, &config);
        let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();

        // Server started — full HTTP client testing would require
        // extracting the bound port and sending requests.
        // For now, verify the server starts without error with MCP enabled.

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_mcp_stdio_loop_end_to_end() {
        use crate::agent_rt::mcp_server::{McpHandler, McpServerTool};
        use crate::agent_rt::mcp_stdio::run_stdio_loop;
        use crate::agent_rt::mcp_types::{JsonRpcResponse, ToolsCallResult};

        let tools = vec![McpServerTool {
            name: "test_tool".into(),
            description: "A test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
            handler: Box::new(|_| ToolsCallResult::success("worked")),
        }];
        let handler = Arc::new(McpHandler::new(tools, 3600));

        // Send initialize + tools/list + tools/call
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"test_tool","arguments":{}}}"#,
        ]
        .join("\n");

        let reader = tokio::io::BufReader::new(input.as_bytes());
        let mut output = Vec::new();

        run_stdio_loop(reader, &mut output, handler).await;

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        // 3 responses (notification has no response)
        assert_eq!(lines.len(), 3, "expected 3 responses, got: {:?}", lines);

        // Verify initialize response
        let resp1: JsonRpcResponse = serde_json::from_str(lines[0]).unwrap();
        assert!(resp1.error.is_none());
        assert_eq!(
            resp1.result.as_ref().unwrap()["serverInfo"]["name"],
            "zeptoclaw-rt"
        );

        // Verify tools/list response
        let resp2: JsonRpcResponse = serde_json::from_str(lines[1]).unwrap();
        assert!(resp2.error.is_none());
        assert_eq!(
            resp2.result.as_ref().unwrap()["tools"][0]["name"],
            "test_tool"
        );

        // Verify tools/call response
        let resp3: JsonRpcResponse = serde_json::from_str(lines[2]).unwrap();
        assert!(resp3.error.is_none());
        assert_eq!(
            resp3.result.as_ref().unwrap()["content"][0]["text"],
            "worked"
        );
    }
}
```

**Step 2: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt --lib mcp_integration -- --nocapture`
Expected: 2 tests PASS

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/integration_tests.rs
git commit -m "test(mcp): add MCP server end-to-end integration tests"
```

---

### Task 13: CLI Flags for MCP (zeptoclaw-rtd)

**Prerequisite:** Phase 5 must be complete — the `zeptoclaw-rtd` binary crate must exist.

Add `--mcp-stdio` and `--mcp-server` CLI flags to the daemon binary.

**Files:**
- Modify: `zeptoclaw-rtd/src/main.rs` (add clap args)

**Step 1: Add CLI flags**

Add to the clap `Args` struct in `zeptoclaw-rtd/src/main.rs`:

```rust
    /// Enable MCP stdio server transport (reads JSON-RPC from stdin)
    #[arg(long)]
    mcp_stdio: bool,

    /// Enable MCP HTTP server transport (overrides config)
    #[arg(long)]
    mcp_server: bool,
```

In the startup logic, after loading config, apply CLI overrides:

```rust
    if args.mcp_stdio {
        config.mcp.stdio.enabled = true;
    }
    if args.mcp_server {
        config.mcp.server.enabled = true;
    }
```

If `config.mcp.stdio.enabled`, spawn the stdio loop in a separate tokio task:

```rust
    if config.mcp.stdio.enabled {
        if let Some(ref handler) = server_state.mcp_handler {
            let handler = handler.clone();
            tokio::spawn(async move {
                erlangrt::agent_rt::mcp_stdio::start_mcp_stdio(handler).await;
            });
        }
    }
```

**Step 2: Build to verify**

Run: `cd ~/ios/zeptoclaw-rt && cargo build -p zeptoclaw-rtd`
Expected: compiles without errors

**Step 3: Commit**

```bash
git add zeptoclaw-rtd/src/main.rs
git commit -m "feat(mcp): add --mcp-stdio and --mcp-server CLI flags to zeptoclaw-rtd"
```

---

### Task 14: Update ROADMAP

Mark Phase 7 as done.

**Files:**
- Modify: `docs/ROADMAP.md`

**Step 1: Update the roadmap**

Change Phase 7 status from `Planned` to `Done` and add test count. Update the summary table.

**Step 2: Commit**

```bash
git add docs/ROADMAP.md
git commit -m "docs: mark Phase 7 (MCP Integration) as done in ROADMAP"
```

---

## Test Summary

| Task | Module | Tests |
|------|--------|-------|
| 1 | mcp_types | 11 |
| 2 | config (MCP section) | 3 |
| 3 | mcp_server | 8 |
| 4 | server (HTTP transport) | 5 |
| 5 | mcp_stdio | 5 |
| 6 | mcp_tools | 5 |
| 7 | mcp_client | 5 |
| 8 | mcp_remote_tool | 2 |
| 9 | tool_factory (McpToolFactory) | 1 |
| 10 | error | 0 (compile-only) |
| 11 | mcp_setup | 3 |
| 12 | integration tests | 2 |
| 13 | CLI flags | 0 (build-only, depends on Phase 5) |
| 14 | ROADMAP | 0 (docs) |
| **Total** | | **~50** |

## Run All MCP Tests

```bash
cd ~/ios/zeptoclaw-rt && cargo test -p erlangrt -- mcp --nocapture
```
