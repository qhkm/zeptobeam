use crate::agent_rt::{rate_limiter::RateLimiter, types::*};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::error::Error as StdError;

const REQUEST_QUEUE_SIZE: usize = 1024;
const RESPONSE_QUEUE_SIZE: usize = 4096;
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION_HEADER: &str = "2023-06-01";
const LLM_REQUEST_KIND: &str = "llm_request";

#[derive(Clone, Debug)]
pub(crate) struct LlmConfig {
  pub openai_api_key: Option<String>,
  pub anthropic_api_key: Option<String>,
  pub openai_base_url: String,
  pub anthropic_base_url: String,
}

impl LlmConfig {
  pub fn from_env() -> Self {
    Self {
      openai_api_key: std::env::var("OPENAI_API_KEY").ok(),
      anthropic_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
      openai_base_url: std::env::var("OPENAI_API_URL")
        .unwrap_or_else(|_| OPENAI_DEFAULT_BASE_URL.to_string()),
      anthropic_base_url: std::env::var("ANTHROPIC_API_URL")
        .unwrap_or_else(|_| ANTHROPIC_DEFAULT_BASE_URL.to_string()),
    }
  }
}

/// Request sent from scheduler thread to Tokio thread
/// pool.
#[derive(Debug)]
pub struct IoRequest {
  pub correlation_id: u64,
  pub source_pid: AgentPid,
  pub op: IoOp,
}

/// Response sent from Tokio thread pool back to
/// scheduler thread.
#[derive(Debug)]
pub struct IoResponse {
  pub correlation_id: u64,
  pub source_pid: AgentPid,
  pub result: IoResult,
}

/// Scheduler-side handle for submitting I/O operations
/// and draining responses.
pub struct BridgeHandle {
  request_tx: Sender<IoRequest>,
  response_rx: Receiver<IoResponse>,
  pub next_correlation_id: u64,
}

/// Tokio-side worker that receives I/O requests
/// and executes them asynchronously.
pub struct BridgeWorker {
  request_rx: Receiver<IoRequest>,
  response_tx: Sender<IoResponse>,
  rate_limiter: Option<RateLimiter>,
  llm_config: LlmConfig,
}

/// Create a bridge pair: one handle for the scheduler
/// thread and one worker for the Tokio runtime.
pub fn create_bridge() -> (BridgeHandle, BridgeWorker) {
  let (req_tx, req_rx) = bounded(REQUEST_QUEUE_SIZE);
  let (resp_tx, resp_rx) = bounded(RESPONSE_QUEUE_SIZE);
  (
    BridgeHandle {
      request_tx: req_tx,
      response_rx: resp_rx,
      next_correlation_id: 0,
    },
    BridgeWorker {
      request_rx: req_rx,
      response_tx: resp_tx,
      rate_limiter: None,
      llm_config: LlmConfig::from_env(),
    },
  )
}

impl BridgeHandle {
  /// Submit an I/O operation. Returns the correlation ID
  /// on success, or an error string if the queue is full.
  pub fn submit(&mut self, pid: AgentPid, op: IoOp) -> Result<u64, String> {
    let cid = self.next_correlation_id;
    let req = IoRequest {
      correlation_id: cid,
      source_pid: pid,
      op,
    };
    match self.request_tx.try_send(req) {
      Ok(()) => {
        self.next_correlation_id += 1;
        Ok(cid)
      }
      Err(TrySendError::Full(_)) => Err("request queue full".into()),
      Err(TrySendError::Disconnected(_)) => Err("bridge worker disconnected".into()),
    }
  }

  /// Non-blocking drain of all pending responses.
  /// Each response is wrapped into
  /// `(AgentPid, Message::System(SystemMsg::IoResponse))`.
  pub fn drain_responses(&self) -> Vec<(AgentPid, Message)> {
    let mut out = Vec::new();
    loop {
      match self.response_rx.try_recv() {
        Ok(resp) => {
          let msg = Message::System(SystemMsg::IoResponse {
            correlation_id: resp.correlation_id,
            result: resp.result,
          });
          out.push((resp.source_pid, msg));
        }
        Err(_) => break,
      }
    }
    out
  }
}

impl BridgeWorker {
  /// Attach a rate limiter. Must be called before `run`.
  pub fn set_rate_limiter(&mut self, rl: RateLimiter) {
    self.rate_limiter = Some(rl);
  }

