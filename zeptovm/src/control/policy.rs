use crate::core::effect::EffectKind;

/// Decision returned by the policy engine after evaluating an effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
  Allow,
  Deny(String),
}

/// A single rule that matches an effect kind and yields a decision.
#[derive(Debug, Clone)]
pub struct PolicyRule {
  pub kind: EffectKind,
  pub decision: PolicyDecision,
  pub max_cost_microdollars: Option<u64>,
}

/// Evaluates effect requests against a list of rules.
///
/// Rules are checked in order; the first rule whose `kind` matches
/// the requested effect kind wins. If that rule has a
/// `max_cost_microdollars` threshold and the caller supplies an
/// estimated cost that exceeds it, the effect is denied. If no rule
/// matches, the engine returns its configured default decision.
pub struct PolicyEngine {
  rules: Vec<PolicyRule>,
  default: PolicyDecision,
}

impl PolicyEngine {
  pub fn new(
    rules: Vec<PolicyRule>,
    default: PolicyDecision,
  ) -> Self {
    Self { rules, default }
  }

  /// Evaluate whether an effect of the given kind should proceed.
  ///
  /// * First matching rule by `EffectKind` wins.
  /// * If the matching rule carries `max_cost_microdollars` AND
  ///   `estimated_cost` is `Some(c)` with `c > max`, the effect
  ///   is denied.
  /// * If `estimated_cost` is `None`, the cost check is skipped
  ///   and the rule's base decision is returned.
  /// * If no rule matches, `self.default` is returned.
  pub fn evaluate(
    &self,
    kind: &EffectKind,
    estimated_cost: Option<u64>,
  ) -> PolicyDecision {
    for rule in &self.rules {
      if &rule.kind == kind {
        // Cost gate: deny if estimate exceeds threshold.
        if let (Some(max), Some(est)) =
          (rule.max_cost_microdollars, estimated_cost)
        {
          if est > max {
            return PolicyDecision::Deny(format!(
              "estimated cost {} exceeds max {}",
              est, max
            ));
          }
        }
        return rule.decision.clone();
      }
    }
    self.default.clone()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_allow_no_rules() {
    let engine =
      PolicyEngine::new(vec![], PolicyDecision::Allow);
    let result =
      engine.evaluate(&EffectKind::LlmCall, None);
    assert_eq!(result, PolicyDecision::Allow);
  }

  #[test]
  fn test_deny_by_kind() {
    let engine = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::DbWrite,
        decision: PolicyDecision::Deny(
          "writes disabled".into(),
        ),
        max_cost_microdollars: None,
      }],
      PolicyDecision::Allow,
    );

    assert_eq!(
      engine.evaluate(&EffectKind::DbWrite, None),
      PolicyDecision::Deny("writes disabled".into()),
    );
    assert_eq!(
      engine.evaluate(&EffectKind::LlmCall, None),
      PolicyDecision::Allow,
    );
  }

  #[test]
  fn test_cost_threshold_deny() {
    let engine = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::LlmCall,
        decision: PolicyDecision::Allow,
        max_cost_microdollars: Some(5_000_000), // $5
      }],
      PolicyDecision::Allow,
    );

    // Under threshold — allowed.
    assert_eq!(
      engine.evaluate(
        &EffectKind::LlmCall,
        Some(4_000_000),
      ),
      PolicyDecision::Allow,
    );

    // Over threshold — denied.
    assert!(matches!(
      engine.evaluate(
        &EffectKind::LlmCall,
        Some(6_000_000),
      ),
      PolicyDecision::Deny(_),
    ));
  }

  #[test]
  fn test_first_match_wins() {
    let engine = PolicyEngine::new(
      vec![
        PolicyRule {
          kind: EffectKind::LlmCall,
          decision: PolicyDecision::Allow,
          max_cost_microdollars: None,
        },
        PolicyRule {
          kind: EffectKind::LlmCall,
          decision: PolicyDecision::Deny(
            "should not reach".into(),
          ),
          max_cost_microdollars: None,
        },
      ],
      PolicyDecision::Deny("default deny".into()),
    );

    assert_eq!(
      engine.evaluate(&EffectKind::LlmCall, None),
      PolicyDecision::Allow,
    );
  }

  #[test]
  fn test_default_deny() {
    let engine = PolicyEngine::new(
      vec![],
      PolicyDecision::Deny("nothing allowed".into()),
    );
    assert_eq!(
      engine.evaluate(&EffectKind::Http, None),
      PolicyDecision::Deny("nothing allowed".into()),
    );
  }

  #[test]
  fn test_cost_unavailable_skips_cost_check() {
    let engine = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::LlmCall,
        decision: PolicyDecision::Allow,
        max_cost_microdollars: Some(5_000_000),
      }],
      PolicyDecision::Deny("default deny".into()),
    );

    // No estimated cost provided — cost check skipped,
    // rule's base decision (Allow) returned.
    assert_eq!(
      engine.evaluate(&EffectKind::LlmCall, None),
      PolicyDecision::Allow,
    );
  }
}
