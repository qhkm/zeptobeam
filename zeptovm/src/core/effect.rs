use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

static NEXT_EFFECT_ID: AtomicU64 = AtomicU64::new(1);

/// Unique identifier for an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectId(u64);

impl EffectId {
    pub fn new() -> Self {
        Self(NEXT_EFFECT_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub fn raw(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for EffectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "eff-{}", self.0)
    }
}

/// What kind of side effect this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    LlmCall,
    Http,
    DbQuery,
    DbWrite,
    CliExec,
    SandboxExec,
    BrowserAutomation,
    PublishEvent,
    HumanApproval,
    HumanInput,
    SleepUntil,
    Spawn,
    ObjectFetch,
    ObjectPut,
    VectorSearch,
    Custom(String),
}

/// Retry policy for effects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// Describes how to undo/compensate a completed effect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompensationSpec {
  pub undo_kind: EffectKind,
  pub undo_input: serde_json::Value,
}

/// A process emits this to request a side effect.
/// This is what gets journaled, policy-checked, and dispatched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectRequest {
    pub effect_id: EffectId,
    pub kind: EffectKind,
    pub input: serde_json::Value,
    pub timeout: Duration,
    pub retry: RetryPolicy,
    pub idempotency_key: Option<String>,
    pub compensation: Option<CompensationSpec>,
}

impl EffectRequest {
    pub fn new(kind: EffectKind, input: serde_json::Value) -> Self {
        Self {
            effect_id: EffectId::new(),
            kind,
            input,
            timeout: Duration::from_secs(60),
            retry: RetryPolicy::default(),
            idempotency_key: None,
            compensation: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    pub fn with_compensation(
        mut self,
        spec: CompensationSpec,
    ) -> Self {
        self.compensation = Some(spec);
        self
    }
}

/// Status of an effect after execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

/// Result returned by the effect worker plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectResult {
    pub effect_id: EffectId,
    pub status: EffectStatus,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl EffectResult {
    pub fn success(effect_id: EffectId, output: serde_json::Value) -> Self {
        Self {
            effect_id,
            status: EffectStatus::Succeeded,
            output: Some(output),
            error: None,
        }
    }

    pub fn failure(effect_id: EffectId, error: impl Into<String>) -> Self {
        Self {
            effect_id,
            status: EffectStatus::Failed,
            output: None,
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effect_id_unique() {
        let a = EffectId::new();
        let b = EffectId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_effect_request_builder() {
        let req = EffectRequest::new(
            EffectKind::LlmCall,
            serde_json::json!({"prompt": "hello"}),
        )
        .with_timeout(Duration::from_secs(30))
        .with_idempotency_key("call-123");

        assert_eq!(req.kind, EffectKind::LlmCall);
        assert_eq!(req.timeout, Duration::from_secs(30));
        assert_eq!(req.idempotency_key.as_deref(), Some("call-123"));
        assert_eq!(req.retry.max_attempts, 3);
    }

    #[test]
    fn test_effect_result_success() {
        let id = EffectId::new();
        let result = EffectResult::success(id, serde_json::json!("ok"));
        assert_eq!(result.status, EffectStatus::Succeeded);
        assert!(result.output.is_some());
        assert!(result.error.is_none());
    }

    #[test]
    fn test_effect_result_failure() {
        let id = EffectId::new();
        let result = EffectResult::failure(id, "network error");
        assert_eq!(result.status, EffectStatus::Failed);
        assert!(result.output.is_none());
        assert_eq!(result.error.as_deref(), Some("network error"));
    }

    #[test]
    fn test_effect_request_serializable() {
        let req = EffectRequest::new(
            EffectKind::Http,
            serde_json::json!({"url": "https://api.example.com"}),
        );
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: EffectRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.kind, EffectKind::Http);
    }

    #[test]
    fn test_effect_kind_custom() {
        let kind = EffectKind::Custom("my-tool".into());
        assert_eq!(kind, EffectKind::Custom("my-tool".into()));
    }
}
