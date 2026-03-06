//! Integration tests for real LLM effect workers.
//! Run with: cargo test --features integration_tests
//! Requires: OPENAI_API_KEY or ANTHROPIC_API_KEY env var

#![cfg(feature = "integration_tests")]

use zeptovm::core::behavior::StepBehavior;
use zeptovm::core::effect::{
  EffectKind, EffectRequest, EffectStatus,
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
        if result.status == EffectStatus::Streaming {
          // Ignore streaming chunks, wait for final
          StepResult::Continue
        } else {
          // Final result received
          StepResult::Done(Reason::Normal)
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
  rt.tick(); // Suspends with LlmCall

  // Wait for real API response (up to 30s)
  for _ in 0..300 {
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
      println!(
        "OpenAI integration test passed!"
      );
      return;
    }
  }
  panic!("LLM call did not complete in 30s");
}