  /// Run the worker loop. Receives requests from the
  /// scheduler via crossbeam, spawns a Tokio task for
  /// each, and sends the result back. Uses
  /// `spawn_blocking` so the blocking crossbeam recv
  /// does not starve the Tokio runtime.
  pub async fn run(self) {
    let req_rx = self.request_rx;
    let resp_tx = self.response_tx;
    let rate_limiter = self.rate_limiter;
    let llm_config = self.llm_config;
    loop {
      let rx = req_rx.clone();
      let recv_result = tokio::task::spawn_blocking(move || rx.recv()).await;
      let req = match recv_result {
        Ok(Ok(r)) => r,
        // Channel disconnected or join error: stop.
        _ => break,
      };
      let tx = resp_tx.clone();
      let rl = rate_limiter.clone();
      let llm_cfg = llm_config.clone();
      tokio::spawn(async move {
        let result = execute_io_op_with_config(&req.op, rl.as_ref(), &llm_cfg).await;
        let resp = IoResponse {
          correlation_id: req.correlation_id,
          source_pid: req.source_pid,
          result,
        };
        let _ = tx.send(resp);
      });
    }
  }
}

/// Execute an I/O operation asynchronously.
pub(crate) async fn execute_io_op(
  op: &IoOp,
  rate_limiter: Option<&RateLimiter>,
) -> IoResult {
  let cfg = LlmConfig::from_env();
  execute_io_op_with_config(op, rate_limiter, &cfg).await
}

pub(crate) async fn execute_io_op_with_config(
  op: &IoOp,
  rate_limiter: Option<&RateLimiter>,
  llm_config: &LlmConfig,
) -> IoResult {
  match op {
    IoOp::Timer { duration } => {
      tokio::time::sleep(*duration).await;
      IoResult::Ok(serde_json::json!({
        "kind": "timer",
        "elapsed_ms": duration.as_millis() as u64
      }))
    }
    IoOp::HttpRequest {
      method,
      url,
      headers,
      body,
      timeout_ms,
    } => {
      execute_http_op(
        method.clone(),
        url.clone(),
        headers.clone(),
        body.clone(),
        *timeout_ms,
        rate_limiter,
      )
      .await
    }
    IoOp::LlmRequest {
      provider,
      model,
      prompt,
      system_prompt,
      max_tokens,
      temperature,
      timeout_ms,
    } => {
      execute_llm_request(
        provider,
        model,
        prompt,
        system_prompt.as_deref(),
        *max_tokens,
        *temperature,
        *timeout_ms,
        rate_limiter,
        llm_config,
      )
      .await
    }
    IoOp::Custom { kind, payload } => {
      if kind == LLM_REQUEST_KIND {
        match llm_request_from_custom_payload(payload) {
          Ok(IoOp::LlmRequest {
            provider,
            model,
            prompt,
            system_prompt,
            max_tokens,
            temperature,
            timeout_ms,
          }) => {
            return execute_llm_request(
              &provider,
              &model,
              &prompt,
              system_prompt.as_deref(),
              max_tokens,
              temperature,
              timeout_ms,
              rate_limiter,
              llm_config,
            )
            .await;
          }
          Ok(_) => {
            return IoResult::Error("invalid llm request payload mapping".to_string());
          }
          Err(err) => {
            return IoResult::Error(err);
          }
        }
      }
      // Custom ops return the payload as-is for now.
      // Real implementations will be pluggable.
      IoResult::Ok(serde_json::json!({
        "kind": kind,
        "payload": payload,
        "status": "placeholder"
      }))
    }
  }
}

async fn execute_http_op(
  method: String,
  url: String,
  headers: std::collections::HashMap<String, String>,
  body: Option<Vec<u8>>,
  timeout_ms: Option<u64>,
  rate_limiter: Option<&RateLimiter>,
) -> IoResult {
  let rl = rate_limiter.cloned();
  match tokio::task::spawn_blocking(move || {
    if let Some(ref rl) = rl {
      let provider = extract_provider(&url);
      rl.acquire_blocking(&provider);
    }
    execute_http_blocking(&method, &url, &headers, body.as_deref(), timeout_ms)
  })
  .await
  {
    Ok(result) => result,
    Err(e) => IoResult::Error(format!("http task panicked: {}", e)),
  }
}

