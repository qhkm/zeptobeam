use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use tracing::{info_span, Instrument};

use crate::core::effect::{
  EffectId, EffectKind, EffectRequest, EffectResult,
  EffectState, EffectStatus,
};
use crate::effects::anthropic::AnthropicClient;
use crate::effects::config::ProviderConfig;
use crate::effects::http_worker::HttpWorker;
use crate::effects::openai::OpenAiClient;
use crate::effects::provider::ProviderClient;
use crate::pid::Pid;

/// Configuration for the reactor's real LLM and HTTP workers.
///
/// When API keys are present, the reactor delegates to real
/// provider clients. Otherwise it falls back to placeholder
/// responses (identical to pre-integration behavior).
pub struct ReactorConfig {
  openai: Option<Arc<OpenAiClient>>,
  anthropic: Option<Arc<AnthropicClient>>,
  http_worker: Arc<HttpWorker>,
  provider_config: ProviderConfig,
  /// When true, HTTP effects use the real HttpWorker.
  /// When false (placeholder mode), all effects return
  /// placeholder responses for backward compatibility.
  use_real_http: bool,
}

impl ReactorConfig {
  /// Build from an existing `ProviderConfig`, creating real
  /// clients for any provider that has an API key.
  pub fn from_provider_config(
    config: &ProviderConfig,
  ) -> Self {
    Self {
      openai: config.openai_api_key.as_ref().map(|key| {
        Arc::new(OpenAiClient::new(
          key.clone(),
          config.openai_base_url.clone(),
        ))
      }),
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
      provider_config: config.clone(),
      use_real_http: true,
    }
  }

  /// Build a placeholder config with no real providers.
  /// All effects return placeholder responses.
  pub fn placeholder() -> Self {
    Self {
      openai: None,
      anthropic: None,
      http_worker: Arc::new(HttpWorker::new()),
      provider_config: ProviderConfig::default(),
      use_real_http: false,
    }
  }
}

/// A request sent to the reactor for dispatch.
pub struct EffectDispatch {
  pub pid: Pid,
  pub request: EffectRequest,
}

/// A completed effect returned from the reactor.
#[derive(Debug)]
pub struct EffectCompletion {
  pub pid: Pid,
  pub result: EffectResult,
}

/// Message from reactor to runtime. Carries either a
/// state transition update or a final completion.
#[derive(Debug)]
pub enum ReactorMessage {
  /// Intermediate state change (Dispatched, Retrying,
  /// Streaming).
  StateChanged {
    effect_id: EffectId,
    pid: Pid,
    new_state: EffectState,
  },
  /// Final or streaming completion (existing type).
  Completion(EffectCompletion),
}

/// The reactor bridge between the sync scheduler and async effect
/// execution.
///
/// The scheduler sends `EffectRequest`s via the dispatch channel.
/// The reactor runs them on a shared Tokio runtime.
/// Completed effects come back via the completion channel.
pub struct Reactor {
  dispatch_tx: Sender<EffectDispatch>,
  completion_rx: Receiver<ReactorMessage>,
  _handle: thread::JoinHandle<()>,
}

impl Reactor {
  /// Start the reactor on a background thread with a Tokio
  /// runtime, using placeholder responses for all effects.
  pub fn start() -> Self {
    Self::start_with_config(None)
  }

  /// Start the reactor with an optional `ProviderConfig`.
  ///
  /// When a config with API keys is supplied, LLM calls are
  /// routed to real provider clients (OpenAI / Anthropic) and
  /// HTTP effects use the real `HttpWorker`. When no config
  /// (or no keys) are present, placeholder responses are
  /// returned — preserving backward-compatible behavior.
  pub fn start_with_config(
    config: Option<ProviderConfig>,
  ) -> Self {
    let reactor_config = Arc::new(match config {
      Some(ref c) if c.has_any_provider() => {
        ReactorConfig::from_provider_config(c)
      }
      _ => ReactorConfig::placeholder(),
    });

    let (dispatch_tx, dispatch_rx) =
      unbounded::<EffectDispatch>();
    let (completion_tx, completion_rx) =
      unbounded::<ReactorMessage>();

    let handle = thread::spawn(move || {
      let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

      rt.block_on(async move {
        loop {
          // Use spawn_blocking to avoid blocking the async
          // runtime while waiting for the next dispatch.
          let rx = dispatch_rx.clone();
          let recv_result = tokio::task::spawn_blocking(
            move || rx.recv(),
          )
          .await;

          match recv_result {
            Ok(Ok(dispatch)) => {
              let tx = completion_tx.clone();
              let cfg = reactor_config.clone();
              let span = info_span!(
                "effect_dispatch",
                effect_id =
                  %dispatch.request.effect_id,
                kind = ?dispatch.request.kind,
              );
              let pid = dispatch.pid;
              tokio::spawn(
                async move {
                  let result =
                    execute_effect_with_retry_configured(
                      &dispatch.request,
                      &cfg,
                      &tx,
                      pid,
                    )
                    .await;
                  let _ = tx.send(
                    ReactorMessage::Completion(
                      EffectCompletion { pid, result },
                    ),
                  );
                }
                .instrument(span),
              );
            }
            _ => break, // channel closed or join error
          }
        }
      });
    });

    Self {
      dispatch_tx,
      completion_rx,
      _handle: handle,
    }
  }

