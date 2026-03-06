/// Configuration for LLM and HTTP effect providers.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
  pub openai_api_key: Option<String>,
  pub openai_base_url: String,
  pub anthropic_api_key: Option<String>,
  pub anthropic_base_url: String,
}

impl Default for ProviderConfig {
  fn default() -> Self {
    Self {
      openai_api_key: None,
      openai_base_url: "https://api.openai.com/v1".into(),
      anthropic_api_key: None,
      anthropic_base_url: "https://api.anthropic.com/v1".into(),
    }
  }
}

impl ProviderConfig {
  pub fn from_env() -> Self {
    Self {
      openai_api_key: std::env::var("OPENAI_API_KEY").ok(),
      openai_base_url: std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
      anthropic_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
      anthropic_base_url: std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/v1".into()),
    }
  }

  pub fn has_any_provider(&self) -> bool {
    self.openai_api_key.is_some() || self.anthropic_api_key.is_some()
  }

  pub fn resolve_provider<'a>(&self, input: &'a serde_json::Value) -> &'a str {
    if let Some(p) = input.get("provider").and_then(|v| v.as_str()) {
      return p;
    }
    if let Some(model) = input.get("model").and_then(|v| v.as_str()) {
      if model.starts_with("claude") {
        return "anthropic";
      }
    }
    "openai"
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_config_default() {
    let config = ProviderConfig::default();
    assert_eq!(config.openai_base_url, "https://api.openai.com/v1");
    assert_eq!(
      config.anthropic_base_url,
      "https://api.anthropic.com/v1"
    );
    assert!(config.openai_api_key.is_none());
  }

  #[test]
  fn test_resolve_provider_by_model() {
    let config = ProviderConfig::default();
    assert_eq!(
      config.resolve_provider(&serde_json::json!({"model": "gpt-4o"})),
      "openai"
    );
    assert_eq!(
      config.resolve_provider(
        &serde_json::json!({"model": "claude-sonnet-4-20250514"})
      ),
      "anthropic"
    );
    assert_eq!(
      config
        .resolve_provider(&serde_json::json!({"model": "o3-mini"})),
      "openai"
    );
  }

  #[test]
  fn test_resolve_provider_explicit_override() {
    let config = ProviderConfig::default();
    assert_eq!(
      config.resolve_provider(
        &serde_json::json!({"model": "gpt-4o", "provider": "anthropic"})
      ),
      "anthropic"
    );
  }

  #[test]
  fn test_has_any_provider() {
    assert!(!ProviderConfig::default().has_any_provider());
    let config = ProviderConfig {
      openai_api_key: Some("sk-test".into()),
      ..Default::default()
    };
    assert!(config.has_any_provider());
  }
}
