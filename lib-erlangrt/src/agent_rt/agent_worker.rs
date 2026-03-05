//! AgentWorkerBehavior wraps a ZeptoClaw agent as a runtime process.
//! On receiving a Text/Json message, it submits an AgentChat IoOp.
//! On receiving an IoResponse, it logs the result and becomes idle.

use crate::agent_rt::types::*;

pub struct AgentWorkerState {
  pub busy: bool,
  pub last_requester: Option<AgentPid>,
  pub pending_messages: Vec<Message>,
}

impl AgentState for AgentWorkerState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

pub struct AgentWorkerBehavior {
  pub name: String,
  pub provider: String,
  pub model: Option<String>,
  pub system_prompt: Option<String>,
  pub tools: Vec<String>,
  pub max_iterations: Option<usize>,
  pub timeout_ms: Option<u64>,
}

impl AgentWorkerBehavior {
  pub fn new(
    name: String,
    provider: String,
    model: Option<String>,
    system_prompt: Option<String>,
    tools: Vec<String>,
    max_iterations: Option<usize>,
    timeout_ms: Option<u64>,
  ) -> Self {
    Self {
      name,
      provider,
      model,
      system_prompt,
      tools,
      max_iterations,
      timeout_ms,
    }
  }

  fn make_chat_op(&self, prompt: &str) -> IoOp {
    IoOp::AgentChat {
      provider: self.provider.clone(),
      model: self.model.clone(),
      system_prompt: self.system_prompt.clone(),
      prompt: prompt.to_string(),
      tools: if self.tools.is_empty() {
        None
      } else {
        Some(self.tools.clone())
      },
      max_iterations: self.max_iterations,
      timeout_ms: self.timeout_ms,
    }
  }
}

impl AgentBehavior for AgentWorkerBehavior {
  fn init(
    &self,
    _args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(AgentWorkerState {
      busy: false,
      last_requester: None,
      pending_messages: Vec::new(),
    }))
  }

  fn handle_message(
    &self,
    msg: Message,
    state: &mut dyn AgentState,
  ) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<AgentWorkerState>()
      .expect("AgentWorkerState");

    match &msg {
      Message::System(SystemMsg::IoResponse { result, .. }) => {
        s.busy = false;
        if let IoResult::Ok(val) = result {
          if let Some(response) =
            val.get("response").and_then(|v| v.as_str())
          {
            tracing::info!(
              agent = %self.name,
              response_len = response.len(),
              "agent response received"
            );
          }
        }
        // If there are pending messages, process the next one
        if let Some(next) = s.pending_messages.pop() {
          return self.handle_message(next, state);
        }
        Action::Continue
      }
      Message::Text(prompt) => {
        if s.busy {
          s.pending_messages.push(msg);
          return Action::Continue;
        }
        s.busy = true;
        Action::IoRequest(self.make_chat_op(prompt))
      }
      Message::Json(val) => {
        let prompt =
          val.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        if prompt.is_empty() {
          return Action::Continue;
        }
        if s.busy {
          s.pending_messages.push(msg);
          return Action::Continue;
        }
        s.busy = true;
        Action::IoRequest(self.make_chat_op(prompt))
      }
      _ => Action::Continue,
    }
  }

  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) -> Action {
    Action::Continue
  }

  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_agent_worker_init_stores_config() {
    let behavior = AgentWorkerBehavior::new(
      "researcher".into(),
      "openrouter".into(),
      Some("anthropic/claude-sonnet-4".into()),
      Some("You are helpful.".into()),
      vec!["web_fetch".into()],
      None,
      None,
    );
    let state = behavior.init(serde_json::json!({})).unwrap();
    let s = state.as_any().downcast_ref::<AgentWorkerState>().unwrap();
    assert!(!s.busy);
    assert!(s.last_requester.is_none());
  }

  #[test]
  fn test_agent_worker_text_message_returns_io_request() {
    let behavior = AgentWorkerBehavior::new(
      "test".into(),
      "openrouter".into(),
      None,
      None,
      vec![],
      None,
      None,
    );
    let mut state = behavior.init(serde_json::json!({})).unwrap();
    let action = behavior.handle_message(
      Message::Text("hello".into()),
      state.as_mut(),
    );
    match action {
      Action::IoRequest(IoOp::AgentChat {
        prompt, provider, ..
      }) => {
        assert_eq!(prompt, "hello");
        assert_eq!(provider, "openrouter");
      }
      _ => panic!("Expected IoRequest(AgentChat)"),
    }
  }

  #[test]
  fn test_agent_worker_rejects_while_busy() {
    let behavior = AgentWorkerBehavior::new(
      "test".into(),
      "openrouter".into(),
      None,
      None,
      vec![],
      None,
      None,
    );
    let mut state = behavior.init(serde_json::json!({})).unwrap();
    // First message -- should submit IoRequest
    let _ = behavior.handle_message(
      Message::Text("hello".into()),
      state.as_mut(),
    );
    // Second message while busy -- should Continue (queue)
    let action = behavior.handle_message(
      Message::Text("world".into()),
      state.as_mut(),
    );
    assert!(matches!(action, Action::Continue));
  }

  #[test]
  fn test_agent_worker_io_response_clears_busy() {
    let behavior = AgentWorkerBehavior::new(
      "test".into(),
      "openrouter".into(),
      None,
      None,
      vec![],
      None,
      None,
    );
    let mut state = behavior.init(serde_json::json!({})).unwrap();
    // Send a message to make it busy
    let _ = behavior.handle_message(
      Message::Text("hello".into()),
      state.as_mut(),
    );
    // Simulate IoResponse
    let action = behavior.handle_message(
      Message::System(SystemMsg::IoResponse {
        correlation_id: 1,
        result: IoResult::Ok(
          serde_json::json!({"response": "hi there"}),
        ),
      }),
      state.as_mut(),
    );
    // Should be Continue (response processed)
    assert!(matches!(action, Action::Continue));
    // No longer busy
    let s = state
      .as_any()
      .downcast_ref::<AgentWorkerState>()
      .unwrap();
    assert!(!s.busy);
  }
}
