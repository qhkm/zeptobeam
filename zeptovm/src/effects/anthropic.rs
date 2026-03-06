use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::core::effect::{EffectId, EffectResult};
use crate::effects::provider::ProviderClient;

const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_MAX_TOKENS: u64 = 1024;
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages API client with streaming support.
pub struct AnthropicClient {
  pub client: Client,
  pub api_key: String,
  pub base_url: String,
}

impl AnthropicClient {
  pub fn new(api_key: String, base_url: String) -> Self {
    Self {
      client: Client::new(),
      api_key,
      base_url,
    }
  }

  /// Build the messages array from input.
  ///
  /// Accepts either a `messages` array directly or a convenience
  /// `prompt` string that gets wrapped as a single user message.
  fn build_messages(input: &Value) -> Vec<Value> {
    if let Some(messages) = input.get("messages").and_then(|v| v.as_array())
    {
      messages.clone()
    } else if let Some(prompt) =
      input.get("prompt").and_then(|v| v.as_str())
    {
      vec![json!({"role": "user", "content": prompt})]
    } else {
      vec![json!({"role": "user", "content": "Hello"})]
    }
  }

  fn resolve_model(input: &Value) -> &str {
    input
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or(DEFAULT_MODEL)
  }

  fn resolve_max_tokens(input: &Value) -> u64 {
    input
      .get("max_tokens")
      .and_then(|v| v.as_u64())
      .unwrap_or(DEFAULT_MAX_TOKENS)
  }

  fn endpoint(&self) -> String {
    format!("{}/messages", self.base_url)
  }
}

#[async_trait]
impl ProviderClient for AnthropicClient {
  /// Non-streaming completion. Posts to /messages without the stream
  /// field and returns the full response.
  async fn complete(
    &self,
    effect_id: EffectId,
    input: &Value,
  ) -> EffectResult {
    let model = Self::resolve_model(input);
    let max_tokens = Self::resolve_max_tokens(input);
    let messages = Self::build_messages(input);

    let body = json!({
      "model": model,
      "messages": messages,
      "max_tokens": max_tokens,
    });

    let response = match self
      .client
      .post(&self.endpoint())
      .header("x-api-key", &self.api_key)
      .header("anthropic-version", ANTHROPIC_VERSION)
      .header("content-type", "application/json")
      .json(&body)
      .send()
      .await
    {
      Ok(resp) => resp,
      Err(e) => {
        return EffectResult::failure(
          effect_id,
          format!("HTTP request failed: {}", e),
        );
      }
    };

    let status = response.status();
    let raw_text = match response.text().await {
      Ok(t) => t,
      Err(e) => {
        return EffectResult::failure(
          effect_id,
          format!("Failed to read response body: {}", e),
        );
      }
    };

    if !status.is_success() {
      return EffectResult::failure(
        effect_id,
        format!("API error ({}): {}", status, raw_text),
      );
    }

    let raw: Value = match serde_json::from_str(&raw_text) {
      Ok(v) => v,
      Err(e) => {
        return EffectResult::failure(
          effect_id,
          format!("Failed to parse JSON response: {}", e),
        );
      }
    };

    // Extract content from content[0].text
    let content = raw
      .get("content")
      .and_then(|c| c.as_array())
      .and_then(|arr| arr.first())
      .and_then(|block| block.get("text"))
      .and_then(|t| t.as_str())
      .unwrap_or("")
      .to_string();

    // Extract token usage
    let input_tokens = raw
      .get("usage")
      .and_then(|u| u.get("input_tokens"))
      .and_then(|v| v.as_u64())
      .unwrap_or(0);
    let output_tokens = raw
      .get("usage")
      .and_then(|u| u.get("output_tokens"))
      .and_then(|v| v.as_u64())
      .unwrap_or(0);
    let tokens_used = input_tokens + output_tokens;

    EffectResult::success(
      effect_id,
      json!({
        "content": content,
        "tokens_used": tokens_used,
        "raw": raw,
      }),
    )
  }

