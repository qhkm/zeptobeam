use serde::{Deserialize, Serialize};

use crate::core::effect::EffectResult;
use crate::pid::Pid;

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_MSG_ID: AtomicU64 = AtomicU64::new(1);

/// Unique message identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MsgId(u64);

impl MsgId {
  pub fn new() -> Self {
    Self(NEXT_MSG_ID.fetch_add(1, Ordering::Relaxed))
  }
}

/// Which mailbox lane this message routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageClass {
  Control,
  Supervisor,
  EffectResult,
  User,
  Background,
}

/// Priority for scheduling and mailbox ordering.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub enum Priority {
  Low = 0,
  Normal = 1,
  High = 2,
}

impl Default for Priority {
  fn default() -> Self {
    Priority::Normal
  }
}

/// The payload carried inside a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
  Text(String),
  Bytes(Vec<u8>),
  Json(serde_json::Value),
}

/// A message envelope with routing metadata.
#[derive(Debug, Clone)]
pub struct Envelope {
  pub msg_id: MsgId,
  pub correlation_id: Option<String>,
  pub from: Option<Pid>,
  pub to: Pid,
  pub class: MessageClass,
  pub priority: Priority,
  pub dedup_key: Option<String>,
  pub payload: EnvelopePayload,
}

/// What's inside the envelope — either a user payload or an effect
/// result.
#[derive(Debug, Clone)]
pub enum EnvelopePayload {
  User(Payload),
  Effect(EffectResult),
  Signal(Signal),
}

/// System/supervisor signals.
#[derive(Debug, Clone)]
pub enum Signal {
  Kill,
  Shutdown,
  ExitLinked(Pid, crate::error::Reason),
  MonitorDown(Pid, crate::error::Reason),
  Suspend,
  Resume,
}

impl Envelope {
  /// Create a user message.
  pub fn user(to: Pid, payload: Payload) -> Self {
    Self {
      msg_id: MsgId::new(),
      correlation_id: None,
      from: None,
      to,
      class: MessageClass::User,
      priority: Priority::Normal,
      dedup_key: None,
      payload: EnvelopePayload::User(payload),
    }
  }

  /// Create a user text message (convenience).
  pub fn text(to: Pid, s: impl Into<String>) -> Self {
    Self::user(to, Payload::Text(s.into()))
  }

  /// Create a user JSON message (convenience).
  pub fn json(to: Pid, v: serde_json::Value) -> Self {
    Self::user(to, Payload::Json(v))
  }

  /// Create an effect result message.
  pub fn effect_result(to: Pid, result: EffectResult) -> Self {
    Self {
      msg_id: MsgId::new(),
      correlation_id: None,
      from: None,
      to,
      class: MessageClass::EffectResult,
      priority: Priority::High,
      dedup_key: None,
      payload: EnvelopePayload::Effect(result),
    }
  }

  /// Create a control signal.
  pub fn signal(to: Pid, signal: Signal) -> Self {
    Self {
      msg_id: MsgId::new(),
      correlation_id: None,
      from: None,
      to,
      class: MessageClass::Control,
      priority: Priority::High,
      dedup_key: None,
      payload: EnvelopePayload::Signal(signal),
    }
  }

  /// Builder: set correlation ID.
  pub fn with_correlation(
    mut self,
    id: impl Into<String>,
  ) -> Self {
    self.correlation_id = Some(id.into());
    self
  }

  /// Builder: set sender.
  pub fn with_from(mut self, from: Pid) -> Self {
    self.from = Some(from);
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::pid::Pid;

  #[test]
  fn test_envelope_text() {
    let to = Pid::from_raw(1);
    let env = Envelope::text(to, "hello");
    assert_eq!(env.to, to);
    assert_eq!(env.class, MessageClass::User);
    assert!(matches!(
      env.payload,
      EnvelopePayload::User(Payload::Text(_))
    ));
  }

  #[test]
  fn test_envelope_effect_result() {
    use crate::core::effect::{EffectId, EffectResult};
    let to = Pid::from_raw(2);
    let result =
      EffectResult::success(EffectId::new(), serde_json::json!("ok"));
    let env = Envelope::effect_result(to, result);
    assert_eq!(env.class, MessageClass::EffectResult);
    assert_eq!(env.priority, Priority::High);
  }

  #[test]
  fn test_envelope_signal() {
    let to = Pid::from_raw(3);
    let env = Envelope::signal(to, Signal::Kill);
    assert_eq!(env.class, MessageClass::Control);
  }

  #[test]
  fn test_msg_id_unique() {
    let a = MsgId::new();
    let b = MsgId::new();
    assert_ne!(a, b);
  }

  #[test]
  fn test_envelope_builder() {
    let to = Pid::from_raw(4);
    let from = Pid::from_raw(5);
    let env = Envelope::text(to, "hi")
      .with_from(from)
      .with_correlation("corr-123");
    assert_eq!(env.from, Some(from));
    assert_eq!(env.correlation_id.as_deref(), Some("corr-123"));
  }

  #[test]
  fn test_priority_ordering() {
    assert!(Priority::High > Priority::Normal);
    assert!(Priority::Normal > Priority::Low);
  }
}
