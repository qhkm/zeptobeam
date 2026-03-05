//! MCP Client for ZeptoBeam Agent Runtime
//!
//! This module provides:
//! - `McpClientSession`: Per-agent session for connecting to external MCP servers
//! - `McpClientManager`: Manages connections to configured external MCP servers from TOML
//! - `McpRemoteTool`: Wrapper that implements the zeptoclaw Tool trait for remote MCP tools
//!
//! MCP tools are namespaced as "servername__toolname" to prevent collisions.

pub use zeptoclaw::tools::mcp::{
  client::McpClient,
  protocol::McpTool,
  transport::{HttpTransport, McpTransport, StdioTransport},
};

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use tokio::sync::{Mutex, RwLock};
use zeptoclaw::tools::{
  mcp::protocol::{McpRequest, McpResponse},
  Tool, ToolCategory, ToolContext, ToolOutput,
};

use crate::agent_rt::{config::McpServerEntry, types::AgentPid};

/// Result type for MCP client operations.
pub type McpResult<T> = Result<T, McpClientError>;

/// Errors that can occur in the MCP client.
#[derive(Debug, Clone)]
pub enum McpClientError {
  /// Server not found in configuration.
  ServerNotFound(String),
  /// Transport error (connection failed, etc.).
  Transport(String),
  /// Protocol error (invalid response, etc.).
  Protocol(String),
  /// Tool not found on the server.
  ToolNotFound(String),
  /// Session not initialized.
  NotInitialized,
  /// Configuration error.
  Config(String),
}

impl std::fmt::Display for McpClientError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      McpClientError::ServerNotFound(name) => {
        write!(f, "MCP server '{}' not found", name)
      }
      McpClientError::Transport(msg) => write!(f, "MCP transport error: {}", msg),
      McpClientError::Protocol(msg) => write!(f, "MCP protocol error: {}", msg),
      McpClientError::ToolNotFound(name) => write!(f, "MCP tool '{}' not found", name),
      McpClientError::NotInitialized => write!(f, "MCP session not initialized"),
      McpClientError::Config(msg) => write!(f, "MCP config error: {}", msg),
    }
  }
}

impl std::error::Error for McpClientError {}

/// HTTP MCP transport with optional bearer auth and session-id propagation.
struct AuthHttpTransport {
  url: String,
  http: reqwest::Client,
  auth_header: Option<HeaderValue>,
  session_id: Mutex<Option<String>>,
}

impl AuthHttpTransport {
  fn new(url: &str, timeout_secs: u64, bearer_token: Option<String>) -> McpResult<Self> {
    let http = reqwest::Client::builder()
      .timeout(Duration::from_secs(timeout_secs.max(1)))
      .build()
      .map_err(|e| {
        McpClientError::Transport(format!("Failed to build HTTP MCP client: {}", e))
      })?;

    let auth_header = match bearer_token {
      Some(token) => Some(HeaderValue::from_str(&format!("Bearer {}", token)).map_err(
        |e| McpClientError::Config(format!("Invalid HTTP auth token: {}", e)),
      )?),
      None => None,
    };

    Ok(Self {
      url: url.to_string(),
      http,
      auth_header,
      session_id: Mutex::new(None),
    })
  }
}

#[async_trait]
impl McpTransport for AuthHttpTransport {
  async fn send(&self, request: &McpRequest) -> Result<McpResponse, String> {
    let session_id = { self.session_id.lock().await.clone() };

    let mut req = self.http.post(&self.url).json(request);

    if let Some(auth) = &self.auth_header {
      req = req.header(AUTHORIZATION, auth.clone());
    }

    if let Some(sid) = session_id.as_deref() {
      req = req.header("mcp-session-id", sid);
    }

    let resp = req
      .send()
      .await
      .map_err(|e| format!("HTTP request failed: {}", e))?;

    let response_session_id = resp
      .headers()
      .get("mcp-session-id")
      .and_then(|h| h.to_str().ok())
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(str::to_string);

    let status = resp.status();
    if !status.is_success() {
      let body = resp.text().await.unwrap_or_default();
      return Err(format!("HTTP {} from MCP server: {}", status, body));
    }

    let parsed = resp
      .json::<McpResponse>()
      .await
      .map_err(|e| format!("Failed to parse MCP response: {}", e))?;

    if let Some(sid) = response_session_id {
      let mut guard = self.session_id.lock().await;
      *guard = Some(sid);
    }

    Ok(parsed)
  }