#[allow(clippy::too_many_arguments)]
async fn execute_llm_request(
  provider: &str,
  model: &str,
  prompt: &str,
  system_prompt: Option<&str>,
  max_tokens: Option<u32>,
  temperature: Option<f32>,
  timeout_ms: Option<u64>,
  rate_limiter: Option<&RateLimiter>,
  llm_config: &LlmConfig,
) -> IoResult {
  match build_llm_http_request(
    provider,
    model,
    prompt,
    system_prompt,
    max_tokens,
    temperature,
    timeout_ms,
    llm_config,
  ) {
    Ok((method, url, headers, body, timeout_ms)) => {
      execute_http_op(method, url, headers, Some(body), timeout_ms, rate_limiter).await
    }
    Err(err) => IoResult::Error(err),
  }
}

#[allow(clippy::too_many_arguments)]
fn build_llm_http_request(
  provider: &str,
  model: &str,
  prompt: &str,
  system_prompt: Option<&str>,
  max_tokens: Option<u32>,
  temperature: Option<f32>,
  timeout_ms: Option<u64>,
  llm_config: &LlmConfig,
) -> Result<
  (
    String,
    String,
    std::collections::HashMap<String, String>,
    Vec<u8>,
    Option<u64>,
  ),
  String,
> {
  let provider_key = provider.to_ascii_lowercase();
  let mut headers = std::collections::HashMap::new();
  headers.insert("Content-Type".to_string(), "application/json".to_string());

  if provider_key == "openai" || provider_key.contains("openai") {
    let api_key = llm_config
      .openai_api_key
      .as_ref()
      .ok_or_else(|| "missing OPENAI_API_KEY for openai provider".to_string())?;
    headers.insert("Authorization".to_string(), format!("Bearer {}", api_key));

    let mut messages = Vec::new();
    if let Some(system) = system_prompt {
      messages.push(serde_json::json!({
        "role": "system",
        "content": system,
      }));
    }
    messages.push(serde_json::json!({
      "role": "user",
      "content": prompt,
    }));

    let mut body = serde_json::json!({
      "model": model,
      "messages": messages,
    });
    if let Some(max) = max_tokens {
      body["max_tokens"] = serde_json::json!(max);
    }
    if let Some(temp) = temperature {
      body["temperature"] = serde_json::json!(temp);
    }

    return Ok((
      "POST".to_string(),
      join_endpoint(&llm_config.openai_base_url, "/chat/completions"),
      headers,
      serde_json::to_vec(&body)
        .map_err(|e| format!("openai body encode failed: {}", e))?,
      timeout_ms,
    ));
  }

  if provider_key == "anthropic" || provider_key.contains("anthropic") {
    let api_key = llm_config
      .anthropic_api_key
      .as_ref()
      .ok_or_else(|| "missing ANTHROPIC_API_KEY for anthropic provider".to_string())?;
    headers.insert("x-api-key".to_string(), api_key.clone());
    headers.insert(
      "anthropic-version".to_string(),
      ANTHROPIC_VERSION_HEADER.to_string(),
    );

    let mut body = serde_json::json!({
      "model": model,
      "messages": [
        {
          "role": "user",
          "content": prompt,
        }
      ],
      "max_tokens": max_tokens.unwrap_or(1024),
    });
    if let Some(system) = system_prompt {
      body["system"] = serde_json::json!(system);
    }
    if let Some(temp) = temperature {
      body["temperature"] = serde_json::json!(temp);
    }

    return Ok((
      "POST".to_string(),
      join_endpoint(&llm_config.anthropic_base_url, "/messages"),
      headers,
      serde_json::to_vec(&body)
        .map_err(|e| format!("anthropic body encode failed: {}", e))?,
      timeout_ms,
    ));
  }

  Err(format!(
    "unsupported llm provider '{}'; expected openai or anthropic",
    provider
  ))
}

