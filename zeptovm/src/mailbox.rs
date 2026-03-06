use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{Message, SystemMsg};

/// The process-side of the mailbox (owned by the process task).
pub struct ProcessMailbox {
  pub user_rx: mpsc::Receiver<Message>,
  pub control_rx: mpsc::Receiver<SystemMsg>,
}

/// External handle for sending messages to a process. Clone-friendly.
#[derive(Clone)]
pub struct ProcessHandle {
  pub kill_token: CancellationToken,
  pub user_tx: mpsc::Sender<Message>,
  pub control_tx: mpsc::Sender<SystemMsg>,
}

impl ProcessHandle {
  pub async fn send_user(
    &self,
    msg: Message,
  ) -> Result<(), mpsc::error::SendError<Message>> {
    self.user_tx.send(msg).await
  }

  pub fn try_send_user(
    &self,
    msg: Message,
  ) -> Result<(), mpsc::error::TrySendError<Message>> {
    self.user_tx.try_send(msg)
  }

  pub fn try_send_control(
    &self,
    msg: SystemMsg,
  ) -> Result<(), mpsc::error::TrySendError<SystemMsg>> {
    self.control_tx.try_send(msg)
  }

  /// Kill the process via out-of-band CancellationToken.
  pub fn kill(&self) {
    self.kill_token.cancel();
  }
}

/// Create a matched pair of (ProcessHandle, ProcessMailbox).
pub fn create_mailbox(
  user_capacity: usize,
  control_capacity: usize,
) -> (ProcessHandle, ProcessMailbox) {
  let (user_tx, user_rx) = mpsc::channel(user_capacity);
  let (control_tx, control_rx) = mpsc::channel(control_capacity);
  let kill_token = CancellationToken::new();

  let handle = ProcessHandle {
    kill_token: kill_token.clone(),
    user_tx,
    control_tx,
  };

  let mailbox = ProcessMailbox {
    user_rx,
    control_rx,
  };

  (handle, mailbox)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::Message;

  #[tokio::test]
  async fn test_mailbox_create_and_send_user_message() {
    let (handle, mut mailbox) = create_mailbox(16, 32);
    handle.send_user(Message::text("hello")).await.unwrap();
    let msg = mailbox.user_rx.recv().await.unwrap();
    assert!(matches!(msg, Message::User(_)));
  }

  #[tokio::test]
  async fn test_mailbox_kill_token() {
    let (handle, _mailbox) = create_mailbox(16, 32);
    assert!(!handle.kill_token.is_cancelled());
    handle.kill();
    assert!(handle.kill_token.is_cancelled());
  }

  #[tokio::test]
  async fn test_mailbox_user_channel_bounded() {
    let (handle, _mailbox) = create_mailbox(2, 32);
    handle.send_user(Message::text("1")).await.unwrap();
    handle.send_user(Message::text("2")).await.unwrap();
    let result = handle.user_tx.try_send(Message::text("3"));
    assert!(result.is_err());
  }

  #[test]
  fn test_process_handle_is_clone() {
    let (handle, _mailbox) = create_mailbox(16, 32);
    let _clone = handle.clone();
  }
}
