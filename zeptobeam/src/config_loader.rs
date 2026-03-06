//! Config loader that converts legacy `AppConfig` / `AgentConfig` values
//! from the erlangrt crate into `AgentAdapterConfig` values for the zeptovm
//! runtime.
//!
//! This is a pure, synchronous module — no async needed.

use crate::agent_adapter::AgentAdapterConfig;
use erlangrt::agent_rt::config::{AgentConfig, AppConfig};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Error type for config loading / validation failures.
#[derive(Debug, Clone)]
pub struct ConfigError {
  pub agent_name: String,
  pub message: String,
}

impl std::fmt::Display for ConfigError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "agent '{}': {}", self.agent_name, self.message)
  }
}

impl std::error::Error for ConfigError {}

/// Result of resolving agent configs from TOML.
#[derive(Debug)]
pub struct ResolvedAgents {
  pub agents: Vec<AgentAdapterConfig>,
  pub config_hash: u64,
}

/// Convert a legacy `AgentConfig` to an `AgentAdapterConfig`.
fn convert_agent(cfg: &AgentConfig) -> AgentAdapterConfig {
  AgentAdapterConfig {
    name: cfg.name.clone(),
    provider: cfg.provider.clone(),
    model: cfg.model.clone(),
    system_prompt: cfg.system_prompt.clone(),
    tools: cfg.tools.clone(),
    max_iterations: cfg.max_iterations,
    timeout_ms: cfg.timeout_ms,
  }
}

/// Validate a single agent config. Returns a list of validation errors.
fn validate_agent(cfg: &AgentConfig) -> Vec<ConfigError> {
  let mut errors = Vec::new();
  let name = if cfg.name.is_empty() {
    "<unnamed>"
  } else {
    &cfg.name
  };

  if cfg.name.trim().is_empty() {
    errors.push(ConfigError {
      agent_name: name.to_string(),
      message: "name must not be empty".into(),
    });
  }
  if cfg.provider.trim().is_empty() {
    errors.push(ConfigError {
      agent_name: name.to_string(),
      message: "provider must not be empty".into(),
    });
  }
  if let Some(max) = cfg.max_iterations {
    if max == 0 {
      errors.push(ConfigError {
        agent_name: name.to_string(),
        message: "max_iterations must be > 0".into(),
      });
    }
  }
  errors
}

/// Compute a simple hash of the agent configs for change detection.
fn compute_config_hash(agents: &[AgentConfig]) -> u64 {
  let mut hasher = DefaultHasher::new();
  for agent in agents {
    agent.name.hash(&mut hasher);
    agent.provider.hash(&mut hasher);
    agent.model.hash(&mut hasher);
    agent.system_prompt.hash(&mut hasher);
    agent.tools.hash(&mut hasher);
    agent.auto_start.hash(&mut hasher);
    agent.max_iterations.hash(&mut hasher);
    agent.timeout_ms.hash(&mut hasher);
  }
  hasher.finish()
}

/// Resolve agents from config:
/// 1. Validate all agents (fail-fast on errors)
/// 2. Filter to `auto_start = true` agents
/// 3. Convert to `AgentAdapterConfig`
/// 4. Compute config hash for change detection
pub fn resolve_agents(
  agents: &[AgentConfig],
) -> Result<ResolvedAgents, Vec<ConfigError>> {
  // Validate ALL agents first (not just auto_start ones)
  let errors: Vec<ConfigError> =
    agents.iter().flat_map(validate_agent).collect();
  if !errors.is_empty() {
    return Err(errors);
  }

  let config_hash = compute_config_hash(agents);

  let resolved: Vec<AgentAdapterConfig> = agents
    .iter()
    .filter(|a| a.auto_start)
    .map(convert_agent)
    .collect();

  Ok(ResolvedAgents {
    agents: resolved,
    config_hash,
  })
}