  /// Send an effect request to the reactor for async execution.
  pub fn dispatch(&self, pid: Pid, request: EffectRequest) {
    let _ =
      self.dispatch_tx.send(EffectDispatch { pid, request });
  }

  /// Try to receive a reactor message (non-blocking).
  pub fn try_recv(&self) -> Option<ReactorMessage> {
    self.completion_rx.try_recv().ok()
  }

  /// Drain all available reactor messages.
  pub fn drain_messages(&self) -> Vec<ReactorMessage> {
    let mut results = Vec::new();
    while let Some(msg) = self.try_recv() {
      results.push(msg);
    }
    results
  }

  /// Drain only completion messages (backward compat).
  #[deprecated(
    note = "Use drain_messages() to receive both state transitions and completions"
  )]
  pub fn drain_completions(&self) -> Vec<EffectCompletion> {
    let mut results = Vec::new();
    while let Some(msg) = self.try_recv() {
      match msg {
        ReactorMessage::Completion(c) => {
          results.push(c);
        }
        ReactorMessage::StateChanged { .. } => {
          // Skip state-change messages in legacy drain
        }
      }
    }
    results
  }

  /// Get a clone of the dispatch sender (for multi-thread use).
  pub fn dispatch_sender(&self) -> Sender<EffectDispatch> {
    self.dispatch_tx.clone()
  }
}

/// Execute an effect with retry logic based on the request's
/// RetryPolicy. Retained for backward-compatible tests.
#[cfg(test)]
async fn execute_effect_with_retry(
  request: &EffectRequest,
) -> EffectResult {
  let max_attempts = request.retry.max_attempts.max(1);
  let mut last_error = String::new();

  for attempt in 0..max_attempts {
    let result = execute_effect_once(request).await;

    match result.status {
      EffectStatus::Succeeded => return result,
      EffectStatus::Failed => {
        last_error =
          result.error.clone().unwrap_or_default();

        // Check retry_on filter
        if !request.retry.retry_on.is_empty()
          && !request
            .retry
            .retry_on
            .iter()
            .any(|cat| last_error.contains(cat))
        {
          return result; // Error category not retryable
        }

        if attempt + 1 < max_attempts {
          let delay =
            request.retry.backoff.delay_ms(attempt);
          if delay > 0 {
            tokio::time::sleep(
              std::time::Duration::from_millis(delay),
            )
            .await;
          }
        } else {
          return result; // Final attempt failed
        }
      }
      _ => return result, // TimedOut, Cancelled — don't retry
    }
  }

  EffectResult::failure(request.effect_id, last_error)
}

/// Execute an effect asynchronously. For v1, most effects return
/// placeholder results. Retained for backward-compatible tests.
#[cfg(test)]
async fn execute_effect_once(
  request: &EffectRequest,
) -> EffectResult {
  match &request.kind {
    EffectKind::SleepUntil => {
      tokio::time::sleep(request.timeout).await;
      EffectResult::success(
        request.effect_id,
        serde_json::json!({
          "slept_ms": request.timeout.as_millis()
        }),
      )
    }
    EffectKind::LlmCall => EffectResult::success(
      request.effect_id,
      serde_json::json!({
        "response": "placeholder LLM response"
      }),
    ),
    EffectKind::Http => EffectResult::success(
      request.effect_id,
      serde_json::json!({
        "status": 200,
        "body": "placeholder"
      }),
    ),
    other => EffectResult::success(
      request.effect_id,
      serde_json::json!({
        "status": "completed",
        "kind": format!("{:?}", other)
      }),
    ),
  }
}

// ---- Configured variants (used by start_with_config) ----

