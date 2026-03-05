use std::{collections::HashSet, sync::Arc};

use zeptoclaw::tools::{
  filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool},
  shell::ShellTool,
  web::{DdgSearchTool, SearxngSearchTool, WebFetchTool, WebSearchTool},
  GitTool, HttpRequestTool, Tool,
};

use crate::agent_rt::{mcp_client::McpClientManager, types::AgentPid};

/// Builds tool sets for ZeptoAgent instances.
/// Implementations know about available tools and filter
/// by an optional whitelist.
///
/// Safety contract: `whitelist == None` must return no tools.
pub trait ToolFactory: Send + Sync {
  fn build_tools(&self, whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>>;
}

/// Concrete tool factory for AgentChat sessions.
///
/// This factory builds a pragmatic default toolset:
/// - Filesystem: `read_file`, `write_file`, `list_dir`, `edit_file`
/// - Runtime: `shell`
/// - Git: `git` (if available on PATH)
/// - Web: `web_search` (Brave/SearXNG/DDG fallback), `web_fetch`
/// - Optional: `http_request` (when allowlist env is configured)
///
/// Tools are only enabled when explicitly requested via whitelist.
pub struct DefaultToolFactory;

/// MCP-aware tool factory that wraps DefaultToolFactory and adds MCP remote tools.
///
/// This factory extends the default toolset with tools from configured external
/// MCP servers. MCP tools are namespaced as "servername__toolname".
pub struct McpToolFactory {
  inner: DefaultToolFactory,
  mcp_manager: Arc<McpClientManager>,
  agent_pid: AgentPid,
}

impl McpToolFactory {
  /// Create a new MCP tool factory.
  pub fn new(
    inner: DefaultToolFactory,
    mcp_manager: Arc<McpClientManager>,
    agent_pid: AgentPid,
  ) -> Self {
    Self {
      inner,
      mcp_manager,
      agent_pid,
    }
  }

  /// Create a new MCP tool factory from environment.
  pub fn from_env(mcp_manager: Arc<McpClientManager>, agent_pid: AgentPid) -> Self {
    Self::new(DefaultToolFactory::from_env(), mcp_manager, agent_pid)
  }

  /// Get the inner DefaultToolFactory.
  pub fn inner(&self) -> &DefaultToolFactory {
    &self.inner
  }

  /// Get the MCP client manager.
  pub fn mcp_manager(&self) -> &McpClientManager {
    &self.mcp_manager
  }

  /// Get the agent PID.
  pub fn agent_pid(&self) -> AgentPid {
    self.agent_pid
  }
}

impl ToolFactory for McpToolFactory {
  fn build_tools(&self, whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>> {
    // Build local tools from inner factory
    // Filter out MCP tool names (those containing "__")
    let local_whitelist = whitelist.map(|names| {
      names
        .iter()
        .filter(|n| !n.contains("__"))
        .cloned()
        .collect::<Vec<_>>()
    });

    let mut tools = self.inner.build_tools(local_whitelist.as_deref());

    // If MCP is enabled and whitelist contains MCP tools, add them
    if self.mcp_manager.is_enabled() {
      if let Some(names) = whitelist {
        let mcp_tool_names: Vec<&str> = names
          .iter()
          .filter(|n| n.contains("__"))
          .map(|n| n.as_str())
          .collect();

        if !mcp_tool_names.is_empty() {
          // Try to get tools from the session
          // This is synchronous, so we use try_get_tools_for_agent which returns
          // immediately if no session exists (lazy initialization on first use)
          if let Ok(mcp_tools) = self.mcp_manager.try_get_tools_for_agent(self.agent_pid)
          {
            for tool in mcp_tools {
              if mcp_tool_names.contains(&tool.name()) {
                tools.push(Box::new(tool));
              }
            }
          }
        }
      }
    }

    tools
  }
}

impl DefaultToolFactory {
  pub fn from_env() -> Self {
    Self
  }

  fn build_tool_by_name(&self, name: &str) -> Option<Box<dyn Tool>> {
    match name {
      "read_file" => Some(Box::new(ReadFileTool)),
      "write_file" => Some(Box::new(WriteFileTool)),
      "list_dir" => Some(Box::new(ListDirTool)),
      "edit_file" => Some(Box::new(EditFileTool)),
      "shell" => Some(Box::new(ShellTool::new())),
      "git" => {
        if GitTool::is_available() {
          Some(Box::new(GitTool::new()))
        } else {
          None
        }
      }
      "web_search" => Some(self.build_web_search_tool()),
      "web_fetch" => Some(Box::new(WebFetchTool::new())),
      "http_request" => self.build_http_request_tool(),
      _ => None,
    }
  }

