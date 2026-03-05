use std::collections::HashSet;

use zeptoclaw::tools::{
  filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool},
  shell::ShellTool,
  web::{DdgSearchTool, SearxngSearchTool, WebFetchTool, WebSearchTool},
  GitTool, HttpRequestTool, Tool,
};

/// Builds tool sets for ZeptoAgent instances.
/// Implementations know about available tools and filter
/// by an optional whitelist.
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
pub struct DefaultToolFactory;

impl DefaultToolFactory {
  pub fn from_env() -> Self {
    Self
  }

  fn supported_tool_names() -> &'static [&'static str] {
    &[
      "read_file",
      "write_file",
      "list_dir",
      "edit_file",
      "shell",
      "git",
      "web_search",
      "web_fetch",
      "http_request",
    ]
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
      None => {
        let mut out = Vec::new();
        for name in Self::supported_tool_names() {
          if let Some(tool) = self.build_tool_by_name(name) {
            out.push(tool);
          }
        }
        out
      }
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
  use super::{DefaultToolFactory, ToolFactory};

  #[test]
  fn test_default_tool_factory_builds_core_tools() {
    let factory = DefaultToolFactory::from_env();
    let tools = factory.build_tools(None);
    let names: Vec<String> = tools.into_iter().map(|t| t.name().to_string()).collect();

    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"write_file".to_string()));
    assert!(names.contains(&"list_dir".to_string()));
    assert!(names.contains(&"edit_file".to_string()));
    assert!(names.contains(&"shell".to_string()));
    assert!(names.contains(&"web_search".to_string()));
    assert!(names.contains(&"web_fetch".to_string()));
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
}
