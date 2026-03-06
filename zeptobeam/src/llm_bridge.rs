//! Async wrapper around the erlangrt `BridgeHandle` for use in zeptovm
//! processes.
//!
//! The bridge uses crossbeam channels (sync) for communication between the
//! scheduler thread and Tokio worker tasks. This module provides an
//! `LlmBridge` that wraps the sync bridge with `spawn_blocking` so callers
//! in async code never block the Tokio runtime.

use std::sync::{Arc, Mutex};

use erlangrt::agent_rt::{
  bridge::BridgeHandle,
  types::{AgentPid, IoOp, IoResult, Message as BridgeMessage, SystemMsg},
};
use tracing::debug;

/// Async wrapper around the sync `BridgeHandle` for use in zeptovm processes.
/// Uses `spawn_blocking` to avoid blocking the tokio runtime.
#[derive(Clone)]
pub struct LlmBridge {
  handle: Arc<Mutex<BridgeHandle>>,
}

impl LlmBridge {
  /// Create a new `LlmBridge` from an existing `BridgeHandle`.
  pub fn new(handle: BridgeHandle) -> Self {
    Self {
      handle: Arc::new(Mutex::new(handle)),
    }
  }

  /// Submit an agent chat request and poll for the response.
  /// Returns the LLM response as a JSON value.
  pub async fn chat(
    &self,
    provider: &str,
    model: Option<&str>,
    system_prompt: Option<&str>,
    prompt: &str,
    tools: Option<Vec<String>>,
    max_iterations: Option<usize>,
    timeout_ms: Option<u64>,
  ) -> Result<serde_json::Value, String> {
    let op = IoOp::AgentChat {
      provider: provider.to_string(),
      model: model.map(String::from),
      system_prompt: system_prompt.map(String::from),
      prompt: prompt.to_string(),
      tools,
      max_iterations,
      timeout_ms,
    };

    // Generate a placeholder AgentPid for bridge submission.
    // Each call gets a fresh PID so responses don't collide.
    let pid = AgentPid::new();

    // Submit the request on a blocking thread (crossbeam send).
    let handle = self.handle.clone();
    let correlation_id = tokio::task::spawn_blocking(move || {
      let mut h = handle.lock().map_err(|e| format!("bridge lock: {e}"))?;
      h.submit(pid, op)
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))??;

    debug!(correlation_id, "LLM request submitted to bridge");

    // Poll for the response on a blocking thread.
    let handle = self.handle.clone();
    let result = tokio::task::spawn_blocking(move || {
      let start = std::time::Instant::now();
      // Slightly longer than the bridge's own default timeout (120s)
      let timeout = std::time::Duration::from_secs(130);

      loop {
        {
          let h = handle.lock().map_err(|e| format!("bridge lock: {e}"))?;
          let responses = h.drain_responses();
          for (_resp_pid, msg) in responses {
            if let BridgeMessage::System(SystemMsg::IoResponse {
              correlation_id: cid,
              result,
            }) = msg
            {
              if cid == correlation_id {
                return match result {
                  IoResult::Ok(value) => Ok(value),
                  IoResult::Error(e) => Err(format!("LLM error: {e}")),
                  IoResult::Timeout => Err("LLM request timed out".into()),
                };
              }
            }
          }
        }
        if start.elapsed() > timeout {
          return Err("bridge response timeout".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
      }
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))??;

    Ok(result)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use erlangrt::agent_rt::bridge::create_bridge;

  #[test]
  fn test_llm_bridge_creates() {
    let (handle, _worker) = create_bridge();
    let bridge = LlmBridge::new(handle);
    // Just verify it constructs without panic.
    let _clone = bridge.clone();
  }

  #[test]
  fn test_llm_bridge_clone_shares_handle() {
    let (handle, _worker) = create_bridge();
    let bridge = LlmBridge::new(handle);
    let clone = bridge.clone();
    // Both should point to the same underlying Arc
    assert!(Arc::ptr_eq(&bridge.handle, &clone.handle));
  }
}