  async fn shutdown(&self) -> Result<(), String> {
    Ok(())
  }

  fn transport_type(&self) -> &str {
    "http"
  }
}

fn timeout_secs_from_ms(timeout_ms: Option<u64>) -> u64 {
  match timeout_ms {
    Some(ms) => {
      let mut secs = ms / 1000;
      if ms % 1000 != 0 {
        secs = secs.saturating_add(1);
      }
      secs.max(1)
    }
    None => 30,
  }
}

/// A cached tool definition with its server name.
#[derive(Debug, Clone)]
struct CachedTool {
  server_name: String,
  remote_name: String,
  description: String,
  input_schema: serde_json::Value,
}

/// Per-agent session for connecting to external MCP servers.
///
/// Created lazily on first MCP tool use and closed when the agent terminates.
pub struct McpClientSession {
  /// The agent this session belongs to.
  agent_pid: AgentPid,
  /// Connected MCP clients by server name.
  clients: RwLock<HashMap<String, Arc<McpClient>>>,
  /// Cached tool definitions from all servers.
  tools_cache: RwLock<Option<Vec<CachedTool>>>,
  /// Whether the session has been initialized.
  initialized: RwLock<bool>,
  /// Server configurations.
  server_configs: Vec<McpServerEntry>,
}

impl McpClientSession {
  /// Create a new MCP client session for the given agent.
  pub fn new(agent_pid: AgentPid, server_configs: Vec<McpServerEntry>) -> Self {
    Self {
      agent_pid,
      clients: RwLock::new(HashMap::new()),
      tools_cache: RwLock::new(None),
      initialized: RwLock::new(false),
      server_configs,
    }
  }

  /// Get the agent PID for this session.
  pub fn agent_pid(&self) -> AgentPid {
    self.agent_pid
  }

  /// Initialize the session by connecting to all configured servers.
  /// This is called lazily on first tool use.
  async fn initialize(&self) -> McpResult<()> {
    let mut initialized = self.initialized.write().await;
    if *initialized {
      return Ok(());
    }

    let mut clients = self.clients.write().await;
    let mut all_tools = Vec::new();

    for config in &self.server_configs {
      let client = Self::connect_to_server(config).await?;

      // Initialize the connection
      client.initialize().await.map_err(|e| {
        McpClientError::Protocol(format!("Failed to initialize {}: {}", config.name, e))
      })?;

      // Discover tools from this server
      let tools = client.list_tools().await.map_err(|e| {
        McpClientError::Protocol(format!(
          "Failed to list tools from {}: {}",
          config.name, e
        ))
      })?;

      // Cache tool definitions with namespacing
      for tool in tools {
        all_tools.push(CachedTool {
          server_name: config.name.clone(),
          remote_name: tool.name.clone(),
          description: tool.description.unwrap_or_default(),
          input_schema: tool.input_schema,
        });
      }

      clients.insert(config.name.clone(), Arc::new(client));
    }

    // Cache all tools
    let mut cache = self.tools_cache.write().await;
    *cache = Some(all_tools);
    *initialized = true;

    Ok(())
  }

