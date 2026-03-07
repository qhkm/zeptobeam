use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::warn;

use crate::core::effect::{EffectId, EffectResult};
use crate::effects::provider::ProviderClient;

const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// OpenAI-compatible API client with streaming support.
pub struct OpenAiClient {
  client: Client,
  api_key: String,
  base_url: String,
}

impl OpenAiClient {
  pub fn new(api_key: String, base_url: String) -> Self {
    Self {
      client: Client::new(),
      api_key,
      base_url,
    }
  }

  /// Build the request body from input, supporting both
  /// `messages` array and convenience `prompt` string.
  fn build_body(
    &self,
    input: &Value,
    stream: bool,
  ) -> Value {
    let model = input
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or(DEFAULT_MODEL);

    let messages = if let Some(msgs) = input.get("messages") {
      msgs.clone()
    } else {
      let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      json!([{"role": "user", "content": prompt}])
    };

    let mut body = json!({
      "model": model,
      "messages": messages,
      "stream": stream,
    });

    // Forward optional parameters
    for key in &[
      "temperature",
      "max_tokens",
      "top_p",
      "frequency_penalty",
      "presence_penalty",
      "stop",
    ] {
      if let Some(val) = input.get(*key) {
        body[*key] = val.clone();
      }
    }

    body
  }
}

#[async_trait]
impl ProviderClient for OpenAiClient {
  async fn complete(
    &self,
    effect_id: EffectId,
    input: &Value,
  ) -> EffectResult {
    let body = self.build_body(input, false);
    let url = format!("{}/chat/completions", self.base_url);

    let response = match self
      .client
      .post(&url)
      .header("Authorization", format!("Bearer {}", self.api_key))
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await
    {
      Ok(r) => r,
      Err(e) => {
        return EffectResult::failure(
          effect_id,
          format!("HTTP request failed: {e}"),
        );
      }
    };

    if !response.status().is_success() {
      let status = response.status();
      let body_text = response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable body>".into());
      return EffectResult::failure(
        effect_id,
        format!("OpenAI API error {status}: {body_text}"),
      );
    }

    let raw: Value = match response.json().await {
      Ok(v) => v,
      Err(e) => {
        return EffectResult::failure(
          effect_id,
          format!("Failed to parse response JSON: {e}"),
        );
      }
    };

    let content = raw
      .pointer("/choices/0/message/content")
      .and_then(|v| v.as_str())
      .unwrap_or("")
      .to_string();

    let tokens_used = raw
      .pointer("/usage/total_tokens")
      .and_then(|v| v.as_u64())
      .unwrap_or(0);

    EffectResult::success(
      effect_id,
      json!({
        "content": content,
        "tokens_used": tokens_used,
        "raw": raw,
      }),
    )
  }

  async fn complete_stream(
    &self,
    effect_id: EffectId,
    input: &Value,
    tx: mpsc::Sender<EffectResult>,
  ) -> Result<(), String> {
    let body = self.build_body(input, true);
    let url = format!("{}/chat/completions", self.base_url);

    let response = self
      .client
      .post(&url)
      .header("Authorization", format!("Bearer {}", self.api_key))
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await
      .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !response.status().is_success() {
      let status = response.status();
      let body_text = response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable body>".into());
      let msg =
        format!("OpenAI API error {status}: {body_text}");
      let _ = tx
        .send(EffectResult::failure(effect_id, &msg))
        .await;
      return Err(msg);
    }

    let mut stream = response.bytes_stream();
    let mut full_text = String::new();
    let mut buffer = String::new();

    while let Some(chunk_result) = stream.next().await {
      let chunk = match chunk_result {
        Ok(bytes) => bytes,
        Err(e) => {
          let msg = format!("Stream read error: {e}");
          warn!("{msg}");
          let _ = tx
            .send(EffectResult::failure(effect_id, &msg))
            .await;
          return Err(msg);
        }
      };

      let text = String::from_utf8_lossy(&chunk);
      buffer.push_str(&text);

      // Process complete SSE lines from the buffer
      while let Some(newline_pos) = buffer.find('\n') {
        let line =
          buffer[..newline_pos].trim_end_matches('\r').to_string();
        buffer = buffer[newline_pos + 1..].to_string();

        if line.is_empty() || line.starts_with(':') {
          continue;
        }

        let data = if let Some(stripped) = line.strip_prefix("data: ")
        {
          stripped
        } else {
          continue;
        };

        if data == "[DONE]" {
          continue;
        }

        let parsed: Value = match serde_json::from_str(data) {
          Ok(v) => v,
          Err(e) => {
            warn!("Failed to parse SSE chunk: {e}");
            continue;
          }
        };

        if let Some(delta_content) = parsed
          .pointer("/choices/0/delta/content")
          .and_then(|v| v.as_str())
        {
          if !delta_content.is_empty() {
            full_text.push_str(delta_content);
            let _ = tx
              .send(EffectResult::streaming(
                effect_id,
                json!({"delta": delta_content}),
              ))
              .await;
          }
        }
      }
    }

    // Send final success with accumulated content
    let _ = tx
      .send(EffectResult::success(
        effect_id,
        json!({"content": full_text}),
      ))
      .await;

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_openai_client_new() {
    let _client = OpenAiClient::new(
      "sk-test".into(),
      "https://api.openai.com/v1".into(),
    );
    // Fields are private; verify construction succeeds
  }

  #[test]
  fn test_build_body_with_prompt() {
    let client = OpenAiClient::new(
      "sk-test".into(),
      "https://api.openai.com/v1".into(),
    );
    let input = json!({"prompt": "Hello, world!"});
    let body = client.build_body(&input, false);
    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "Hello, world!");
  }

  #[test]
  fn test_build_body_with_messages() {
    let client = OpenAiClient::new(
      "sk-test".into(),
      "https://api.openai.com/v1".into(),
    );
    let input = json!({
      "model": "gpt-4o",
      "messages": [
        {"role": "system", "content": "You are helpful."},
        {"role": "user", "content": "Hi"}
      ],
      "temperature": 0.7,
    });
    let body = client.build_body(&input, true);
    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["temperature"], 0.7);
  }

  #[test]
  fn test_build_body_defaults_model() {
    let client = OpenAiClient::new(
      "sk-test".into(),
      "https://api.openai.com/v1".into(),
    );
    let input = json!({"prompt": "test"});
    let body = client.build_body(&input, false);
    assert_eq!(body["model"], "gpt-4o-mini");
  }

  #[test]
  fn test_build_body_forwards_optional_params() {
    let client = OpenAiClient::new(
      "sk-test".into(),
      "https://api.openai.com/v1".into(),
    );
    let input = json!({
      "prompt": "test",
      "max_tokens": 100,
      "top_p": 0.9,
      "stop": ["\n"],
    });
    let body = client.build_body(&input, false);
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["stop"], json!(["\n"]));
  }
}
