use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::core::effect::{EffectId, EffectResult};

/// Trait for LLM provider clients.
///
/// Implementations handle HTTP communication with
/// a specific provider (OpenAI, Anthropic, etc.).
#[async_trait]
pub trait ProviderClient: Send + Sync {
  /// Non-streaming completion. Blocks until full response.
  async fn complete(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
  ) -> EffectResult;

  /// Streaming completion. Sends partial EffectResult(Streaming)
  /// through tx, then a final EffectResult(Succeeded or Failed).
  async fn complete_stream(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
    tx: mpsc::Sender<EffectResult>,
  ) -> Result<(), String>;
}
