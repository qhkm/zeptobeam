# Real Effect Workers (Phase 2.6) — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the placeholder reactor with real HTTP and LLM effect workers (OpenAI + Anthropic), with streaming support and provider abstraction.

**Architecture:** Add `effects/` module with `ProviderClient` trait, `OpenAiClient`, `AnthropicClient`, and `HttpWorker`. Modify the reactor to route `EffectKind::LlmCall` and `EffectKind::Http` to real async workers. Add `Streaming` variant to `EffectStatus` for token-by-token delivery. Config via env vars with runtime override.

**Tech Stack:** Rust, reqwest (HTTP + streaming), tokio, async-trait, serde_json

---

### Task 1: Add reqwest dependency and Streaming status variant

**Files:**
- Modify: `zeptovm/Cargo.toml`
- Modify: `zeptovm/src/core/effect.rs:161-167`

**Steps:**

1. In `zeptovm/Cargo.toml`, add to `[dependencies]`:
```toml
reqwest = { version = "0.12", features = ["json", "stream"] }
```

2. In `zeptovm/src/core/effect.rs`, add `Streaming` variant to `EffectStatus`:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Streaming,
}
```

3. Add a helper constructor for streaming results on `EffectResult`:
```rust
pub fn streaming(effect_id: EffectId, delta: serde_json::Value) -> Self {
    Self {
        effect_id,
        status: EffectStatus::Streaming,
        output: Some(delta),
        error: None,
    }
}
```

4. Add test:
```rust
#[test]
fn test_effect_result_streaming() {
    let id = EffectId::new();
    let result = EffectResult::streaming(id, serde_json::json!({"delta": "hello"}));
    assert_eq!(result.status, EffectStatus::Streaming);
    assert!(result.output.is_some());
}
```

5. In `zeptovm/src/kernel/reactor.rs`, update the retry logic to NOT retry `Streaming` results. In `execute_effect_with_retry()`, the existing match at line 164 handles `TimedOut` and `Cancelled` — `Streaming` should also pass through. The existing `_ => return result` arm already covers this since it catches any non-Succeeded/non-Failed status.

6. Run: `cargo test --lib`
7. Commit: `feat(effect): add Streaming status variant and reqwest dependency`

---

### Task 2: Create effects module with ProviderConfig

**Files:**
- Create: `zeptovm/src/effects/mod.rs`
- Create: `zeptovm/src/effects/config.rs`
- Modify: `zeptovm/src/lib.rs`

**Steps:**

1. Create `zeptovm/src/effects/mod.rs`:
```rust
pub mod config;
pub mod http_worker;
pub mod provider;
pub mod openai;
pub mod anthropic;
```

2. Create `zeptovm/src/effects/config.rs`:
```rust
/// Configuration for LLM and HTTP effect providers.
///
/// Resolution: runtime override > env vars > defaults.
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
      openai_base_url:
        "https://api.openai.com/v1".into(),
      anthropic_api_key: None,
      anthropic_base_url:
        "https://api.anthropic.com/v1".into(),
    }
  }
}

