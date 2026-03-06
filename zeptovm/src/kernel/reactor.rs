use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::core::effect::{
  EffectKind, EffectRequest, EffectResult,
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
              tokio::spawn(async move {
                let result =
                  execute_effect(&dispatch.request).await;
                let _ = tx.send(EffectCompletion {
                  pid: dispatch.pid,
                  result,
                });
              });
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

/// Execute an effect asynchronously. For v1, most effects return
/// placeholder results.
async fn execute_effect(request: &EffectRequest) -> EffectResult {
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
}
