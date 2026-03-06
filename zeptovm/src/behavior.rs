use async_trait::async_trait;

use crate::error::{Action, Message, Reason};

/// The core process behavior trait. Each process owns one Behavior instance.
#[async_trait]
pub trait Behavior: Send + 'static {
  /// Called once at spawn. checkpoint is Some if recovering from a prior run.
  async fn init(
    &mut self,
    checkpoint: Option<Vec<u8>>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

  /// Called for each user message. Returns an Action directing the run loop.
  async fn handle(&mut self, msg: Message) -> Action;

  /// Called when the process is terminating.
  async fn terminate(&mut self, reason: &Reason);

  /// Opt-in: return true if runtime should checkpoint after handle().
  fn should_checkpoint(&self) -> bool {
    false
  }

  /// Opt-in: serialize state for durable recovery.
  fn checkpoint(&self) -> Option<Vec<u8>> {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::{Action, Message, Reason};

  struct EchoBehavior;

  #[async_trait]
  impl Behavior for EchoBehavior {
    async fn init(
      &mut self,
      _checkpoint: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
      Ok(())
    }
    async fn handle(&mut self, _msg: Message) -> Action {
      Action::Continue
    }
    async fn terminate(&mut self, _reason: &Reason) {}
  }

  #[tokio::test]
  async fn test_behavior_can_be_boxed() {
    let mut b: Box<dyn Behavior> = Box::new(EchoBehavior);
    b.init(None).await.unwrap();
    let action = b.handle(Message::text("hi")).await;
    assert!(matches!(action, Action::Continue));
  }

  #[tokio::test]
  async fn test_behavior_checkpoint_defaults_to_none() {
    let b = EchoBehavior;
    assert!(!b.should_checkpoint());
    assert!(b.checkpoint().is_none());
  }
}
