use std::collections::HashMap;

use serde::Deserialize;

use crate::agent_rt::error::AgentRtError;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
  pub runtime: RuntimeConfig,
  pub checkpoint: CheckpointConfig,
  pub server: ServerConfig,
  pub logging: LogConfig,
  pub orchestration: OrchestrationConfig,
  pub mcp: McpConfig,
}

/// MCP configuration section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpConfig {
  /// MCP server configuration (for exposing zeptoclaw as MCP server).
  pub server: McpServerConfig,
  /// Stdio transport configuration.
  pub stdio: McpStdioConfig,
  /// External MCP servers to connect to.
  pub servers: Vec<McpServerEntry>,
}

/// MCP server configuration (for exposing zeptoclaw as MCP server).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
  pub enabled: bool,
  pub auth_token_env: Option<String>,
  pub session_timeout_secs: u64,
}

/// MCP stdio transport configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpStdioConfig {
  pub enabled: bool,
}

/// External MCP server entry for [[mcp.servers]] config.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerEntry {
  /// Server name (used for tool namespacing: "servername__toolname").
  pub name: String,
  /// Transport type: "stdio" or "http".
  pub transport: String,
  /// Command to spawn (for stdio transport).
  pub command: Option<String>,
  /// Arguments for the command (for stdio transport).
  pub args: Option<Vec<String>>,
  /// Environment variables to set (for stdio transport).
  pub env: Option<HashMap<String, String>>,
  /// URL for the MCP server (for http transport).
  pub url: Option<String>,
  /// Environment variable name containing auth token (for http transport).
  pub auth_token_env: Option<String>,
  /// Timeout in milliseconds (for http transport).
  pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
  pub worker_count: usize,
  pub mailbox_capacity: usize,
  pub max_reductions: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CheckpointConfig {
  pub store: String,
  pub path: String,
  pub ttl_hours: u64,
  pub prune_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
  pub enabled: bool,
  pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
  pub level: String,
  pub format: String,
}