  /// Connect to a single MCP server based on its configuration.
  async fn connect_to_server(config: &McpServerEntry) -> McpResult<McpClient> {
    match config.transport.as_str() {
      "stdio" => {
        let command = config.command.as_ref().ok_or_else(|| {
          McpClientError::Config(format!(
            "Missing 'command' for stdio transport in '{}'",
            config.name
          ))
        })?;

        let args = config.args.clone().unwrap_or_default();
        let env = config.env.clone().unwrap_or_default();
        let timeout_secs = timeout_secs_from_ms(config.timeout_ms);

        McpClient::new_stdio(&config.name, command, &args, &env, timeout_secs)
          .await
          .map_err(McpClientError::Transport)
      }
      "http" => {
        let url = config.url.as_ref().ok_or_else(|| {
          McpClientError::Config(format!(
            "Missing 'url' for http transport in '{}'",
            config.name
          ))
        })?;

        let timeout_secs = timeout_secs_from_ms(config.timeout_ms);

        let bearer_token = match &config.auth_token_env {
          Some(env_name) => {
            let raw = std::env::var(env_name).map_err(|_| {
              McpClientError::Config(format!(
                "Missing auth token env var '{}' for MCP server '{}'",
                env_name, config.name
              ))
            })?;
            let token = raw.trim().to_string();
            if token.is_empty() {
              return Err(McpClientError::Config(format!(
                "Auth token env var '{}' is empty for MCP server '{}'",
                env_name, config.name
              )));
            }
            Some(token)
          }
          None => None,
        };

        let transport =
          Arc::new(AuthHttpTransport::new(url, timeout_secs, bearer_token)?);
        Ok(McpClient::with_transport(&config.name, transport))
      }
      other => Err(McpClientError::Config(format!(
        "Unknown transport '{}' for server '{}'",
        other, config.name
      ))),
    }
  }

  /// Get all available tools from all connected servers.
  /// Tool names are namespaced as "servername__toolname".
  pub async fn list_tools(&self) -> McpResult<Vec<McpRemoteTool>> {
    self.initialize().await?;
    self.get_cached_tools().await
  }

  /// Get cached tools without triggering initialization.
  /// Used by McpToolFactory when tools have already been discovered.
  pub async fn get_cached_tools(&self) -> McpResult<Vec<McpRemoteTool>> {
    let cache = self.tools_cache.read().await;
    let cached_tools = cache.as_ref().ok_or(McpClientError::NotInitialized)?;

    let clients = self.clients.read().await;
    let mut tools = Vec::new();

    for cached in cached_tools {
      let client = clients
        .get(&cached.server_name)
        .ok_or_else(|| McpClientError::ServerNotFound(cached.server_name.clone()))?;

      let tool_name = format!("{}__{}", cached.server_name, cached.remote_name);

      tools.push(McpRemoteTool {
        server_name: cached.server_name.clone(),
        tool_name,
        remote_name: cached.remote_name.clone(),
        description: cached.description.clone(),
        parameters: cached.input_schema.clone(),
        client: Arc::clone(client),
      });
    }

    Ok(tools)
  }

  /// Check if a tool name matches an MCP tool (has "__" separator).
  pub fn is_mcp_tool_name(name: &str) -> bool {
    name.contains("__")
  }

  /// Parse a namespaced tool name to extract server and tool names.
  /// Returns None if the name is not namespaced.
  pub fn parse_tool_name(name: &str) -> Option<(&str, &str)> {
    name.find("__").map(|pos| (&name[..pos], &name[pos + 2..]))
  }

  /// Call a tool by its namespaced name ("servername__toolname").
  pub async fn call_tool(
    &self,
    namespaced_name: &str,
    arguments: serde_json::Value,
  ) -> McpResult<ToolOutput> {
    self.initialize().await?;

    // Parse the namespaced name
    let (server_name, tool_name) = Self::parse_namespaced_name(namespaced_name)?;

    let clients = self.clients.read().await;
    let client = clients
      .get(server_name)
      .ok_or_else(|| McpClientError::ServerNotFound(server_name.to_string()))?;

    let result = client
      .call_tool(tool_name, arguments)
      .await
      .map_err(|e| McpClientError::Protocol(format!("Tool call failed: {}", e)))?;

    // Extract text from content blocks
    let text: String = result
      .content
      .iter()
      .filter_map(|block| block.as_text())
      .collect::<Vec<_>>()
      .join("\n");

    if result.is_error {
      Ok(ToolOutput::error(if text.is_empty() {
        "MCP tool returned error".to_string()
      } else {
        text
      }))
    } else {
      Ok(ToolOutput::llm_only(if text.is_empty() {
        "(no output)".to_string()
      } else {
        text
      }))
    }
  }