/// Execute an effect with retry logic, routing through real
/// providers when configured.
async fn execute_effect_with_retry_configured(
  request: &EffectRequest,
  config: &ReactorConfig,
  completion_tx: &Sender<ReactorMessage>,
  pid: Pid,
) -> EffectResult {
  let max_attempts = request.retry.max_attempts.max(1);
  let mut last_error = String::new();

  // Signal Dispatched state before first attempt
  let now_ms = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis() as u64;
  let _ =
    completion_tx.send(ReactorMessage::StateChanged {
      effect_id: request.effect_id,
      pid,
      new_state: EffectState::Dispatched {
        dispatched_at_ms: now_ms,
      },
    });

  for attempt in 0..max_attempts {
    let result = execute_effect_once_configured(
      request,
      config,
      completion_tx,
      pid,
    )
    .await;

    match result.status {
      EffectStatus::Succeeded => return result,
      EffectStatus::Failed => {
        last_error =
          result.error.clone().unwrap_or_default();

        // Check retry_on filter
        if !request.retry.retry_on.is_empty()
          && !request
            .retry
            .retry_on
            .iter()
            .any(|cat| last_error.contains(cat))
        {
          return result; // Error category not retryable
        }

        if attempt + 1 < max_attempts {
          let delay =
            request.retry.backoff.delay_ms(attempt);
          if delay > 0 {
            let retry_at = std::time::SystemTime::now()
              .duration_since(std::time::UNIX_EPOCH)
              .unwrap_or_default()
              .as_millis() as u64
              + delay;
            let _ = completion_tx.send(
              ReactorMessage::StateChanged {
                effect_id: request.effect_id,
                pid,
                new_state: EffectState::Retrying {
                  attempt: attempt + 1,
                  next_at_ms: retry_at,
                },
              },
            );
            tokio::time::sleep(
              std::time::Duration::from_millis(delay),
            )
            .await;
          }
        } else {
          return result; // Final attempt failed
        }
      }
      _ => return result, // TimedOut, Cancelled — no retry
    }
  }

  EffectResult::failure(request.effect_id, last_error)
}

