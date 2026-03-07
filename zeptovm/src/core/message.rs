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
  pub expires_at: Option<u64>,
  pub tag: Option<String>,
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
  TimerFired(crate::core::timer::TimerId),
  ChildExited {
    child_pid: Pid,
    reason: crate::error::Reason,
  },
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
      expires_at: None,
      tag: None,
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
      expires_at: None,
      tag: None,
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
      expires_at: None,
      tag: None,
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

  /// Builder: set explicit expiry timestamp (epoch millis).
  /// The caller is responsible for computing this from clock_ms + ttl.
  pub fn with_expires_at(mut self, epoch_ms: u64) -> Self {
    self.expires_at = Some(epoch_ms);
    self
  }

  /// Builder: set message tag for selective receive.
  pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
    self.tag = Some(tag.into());
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

  #[test]
  fn test_envelope_with_expires_at() {
    let to = Pid::from_raw(1);
    let env = Envelope::text(to, "hi").with_expires_at(99999);
    assert_eq!(env.expires_at, Some(99999));
  }

  #[test]
  fn test_envelope_with_tag() {
    let to = Pid::from_raw(1);
    let env = Envelope::text(to, "hi").with_tag("effect_result");
    assert_eq!(env.tag.as_deref(), Some("effect_result"));
  }

  #[test]
  fn test_envelope_defaults_no_expiry_no_tag() {
    let to = Pid::from_raw(1);
    let env = Envelope::text(to, "hi");
    assert!(env.expires_at.is_none());
    assert!(env.tag.is_none());
  }

  #[test]
  fn test_envelope_builder_chain() {
    let to = Pid::from_raw(1);
    let from = Pid::from_raw(2);
    let env = Envelope::text(to, "hi")
      .with_from(from)
      .with_expires_at(5000)
      .with_tag("important")
      .with_correlation("c-1");
    assert_eq!(env.from, Some(from));
    assert_eq!(env.expires_at, Some(5000));
    assert_eq!(env.tag.as_deref(), Some("important"));
    assert_eq!(env.correlation_id.as_deref(), Some("c-1"));
  }
}