  /// Streaming completion. Sends partial EffectResult(Streaming) deltas
  /// through the channel, followed by a final success or failure result.
  async fn complete_stream(
    &self,
    effect_id: EffectId,
    input: &Value,
    tx: mpsc::Sender<EffectResult>,
  ) -> Result<(), String> {
    let model = Self::resolve_model(input);
    let max_tokens = Self::resolve_max_tokens(input);
    let messages = Self::build_messages(input);

    let body = json!({
      "model": model,
      "messages": messages,
      "max_tokens": max_tokens,
      "stream": true,
    });

    let response = self
      .client
      .post(&self.endpoint())
      .header("x-api-key", &self.api_key)
      .header("anthropic-version", ANTHROPIC_VERSION)
      .header("content-type", "application/json")
      .json(&body)
      .send()
      .await
      .map_err(|e| format!("HTTP request failed: {}", e))?;

    if !response.status().is_success() {
      let status = response.status();
      let body_text = response
        .text()
        .await
        .unwrap_or_else(|_| "unknown".into());
      let err_msg = format!("API error ({}): {}", status, body_text);
      let _ = tx
        .send(EffectResult::failure(effect_id, &err_msg))
        .await;
      return Err(err_msg);
    }

    // Read the SSE stream line by line
    let mut accumulated_content = String::new();
    let mut current_event = String::new();
    let mut bytes_stream = response.bytes_stream();

    use futures::StreamExt;
    let mut leftover = String::new();

    while let Some(chunk_result) = bytes_stream.next().await {
      let chunk = match chunk_result {
        Ok(c) => c,
        Err(e) => {
          let err_msg = format!("Stream read error: {}", e);
          let _ = tx
            .send(EffectResult::failure(effect_id, &err_msg))
            .await;
          return Err(err_msg);
        }
      };

      let text = match std::str::from_utf8(&chunk) {
        Ok(t) => t,
        Err(e) => {
          let err_msg = format!("Invalid UTF-8 in stream: {}", e);
          let _ = tx
            .send(EffectResult::failure(effect_id, &err_msg))
            .await;
          return Err(err_msg);
        }
      };

      leftover.push_str(text);

      // Process complete lines
      while let Some(newline_pos) = leftover.find('\n') {
        let line = leftover[..newline_pos].trim_end_matches('\r');
        let line = line.to_string();
        leftover = leftover[newline_pos + 1..].to_string();

        if line.starts_with("event: ") {
          current_event = line["event: ".len()..].to_string();
        } else if line.starts_with("data: ") {
          let data = &line["data: ".len()..];

          if current_event == "content_block_delta" {
            if let Ok(parsed) = serde_json::from_str::<Value>(data)
            {
              if let Some(delta_text) = parsed
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|t| t.as_str())
              {
                accumulated_content.push_str(delta_text);
                let _ = tx
                  .send(EffectResult::streaming(
                    effect_id,
                    json!({ "delta": delta_text }),
                  ))
                  .await;
              }
            }
          } else if current_event == "message_delta" {
            // message_delta may contain usage info; we extract
            // it but don't need to act on it here.
          } else if current_event == "message_stop" {
            // End of stream marker
          }
        }
        // Empty lines or other lines are ignored (SSE separators)
      }
    }

    // Send final success result with accumulated content
    let _ = tx
      .send(EffectResult::success(
        effect_id,
        json!({
          "content": accumulated_content,
          "tokens_used": null,
          "raw": null,
        }),
      ))
      .await;

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_anthropic_client_new() {
    let client = AnthropicClient::new(
      "sk-ant-test".into(),
      "https://api.anthropic.com/v1".into(),
    );
    assert_eq!(client.api_key, "sk-ant-test");
    assert_eq!(client.base_url, "https://api.anthropic.com/v1");
  }

  #[test]
  fn test_build_messages_from_prompt() {
    let input = json!({"prompt": "Hello, Claude"});
    let messages = AnthropicClient::build_messages(&input);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "Hello, Claude");
  }

  #[test]
  fn test_build_messages_from_array() {
    let input = json!({
      "messages": [
        {"role": "user", "content": "Hi"},
        {"role": "assistant", "content": "Hello!"},
        {"role": "user", "content": "How are you?"}
      ]
    });
    let messages = AnthropicClient::build_messages(&input);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[2]["content"], "How are you?");
  }

  #[test]
  fn test_build_messages_fallback() {
    let input = json!({});
    let messages = AnthropicClient::build_messages(&input);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
  }

  #[test]
  fn test_resolve_model_default() {
    let input = json!({"prompt": "test"});
    assert_eq!(
      AnthropicClient::resolve_model(&input),
      DEFAULT_MODEL
    );
  }

  #[test]
  fn test_resolve_model_custom() {
    let input = json!({"model": "claude-3-haiku-20240307"});
    assert_eq!(
      AnthropicClient::resolve_model(&input),
      "claude-3-haiku-20240307"
    );
  }

  #[test]
  fn test_resolve_max_tokens_default() {
    let input = json!({});
    assert_eq!(
      AnthropicClient::resolve_max_tokens(&input),
      DEFAULT_MAX_TOKENS
    );
  }

  #[test]
  fn test_resolve_max_tokens_custom() {
    let input = json!({"max_tokens": 2048});
    assert_eq!(AnthropicClient::resolve_max_tokens(&input), 2048);
  }

  #[test]
  fn test_endpoint() {
    let client = AnthropicClient::new(
      "key".into(),
      "https://api.anthropic.com/v1".into(),
    );
    assert_eq!(
      client.endpoint(),
      "https://api.anthropic.com/v1/messages"
    );
  }
}