  /// Parse a namespaced tool name into (server_name, tool_name).
  pub fn parse_namespaced_name(namespaced: &str) -> McpResult<(&str, &str)> {
    match namespaced.find("__") {
      Some(pos) => {
        let server = &namespaced[..pos];
        let tool = &namespaced[pos + 2..];
        if server.is_empty() || tool.is_empty() {
          Err(McpClientError::ToolNotFound(namespaced.to_string()))
        } else {
          Ok((server, tool))
        }
      }
      None => Err(McpClientError::ToolNotFound(namespaced.to_string())),
    }
  }

  /// Disconnect from all servers and clean up resources.
  pub async fn disconnect(&self) -> McpResult<()> {
    let mut clients = self.clients.write().await;

    for (name, client) in clients.iter() {
      if let Err(e) = client.shutdown().await {
        tracing::warn!("Failed to shutdown MCP client for {}: {}", name, e);
      }
    }

    clients.clear();

    let mut cache = self.tools_cache.write().await;
    *cache = None;

    let mut initialized = self.initialized.write().await;
    *initialized = false;

    Ok(())
  }
}

impl Drop for McpClientSession {
  fn drop(&mut self) {
    // Best-effort cleanup on drop
    // Note: We can't use async drop, so this is a no-op for async cleanup
    // Proper cleanup should be done via disconnect() before dropping
  }
}

/// Manages MCP client sessions and connections to configured external MCP servers.
///
/// This is a singleton per runtime that:
/// - Reads `[[mcp.servers]]` from TOML config at startup
/// - Manages per-agent sessions (lazy creation, cleanup on agent terminate)
/// - Provides tool discovery for the McpToolFactory
pub struct McpClientManager {
  /// Server configurations from TOML.
  server_configs: Vec<McpServerEntry>,
  /// Active sessions by agent PID.
  sessions: Mutex<HashMap<AgentPid, Arc<McpClientSession>>>,
  /// Whether MCP is enabled (has server configurations).
  enabled: bool,
}

impl McpClientManager {
  /// Create a new MCP client manager from configuration.
  pub fn new(server_configs: Vec<McpServerEntry>) -> Self {
    let enabled = !server_configs.is_empty();
    Self {
      server_configs,
      sessions: Mutex::new(HashMap::new()),
      enabled,
    }
  }

  /// Create an empty manager with no servers (MCP disabled).
  pub fn empty() -> Self {
    Self {
      server_configs: Vec::new(),
      sessions: Mutex::new(HashMap::new()),
      enabled: false,
    }
  }

  /// Check if MCP is enabled (has server configurations).
  pub fn is_enabled(&self) -> bool {
    self.enabled
  }

  /// Get or create a session for the given agent.
  /// Sessions are created lazily on first use.
  pub async fn get_or_create_session(
    &self,
    agent_pid: AgentPid,
  ) -> McpResult<Arc<McpClientSession>> {
    if !self.enabled {
      return Err(McpClientError::Config("MCP is not configured".to_string()));
    }

    let mut sessions = self.sessions.lock().await;

    if let Some(session) = sessions.get(&agent_pid) {
      return Ok(Arc::clone(session));
    }

    let session = Arc::new(McpClientSession::new(
      agent_pid,
      self.server_configs.clone(),
    ));
    sessions.insert(agent_pid, Arc::clone(&session));

    Ok(session)
  }

  /// Get an existing session without creating a new one.
  pub async fn get_session(&self, agent_pid: AgentPid) -> Option<Arc<McpClientSession>> {
    let sessions = self.sessions.lock().await;
    sessions.get(&agent_pid).map(Arc::clone)
  }

  /// Close the session for the given agent (called on agent terminate).
  pub async fn close_session(&self, agent_pid: AgentPid) -> McpResult<()> {
    let mut sessions = self.sessions.lock().await;

    if let Some(session) = sessions.remove(&agent_pid) {
      session.disconnect().await?;
    }

    Ok(())
  }

  /// Get all available MCP tools for the given agent.
  /// This creates a session if one doesn't exist.
  pub async fn get_tools_for_agent(
    &self,
    agent_pid: AgentPid,
  ) -> McpResult<Vec<McpRemoteTool>> {
    let session = self.get_or_create_session(agent_pid).await?;
    session.list_tools().await
  }

