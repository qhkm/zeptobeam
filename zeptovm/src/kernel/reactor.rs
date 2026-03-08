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
use crate::durability::artifact_store::ArtifactBackend;
use crate::kernel::approval_store::ApprovalStore;
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
  artifact_store: Option<Arc<dyn ArtifactBackend>>,
  approval_store: Option<ApprovalStore>,
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
      artifact_store: None,
      approval_store: None,
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
      artifact_store: None,
      approval_store: None,
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
  completion_tx: Sender<ReactorMessage>,
  approval_store: Option<ApprovalStore>,
  _handle: thread::JoinHandle<()>,
}

impl Reactor {
  /// Start the reactor on a background thread with a Tokio
  /// runtime, using placeholder responses for all effects.
  pub fn start() -> Self {
    Self::start_with_config(None, None, None)
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
    artifact_store: Option<Arc<dyn ArtifactBackend>>,
    approval_store: Option<ApprovalStore>,
  ) -> Self {
    let mut reactor_config = match config {
      Some(ref c) if c.has_any_provider() => {
        ReactorConfig::from_provider_config(c)
      }
      _ => ReactorConfig::placeholder(),
    };
    reactor_config.artifact_store = artifact_store;
    reactor_config.approval_store = approval_store.clone();
    let reactor_config = Arc::new(reactor_config);

    let (dispatch_tx, dispatch_rx) =
      unbounded::<EffectDispatch>();
    let (completion_tx, completion_rx) =
      unbounded::<ReactorMessage>();
    let external_completion_tx = completion_tx.clone();
    let external_approval_store = approval_store;

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
                  if result.status != EffectStatus::Deferred
                  {
                    let _ = tx.send(
                      ReactorMessage::Completion(
                        EffectCompletion { pid, result },
                      ),
                    );
                  }
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
      completion_tx: external_completion_tx,
      approval_store: external_approval_store,
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

  /// Get the approval store, if configured.
  pub fn approval_store(&self) -> Option<&ApprovalStore> {
    self.approval_store.as_ref()
  }

  /// Get a clone of the completion sender (for HTTP handlers).
  pub fn completion_sender(
    &self,
  ) -> Sender<ReactorMessage> {
    self.completion_tx.clone()
  }

  /// Resolve a pending approval and send the completion.
  /// Returns `true` if the approval was found and resolved.
  pub fn resolve_approval(
    &self,
    effect_id: u64,
    action: &str,
    payload: serde_json::Value,
  ) -> bool {
    let store = match &self.approval_store {
      Some(s) => s,
      None => return false,
    };

    let approval = match store.resolve(effect_id) {
      Some(a) => a,
      None => return false,
    };

    let result = match action {
      "approve" => EffectResult::success(
        approval.effect_id,
        serde_json::json!({"approved": true}),
      ),
      "deny" => {
        let reason = payload
          .get("reason")
          .and_then(|v| v.as_str())
          .unwrap_or("");
        EffectResult::success(
          approval.effect_id,
          serde_json::json!({
            "approved": false,
            "reason": reason,
          }),
        )
      }
      "respond" => {
        let response = payload
          .get("response")
          .cloned()
          .unwrap_or(serde_json::Value::Null);
        EffectResult::success(
          approval.effect_id,
          serde_json::json!({"response": response}),
        )
      }
      _ => EffectResult::failure(
        approval.effect_id,
        format!("unknown action: {action}"),
      ),
    };

    let _ = self.completion_tx.send(
      ReactorMessage::Completion(EffectCompletion {
        pid: approval.pid,
        result,
      }),
    );
    true
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

    EffectKind::ObjectPut => {
      if let Some(ref store) = config.artifact_store {
        let data_b64 = match request
          .input
          .get("data_base64")
          .and_then(|v| v.as_str())
        {
          Some(s) => s,
          None => {
            return EffectResult::failure(
              request.effect_id,
              "missing required field: data_base64",
            );
          }
        };
        let media_type = request
          .input
          .get("media_type")
          .and_then(|v| v.as_str())
          .unwrap_or("application/octet-stream");
        let ttl_ms = request
          .input
          .get("ttl_ms")
          .and_then(|v| v.as_u64());

        use base64::Engine;
        let data =
          base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| format!("base64 decode: {e}"));

        match data {
          Ok(bytes) => {
            match store.store(&bytes, media_type, ttl_ms)
            {
              Ok(obj_ref) => EffectResult::success(
                request.effect_id,
                serde_json::to_value(&obj_ref)
                  .unwrap_or_default(),
              ),
              Err(e) => EffectResult::failure(
                request.effect_id,
                e,
              ),
            }
          }
          Err(e) => {
            EffectResult::failure(request.effect_id, e)
          }
        }
      } else {
        EffectResult::failure(
          request.effect_id,
          "no artifact store configured",
        )
      }
    }

    EffectKind::ObjectFetch => {
      if let Some(ref store) = config.artifact_store {
        let obj_id = match request
          .input
          .get("object_id")
          .and_then(|v| v.as_u64())
        {
          Some(id) => id,
          None => {
            return EffectResult::failure(
              request.effect_id,
              "missing required field: object_id",
            );
          }
        };

        use crate::core::object::ObjectId;
        match store
          .retrieve(&ObjectId::from_raw(obj_id))
        {
          Ok((obj_ref, data)) => {
            use base64::Engine;
            let data_b64 =
              base64::engine::general_purpose::STANDARD
                .encode(&data);
            EffectResult::success(
              request.effect_id,
              serde_json::json!({
                "ref": obj_ref,
                "data_base64": data_b64,
              }),
            )
          }
          Err(e) => EffectResult::failure(
            request.effect_id,
            e,
          ),
        }
      } else {
        EffectResult::failure(
          request.effect_id,
          "no artifact store configured",
        )
      }
    }

    EffectKind::ObjectDelete => {
      if let Some(ref store) = config.artifact_store {
        let obj_id = match request
          .input
          .get("object_id")
          .and_then(|v| v.as_u64())
        {
          Some(id) => id,
          None => {
            return EffectResult::failure(
              request.effect_id,
              "missing required field: object_id",
            );
          }
        };

        use crate::core::object::ObjectId;
        match store.delete(&ObjectId::from_raw(obj_id))
        {
          Ok(existed) => EffectResult::success(
            request.effect_id,
            serde_json::json!({ "deleted": existed }),
          ),
          Err(e) => EffectResult::failure(
            request.effect_id,
            e,
          ),
        }
      } else {
        EffectResult::failure(
          request.effect_id,
          "no artifact store configured",
        )
      }
    }

    EffectKind::HumanApproval
    | EffectKind::HumanInput => {
      if let Some(ref store) = config.approval_store {
        let description = match request
          .input
          .get("description")
          .and_then(|v| v.as_str())
        {
          Some(s) => s.to_string(),
          None => {
            return EffectResult::failure(
              request.effect_id,
              "missing required field: description",
            );
          }
        };

        let now_ms = std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .unwrap_or_default()
          .as_millis() as u64;
        let expires_at_ms = now_ms
          .saturating_add(
            request.timeout.as_millis() as u64,
          );

        let approval =
          crate::kernel::approval_store::PendingApproval {
            effect_id: request.effect_id,
            pid,
            kind: request.kind.clone(),
            description,
            input: request.input.clone(),
            created_at_ms: now_ms,
            expires_at_ms,
          };

        store.insert(approval);

        // Spawn timeout task
        let timeout = request.timeout;
        let effect_id_raw = request.effect_id.raw();
        let effect_id = request.effect_id;
        let timeout_store = store.clone();
        let timeout_tx = completion_tx.clone();
        tokio::spawn(async move {
          tokio::time::sleep(timeout).await;
          if timeout_store.is_pending(effect_id_raw) {
            if let Some(_) =
              timeout_store.resolve(effect_id_raw)
            {
              let _ = timeout_tx.send(
                ReactorMessage::Completion(
                  EffectCompletion {
                    pid,
                    result: EffectResult::failure(
                      effect_id,
                      "approval timed out",
                    ),
                  },
                ),
              );
            }
          }
        });

        EffectResult {
          effect_id: request.effect_id,
          status: EffectStatus::Deferred,
          output: None,
          error: None,
        }
      } else {
        EffectResult::failure(
          request.effect_id,
          "no approval store configured",
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

  /// Helper: poll the reactor until a Completion message
  /// arrives, up to ~2 seconds.
  fn poll_for_completion(
    reactor: &Reactor,
  ) -> Option<EffectCompletion> {
    for _ in 0..200 {
      if let Some(msg) = reactor.try_recv() {
        if let ReactorMessage::Completion(c) = msg {
          return Some(c);
        }
        // skip StateChanged
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    None
  }

  #[test]
  fn test_reactor_object_put_and_fetch() {
    use crate::durability::artifact_store::{
      ArtifactBackend, SqliteArtifactStore,
    };

    let store: Arc<dyn ArtifactBackend> = Arc::new(
      SqliteArtifactStore::open_in_memory().unwrap(),
    );

    // Pass artifact store via start_with_config
    let reactor = Reactor::start_with_config(
      None,
      Some(store),
      None,
    );

    // ObjectPut — store base64-encoded data
    use base64::Engine;
    let data_b64 =
      base64::engine::general_purpose::STANDARD
        .encode(b"hello artifact");
    let put_req = EffectRequest::new(
      EffectKind::ObjectPut,
      serde_json::json!({
        "data_base64": data_b64,
        "media_type": "text/plain",
      }),
    );
    let pid = Pid::from_raw(1);
    reactor.dispatch(pid, put_req);

    let put_completion = poll_for_completion(&reactor)
      .expect("should get put completion");
    assert_eq!(
      put_completion.result.status,
      EffectStatus::Succeeded
    );

    let obj_id = put_completion
      .result
      .output
      .as_ref()
      .and_then(|v| v.get("object_id"))
      .and_then(|v| v.as_u64())
      .expect("should have object_id");

    // ObjectFetch
    let fetch_req = EffectRequest::new(
      EffectKind::ObjectFetch,
      serde_json::json!({ "object_id": obj_id }),
    );
    reactor.dispatch(pid, fetch_req);

    let fetch_completion =
      poll_for_completion(&reactor)
        .expect("should get fetch completion");
    assert_eq!(
      fetch_completion.result.status,
      EffectStatus::Succeeded
    );

    let fetched_b64 = fetch_completion
      .result
      .output
      .as_ref()
      .and_then(|v| v.get("data_base64"))
      .and_then(|v| v.as_str())
      .expect("should have data_base64");
    let decoded =
      base64::engine::general_purpose::STANDARD
        .decode(fetched_b64)
        .unwrap();
    assert_eq!(decoded, b"hello artifact");

    // Verify ref metadata is present
    assert!(
      fetch_completion
        .result
        .output
        .as_ref()
        .and_then(|v| v.get("ref"))
        .is_some()
    );
  }

  #[test]
  fn test_reactor_object_delete() {
    use crate::durability::artifact_store::{
      ArtifactBackend, SqliteArtifactStore,
    };

    let store: Arc<dyn ArtifactBackend> = Arc::new(
      SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let reactor = Reactor::start_with_config(
      None,
      Some(store),
      None,
    );

    // Store an artifact first
    use base64::Engine;
    let data_b64 =
      base64::engine::general_purpose::STANDARD
        .encode(b"to-delete");
    let put_req = EffectRequest::new(
      EffectKind::ObjectPut,
      serde_json::json!({
        "data_base64": data_b64,
        "media_type": "text/plain",
      }),
    );
    let pid = Pid::from_raw(1);
    reactor.dispatch(pid, put_req);

    let put_completion = poll_for_completion(&reactor)
      .expect("should get put completion");
    let obj_id = put_completion
      .result
      .output
      .as_ref()
      .and_then(|v| v.get("object_id"))
      .and_then(|v| v.as_u64())
      .expect("should get object_id from put");

    // ObjectDelete
    let del_req = EffectRequest::new(
      EffectKind::ObjectDelete,
      serde_json::json!({ "object_id": obj_id }),
    );
    reactor.dispatch(pid, del_req);

    let del_completion =
      poll_for_completion(&reactor)
        .expect("should get delete completion");
    assert_eq!(
      del_completion.result.status,
      EffectStatus::Succeeded
    );

    // Verify fetch now fails
    let fetch_req = EffectRequest::new(
      EffectKind::ObjectFetch,
      serde_json::json!({ "object_id": obj_id }),
    );
    reactor.dispatch(pid, fetch_req);

    let fetch_completion =
      poll_for_completion(&reactor)
        .expect("should get fetch-after-delete completion");
    assert_eq!(
      fetch_completion.result.status,
      EffectStatus::Failed
    );
  }

  #[test]
  fn test_reactor_object_no_store_configured() {
    // Reactor without artifact store — ObjectPut fails
    let reactor = Reactor::start();
    let pid = Pid::from_raw(1);

    use base64::Engine;
    let data_b64 =
      base64::engine::general_purpose::STANDARD
        .encode(b"test");
    let req = EffectRequest::new(
      EffectKind::ObjectPut,
      serde_json::json!({
        "data_base64": data_b64,
        "media_type": "text/plain",
      }),
    );
    reactor.dispatch(pid, req);

    let completion = poll_for_completion(&reactor)
      .expect("should get completion");
    assert_eq!(
      completion.result.status,
      EffectStatus::Failed
    );
    assert!(
      completion
        .result
        .error
        .as_ref()
        .unwrap()
        .contains("no artifact store")
    );
  }

  #[test]
  fn test_reactor_object_put_missing_data_base64() {
    use crate::durability::artifact_store::{
      ArtifactBackend, SqliteArtifactStore,
    };

    let store: Arc<dyn ArtifactBackend> = Arc::new(
      SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let reactor =
      Reactor::start_with_config(None, Some(store), None);
    let pid = Pid::from_raw(1);

    // ObjectPut without data_base64 field
    let req = EffectRequest::new(
      EffectKind::ObjectPut,
      serde_json::json!({
        "media_type": "text/plain",
      }),
    );
    reactor.dispatch(pid, req);

    let completion = poll_for_completion(&reactor)
      .expect("should get completion");
    assert_eq!(
      completion.result.status,
      EffectStatus::Failed
    );
    assert!(
      completion
        .result
        .error
        .as_ref()
        .unwrap()
        .contains("data_base64")
    );
  }

  #[test]
  fn test_reactor_object_fetch_missing_object_id() {
    use crate::durability::artifact_store::{
      ArtifactBackend, SqliteArtifactStore,
    };

    let store: Arc<dyn ArtifactBackend> = Arc::new(
      SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let reactor =
      Reactor::start_with_config(None, Some(store), None);
    let pid = Pid::from_raw(1);

    // ObjectFetch without object_id field
    let req = EffectRequest::new(
      EffectKind::ObjectFetch,
      serde_json::json!({}),
    );
    reactor.dispatch(pid, req);

    let completion = poll_for_completion(&reactor)
      .expect("should get completion");
    assert_eq!(
      completion.result.status,
      EffectStatus::Failed
    );
    assert!(
      completion
        .result
        .error
        .as_ref()
        .unwrap()
        .contains("object_id")
    );
  }

  #[test]
  fn test_reactor_object_fetch_expired_artifact() {
    use crate::durability::artifact_store::{
      ArtifactBackend, SqliteArtifactStore,
    };

    let store: Arc<dyn ArtifactBackend> = Arc::new(
      SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let reactor =
      Reactor::start_with_config(None, Some(store), None);
    let pid = Pid::from_raw(1);

    // Store with TTL=0 (expires immediately)
    use base64::Engine;
    let data_b64 =
      base64::engine::general_purpose::STANDARD
        .encode(b"ephemeral");
    let put_req = EffectRequest::new(
      EffectKind::ObjectPut,
      serde_json::json!({
        "data_base64": data_b64,
        "media_type": "text/plain",
        "ttl_ms": 0,
      }),
    );
    reactor.dispatch(pid, put_req);

    let put_completion =
      poll_for_completion(&reactor)
        .expect("should get put completion");
    assert_eq!(
      put_completion.result.status,
      EffectStatus::Succeeded
    );
    let obj_id = put_completion
      .result
      .output
      .as_ref()
      .and_then(|v| v.get("object_id"))
      .and_then(|v| v.as_u64())
      .expect("should get object_id from put");

    // Wait for expiry
    std::thread::sleep(
      std::time::Duration::from_millis(10),
    );

    // Fetch the expired artifact via reactor
    let fetch_req = EffectRequest::new(
      EffectKind::ObjectFetch,
      serde_json::json!({ "object_id": obj_id }),
    );
    reactor.dispatch(pid, fetch_req);

    let fetch_completion =
      poll_for_completion(&reactor)
        .expect("should get fetch completion");
    assert_eq!(
      fetch_completion.result.status,
      EffectStatus::Failed
    );
    assert!(
      fetch_completion
        .result
        .error
        .as_ref()
        .unwrap()
        .contains("expired")
    );
  }
}
