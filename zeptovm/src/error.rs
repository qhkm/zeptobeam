use crate::pid::Pid;

/// Reason for process termination.
#[derive(Debug, Clone)]
pub enum Reason {
  Normal,
  Shutdown,
  Kill,
  Custom(String),
}

impl Reason {
  pub fn is_abnormal(&self) -> bool {
    matches!(self, Reason::Kill | Reason::Custom(_))
  }
}

impl std::fmt::Display for Reason {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Reason::Normal => write!(f, "normal"),
      Reason::Shutdown => write!(f, "shutdown"),
      Reason::Kill => write!(f, "kill"),
      Reason::Custom(s) => write!(f, "{s}"),
    }
  }
}

/// Action returned by a process handler to indicate what should happen next.
pub enum Action {
  Continue,
  Stop(Reason),
  Checkpoint,
}

/// Payload variants carried inside a user-level message.
#[derive(Debug, Clone)]
pub enum UserPayload {
  Text(String),
  Bytes(Vec<u8>),
  Json(serde_json::Value),
}

/// A message that can be sent to a process mailbox.
#[derive(Debug, Clone)]
pub enum Message {
  User(UserPayload),
}

impl Message {
  pub fn text(s: impl Into<String>) -> Self {
    Message::User(UserPayload::Text(s.into()))
  }

  pub fn bytes(b: Vec<u8>) -> Self {
    Message::User(UserPayload::Bytes(b))
  }

  pub fn json(v: serde_json::Value) -> Self {
    Message::User(UserPayload::Json(v))
  }
}

/// System-level messages delivered out-of-band to a process.
#[derive(Debug)]
pub enum SystemMsg {
  ExitLinked(Pid, Reason),
  MonitorDown(Pid, Reason),
  Suspend,
  Resume,
  GetState(tokio::sync::oneshot::Sender<ProcessInfo>),
}

/// Snapshot of a process's current state.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
  pub pid: Pid,
  pub status: ProcessStatus,
  pub mailbox_depth: usize,
}

/// Lifecycle status of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
  Running,
  Suspended,
  Exiting,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_reason_normal_is_not_abnormal() {
    assert!(!Reason::Normal.is_abnormal());
  }

  #[test]
  fn test_reason_shutdown_is_not_abnormal() {
    assert!(!Reason::Shutdown.is_abnormal());
  }

  #[test]
  fn test_reason_custom_is_abnormal() {
    assert!(Reason::Custom("oops".into()).is_abnormal());
  }

  #[test]
  fn test_reason_kill_is_abnormal() {
    assert!(Reason::Kill.is_abnormal());
  }

  #[test]
  fn test_action_variants() {
    let a = Action::Continue;
    assert!(matches!(a, Action::Continue));
    let b = Action::Stop(Reason::Normal);
    assert!(matches!(b, Action::Stop(Reason::Normal)));
    let c = Action::Checkpoint;
    assert!(matches!(c, Action::Checkpoint));
  }

  #[test]
  fn test_message_user_text() {
    let msg = Message::text("hello");
    assert!(matches!(msg, Message::User(UserPayload::Text(_))));
  }

  #[test]
  fn test_message_user_bytes() {
    let msg = Message::bytes(vec![1, 2, 3]);
    assert!(matches!(msg, Message::User(UserPayload::Bytes(_))));
  }
}