  fn build_web_search_tool(&self) -> Box<dyn Tool> {
    let max_results = std::env::var("ZEPTOCLAW_TOOLS_WEB_SEARCH_MAX_RESULTS")
      .ok()
      .and_then(|v| v.parse::<usize>().ok())
      .unwrap_or(5)
      .clamp(1, 10);

    let provider = std::env::var("ZEPTOCLAW_TOOLS_WEB_SEARCH_PROVIDER")
      .ok()
      .map(|s| s.trim().to_ascii_lowercase())
      .filter(|s| !s.is_empty());

    match provider.as_deref() {
      Some("searxng") => {
        if let Some(url) = std::env::var("ZEPTOCLAW_TOOLS_WEB_SEARCH_API_URL")
          .ok()
          .map(|s| s.trim().to_string())
          .filter(|s| !s.is_empty())
        {
          if let Ok(tool) = SearxngSearchTool::with_max_results(&url, max_results) {
            return Box::new(tool);
          }
        }
        Box::new(DdgSearchTool::with_max_results(max_results))
      }
      Some("brave") => {
        if let Some(key) = brave_api_key() {
          return Box::new(WebSearchTool::with_max_results(&key, max_results));
        }
        Box::new(DdgSearchTool::with_max_results(max_results))
      }
      Some("ddg") => Box::new(DdgSearchTool::with_max_results(max_results)),
      Some(_) | None => {
        if let Some(url) = std::env::var("ZEPTOCLAW_TOOLS_WEB_SEARCH_API_URL")
          .ok()
          .map(|s| s.trim().to_string())
          .filter(|s| !s.is_empty())
        {
          if let Ok(tool) = SearxngSearchTool::with_max_results(&url, max_results) {
            return Box::new(tool);
          }
        }
        if let Some(key) = brave_api_key() {
          return Box::new(WebSearchTool::with_max_results(&key, max_results));
        }
        Box::new(DdgSearchTool::with_max_results(max_results))
      }
    }
  }

  fn build_http_request_tool(&self) -> Option<Box<dyn Tool>> {
    let allowed_domains = parse_csv_env("ZEPTOCLAW_TOOLS_HTTP_ALLOWED_DOMAINS");
    if allowed_domains.is_empty() {
      return None;
    }
    let timeout_secs = std::env::var("ZEPTOCLAW_TOOLS_HTTP_TIMEOUT_SECS")
      .ok()
      .and_then(|v| v.parse::<u64>().ok())
      .unwrap_or(30);
    let max_response_bytes = std::env::var("ZEPTOCLAW_TOOLS_HTTP_MAX_RESPONSE_BYTES")
      .ok()
      .and_then(|v| v.parse::<usize>().ok())
      .unwrap_or(200_000);
    Some(Box::new(HttpRequestTool::new(
      allowed_domains,
      timeout_secs,
      max_response_bytes,
    )))
  }
}

impl ToolFactory for DefaultToolFactory {
  fn build_tools(&self, whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>> {
    match whitelist {
      Some(names) => {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for name in names {
          let normalized = name.trim().to_ascii_lowercase();
          if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
          }
          if let Some(tool) = self.build_tool_by_name(&normalized) {
            out.push(tool);
          }
        }
        out
      }
      None => Vec::new(),
    }
  }
}

fn brave_api_key() -> Option<String> {
  for key in [
    "ZEPTOCLAW_TOOLS_WEB_SEARCH_API_KEY",
    "ZEPTOCLAW_INTEGRATIONS_BRAVE_API_KEY",
    "BRAVE_API_KEY",
  ] {
    if let Ok(value) = std::env::var(key) {
      let value = value.trim();
      if !value.is_empty() {
        return Some(value.to_string());
      }
    }
  }
  None
}

fn parse_csv_env(key: &str) -> Vec<String> {
  std::env::var(key)
    .ok()
    .map(|v| {
      v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::{DefaultToolFactory, McpToolFactory, ToolFactory};
  use crate::agent_rt::{mcp_client::McpClientManager, types::AgentPid};

  #[test]
  fn test_default_tool_factory_returns_empty_when_no_whitelist() {
    let factory = DefaultToolFactory::from_env();
    let tools = factory.build_tools(None);
    assert!(tools.is_empty());
  }

  #[test]
  fn test_default_tool_factory_honors_whitelist() {
    let factory = DefaultToolFactory::from_env();
    let tools = factory.build_tools(Some(&[
      "web_search".to_string(),
      "shell".to_string(),
      "unknown_tool".to_string(),
      "web_search".to_string(),
    ]));
    let names: Vec<String> = tools.into_iter().map(|t| t.name().to_string()).collect();

    assert_eq!(names, vec!["web_search".to_string(), "shell".to_string()]);
  }

  #[test]
  fn test_mcp_tool_factory_disabled_when_empty() {
    let inner = DefaultToolFactory::from_env();
    let mcp_manager = std::sync::Arc::new(McpClientManager::empty());
    let factory = McpToolFactory::new(inner, mcp_manager, AgentPid::new());

    assert!(!factory.mcp_manager().is_enabled());

    // Should still return local tools
    let tools = factory.build_tools(Some(&["shell".to_string()]));
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "shell");
  }

  #[test]
  fn test_mcp_tool_factory_returns_local_tools() {
    let inner = DefaultToolFactory::from_env();
    let mcp_manager = std::sync::Arc::new(McpClientManager::empty());
    let factory = McpToolFactory::new(inner, mcp_manager, AgentPid::new());

    let tools =
      factory.build_tools(Some(&["read_file".to_string(), "write_file".to_string()]));

    assert_eq!(tools.len(), 2);
    let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"write_file".to_string()));
  }
}