/// Execute a single effect attempt, using real providers when
/// API keys are configured, falling back to placeholders
/// otherwise.
async fn execute_effect_once_configured(
  request: &EffectRequest,
  config: &ReactorConfig,
  completion_tx: &Sender<ReactorMessage>,
  pid: Pid,
) -> EffectResult {
  match &request.kind {
    EffectKind::SleepUntil => {
      tokio::time::sleep(request.timeout).await;
      EffectResult::success(
        request.effect_id,
        serde_json::json!({
          "slept_ms": request.timeout.as_millis()
        }),
      )
    }

    EffectKind::LlmCall => {
      let provider_name = config
        .provider_config
        .resolve_provider(&request.input);

      // Select the appropriate provider client.
      let provider: Option<Arc<dyn ProviderClient>> =
        match provider_name {
          "anthropic" => config
            .anthropic
            .clone()
            .map(|c| c as Arc<dyn ProviderClient>),
          _ => config
            .openai
            .clone()
            .map(|c| c as Arc<dyn ProviderClient>),
        };

      match provider {
        Some(p) => {
          // Default to streaming for LLM calls unless
          // explicitly disabled.
          let use_streaming = request
            .input
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

          if use_streaming {
            let (tx, mut rx) =
              tokio::sync::mpsc::channel(64);
            let effect_id = request.effect_id;
            let input = request.input.clone();

            tokio::spawn(async move {
              let _ = p
                .complete_stream(effect_id, &input, tx)
                .await;
            });

            let mut chunk_count: u32 = 0;
            let mut final_result = None;
            while let Some(result) = rx.recv().await {
              if result.status == EffectStatus::Streaming
              {
                chunk_count += 1;
                // Signal streaming state change
                let _ = completion_tx.send(
                  ReactorMessage::StateChanged {
                    effect_id,
                    pid,
                    new_state:
                      EffectState::Streaming {
                        chunks_received: chunk_count,
                      },
                  },
                );
                // Forward streaming deltas to the
                // completion channel.
                let _ = completion_tx.send(
                  ReactorMessage::Completion(
                    EffectCompletion { pid, result },
                  ),
                );
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
          } else {
            p.complete(request.effect_id, &request.input)
              .await
          }
        }
        None => {
          // No API key configured — placeholder fallback.
          EffectResult::success(
            request.effect_id,
            serde_json::json!({
              "response": "placeholder LLM response"
            }),
          )
        }
      }
    }

    EffectKind::Http => {
      if config.use_real_http {
        // Real HTTP worker (no API key required).
        config
          .http_worker
          .execute(
            request.effect_id,
            &request.input,
            request.timeout,
          )
          .await
      } else {
        // Placeholder fallback.
        EffectResult::success(
          request.effect_id,
          serde_json::json!({
            "status": 200,
            "body": "placeholder"
          }),
        )
      }
    }

    other => {
      // Placeholder for other effect kinds.
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::effect::{
    EffectKind, EffectRequest, EffectStatus,
  };
  use crate::pid::Pid;
  use std::time::Duration;

  #[test]
  fn test_reactor_start_and_dispatch() {
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);
    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({"prompt": "test"}),
    );
    let effect_id = req.effect_id;
    reactor.dispatch(pid, req);

    // Wait for result — skip StateChanged messages
    let mut result = None;
    for _ in 0..100 {
      if let Some(msg) = reactor.try_recv() {
        if let ReactorMessage::Completion(completion) =
          msg
        {
          result = Some(completion);
          break;
        }
        // StateChanged messages are expected, skip
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    let completion =
      result.expect("should receive completion");
    assert_eq!(completion.pid, pid);
    assert_eq!(completion.result.effect_id, effect_id);
    assert_eq!(
      completion.result.status,
      EffectStatus::Succeeded
    );
  }

  #[test]
  fn test_reactor_multiple_effects() {
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);

    // Dispatch 5 effects
    let mut ids = Vec::new();
    for i in 0..5 {
      let req = EffectRequest::new(
        EffectKind::Http,
        serde_json::json!({
          "url": format!("https://api.example.com/{i}")
        }),
      );
      ids.push(req.effect_id);
      reactor.dispatch(pid, req);
    }

    // Collect all completion results (skip StateChanged)
    let mut received = 0;
    for _ in 0..200 {
      if let Some(msg) = reactor.try_recv() {
        if matches!(
          msg,
          ReactorMessage::Completion(_)
        ) {
          received += 1;
          if received == 5 {
            break;
          }
        }
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(received, 5);
  }

  #[test]
  fn test_reactor_sleep_effect() {
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);
    let req = EffectRequest::new(
      EffectKind::SleepUntil,
      serde_json::json!({}),
    )
    .with_timeout(Duration::from_millis(50));
    let effect_id = req.effect_id;
    reactor.dispatch(pid, req);

    // Should take at least 50ms — skip StateChanged
    let start = std::time::Instant::now();
    let mut result = None;
    for _ in 0..100 {
      if let Some(msg) = reactor.try_recv() {
        if let ReactorMessage::Completion(completion) =
          msg
        {
          result = Some(completion);
          break;
        }
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    let elapsed = start.elapsed();

    let completion =
      result.expect("should receive sleep completion");
    assert_eq!(completion.result.effect_id, effect_id);
    // Allow some tolerance
    assert!(elapsed >= Duration::from_millis(40));
  }

  #[test]
  #[allow(deprecated)]
  fn test_reactor_drain_completions() {
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);

    for _ in 0..3 {
      let req = EffectRequest::new(
        EffectKind::LlmCall,
        serde_json::json!({}),
      );
      reactor.dispatch(pid, req);
    }

    // Wait for all to complete
    std::thread::sleep(Duration::from_millis(100));
    let completions = reactor.drain_completions();
    assert_eq!(completions.len(), 3);
  }

  #[test]
  #[allow(deprecated)]
  fn test_reactor_custom_effect() {
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);
    let req = EffectRequest::new(
      EffectKind::Custom("my-tool".into()),
      serde_json::json!({"tool": "my-tool"}),
    );
    reactor.dispatch(pid, req);

    std::thread::sleep(Duration::from_millis(100));
    let completions = reactor.drain_completions();
    assert_eq!(completions.len(), 1);
    assert_eq!(
      completions[0].result.status,
      EffectStatus::Succeeded
    );
  }

  #[tokio::test]
  async fn test_execute_effect_with_retry_succeeds_first_try()
  {
    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({}),
    );
    let result =
      execute_effect_with_retry(&req).await;
    assert_eq!(result.status, EffectStatus::Succeeded);
  }

  #[test]
  #[allow(deprecated)]
  fn test_reactor_retry_integration() {
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);
    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({}),
    );
    reactor.dispatch(pid, req);
    std::thread::sleep(Duration::from_millis(100));
    let completions = reactor.drain_completions();
    assert_eq!(completions.len(), 1);
  }

  #[test]
  fn test_reactor_drain_messages_includes_state_changes() {
    let reactor = Reactor::start();
    let request = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({"prompt": "hi"}),
    );
    let pid = Pid::from_raw(1);
    reactor.dispatch(pid, request);
    std::thread::sleep(Duration::from_millis(100));

    let messages = reactor.drain_messages();
    let has_state_changed = messages.iter().any(|m| {
      matches!(
        m,
        ReactorMessage::StateChanged {
          new_state: EffectState::Dispatched { .. },
          ..
        }
      )
    });
    let has_completion = messages.iter().any(|m| {
      matches!(m, ReactorMessage::Completion(_))
    });
    assert!(
      has_state_changed,
      "should have StateChanged::Dispatched"
    );
    assert!(has_completion, "should have Completion");
  }
}