fn llm_request_from_custom_payload(payload: &serde_json::Value) -> Result<IoOp, String> {
  let task = payload.get("task").unwrap_or(payload);
  let provider = task
    .get("provider")
    .or_else(|| payload.get("provider"))
    .and_then(|v| v.as_str())
    .unwrap_or("openai")
    .to_string();

  let provider_key = provider.to_ascii_lowercase();
  let default_model = if provider_key == "anthropic" || provider_key.contains("anthropic")
  {
    "claude-3-5-sonnet-latest"
  } else {
    "gpt-4o-mini"
  };
  let model = task
    .get("model")
    .or_else(|| payload.get("model"))
    .and_then(|v| v.as_str())
    .unwrap_or(default_model)
    .to_string();

  let prompt = task
    .get("prompt")
    .or_else(|| payload.get("prompt"))
    .and_then(|v| v.as_str())
    .or_else(|| task.get("goal").and_then(|v| v.as_str()))
    .map(str::to_string)
    .unwrap_or_else(|| task.to_string());

  let system_prompt = task
    .get("system")
    .or_else(|| payload.get("system"))
    .and_then(|v| v.as_str())
    .map(str::to_string);

  let max_tokens = task
    .get("max_tokens")
    .or_else(|| payload.get("max_tokens"))
    .and_then(|v| v.as_u64())
    .and_then(|v| {
      if v <= u32::MAX as u64 {
        Some(v as u32)
      } else {
        None
      }
    });

  let temperature = task
    .get("temperature")
    .or_else(|| payload.get("temperature"))
    .and_then(|v| v.as_f64())
    .map(|v| v as f32);

  let timeout_ms = task
    .get("timeout_ms")
    .or_else(|| payload.get("timeout_ms"))
    .and_then(|v| v.as_u64());

  Ok(IoOp::LlmRequest {
    provider,
    model,
    prompt,
    system_prompt,
    max_tokens,
    temperature,
    timeout_ms,
  })
}

fn join_endpoint(base: &str, path: &str) -> String {
  if base.ends_with(path) {
    return base.to_string();
  }
  format!(
    "{}/{}",
    base.trim_end_matches('/'),
    path.trim_start_matches('/')
  )
}

fn execute_http_blocking(
  method: &str,
  url: &str,
  headers: &std::collections::HashMap<String, String>,
  body: Option<&[u8]>,
  timeout_ms: Option<u64>,
) -> IoResult {
  let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(30_000));
  let agent = ureq::AgentBuilder::new()
    .timeout_connect(timeout)
    .timeout_read(timeout)
    .build();

  let mut req = match method.to_uppercase().as_str() {
    "GET" => agent.get(url),
    "POST" => agent.post(url),
    "PUT" => agent.put(url),
    "DELETE" => agent.delete(url),
    "PATCH" => agent.request("PATCH", url),
    "HEAD" => agent.head(url),
    other => agent.request(other, url),
  };

  for (k, v) in headers {
    req = req.set(k, v);
  }

  let response = match body {
    Some(b) => req.send_bytes(b),
    None => req.call(),
  };

  match response {
    Ok(resp) => {
      let status = resp.status();
      let resp_body = resp.into_string().unwrap_or_default();
      IoResult::Ok(serde_json::json!({
        "status": status,
        "body": resp_body
      }))
    }
    Err(ureq::Error::Status(code, resp)) => {
      let resp_body = resp.into_string().unwrap_or_default();
      IoResult::Ok(serde_json::json!({
        "status": code,
        "body": resp_body
      }))
    }
    Err(ureq::Error::Transport(t)) => {
      if is_timeout_error(&t) {
        return IoResult::Timeout;
      }
      match t.kind() {
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => {
          IoResult::Error(format!("connection error: {}", t))
        }
        _ => IoResult::Error(format!("http error: {}", t)),
      }
    }
  }
}

fn is_timeout_error(t: &ureq::Transport) -> bool {
  let msg = t.to_string();
  if msg.contains("timed out") || msg.contains("Timeout") {
    return true;
  }
  if let Some(source) = t.source() {
    if let Some(io_err) = source.downcast_ref::<std::io::Error>() {
      return io_err.kind() == std::io::ErrorKind::TimedOut;
    }
  }
  false
}

/// Extract a provider key from a URL for rate limiting.
/// Uses the hostname (e.g., "api.openai.com").
pub(crate) fn extract_provider(url: &str) -> String {
  url
    .split("://")
    .nth(1)
    .unwrap_or(url)
    .split('/')
    .next()
    .unwrap_or(url)
    .split(':')
    .next()
    .unwrap_or(url)
    .to_string()
}
