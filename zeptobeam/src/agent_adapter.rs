//! Adapter that implements `zeptovm::Behavior` for ZeptoClaw agent configs.
//!
//! This bridges the agent-specific configuration (provider, model, tools, etc.)
//! into the generic `Behavior` trait so agents can run inside the zeptovm
//! process runtime.

use async_trait::async_trait;
use tracing::{debug, info, warn};

use zeptovm::error::{Action, Message, Reason, UserPayload};
use zeptovm::Behavior;

use crate::llm_bridge::LlmBridge;

/// Configuration for an agent adapter, mirroring the legacy `AgentConfig`
/// fields that are relevant to the zeptovm runtime.
#[derive(Debug, Clone)]
pub struct AgentAdapterConfig {
  pub name: String,
  pub provider: String,
  pub model: Option<String>,
  pub system_prompt: Option<String>,
  pub tools: Vec<String>,
  pub max_iterations: Option<usize>,
  pub timeout_ms: Option<u64>,
}

/// An agent adapter that implements `zeptovm::Behavior`.
///
/// Holds agent configuration plus mutable runtime state (message count,
/// conversation history). When a `LlmBridge` is present, text messages
/// are forwarded to the LLM for a response.
pub struct AgentAdapter {
  config: AgentAdapterConfig,
  message_count: usize,
  conversation_history: Vec<String>,
  bridge: Option<LlmBridge>,
}

/// Create a new `AgentAdapter` from a config (no LLM bridge).
pub fn create_agent_adapter(config: AgentAdapterConfig) -> AgentAdapter {
  AgentAdapter {
    config,
    message_count: 0,
    conversation_history: Vec::new(),
    bridge: None,
  }
}

/// Create a new `AgentAdapter` wired to an `LlmBridge` for actual LLM calls.
pub fn create_agent_adapter_with_bridge(
  config: AgentAdapterConfig,
  bridge: LlmBridge,
) -> AgentAdapter {
  AgentAdapter {
    config,
    message_count: 0,
    conversation_history: Vec::new(),
    bridge: Some(bridge),
  }
}

#[async_trait]
impl Behavior for AgentAdapter {
  async fn init(
    &mut self,
    checkpoint: Option<Vec<u8>>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(data) = checkpoint {
      let history: Vec<String> = serde_json::from_slice(&data)?;
      self.conversation_history = history;
      self.message_count = self.conversation_history.len();
      info!(
        agent = %self.config.name,
        restored_messages = self.message_count,
        "agent restored from checkpoint"
      );
    } else {
      info!(
        agent = %self.config.name,
        provider = %self.config.provider,
        "agent starting"
      );
    }
    Ok(())
  }

  async fn handle(&mut self, msg: Message) -> Action {
    match msg {
      Message::User(UserPayload::Text(text)) => {
        self.conversation_history.push(text.clone());
        self.message_count += 1;
        debug!(
          agent = %self.config.name,
          message_count = self.message_count,
          "handled text message"
        );

        if let Some(ref bridge) = self.bridge {
          let tools = if self.config.tools.is_empty() {
            None
          } else {
            Some(self.config.tools.clone())
          };
          match bridge
            .chat(
              &self.config.provider,
              self.config.model.as_deref(),
              self.config.system_prompt.as_deref(),
              &text,
              tools,
              self.config.max_iterations,
              self.config.timeout_ms,
            )
            .await
          {
            Ok(response) => {
              let response_str = response.to_string();
              self.conversation_history.push(response_str.clone());
              info!(
                agent = %self.config.name,
                "LLM response received"
              );
              debug!(
                agent = %self.config.name,
                response = %response_str,
                "LLM response"
              );
            }
            Err(e) => {
              warn!(
                agent = %self.config.name,
                error = %e,
                "LLM call failed"
              );
            }
          }
        }

        Action::Continue
      }
      Message::User(UserPayload::Json(value)) => {
        if let Some(cmd) = value.get("command").and_then(|v| v.as_str()) {
          if cmd == "stop" {
            let reason_str = value
              .get("reason")
              .and_then(|v| v.as_str())
              .unwrap_or("requested");
            info!(
              agent = %self.config.name,
              reason = reason_str,
              "stop command received"
            );
            return Action::Stop(Reason::Custom(reason_str.to_string()));
          }
        }
        // Non-stop JSON: record it and continue
        let json_str = value.to_string();
        self.conversation_history.push(json_str);
        self.message_count += 1;
        debug!(
          agent = %self.config.name,
          message_count = self.message_count,
          "handled json message"
        );
        Action::Continue
      }
      Message::User(UserPayload::Bytes(_)) => {
        self.message_count += 1;
        debug!(
          agent = %self.config.name,
          message_count = self.message_count,
          "handled bytes message"
        );
        Action::Continue
      }
    }
  }

  async fn terminate(&mut self, reason: &Reason) {
    info!(
      agent = %self.config.name,
      reason = %reason,
      total_messages = self.message_count,
      "agent shutting down"
    );
  }

  fn should_checkpoint(&self) -> bool {
    self.message_count > 0 && self.message_count % 10 == 0
  }