  /// Synchronously try to get tools for an agent without creating a new session.
  /// Returns empty Vec if no session exists yet (lazy initialization).
  /// This is used by McpToolFactory which needs a synchronous interface.
  pub fn try_get_tools_for_agent(
    &self,
    agent_pid: AgentPid,
  ) -> McpResult<Vec<McpRemoteTool>> {
    // Check if we have a session without blocking
    let sessions = self.sessions.try_lock();
    match sessions {
      Ok(guard) => {
        if let Some(session) = guard.get(&agent_pid) {
          // We have a session, try to get cached tools
          // This spawns a blocking task to avoid async in sync context
          let rt = tokio::runtime::Handle::try_current();
          match rt {
            Ok(handle) => {
              // Use block_in_place for sync context to avoid deadlocks
              match tokio::task::block_in_place(|| {
                handle.block_on(session.get_cached_tools())
              }) {
                Ok(tools) => Ok(tools),
                Err(e) => Err(e),
              }
            }
            Err(_) => Err(McpClientError::NotInitialized),
          }
        } else {
          // No session yet - return empty list for lazy initialization
          Ok(Vec::new())
        }
      }
      Err(_) => {
        // Lock is held, session is being created - return empty for now
        Ok(Vec::new())
      }
    }
  }

  /// Get the list of configured server names.
  pub fn server_names(&self) -> Vec<String> {
    self.server_configs.iter().map(|s| s.name.clone()).collect()
  }

  /// Get the server configurations.
  pub fn server_configs(&self) -> &[McpServerEntry] {
    &self.server_configs
  }

  /// Shutdown all sessions and clean up.
  pub async fn shutdown(&self) -> McpResult<()> {
    let mut sessions = self.sessions.lock().await;

    for (pid, session) in sessions.iter() {
      if let Err(e) = session.disconnect().await {
        tracing::warn!("Failed to disconnect session for agent {:?}: {}", pid, e);
      }
    }

    sessions.clear();
    Ok(())
  }
}

/// Wrapper for remote MCP tools that implements the zeptoclaw Tool trait.
///
/// Tool names are namespaced as "servername__toolname" to prevent collisions
/// with local tools.
#[derive(Clone)]
pub struct McpRemoteTool {
  server_name: String,
  tool_name: String,   // Namespaced: "servername__toolname"
  remote_name: String, // Original name on the server
  description: String,
  parameters: serde_json::Value,
  client: Arc<McpClient>,
}

impl McpRemoteTool {
  /// Get the server name.
  pub fn server_name(&self) -> &str {
    &self.server_name
  }

  /// Get the namespaced tool name.
  pub fn tool_name(&self) -> &str {
    &self.tool_name
  }

  /// Get the remote (unprefixed) tool name.
  pub fn remote_name(&self) -> &str {
    &self.remote_name
  }

  /// Get the tool description.
  pub fn description(&self) -> &str {
    &self.description
  }