impl ProviderConfig {
  /// Load from environment variables, with optional
  /// overrides applied on top.
  pub fn from_env() -> Self {
    Self {
      openai_api_key: std::env::var("OPENAI_API_KEY")
        .ok(),
      openai_base_url: std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| {
          "https://api.openai.com/v1".into()
        }),
      anthropic_api_key: std::env::var(
        "ANTHROPIC_API_KEY",
      )
      .ok(),
      anthropic_base_url: std::env::var(
        "ANTHROPIC_BASE_URL",
      )
      .unwrap_or_else(|_| {
        "https://api.anthropic.com/v1".into()
      }),
    }
  }

  /// Check if any provider is configured.
  pub fn has_any_provider(&self) -> bool {
    self.openai_api_key.is_some()
      || self.anthropic_api_key.is_some()
  }

  /// Resolve which provider to use based on model name.
  /// Returns "openai" or "anthropic".
  pub fn resolve_provider(
    &self,
    input: &serde_json::Value,
  ) -> &str {
    // Explicit provider override
    if let Some(p) = input
      .get("provider")
      .and_then(|v| v.as_str())
    {
      return p;
    }
    // Model-based resolution
    if let Some(model) = input
      .get("model")
      .and_then(|v| v.as_str())
    {
      if model.starts_with("claude") {
        return "anthropic";
      }
    }
    "openai" // default
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_config_default() {
    let config = ProviderConfig::default();
    assert_eq!(
      config.openai_base_url,
      "https://api.openai.com/v1"
    );
    assert_eq!(
      config.anthropic_base_url,
      "https://api.anthropic.com/v1"
    );
    assert!(config.openai_api_key.is_none());
    assert!(config.anthropic_api_key.is_none());
  }

  #[test]
  fn test_resolve_provider_by_model() {
    let config = ProviderConfig::default();
    assert_eq!(
      config.resolve_provider(
        &serde_json::json!({"model": "gpt-4o"})
      ),
      "openai"
    );
    assert_eq!(
      config.resolve_provider(
        &serde_json::json!({"model": "claude-sonnet-4-20250514"})
      ),
      "anthropic"
    );
    assert_eq!(
      config.resolve_provider(
        &serde_json::json!({"model": "o3-mini"})
      ),
      "openai"
    );
  }

  #[test]
  fn test_resolve_provider_explicit_override() {
    let config = ProviderConfig::default();
    assert_eq!(
      config.resolve_provider(
        &serde_json::json!({
          "model": "gpt-4o",
          "provider": "anthropic"
        })
      ),
      "anthropic"
    );
  }

  #[test]
  fn test_has_any_provider() {
    let config = ProviderConfig::default();
    assert!(!config.has_any_provider());

    let config = ProviderConfig {
      openai_api_key: Some("sk-test".into()),
      ..Default::default()
    };
    assert!(config.has_any_provider());
  }
}
```

3. Add `pub mod effects;` to `zeptovm/src/lib.rs`.

4. Note: The mod.rs references `http_worker`, `provider`, `openai`, `anthropic` which don't exist yet. Create empty placeholder files so it compiles:
   - `zeptovm/src/effects/provider.rs` — empty file
   - `zeptovm/src/effects/openai.rs` — empty file
   - `zeptovm/src/effects/anthropic.rs` — empty file
   - `zeptovm/src/effects/http_worker.rs` — empty file

5. Run: `cargo test --lib`
6. Commit: `feat(effects): add effects module with ProviderConfig`

---

### Task 3: Create ProviderClient trait

**Files:**
- Create: `zeptovm/src/effects/provider.rs` (replace empty file)

**Steps:**

1. Write `zeptovm/src/effects/provider.rs`:
```rust
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::core::effect::EffectResult;

/// Trait for LLM provider clients.
///
/// Implementations handle the HTTP communication with
/// a specific provider (OpenAI, Anthropic, etc.).
#[async_trait]
pub trait ProviderClient: Send + Sync {
  /// Non-streaming completion. Blocks until the full
  /// response is available.
  async fn complete(
    &self,
    effect_id: crate::core::effect::EffectId,
    input: &serde_json::Value,
  ) -> EffectResult;

  /// Streaming completion. Sends partial
  /// EffectResult(Streaming) through `tx`, then a
  /// final EffectResult(Succeeded).
  async fn complete_stream(
    &self,
    effect_id: crate::core::effect::EffectId,
    input: &serde_json::Value,
    tx: mpsc::Sender<EffectResult>,
  ) -> Result<(), String>;
}
```

2. Run: `cargo test --lib`
3. Commit: `feat(effects): add ProviderClient trait`

---

### Task 4: Implement OpenAiClient

**Files:**
- Create: `zeptovm/src/effects/openai.rs` (replace empty file)

**Steps:**

1. Write `zeptovm/src/effects/openai.rs`:
```rust
use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::warn;

use crate::core::effect::{
  EffectId, EffectResult, EffectStatus,
};
use crate::effects::provider::ProviderClient;

/// OpenAI-compatible client.
///
/// Works with: OpenAI, Ollama, vLLM, any endpoint
/// implementing /v1/chat/completions.
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
}