  fn checkpoint(&self) -> Option<Vec<u8>> {
    serde_json::to_vec(&self.conversation_history).ok()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn test_config() -> AgentAdapterConfig {
    AgentAdapterConfig {
      name: "test-agent".into(),
      provider: "openrouter".into(),
      model: Some("test-model".into()),
      system_prompt: Some("You are a test agent.".into()),
      tools: vec!["tool1".into(), "tool2".into()],
      max_iterations: Some(10),
      timeout_ms: Some(5000),
    }
  }

  #[tokio::test]
  async fn test_adapter_init_no_checkpoint() {
    let mut adapter = create_agent_adapter(test_config());
    let result = adapter.init(None).await;
    assert!(result.is_ok());
    assert_eq!(adapter.message_count, 0);
    assert!(adapter.conversation_history.is_empty());
  }

  #[tokio::test]
  async fn test_adapter_init_with_checkpoint() {
    let history = vec!["hello".to_string(), "world".to_string()];
    let checkpoint_data = serde_json::to_vec(&history).unwrap();

    let mut adapter = create_agent_adapter(test_config());
    let result = adapter.init(Some(checkpoint_data)).await;
    assert!(result.is_ok());
    assert_eq!(adapter.conversation_history.len(), 2);
    assert_eq!(adapter.conversation_history[0], "hello");
    assert_eq!(adapter.conversation_history[1], "world");
    assert_eq!(adapter.message_count, 2);
  }

  #[tokio::test]
  async fn test_adapter_handle_text_message() {
    let mut adapter = create_agent_adapter(test_config());
    adapter.init(None).await.unwrap();

    let action = adapter.handle(Message::text("hi there")).await;
    assert!(matches!(action, Action::Continue));
    assert_eq!(adapter.message_count, 1);
    assert_eq!(adapter.conversation_history.len(), 1);
    assert_eq!(adapter.conversation_history[0], "hi there");
  }

  #[tokio::test]
  async fn test_adapter_handle_stop_command() {
    let mut adapter = create_agent_adapter(test_config());
    adapter.init(None).await.unwrap();

    let stop_msg = Message::json(serde_json::json!({"command": "stop"}));
    let action = adapter.handle(stop_msg).await;
    assert!(matches!(action, Action::Stop(_)));
    if let Action::Stop(reason) = action {
      assert!(matches!(reason, Reason::Custom(_)));
    }
  }

  #[tokio::test]
  async fn test_adapter_checkpoint_serialization() {
    let mut adapter = create_agent_adapter(test_config());
    adapter.init(None).await.unwrap();

    // Before 10 messages, should_checkpoint returns false
    for i in 0..9 {
      let action = adapter
        .handle(Message::text(format!("message {}", i)))
        .await;
      assert!(matches!(action, Action::Continue));
      assert!(!adapter.should_checkpoint());
    }

    // 10th message triggers checkpoint
    let action = adapter.handle(Message::text("message 9")).await;
    assert!(matches!(action, Action::Continue));
    assert_eq!(adapter.message_count, 10);
    assert!(adapter.should_checkpoint());

    // Verify checkpoint data is valid JSON
    let checkpoint = adapter.checkpoint();
    assert!(checkpoint.is_some());
    let data = checkpoint.unwrap();
    let restored: Vec<String> = serde_json::from_slice(&data).unwrap();
    assert_eq!(restored.len(), 10);
    assert_eq!(restored[0], "message 0");
    assert_eq!(restored[9], "message 9");
  }

  #[tokio::test]
  async fn test_adapter_terminate() {
    let mut adapter = create_agent_adapter(test_config());
    adapter.init(None).await.unwrap();
    // Should not panic
    adapter.terminate(&Reason::Normal).await;
    adapter.terminate(&Reason::Shutdown).await;
    adapter.terminate(&Reason::Kill).await;
    adapter
      .terminate(&Reason::Custom("test reason".into()))
      .await;
  }

  #[tokio::test]
  async fn test_adapter_with_bridge_none_still_works() {
    // Adapter created without bridge should handle text normally
    let mut adapter = create_agent_adapter(test_config());
    adapter.init(None).await.unwrap();
    assert!(adapter.bridge.is_none());

    let action = adapter.handle(Message::text("hello")).await;
    assert!(matches!(action, Action::Continue));
    assert_eq!(adapter.message_count, 1);
    assert_eq!(adapter.conversation_history.len(), 1);
    assert_eq!(adapter.conversation_history[0], "hello");
  }

  #[tokio::test]
  async fn test_adapter_with_bridge_factory() {
    use erlangrt::agent_rt::bridge::create_bridge;

    let (handle, _worker) = create_bridge();
    let bridge = LlmBridge::new(handle);
    let adapter =
      create_agent_adapter_with_bridge(test_config(), bridge);
    assert!(adapter.bridge.is_some());
    assert_eq!(adapter.config.name, "test-agent");
    assert_eq!(adapter.message_count, 0);
    assert!(adapter.conversation_history.is_empty());
  }
}