  /// Get the tool parameters schema.
  pub fn parameters(&self) -> &serde_json::Value {
    &self.parameters
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

  fn compact_description(&self) -> &str {
    &self.description
  }

  fn category(&self) -> ToolCategory {
    ToolCategory::NetworkWrite
  }

  fn parameters(&self) -> serde_json::Value {
    self.parameters.clone()
  }

  async fn execute(
    &self,
    args: serde_json::Value,
    _ctx: &ToolContext,
  ) -> zeptoclaw::error::Result<ToolOutput> {
    let result = self
      .client
      .call_tool(&self.remote_name, args)
      .await
      .map_err(|e| zeptoclaw::error::ZeptoError::Mcp(e))?;

    // Extract text from content blocks
    let text: String = result
      .content
      .iter()
      .filter_map(|block| block.as_text())
      .collect::<Vec<_>>()
      .join("\n");

    if result.is_error {
      Ok(ToolOutput::error(if text.is_empty() {
        "MCP tool returned error".to_string()
      } else {
        text
      }))
    } else {
      Ok(ToolOutput::llm_only(if text.is_empty() {
        "(no output)".to_string()
      } else {
        text
      }))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_namespaced_name_valid() {
    let result = McpClientSession::parse_namespaced_name("github__create_issue");
    assert!(result.is_ok());
    let (server, tool) = result.unwrap();
    assert_eq!(server, "github");
    assert_eq!(tool, "create_issue");
  }

  #[test]
  fn test_parse_namespaced_name_multiple_underscores() {
    let result = McpClientSession::parse_namespaced_name("my_server__some_tool");
    assert!(result.is_ok());
    let (server, tool) = result.unwrap();
    assert_eq!(server, "my_server");
    assert_eq!(tool, "some_tool");
  }

  #[test]
  fn test_parse_namespaced_name_no_separator() {
    let result = McpClientSession::parse_namespaced_name("tool_without_namespace");
    assert!(result.is_err());
  }

  #[test]
  fn test_parse_namespaced_name_empty_server() {
    let result = McpClientSession::parse_namespaced_name("__tool");
    assert!(result.is_err());
  }

  #[test]
  fn test_parse_namespaced_name_empty_tool() {
    let result = McpClientSession::parse_namespaced_name("server__");
    assert!(result.is_err());
  }

  #[test]
  fn test_mcp_client_error_display() {
    let err = McpClientError::ServerNotFound("test".to_string());
    assert_eq!(err.to_string(), "MCP server 'test' not found");

    let err = McpClientError::Transport("connection failed".to_string());
    assert_eq!(err.to_string(), "MCP transport error: connection failed");

    let err = McpClientError::NotInitialized;
    assert_eq!(err.to_string(), "MCP session not initialized");
  }

  #[test]
  fn test_mcp_client_manager_empty() {
    let manager = McpClientManager::empty();
    assert!(!manager.is_enabled());
    assert!(manager.server_names().is_empty());
  }

  #[test]
  fn test_mcp_client_manager_with_servers() {
    let configs = vec![McpServerEntry {
      name: "github".to_string(),
      transport: "stdio".to_string(),
      command: Some("npx".to_string()),
      args: Some(
        vec!["-y", "@modelcontextprotocol/server-github"]
          .into_iter()
          .map(String::from)
          .collect(),
      ),
      env: None,
      url: None,
      auth_token_env: None,
      timeout_ms: None,
    }];
    let manager = McpClientManager::new(configs);
    assert!(manager.is_enabled());
    assert_eq!(manager.server_names(), vec!["github"]);
  }

  #[tokio::test]
  async fn test_session_lifecycle() {
    let manager = McpClientManager::empty();
    let pid = AgentPid::new();

    // Should fail because MCP is not configured
    let result = manager.get_or_create_session(pid).await;
    assert!(result.is_err());
  }

  #[test]
  fn test_timeout_secs_from_ms_rounds_up() {
    assert_eq!(timeout_secs_from_ms(None), 30);
    assert_eq!(timeout_secs_from_ms(Some(0)), 1);
    assert_eq!(timeout_secs_from_ms(Some(1)), 1);
    assert_eq!(timeout_secs_from_ms(Some(999)), 1);
    assert_eq!(timeout_secs_from_ms(Some(1000)), 1);
    assert_eq!(timeout_secs_from_ms(Some(1001)), 2);
  }

  #[tokio::test]
  async fn test_http_auth_env_missing_fails_fast() {
    let cfg = McpServerEntry {
      name: "remote".to_string(),
      transport: "http".to_string(),
      command: None,
      args: None,
      env: None,
      url: Some("http://127.0.0.1:9/mcp".to_string()),
      auth_token_env: Some("ZEPTO_TEST_MISSING_TOKEN".to_string()),
      timeout_ms: Some(1000),
    };

    std::env::remove_var("ZEPTO_TEST_MISSING_TOKEN");

    let manager = McpClientManager::new(vec![cfg]);
    let result = manager.get_tools_for_agent(AgentPid::new()).await;

    match result {
      Err(McpClientError::Config(msg)) => {
        assert!(msg.contains("Missing auth token env var"));
      }
      _ => panic!("Expected Config error"),
    }
  }
}