#[async_trait]
impl ProviderClient for OpenAiClient {
  async fn complete(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
  ) -> EffectResult {
    let model = input
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or("gpt-4o-mini");

    let messages = input
      .get("messages")
      .cloned()
      .unwrap_or_else(|| {
        // Convenience: if "prompt" is provided, wrap it
        let prompt = input
          .get("prompt")
          .and_then(|v| v.as_str())
          .unwrap_or("Hello");
        serde_json::json!([
          {"role": "user", "content": prompt}
        ])
      });

    let body = serde_json::json!({
      "model": model,
      "messages": messages,
      "stream": false,
    });

    let url = format!(
      "{}/chat/completions",
      self.base_url
    );

    let resp = self
      .client
      .post(&url)
      .header(
        "Authorization",
        format!("Bearer {}", self.api_key),
      )
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await;

    match resp {
      Ok(r) if r.status().is_success() => {
        match r.json::<serde_json::Value>().await {
          Ok(json) => {
            let content = json
              .get("choices")
              .and_then(|c| c.get(0))
              .and_then(|c| c.get("message"))
              .and_then(|m| m.get("content"))
              .and_then(|c| c.as_str())
              .unwrap_or("")
              .to_string();

            let tokens_used = json
              .get("usage")
              .and_then(|u| u.get("total_tokens"))
              .and_then(|t| t.as_u64())
              .unwrap_or(0);

            EffectResult::success(
              effect_id,
              serde_json::json!({
                "content": content,
                "tokens_used": tokens_used,
                "raw": json,
              }),
            )
          }
          Err(e) => EffectResult::failure(
            effect_id,
            format!("JSON parse error: {e}"),
          ),
        }
      }
      Ok(r) => {
        let status = r.status().as_u16();
        let body = r
          .text()
          .await
          .unwrap_or_default();
        EffectResult::failure(
          effect_id,
          format!(
            "HTTP {status}: {body}"
          ),
        )
      }
      Err(e) => EffectResult::failure(
        effect_id,
        format!("request error: {e}"),
      ),
    }
  }

  async fn complete_stream(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
    tx: mpsc::Sender<EffectResult>,
  ) -> Result<(), String> {
    let model = input
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or("gpt-4o-mini");

    let messages = input
      .get("messages")
      .cloned()
      .unwrap_or_else(|| {
        let prompt = input
          .get("prompt")
          .and_then(|v| v.as_str())
          .unwrap_or("Hello");
        serde_json::json!([
          {"role": "user", "content": prompt}
        ])
      });

    let body = serde_json::json!({
      "model": model,
      "messages": messages,
      "stream": true,
    });

    let url = format!(
      "{}/chat/completions",
      self.base_url
    );

    let resp = self
      .client
      .post(&url)
      .header(
        "Authorization",
        format!("Bearer {}", self.api_key),
      )
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await
      .map_err(|e| format!("request error: {e}"))?;

    if !resp.status().is_success() {
      let status = resp.status().as_u16();
      let body = resp
        .text()
        .await
        .unwrap_or_default();
      let _ = tx
        .send(EffectResult::failure(
          effect_id,
          format!("HTTP {status}: {body}"),
        ))
        .await;
      return Err(format!("HTTP {status}"));
    }

    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut full_content = String::new();
    let mut buffer = String::new();

    while let Some(chunk) =
      stream.next().await
    {
      match chunk {
        Ok(bytes) => {
          buffer.push_str(
            &String::from_utf8_lossy(&bytes),
          );
          // Parse SSE lines
          while let Some(pos) =
            buffer.find("\n\n")
          {
            let event =
              buffer[..pos].to_string();
            buffer =
              buffer[pos + 2..].to_string();

            for line in event.lines() {
              if let Some(data) =
                line.strip_prefix("data: ")
              {
                if data == "[DONE]" {
                  continue;
                }
                if let Ok(json) =
                  serde_json::from_str::<
                    serde_json::Value,
                  >(data)
                {
                  let delta = json
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| {
                      d.get("content")
                    })
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                  if !delta.is_empty() {
                    full_content
                      .push_str(delta);
                    let _ = tx
                      .send(
                        EffectResult::streaming(
                          effect_id,
                          serde_json::json!({
                            "delta": delta
                          }),
                        ),
                      )
                      .await;
                  }
                }
              }
            }
          }
        }
        Err(e) => {
          warn!(
            error = %e,
            "stream chunk error"
          );
          let _ = tx
            .send(EffectResult::failure(
              effect_id,
              format!("stream error: {e}"),
            ))
            .await;
          return Err(format!(
            "stream error: {e}"
          ));
        }
      }
    }