impl Default for AppConfig {
  fn default() -> Self {
    Self {
      runtime: RuntimeConfig::default(),
      checkpoint: CheckpointConfig::default(),
      server: ServerConfig::default(),
      logging: LogConfig::default(),
      orchestration: OrchestrationConfig::default(),
      mcp: McpConfig::default(),
    }
  }
}

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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrchestrationConfig {
  pub max_concurrency: usize,
  pub max_orchestration_depth: usize,
  pub default_retry: String,
  pub budget: BudgetConfig,
  pub aggregator: AggregatorConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
  pub max_tokens: u64,
  pub max_cost_usd: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AggregatorConfig {
  pub strategy: String,
}

impl Default for OrchestrationConfig {
  fn default() -> Self {
    Self {
      max_concurrency: 4,
      max_orchestration_depth: 3,
      default_retry: "none".into(),
      budget: BudgetConfig::default(),
      aggregator: AggregatorConfig::default(),
    }
  }
}

impl Default for BudgetConfig {
  fn default() -> Self {
    Self {
      max_tokens: 0,
      max_cost_usd: 0.0,
    }
  }
}

impl Default for AggregatorConfig {
  fn default() -> Self {
    Self {
      strategy: "concat".into(),
    }
  }
}

impl Default for RuntimeConfig {
  fn default() -> Self {
    Self {
      worker_count: 4,
      mailbox_capacity: 1024,
      max_reductions: 200,
    }
  }
}

impl Default for CheckpointConfig {
  fn default() -> Self {
    Self {
      store: "sqlite".into(),
      path: "./zeptobeam.db".into(),
      ttl_hours: 24,
      prune_interval_secs: 3600,
    }
  }
}

impl Default for ServerConfig {
  fn default() -> Self {
    Self {
      enabled: true,
      bind: "127.0.0.1:9090".into(),
    }
  }
}

impl Default for LogConfig {
  fn default() -> Self {
    Self {
      level: "info".into(),
      format: "pretty".into(),
    }
  }
}

pub fn load_config_from_str(s: &str) -> Result<AppConfig, AgentRtError> {
  toml::from_str(s).map_err(|e| AgentRtError::Config(format!("parse TOML: {}", e)))
}

pub fn load_config(path: &str) -> Result<AppConfig, AgentRtError> {
  let content = std::fs::read_to_string(path)
    .map_err(|e| AgentRtError::Config(format!("read config file {}: {}", path, e)))?;
  load_config_from_str(&content)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_config() {
    let config = AppConfig::default();
    assert_eq!(config.runtime.worker_count, 4);
    assert_eq!(config.runtime.mailbox_capacity, 1024);
    assert_eq!(config.checkpoint.store, "sqlite");
    assert_eq!(config.checkpoint.ttl_hours, 24);
    assert_eq!(config.server.bind, "127.0.0.1:9090");
    assert!(config.server.enabled);
    assert_eq!(config.logging.level, "info");
    assert_eq!(config.logging.format, "pretty");
  }

  #[test]
  fn test_parse_toml_config() {
    let toml_str = r#"
[runtime]
worker_count = 8
mailbox_capacity = 2048

[checkpoint]
store = "file"
path = "/tmp/checkpoints"

[server]
enabled = false
bind = "0.0.0.0:8080"

[logging]
level = "debug"
format = "json"
"#;
    let config = load_config_from_str(toml_str).unwrap();
    assert_eq!(config.runtime.worker_count, 8);
    assert_eq!(config.runtime.mailbox_capacity, 2048);
    assert_eq!(config.checkpoint.store, "file");
    assert!(!config.server.enabled);
    assert_eq!(config.logging.level, "debug");
    assert_eq!(config.logging.format, "json");
  }

  #[test]
  fn test_partial_toml_uses_defaults() {
    let toml_str = r#"
[runtime]
worker_count = 16
"#;
    let config = load_config_from_str(toml_str).unwrap();
    assert_eq!(config.runtime.worker_count, 16);
    // Everything else should be default
    assert_eq!(config.runtime.mailbox_capacity, 1024);
    assert_eq!(config.checkpoint.store, "sqlite");
    assert!(config.server.enabled);
  }

  #[test]
  fn test_empty_toml_gives_defaults() {
    let config = load_config_from_str("").unwrap();
    assert_eq!(config.runtime.worker_count, 4);
  }

  #[test]
  fn test_invalid_toml_returns_error() {
    let result = load_config_from_str("not [valid toml {{{}");
    assert!(result.is_err());
  }

  #[test]
  fn test_orchestration_config_defaults() {
    let config = AppConfig::default();
    assert_eq!(config.orchestration.max_concurrency, 4);
    assert_eq!(config.orchestration.max_orchestration_depth, 3);
    assert_eq!(config.orchestration.default_retry, "none");
    assert_eq!(config.orchestration.budget.max_tokens, 0);
    assert_eq!(config.orchestration.aggregator.strategy, "concat");
  }

  #[test]
  fn test_orchestration_config_from_toml() {
    let toml_str = r#"
[orchestration]
max_concurrency = 8
default_retry = "backoff:3:1000:30000"

[orchestration.budget]
max_tokens = 100000
max_cost_usd = 5.0

[orchestration.aggregator]
strategy = "merge"
"#;
    let config = load_config_from_str(toml_str).unwrap();
    assert_eq!(config.orchestration.max_concurrency, 8);
    assert_eq!(config.orchestration.budget.max_tokens, 100000);
    assert_eq!(config.orchestration.budget.max_cost_usd, 5.0);
    assert_eq!(config.orchestration.aggregator.strategy, "merge");
  }

  #[test]
  fn test_default_mcp_config() {
    let config = AppConfig::default();
    assert!(!config.mcp.server.enabled);
    assert!(!config.mcp.stdio.enabled);
    assert!(config.mcp.servers.is_empty());
    assert_eq!(config.mcp.server.session_timeout_secs, 3600);
    assert!(config.mcp.server.auth_token_env.is_none());
  }

  #[test]
  fn test_parse_mcp_config() {
    let toml_str = r#"
[mcp.server]
enabled = true
auth_token_env = "ZEPTOCLAW_MCP_TOKEN"
session_timeout_secs = 1800

[mcp.stdio]
enabled = false

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[[mcp.servers]]
name = "database"
transport = "http"
url = "http://localhost:8080/mcp"
auth_token_env = "DB_MCP_TOKEN"
timeout_ms = 30000
"#;
    let config = load_config_from_str(toml_str).unwrap();

    // MCP server config
    assert!(config.mcp.server.enabled);
    assert_eq!(
      config.mcp.server.auth_token_env,
      Some("ZEPTOCLAW_MCP_TOKEN".to_string())
    );
    assert_eq!(config.mcp.server.session_timeout_secs, 1800);

    // MCP stdio config
    assert!(!config.mcp.stdio.enabled);

    // MCP servers
    assert_eq!(config.mcp.servers.len(), 2);

    // First server (stdio)
    let github = &config.mcp.servers[0];
    assert_eq!(github.name, "github");
    assert_eq!(github.transport, "stdio");
    assert_eq!(github.command, Some("npx".to_string()));
    assert_eq!(
      github.args,
      Some(vec![
        "-y".to_string(),
        "@modelcontextprotocol/server-github".to_string()
      ])
    );
    assert!(github.env.is_none());

    // Second server (http)
    let db = &config.mcp.servers[1];
    assert_eq!(db.name, "database");
    assert_eq!(db.transport, "http");
    assert_eq!(db.url, Some("http://localhost:8080/mcp".to_string()));
    assert_eq!(db.auth_token_env, Some("DB_MCP_TOKEN".to_string()));
    assert_eq!(db.timeout_ms, Some(30000));
  }

  #[test]
  fn test_parse_mcp_servers_with_env() {
    let toml_str = r#"
[[mcp.servers]]
name = "filesystem"
transport = "stdio"
command = "node"
args = ["server.js"]
[mcp.servers.env]
ROOT_PATH = "/tmp"
"#;
    let config = load_config_from_str(toml_str).unwrap();
    assert_eq!(config.mcp.servers.len(), 1);

    let fs = &config.mcp.servers[0];
    assert_eq!(fs.name, "filesystem");
    assert!(fs.env.is_some());
    let env = fs.env.as_ref().unwrap();
    assert_eq!(env.get("ROOT_PATH"), Some(&"/tmp".to_string()));
  }
}
