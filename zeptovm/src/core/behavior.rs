use crate::core::message::Envelope;
use crate::core::step_result::StepResult;
use crate::core::turn_context::TurnContext;
use crate::error::Reason;

/// Step-based process behavior.
///
/// handle() is synchronous and takes a TurnContext for emitting
/// intents. Side effects go through ctx.request_effect() and return
/// Suspend(EffectRequest). Outbound messages go through ctx.send().
/// State patches through ctx.set_state(). The runtime collects all
/// intents and commits them atomically.
pub trait StepBehavior: Send + 'static {
  /// Called once at spawn. checkpoint is Some if recovering.
  fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult;

  /// Called for each message. Emit intents via ctx; return what to
  /// do next.
  fn handle(
    &mut self,
    msg: Envelope,
    ctx: &mut TurnContext,
  ) -> StepResult;

  /// Called when the process is terminating.
  fn terminate(&mut self, reason: &Reason);

  /// Opt-in: serialize state for snapshot.
  fn snapshot(&self) -> Option<Vec<u8>> {
    None
  }

  /// Opt-in: restore state from a snapshot blob.
  fn restore(&mut self, _state: &[u8]) -> Result<(), String> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::effect::{
    EffectId, EffectKind, EffectRequest, EffectResult,
  };
  use crate::core::message::{Envelope, EnvelopePayload};
  use crate::error::Reason;
  use crate::pid::Pid;

  struct EchoStep;
  impl StepBehavior for EchoStep {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _msg: Envelope,
      _ctx: &mut TurnContext,
    ) -> StepResult {
      StepResult::Continue
    }
    fn terminate(&mut self, _reason: &Reason) {}
  }

  struct LlmAgent {
    awaiting_response: bool,
  }
  impl StepBehavior for LlmAgent {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }

    fn handle(
      &mut self,
      msg: Envelope,
      _ctx: &mut TurnContext,
    ) -> StepResult {
      match &msg.payload {
        EnvelopePayload::Effect(_result) => {
          // Got LLM response -- forward to requester, then done
          self.awaiting_response = false;
          StepResult::Done(Reason::Normal)
        }
        EnvelopePayload::User(_) if !self.awaiting_response => {
          // User trigger -> request LLM call via context
          self.awaiting_response = true;
          let req = EffectRequest::new(
            EffectKind::LlmCall,
            serde_json::json!({"prompt": "hello"}),
          );
          StepResult::Suspend(req)
        }
        _ => StepResult::Continue,
      }
    }

    fn terminate(&mut self, _reason: &Reason) {}
  }

  /// Agent that sends outbound messages via TurnContext
  struct Forwarder {
    target: Pid,
  }
  impl StepBehavior for Forwarder {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _msg: Envelope,
      ctx: &mut TurnContext,
    ) -> StepResult {
      ctx.send_text(self.target, "forwarded");
      StepResult::Continue
    }
    fn terminate(&mut self, _reason: &Reason) {}
  }

  #[test]
  fn test_step_behavior_boxed() {
    let mut b: Box<dyn StepBehavior> = Box::new(EchoStep);
    let result = b.init(None);
    assert!(matches!(result, StepResult::Continue));
  }

  #[test]
  fn test_step_behavior_snapshot_default() {
    let b = EchoStep;
    assert!(b.snapshot().is_none());
  }

  #[test]
  fn test_llm_agent_suspend() {
    let mut agent = LlmAgent {
      awaiting_response: false,
    };
    agent.init(None);
    let pid = Pid::from_raw(1);
    let mut ctx = TurnContext::new(pid);
    let msg = Envelope::text(pid, "call the LLM");
    let result = agent.handle(msg, &mut ctx);
    assert!(matches!(result, StepResult::Suspend(_)));
  }

  #[test]
  fn test_llm_agent_done_on_effect_result() {
    let mut agent = LlmAgent {
      awaiting_response: true,
    };
    let pid = Pid::from_raw(1);
    let mut ctx = TurnContext::new(pid);
    let result = EffectResult::success(
      EffectId::new(),
      serde_json::json!("response"),
    );
    let msg = Envelope::effect_result(pid, result);
    let step = agent.handle(msg, &mut ctx);
    assert!(matches!(step, StepResult::Done(Reason::Normal)));
  }

  #[test]
  fn test_forwarder_emits_intent() {
    let target = Pid::from_raw(99);
    let mut fwd = Forwarder { target };
    fwd.init(None);
    let pid = Pid::from_raw(1);
    let mut ctx = TurnContext::new(pid);
    let msg = Envelope::text(pid, "trigger");
    fwd.handle(msg, &mut ctx);
    assert_eq!(ctx.intent_count(), 1);
    let intents = ctx.take_intents();
    assert!(matches!(
      intents[0],
      crate::core::turn_context::TurnIntent::SendMessage(_)
    ));
  }
}