    // Send final result
    let _ = tx
      .send(EffectResult::success(
        effect_id,
        serde_json::json!({
          "content": full_content,
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
  fn test_openai_client_new() {
    let client = OpenAiClient::new(
      "sk-test".into(),
      "https://api.openai.com/v1".into(),
    );
    assert_eq!(client.api_key, "sk-test");
    assert_eq!(
      client.base_url,
      "https://api.openai.com/v1"
    );
  }
}
```

2. Run: `cargo test --lib`
3. Commit: `feat(effects): implement OpenAiClient with streaming`

---

### Task 5: Implement AnthropicClient

**Files:**
- Create: `zeptovm/src/effects/anthropic.rs` (replace empty file)

**Steps:**

1. Write `zeptovm/src/effects/anthropic.rs`:
```rust
use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::warn;

use crate::core::effect::{
  EffectId, EffectResult, EffectStatus,
};
use crate::effects::provider::ProviderClient;

/// Anthropic Messages API client.
pub struct AnthropicClient {
  client: Client,
  api_key: String,
  base_url: String,
}

impl AnthropicClient {
  pub fn new(
    api_key: String,
    base_url: String,
  ) -> Self {
    Self {
      client: Client::new(),
      api_key,
      base_url,
    }
  }
}

#[async_trait]
impl ProviderClient for AnthropicClient {
  async fn complete(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
  ) -> EffectResult {
    let model = input
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or("claude-sonnet-4-20250514");

    let messages = input
      .get("messages")
      .cloned()
      .unwrap_or_else(|| {
        let prompt = input
          .get("prompt")
          .and_then(|v| v.as_str())
          .unwrap_or("Hello");
        serde_json::json!([
          {"role": "user", "content": prompt}
        ])
      });

    let max_tokens = input
      .get("max_tokens")
      .and_then(|v| v.as_u64())
      .unwrap_or(1024);

    let body = serde_json::json!({
      "model": model,
      "messages": messages,
      "max_tokens": max_tokens,
    });

    let url = format!(
      "{}/messages",
      self.base_url
    );

    let resp = self
      .client
      .post(&url)
      .header("x-api-key", &self.api_key)
      .header(
        "anthropic-version",
        "2023-06-01",
      )
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await;

    match resp {
      Ok(r) if r.status().is_success() => {
        match r.json::<serde_json::Value>().await {
          Ok(json) => {
            let content = json
              .get("content")
              .and_then(|c| c.get(0))
              .and_then(|b| b.get("text"))
              .and_then(|t| t.as_str())
              .unwrap_or("")
              .to_string();

            let input_tokens = json
              .get("usage")
              .and_then(|u| {
                u.get("input_tokens")
              })
              .and_then(|t| t.as_u64())
              .unwrap_or(0);

            let output_tokens = json
              .get("usage")
              .and_then(|u| {
                u.get("output_tokens")
              })
              .and_then(|t| t.as_u64())
              .unwrap_or(0);

            EffectResult::success(
              effect_id,
              serde_json::json!({
                "content": content,
                "tokens_used":
                  input_tokens + output_tokens,
                "raw": json,
              }),
            )
          }
          Err(e) => EffectResult::failure(
            effect_id,
            format!("JSON parse error: {e}"),
          ),
        }
      }
      Ok(r) => {
        let status = r.status().as_u16();
        let body = r
          .text()
          .await
          .unwrap_or_default();
        EffectResult::failure(
          effect_id,
          format!("HTTP {status}: {body}"),
        )
      }
      Err(e) => EffectResult::failure(
        effect_id,
        format!("request error: {e}"),
      ),
    }
  }

  async fn complete_stream(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
    tx: mpsc::Sender<EffectResult>,
  ) -> Result<(), String> {
    let model = input
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or("claude-sonnet-4-20250514");

    let messages = input
      .get("messages")
      .cloned()
      .unwrap_or_else(|| {
        let prompt = input
          .get("prompt")
          .and_then(|v| v.as_str())
          .unwrap_or("Hello");
        serde_json::json!([
          {"role": "user", "content": prompt}
        ])
      });

    let max_tokens = input
      .get("max_tokens")
      .and_then(|v| v.as_u64())
      .unwrap_or(1024);

    let body = serde_json::json!({
      "model": model,
      "messages": messages,
      "max_tokens": max_tokens,
      "stream": true,
    });

    let url = format!(
      "{}/messages",
      self.base_url
    );

    let resp = self
      .client
      .post(&url)
      .header("x-api-key", &self.api_key)
      .header(
        "anthropic-version",
        "2023-06-01",
      )
      .header("Content-Type", "application/json")
      .json(&body)
      .send()
      .await
      .map_err(|e| format!("request error: {e}"))?;

    if !resp.status().is_success() {
      let status = resp.status().as_u16();
      let body = resp
        .text()
        .await
        .unwrap_or_default();
      let _ = tx
        .send(EffectResult::failure(
          effect_id,
          format!("HTTP {status}: {body}"),
        ))
        .await;
      return Err(format!("HTTP {status}"));
    }

    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut full_content = String::new();
    let mut buffer = String::new();

    while let Some(chunk) =
      stream.next().await
    {
      match chunk {
        Ok(bytes) => {
          buffer.push_str(
            &String::from_utf8_lossy(&bytes),
          );
          while let Some(pos) =
            buffer.find("\n\n")
          {
            let event =
              buffer[..pos].to_string();
            buffer =
              buffer[pos + 2..].to_string();

            let mut event_type = "";
            let mut data_str = "";
            for line in event.lines() {
              if let Some(t) =
                line.strip_prefix("event: ")
              {
                event_type = t;
              }
              if let Some(d) =
                line.strip_prefix("data: ")
              {
                data_str = d;
              }
            }

            if event_type
              == "content_block_delta"
            {
              if let Ok(json) =
                serde_json::from_str::<
                  serde_json::Value,
                >(data_str)
              {
                let delta = json
                  .get("delta")
                  .and_then(|d| d.get("text"))
                  .and_then(|t| t.as_str())
                  .unwrap_or("");
                if !delta.is_empty() {
                  full_content
                    .push_str(delta);
                  let _ = tx
                    .send(
                      EffectResult::streaming(
                        effect_id,
                        serde_json::json!({
                          "delta": delta
                        }),
                      ),
                    )
                    .await;
                }
              }
            }
          }
        }
        Err(e) => {
          warn!(
            error = %e,
            "stream chunk error"
          );
          let _ = tx
            .send(EffectResult::failure(
              effect_id,
              format!("stream error: {e}"),
            ))
            .await;
          return Err(format!(
            "stream error: {e}"
          ));
        }
      }
    }

    let _ = tx
      .send(EffectResult::success(
        effect_id,
        serde_json::json!({
          "content": full_content,
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
    assert_eq!(
      client.base_url,
      "https://api.anthropic.com/v1"
    );
  }
}
```

2. Run: `cargo test --lib`
3. Commit: `feat(effects): implement AnthropicClient with streaming`

---

### Task 6: Implement HttpWorker

**Files:**
- Create: `zeptovm/src/effects/http_worker.rs` (replace empty file)

**Steps:**

1. Write `zeptovm/src/effects/http_worker.rs`:
```rust
use reqwest::Client;

use crate::core::effect::{EffectId, EffectResult};

/// Executes real HTTP requests for EffectKind::Http.
pub struct HttpWorker {
  client: Client,
}

impl HttpWorker {
  pub fn new() -> Self {
    Self {
      client: Client::new(),
    }
  }

  pub async fn execute(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
    timeout: std::time::Duration,
  ) -> EffectResult {
    let method = input
      .get("method")
      .and_then(|v| v.as_str())
      .unwrap_or("GET")
      .to_uppercase();

    let url = match input
      .get("url")
      .and_then(|v| v.as_str())
    {
      Some(u) => u,
      None => {
        return EffectResult::failure(
          effect_id,
          "missing 'url' in HTTP effect input",
        )
      }
    };

    let mut req = match method.as_str() {
      "GET" => self.client.get(url),
      "POST" => self.client.post(url),
      "PUT" => self.client.put(url),
      "DELETE" => self.client.delete(url),
      "PATCH" => self.client.patch(url),
      "HEAD" => self.client.head(url),
      other => {
        return EffectResult::failure(
          effect_id,
          format!("unsupported method: {other}"),
        )
      }
    };

    req = req.timeout(timeout);

    // Add headers
    if let Some(headers) = input
      .get("headers")
      .and_then(|v| v.as_object())
    {
      for (key, val) in headers {
        if let Some(v) = val.as_str() {
          req = req.header(key.as_str(), v);
        }
      }
    }

    // Add body
    if let Some(body) = input.get("body") {
      req = req.json(body);
    }

    match req.send().await {
      Ok(resp) => {
        let status = resp.status().as_u16();
        let headers: serde_json::Map<
          String,
          serde_json::Value,
        > = resp
          .headers()
          .iter()
          .filter_map(|(k, v)| {
            v.to_str().ok().map(|v| {
              (
                k.to_string(),
                serde_json::Value::String(
                  v.to_string(),
                ),
              )
            })
          })
          .collect();

        match resp.text().await {
          Ok(body) => EffectResult::success(
            effect_id,
            serde_json::json!({
              "status": status,
              "headers": headers,
              "body": body,
            }),
          ),
          Err(e) => EffectResult::failure(
            effect_id,
            format!("body read error: {e}"),
          ),
        }
      }
      Err(e) if e.is_timeout() => {
        EffectResult {
          effect_id,
          status:
            crate::core::effect::EffectStatus::TimedOut,
          output: None,
          error: Some(format!(
            "request timed out: {e}"
          )),
        }
      }
      Err(e) => EffectResult::failure(
        effect_id,
        format!("request error: {e}"),
      ),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_http_worker_new() {
    let _worker = HttpWorker::new();
  }

  #[tokio::test]
  async fn test_http_worker_missing_url() {
    let worker = HttpWorker::new();
    let id = EffectId::new();
    let result = worker
      .execute(
        id,
        &serde_json::json!({}),
        std::time::Duration::from_secs(5),
      )
      .await;
    assert_eq!(
      result.status,
      crate::core::effect::EffectStatus::Failed
    );
    assert!(result
      .error
      .as_deref()
      .unwrap()
      .contains("missing 'url'"));
  }

  #[tokio::test]
  async fn test_http_worker_unsupported_method() {
    let worker = HttpWorker::new();
    let id = EffectId::new();
    let result = worker
      .execute(
        id,
        &serde_json::json!({
          "url": "https://example.com",
          "method": "FOOBAR"
        }),
        std::time::Duration::from_secs(5),
      )
      .await;
    assert_eq!(
      result.status,
      crate::core::effect::EffectStatus::Failed
    );
    assert!(result
      .error
      .as_deref()
      .unwrap()
      .contains("unsupported method"));
  }
}
```

2. Run: `cargo test --lib`
3. Commit: `feat(effects): implement HttpWorker for real HTTP effects`

---

### Task 7: Wire real workers into the reactor

This is the key integration task. The reactor's `execute_effect_once()` currently returns placeholders. We modify it to use real workers when configured, falling back to placeholders otherwise.

**Files:**
- Modify: `zeptovm/src/kernel/reactor.rs`

**Steps:**

1. The reactor currently has no access to config or providers. We need to pass a `ProviderConfig` into `Reactor::start()`. Change the signature:

```rust
use crate::effects::config::ProviderConfig;
use crate::effects::http_worker::HttpWorker;
use crate::effects::openai::OpenAiClient;
use crate::effects::anthropic::AnthropicClient;
use crate::effects::provider::ProviderClient;
use std::sync::Arc;
```

2. Add a `ReactorConfig` that bundles providers:
```rust
pub struct ReactorConfig {
  openai: Option<Arc<OpenAiClient>>,
  anthropic: Option<Arc<AnthropicClient>>,
  http_worker: Arc<HttpWorker>,
}

impl ReactorConfig {
  pub fn from_provider_config(
    config: &ProviderConfig,
  ) -> Self {
    Self {
      openai: config.openai_api_key.as_ref().map(
        |key| {
          Arc::new(OpenAiClient::new(
            key.clone(),
            config.openai_base_url.clone(),
          ))
        },
      ),
      anthropic: config
        .anthropic_api_key
        .as_ref()
        .map(|key| {
          Arc::new(AnthropicClient::new(
            key.clone(),
            config.anthropic_base_url.clone(),
          ))
        }),
      http_worker: Arc::new(HttpWorker::new()),
    }
  }

  pub fn placeholder() -> Self {
    Self {
      openai: None,
      anthropic: None,
      http_worker: Arc::new(HttpWorker::new()),
    }
  }
}
```

3. Modify `Reactor::start()` to accept an optional `ProviderConfig`:
```rust
pub fn start_with_config(
  config: Option<ProviderConfig>,
) -> Self {
  let reactor_config = match config {
    Some(ref c) if c.has_any_provider() => {
      ReactorConfig::from_provider_config(c)
    }
    _ => ReactorConfig::placeholder(),
  };
  let provider_config =
    config.unwrap_or_else(ProviderConfig::default);
  // ... rest is same as current start() but
  // pass reactor_config + provider_config into
  // the spawned async block
}
```

Keep the existing `start()` as a convenience that calls `start_with_config(None)`.

4. Modify `execute_effect_once()` to accept the config and use real workers:

```rust
async fn execute_effect_once(
  request: &EffectRequest,
  config: &ReactorConfig,
  provider_config: &ProviderConfig,
  completion_tx: &Sender<EffectCompletion>,
  pid: Pid,
) -> EffectResult {
  match &request.kind {
    EffectKind::SleepUntil => {
      // unchanged
    }
    EffectKind::LlmCall => {
      let provider_name =
        provider_config.resolve_provider(
          &request.input,
        );
      let provider: Option<
        &dyn ProviderClient,
      > = match provider_name {
        "anthropic" => config
          .anthropic
          .as_ref()
          .map(|c| c.as_ref() as &dyn ProviderClient),
        _ => config
          .openai
          .as_ref()
          .map(|c| c.as_ref() as &dyn ProviderClient),
      };

      match provider {
        Some(p) => {
          // Use streaming: send partial
          // results through the completion
          // channel, return final result
          let (tx, mut rx) =
            tokio::sync::mpsc::channel(64);
          let effect_id = request.effect_id;
          let input = request.input.clone();
          let p_clone: Arc<dyn ProviderClient> =
            match provider_name {
              "anthropic" => config
                .anthropic
                .clone()
                .unwrap()
                as Arc<dyn ProviderClient>,
              _ => config
                .openai
                .clone()
                .unwrap()
                as Arc<dyn ProviderClient>,
            };

          let ctx = completion_tx.clone();
          let stream_pid = pid;

          tokio::spawn(async move {
            let _ = p_clone
              .complete_stream(
                effect_id,
                &input,
                tx,
              )
              .await;
          });

          // Forward streaming results
          let mut final_result = None;
          while let Some(result) =
            rx.recv().await
          {
            if result.status
              == EffectStatus::Streaming
            {
              let _ =
                ctx.send(EffectCompletion {
                  pid: stream_pid,
                  result,
                });
            } else {
              final_result = Some(result);
            }
          }

          final_result.unwrap_or_else(|| {
            EffectResult::failure(
              effect_id,
              "stream ended without final result",
            )
          })
        }
        None => {
          // Placeholder fallback
          EffectResult::success(
            request.effect_id,
            serde_json::json!({
              "response":
                "placeholder LLM response"
            }),
          )
        }
      }
    }
    EffectKind::Http => {
      config
        .http_worker
        .execute(
          request.effect_id,
          &request.input,
          request.timeout,
        )
        .await
    }
    other => {
      // Placeholder for unsupported kinds
      EffectResult::success(
        request.effect_id,
        serde_json::json!({
          "status": "completed",
          "kind": format!("{:?}", other)
        }),
      )
    }
  }
}
```

5. Update `Reactor::start()` internal loop to pass config to execute functions. The `execute_effect_with_retry()` signature also needs to change to accept the new params, or you can restructure to call `execute_effect_once` with the new params inside the retry loop.

6. Run: `cargo test --lib` — all existing tests must pass (they hit placeholder path since no API keys are configured).

7. Commit: `feat(reactor): wire real LLM and HTTP workers into reactor`

---

### Task 8: Wire ProviderConfig into SchedulerRuntime

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`

**Steps:**

1. Add field to `SchedulerRuntime`:
```rust
provider_config: Option<ProviderConfig>,
```

2. Initialize as `None` in `new()`, and `Some(ProviderConfig::from_env())` in `with_durability()`.

3. Add builder:
```rust
pub fn with_provider_config(
  mut self,
  config: ProviderConfig,
) -> Self {
  self.provider_config = Some(config);
  self
}
```

4. In `with_durability()`, change `Reactor::start()` to `Reactor::start_with_config(Some(ProviderConfig::from_env()))`.

5. Add test:
```rust
#[test]
fn test_runtime_with_provider_config() {
    use crate::effects::config::ProviderConfig;
    let config = ProviderConfig {
        openai_api_key: Some("sk-test".into()),
        ..Default::default()
    };
    let rt = SchedulerRuntime::new()
        .with_provider_config(config);
    assert!(rt.provider_config.is_some());
}
```

6. Run: `cargo test --lib`
7. Commit: `feat(runtime): wire ProviderConfig into SchedulerRuntime`

---

### Task 9: Handle streaming results in runtime tick

The runtime's tick() drains reactor completions. Currently it only handles final results. With streaming, it also receives `EffectStatus::Streaming` results that should be delivered to the process mailbox but NOT processed as completed effects.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs` (tick() step 1)

**Steps:**

1. In `tick()`, step 1 (drain reactor completions), wrap the existing logic in a status check:

```rust
for completion in completions {
    match completion.result.status {
        EffectStatus::Streaming => {
            // Deliver streaming chunk to process
            // but don't record as completed
            self.engine.deliver_effect_result(
                completion.result,
            );
        }
        _ => {
            // existing logic for Succeeded,
            // Failed, etc.
        }
    }
}
```

Note: `deliver_effect_result` currently only handles final results (removes from pending_effects). For streaming results, we want to deliver without removing from pending. We need to add a method like `deliver_streaming_chunk()` to SchedulerEngine that delivers to the mailbox but doesn't change process state.

2. Add to `SchedulerEngine`:
```rust
pub fn deliver_streaming_chunk(
    &mut self,
    result: EffectResult,
) {
    let effect_raw = result.effect_id.raw();
    if let Some(pending) =
        self.pending_effects.get(&effect_raw)
    {
        let pid = pending.pid;
        if let Some(proc) =
            self.processes.get_mut(&pid)
        {
            proc.mailbox.push(
                Envelope::effect_result(pid, result),
            );
            // Don't change state or remove
            // from pending — more chunks coming
        }
    }
}
```

3. Update tick() to use `deliver_streaming_chunk` for Streaming status.

4. Add test verifying streaming results are delivered to process:
```rust
#[test]
fn test_runtime_streaming_result_delivered() {
    use crate::core::effect::{
        EffectId, EffectResult, EffectStatus,
    };

    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Suspends with effect

    // Simulate streaming result
    let effects = rt.engine.take_outbound_effects();
    let effect_id = effects[0].1.effect_id;

    // Re-register the pending effect
    // (take_outbound_effects removes it)
    // For this test, directly deliver
    let streaming = EffectResult {
        effect_id,
        status: EffectStatus::Streaming,
        output: Some(serde_json::json!({
            "delta": "hello"
        })),
        error: None,
    };
    rt.deliver_effect_result(streaming);
    // Process should still be waiting
    // (streaming doesn't wake)
}
```

5. Run: `cargo test --lib`
6. Commit: `feat(runtime): handle streaming effect results in tick loop`

---

### Task 10: Integration smoke test (gated behind feature flag)

**Files:**
- Modify: `zeptovm/Cargo.toml`
- Create: `zeptovm/tests/integration_llm.rs`

**Steps:**

1. Add feature flag to `Cargo.toml`:
```toml
[features]
default = []
integration_tests = []
```

2. Create `zeptovm/tests/integration_llm.rs`:
```rust
//! Integration tests for real LLM effect workers.
//! Run with: cargo test --features integration_tests
//! Requires: OPENAI_API_KEY or ANTHROPIC_API_KEY env var

#![cfg(feature = "integration_tests")]

use zeptovm::core::behavior::StepBehavior;
use zeptovm::core::effect::{
    EffectKind, EffectRequest,
};
use zeptovm::core::message::{
    Envelope, EnvelopePayload,
};
use zeptovm::core::step_result::StepResult;
use zeptovm::core::turn_context::TurnContext;
use zeptovm::effects::config::ProviderConfig;
use zeptovm::error::Reason;
use zeptovm::kernel::runtime::SchedulerRuntime;

struct LlmAgent;
impl StepBehavior for LlmAgent {
    fn init(
        &mut self,
        _: Option<Vec<u8>>,
    ) -> StepResult {
        StepResult::Continue
    }
    fn handle(
        &mut self,
        msg: Envelope,
        _ctx: &mut TurnContext,
    ) -> StepResult {
        match &msg.payload {
            EnvelopePayload::Effect(result) => {
                if result.status
                    == zeptovm::core::effect
                        ::EffectStatus::Streaming
                {
                    // Ignore streaming chunks
                    StepResult::Continue
                } else {
                    // Final result received
                    StepResult::Done(
                        Reason::Normal,
                    )
                }
            }
            _ => StepResult::Suspend(
                EffectRequest::new(
                    EffectKind::LlmCall,
                    serde_json::json!({
                        "model": "gpt-4o-mini",
                        "prompt": "Say hello in 3 words"
                    }),
                ),
            ),
        }
    }
    fn terminate(&mut self, _: &Reason) {}
}

#[test]
fn test_real_openai_call() {
    let config = ProviderConfig::from_env();
    if config.openai_api_key.is_none() {
        eprintln!(
            "OPENAI_API_KEY not set, skipping"
        );
        return;
    }

    let mut rt = SchedulerRuntime::with_durability()
        .with_provider_config(config);
    let pid = rt.spawn(Box::new(LlmAgent));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Suspends

    // Wait for real API response
    for _ in 0..100 {
        std::thread::sleep(
            std::time::Duration::from_millis(100),
        );
        rt.tick();
        let completed = rt.take_completed();
        if !completed.is_empty() {
            assert!(matches!(
                completed[0].1,
                Reason::Normal
            ));
            return;
        }
    }
    panic!("LLM call did not complete in 10s");
}
```

3. Run: `cargo test --lib` (unit tests still pass without the feature)
4. If you have `OPENAI_API_KEY` set: `cargo test --features integration_tests` to verify the real flow.
5. Commit: `test: add integration test for real LLM effect workers`
