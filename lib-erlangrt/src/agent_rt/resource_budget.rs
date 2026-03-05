use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ModelPrice {
  pub input_per_1k: f64,
  pub output_per_1k: f64,
}

#[derive(Debug, Clone, Default)]
pub struct TokenPricing {
  pub model_prices: HashMap<String, ModelPrice>,
}

#[derive(Debug, Clone)]
pub struct ResourceBudget {
  max_tokens: Option<u64>,
  max_cost_usd: Option<f64>,
  tokens_used: u64,
  cost_usd: f64,
  pricing: TokenPricing,
}

#[derive(Debug, Clone)]
pub struct UsageReport {
  pub input_tokens: u64,
  pub output_tokens: u64,
  pub model: Option<String>,
}

impl ResourceBudget {
  pub fn new(
    max_tokens: Option<u64>,
    max_cost_usd: Option<f64>,
    pricing: TokenPricing,
  ) -> Self {
    Self {
      max_tokens,
      max_cost_usd,
      tokens_used: 0,
      cost_usd: 0.0,
      pricing,
    }
  }

  pub fn unlimited() -> Self {
    Self::new(None, None, TokenPricing::default())
  }

  /// Record token usage from a worker result. Returns true if budget is now exhausted.
  pub fn record_usage(&mut self, usage: &UsageReport) -> bool {
    let total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
    self.tokens_used = self.tokens_used.saturating_add(total_tokens);

    if let Some(ref model) = usage.model {
      if let Some(prices) = self.pricing.model_prices.get(model) {
        let input_cost = (usage.input_tokens as f64) / 1000.0 * prices.input_per_1k;
        let output_cost = (usage.output_tokens as f64) / 1000.0 * prices.output_per_1k;
        self.cost_usd += input_cost + output_cost;
      }
    }

    self.is_exhausted()
  }

  pub fn is_exhausted(&self) -> bool {
    let tokens_exhausted = self
      .max_tokens
      .map(|max| self.tokens_used >= max)
      .unwrap_or(false);
    let cost_exhausted = self
      .max_cost_usd
      .map(|max| self.cost_usd >= max)
      .unwrap_or(false);
    tokens_exhausted || cost_exhausted
  }

  pub fn tokens_used(&self) -> u64 {
    self.tokens_used
  }

  pub fn cost_usd(&self) -> f64 {
    self.cost_usd
  }

  pub fn snapshot(&self) -> serde_json::Value {
    serde_json::json!({
        "tokens_used": self.tokens_used,
        "max_tokens": self.max_tokens,
        "cost_usd": self.cost_usd,
        "max_cost_usd": self.max_cost_usd,
    })
  }

  pub fn parse_usage(payload: &serde_json::Value) -> Option<UsageReport> {
    let usage = payload.get("usage")?;
    let input_tokens = usage.get("input_tokens")?.as_u64()?;
    let output_tokens = usage.get("output_tokens")?.as_u64()?;
    let model = usage
      .get("model")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    Some(UsageReport {
      input_tokens,
      output_tokens,
      model,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_unlimited_budget_never_exhausted() {
    let mut budget = ResourceBudget::unlimited();
    let exhausted = budget.record_usage(&UsageReport {
      input_tokens: 1_000_000,
      output_tokens: 500_000,
      model: None,
    });
    assert!(!exhausted);
    assert!(!budget.is_exhausted());
    assert_eq!(budget.tokens_used(), 1_500_000);
  }

  #[test]
  fn test_token_budget_exhaustion() {
    let mut budget = ResourceBudget::new(Some(1000), None, TokenPricing::default());
    budget.record_usage(&UsageReport {
      input_tokens: 400,
      output_tokens: 300,
      model: None,
    });
    assert!(!budget.is_exhausted());
    assert_eq!(budget.tokens_used(), 700);

    budget.record_usage(&UsageReport {
      input_tokens: 200,
      output_tokens: 200,
      model: None,
    });
    assert!(budget.is_exhausted());
    assert_eq!(budget.tokens_used(), 1100);
  }

  #[test]
  fn test_cost_budget_exhaustion() {
    let mut pricing = TokenPricing::default();
    pricing.model_prices.insert(
      "gpt-4o".into(),
      ModelPrice {
        input_per_1k: 0.0025,
        output_per_1k: 0.01,
      },
    );
    let mut budget = ResourceBudget::new(None, Some(1.0), pricing);

    budget.record_usage(&UsageReport {
      input_tokens: 10_000,
      output_tokens: 5_000,
      model: Some("gpt-4o".into()),
    });
    assert!(!budget.is_exhausted());
    assert!(budget.cost_usd() > 0.07);

    budget.record_usage(&UsageReport {
      input_tokens: 100_000,
      output_tokens: 100_000,
      model: Some("gpt-4o".into()),
    });
    assert!(budget.is_exhausted());
  }

  #[test]
  fn test_unknown_model_no_cost() {
    let mut budget = ResourceBudget::new(None, Some(1.0), TokenPricing::default());
    budget.record_usage(&UsageReport {
      input_tokens: 1_000_000,
      output_tokens: 1_000_000,
      model: Some("unknown".into()),
    });
    assert_eq!(budget.cost_usd(), 0.0);
    assert!(!budget.is_exhausted());
  }

  #[test]
  fn test_snapshot() {
    let mut budget = ResourceBudget::new(Some(10000), Some(5.0), TokenPricing::default());
    budget.record_usage(&UsageReport {
      input_tokens: 100,
      output_tokens: 50,
      model: None,
    });
    let snap = budget.snapshot();
    assert_eq!(snap["tokens_used"], 150);
    assert_eq!(snap["max_tokens"], 10000);
  }

  #[test]
  fn test_parse_usage_from_worker_result() {
    let payload = serde_json::json!({
        "type": "worker_result",
        "usage": { "input_tokens": 500, "output_tokens": 200, "model": "gpt-4o" }
    });
    let usage = ResourceBudget::parse_usage(&payload).unwrap();
    assert_eq!(usage.input_tokens, 500);
    assert_eq!(usage.output_tokens, 200);
    assert_eq!(usage.model, Some("gpt-4o".into()));
  }

  #[test]
  fn test_parse_usage_missing() {
    let payload = serde_json::json!({"type": "worker_result"});
    assert!(ResourceBudget::parse_usage(&payload).is_none());
  }

  #[test]
  fn test_record_returns_true_on_exhaustion() {
    let mut budget = ResourceBudget::new(Some(100), None, TokenPricing::default());
    let exhausted = budget.record_usage(&UsageReport {
      input_tokens: 200,
      output_tokens: 0,
      model: None,
    });
    assert!(exhausted);
  }
}