/// Convenience: resolve agents from the full `AppConfig`.
pub fn resolve_agents_from_config(
  config: &AppConfig,
) -> Result<ResolvedAgents, Vec<ConfigError>> {
  resolve_agents(&config.agents)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Helper: build an `AgentConfig` with sensible defaults.
  fn make_agent(
    name: &str,
    provider: &str,
    auto_start: bool,
  ) -> AgentConfig {
    AgentConfig {
      name: name.into(),
      provider: provider.into(),
      model: Some("test-model".into()),
      system_prompt: Some("You are a helper.".into()),
      tools: vec!["tool_a".into()],
      auto_start,
      max_iterations: Some(10),
      timeout_ms: Some(30000),
    }
  }

  #[test]
  fn test_resolve_empty_agents() {
    let result = resolve_agents(&[]).unwrap();
    assert!(result.agents.is_empty());
  }

  #[test]
  fn test_resolve_auto_start_only() {
    let agents = vec![
      make_agent("alpha", "openai", true),
      make_agent("beta", "anthropic", false),
    ];
    let result = resolve_agents(&agents).unwrap();
    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].name, "alpha");
  }

  #[test]
  fn test_resolve_all_manual() {
    let agents = vec![
      make_agent("alpha", "openai", false),
      make_agent("beta", "anthropic", false),
    ];
    let result = resolve_agents(&agents).unwrap();
    assert!(result.agents.is_empty());
  }

  #[test]
  fn test_validate_empty_name() {
    let agents = vec![AgentConfig {
      name: "".into(),
      provider: "openai".into(),
      model: None,
      system_prompt: None,
      tools: vec![],
      auto_start: false,
      max_iterations: None,
      timeout_ms: None,
    }];
    let err = resolve_agents(&agents).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("name")));
  }

  #[test]
  fn test_validate_empty_provider() {
    let agents = vec![AgentConfig {
      name: "good-name".into(),
      provider: "  ".into(),
      model: None,
      system_prompt: None,
      tools: vec![],
      auto_start: false,
      max_iterations: None,
      timeout_ms: None,
    }];
    let err = resolve_agents(&agents).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("provider")));
  }

  #[test]
  fn test_validate_zero_max_iterations() {
    let agents = vec![AgentConfig {
      name: "agent1".into(),
      provider: "openai".into(),
      model: None,
      system_prompt: None,
      tools: vec![],
      auto_start: true,
      max_iterations: Some(0),
      timeout_ms: None,
    }];
    let err = resolve_agents(&agents).unwrap_err();
    assert!(err.iter().any(|e| e.message.contains("max_iterations")));
  }

  #[test]
  fn test_config_hash_deterministic() {
    let agents = vec![make_agent("a", "p", true)];
    let h1 = compute_config_hash(&agents);
    let h2 = compute_config_hash(&agents);
    assert_eq!(h1, h2);
  }

  #[test]
  fn test_config_hash_changes_on_mutation() {
    let agents_a = vec![make_agent("a", "p", true)];
    let agents_b = vec![make_agent("b", "p", true)];
    let h1 = compute_config_hash(&agents_a);
    let h2 = compute_config_hash(&agents_b);
    assert_ne!(h1, h2);
  }

  #[test]
  fn test_convert_preserves_all_fields() {
    let cfg = AgentConfig {
      name: "researcher".into(),
      provider: "openrouter".into(),
      model: Some("claude-sonnet-4".into()),
      system_prompt: Some("Be helpful.".into()),
      tools: vec!["web".into(), "fs".into()],
      auto_start: true,
      max_iterations: Some(5),
      timeout_ms: Some(60000),
    };
    let adapted = convert_agent(&cfg);
    assert_eq!(adapted.name, "researcher");
    assert_eq!(adapted.provider, "openrouter");
    assert_eq!(adapted.model, Some("claude-sonnet-4".into()));
    assert_eq!(adapted.system_prompt, Some("Be helpful.".into()));
    assert_eq!(adapted.tools, vec!["web", "fs"]);
    assert_eq!(adapted.max_iterations, Some(5));
    assert_eq!(adapted.timeout_ms, Some(60000));
  }

  #[test]
  fn test_resolve_from_app_config() {
    let mut config = AppConfig::default();
    config.agents = vec![
      make_agent("daemon-agent", "openai", true),
      make_agent("manual-agent", "anthropic", false),
    ];
    let result = resolve_agents_from_config(&config).unwrap();
    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].name, "daemon-agent");
    assert_eq!(result.agents[0].provider, "openai");
  }
}
