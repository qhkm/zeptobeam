use crate::core::effect::EffectRequest;
use crate::error::Reason;

/// Result of a single step of process execution.
///
/// Key design: Suspend takes EffectRequest (intent), not Future (opaque).
/// This preserves journaling, replay, policy checks, and budget interception.
#[derive(Debug)]
pub enum StepResult {
    /// Handled message successfully, may have more to process.
    Continue,
    /// No messages in mailbox, want to wait.
    Wait,
    /// Wait for a message with a specific tag (selective receive).
    WaitForTag(String),
    /// Requesting a side effect — will be journaled, policy-checked, dispatched.
    Suspend(EffectRequest),
    /// Process is finished normally.
    Done(Reason),
    /// Process encountered an error.
    Fail(Reason),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectKind, EffectRequest};
    use crate::error::Reason;

    #[test]
    fn test_step_result_continue() {
        let r = StepResult::Continue;
        assert!(matches!(r, StepResult::Continue));
    }

    #[test]
    fn test_step_result_suspend_effect() {
        let req = EffectRequest::new(
            EffectKind::LlmCall,
            serde_json::json!({"prompt": "test"}),
        );
        let effect_id = req.effect_id;
        let r = StepResult::Suspend(req);
        match r {
            StepResult::Suspend(req) => {
                assert_eq!(req.effect_id, effect_id);
                assert_eq!(req.kind, EffectKind::LlmCall);
            }
            _ => panic!("expected Suspend"),
        }
    }

    #[test]
    fn test_step_result_done() {
        let r = StepResult::Done(Reason::Normal);
        assert!(matches!(r, StepResult::Done(Reason::Normal)));
    }

    #[test]
    fn test_step_result_fail() {
        let r = StepResult::Fail(Reason::Custom("budget exceeded".into()));
        assert!(matches!(r, StepResult::Fail(Reason::Custom(_))));
    }

    #[test]
    fn test_step_result_wait_for_tag() {
        let r = StepResult::WaitForTag("effect_result".into());
        match r {
            StepResult::WaitForTag(tag) => {
                assert_eq!(tag, "effect_result")
            }
            _ => panic!("expected WaitForTag"),
        }
    }
}
