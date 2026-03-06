use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use tracing::{info_span, Instrument};

use crate::core::effect::{
  EffectKind, EffectRequest, EffectResult, EffectStatus,
};
use crate::pid::Pid;

/// A request sent to the reactor for dispatch.
pub struct EffectDispatch {
  pub pid: Pid,
  pub request: EffectRequest,
}

/// A completed effect returned from the reactor.
pub struct EffectCompletion {
  pub pid: Pid,
  pub result: EffectResult,
}

/// The reactor bridge between the sync scheduler and async effect
/// execution.
///
/// The scheduler sends `EffectRequest`s via the dispatch channel.
/// The reactor runs them on a shared Tokio runtime.
/// Completed effects come back via the completion channel.
pub struct Reactor {
  dispatch_tx: Sender<EffectDispatch>,
  completion_rx: Receiver<EffectCompletion>,
  _handle: thread::JoinHandle<()>,
}

impl Reactor {
  /// Start the reactor on a background thread with a Tokio runtime.
  pub fn start() -> Self {
    let (dispatch_tx, dispatch_rx) =
      unbounded::<EffectDispatch>();
    let (completion_tx, completion_rx) =
      unbounded::<EffectCompletion>();

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
              let span = info_span!(
                "effect_dispatch",
                effect_id =
                  %dispatch.request.effect_id,
                kind = ?dispatch.request.kind,
              );
              tokio::spawn(
                async move {
                  let result =
                    execute_effect_with_retry(
                      &dispatch.request,
                    )
                    .await;
                  let _ = tx.send(EffectCompletion {
                    pid: dispatch.pid,
                    result,
                  });
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

  /// Try to receive completed effects (non-blocking).
  pub fn try_recv(&self) -> Option<EffectCompletion> {
    self.completion_rx.try_recv().ok()
  }

  /// Drain all available completions.
  pub fn drain_completions(&self) -> Vec<EffectCompletion> {
    let mut results = Vec::new();
    while let Some(completion) = self.try_recv() {
      results.push(completion);
    }
    results
  }

  /// Get a clone of the dispatch sender (for multi-thread use).
  pub fn dispatch_sender(&self) -> Sender<EffectDispatch> {
    self.dispatch_tx.clone()
  }
}

/// Execute an effect with retry logic based on the request's
/// RetryPolicy.
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
/// placeholder results.
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

    // Wait for result
    let mut result = None;
    for _ in 0..100 {
      if let Some(completion) = reactor.try_recv() {
        result = Some(completion);
        break;
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

    // Collect all results
    let mut received = 0;
    for _ in 0..200 {
      if let Some(_completion) = reactor.try_recv() {
        received += 1;
        if received == 5 {
          break;
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

    // Should take at least 50ms
    let start = std::time::Instant::now();
    let mut result = None;
    for _ in 0..100 {
      if let Some(completion) = reactor.try_recv() {
        result = Some(completion);
        break;
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
}
