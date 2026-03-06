use std::sync::{
  atomic::{AtomicU64, Ordering},
  Arc,
};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::{
  behavior::Behavior,
  durability::checkpoint::CheckpointStore,
  error::{Action, ProcessInfo, ProcessStatus, Reason, SystemMsg},
  mailbox::{create_mailbox, ProcessHandle, ProcessMailbox},
  pid::Pid,
};

#[derive(Debug)]
pub struct ProcessExit {
  pub pid: Pid,
  pub reason: Reason,
}

/// Spawn a new process as a tokio task.
///
/// `last_wal_seq` is an optional shared counter that the `DurableHandle`
/// writes to after each WAL append. The process reads it when encoding
/// checkpoints so the checkpoint records the latest acknowledged WAL
/// sequence. Pass `None` for non-durable scenarios.
pub fn spawn_process(
  mut behavior: impl Behavior,
  user_mailbox_capacity: usize,
  checkpoint: Option<Vec<u8>>,
  checkpoint_store: Option<Arc<dyn CheckpointStore>>,
  last_wal_seq: Option<Arc<AtomicU64>>,
) -> (Pid, ProcessHandle, JoinHandle<ProcessExit>) {
  let pid = Pid::new();
  let (handle, mailbox) = create_mailbox(user_mailbox_capacity, 32);
  let kill_token = handle.kill_token.clone();
  let join = tokio::spawn(async move {
    run_process(
      pid,
      &mut behavior,
      mailbox,
      kill_token,
      checkpoint,
      checkpoint_store,
      last_wal_seq,
    )
    .await
  });
  (pid, handle, join)
}

/// Spawn from Box<dyn Behavior> (used by supervisor).
pub fn spawn_process_boxed(
  mut behavior: Box<dyn Behavior>,
  user_mailbox_capacity: usize,
  checkpoint: Option<Vec<u8>>,
  checkpoint_store: Option<Arc<dyn CheckpointStore>>,
  last_wal_seq: Option<Arc<AtomicU64>>,
) -> (Pid, ProcessHandle, JoinHandle<ProcessExit>) {
  let pid = Pid::new();
  let (handle, mailbox) = create_mailbox(user_mailbox_capacity, 32);
  let kill_token = handle.kill_token.clone();
  let join = tokio::spawn(async move {
    run_process(
      pid,
      behavior.as_mut(),
      mailbox,
      kill_token,
      checkpoint,
      checkpoint_store,
      last_wal_seq,
    )
    .await
  });
  (pid, handle, join)
}

async fn run_process(
  pid: Pid,
  behavior: &mut dyn Behavior,
  mut mailbox: ProcessMailbox,
  kill_token: tokio_util::sync::CancellationToken,
  checkpoint: Option<Vec<u8>>,
  checkpoint_store: Option<Arc<dyn CheckpointStore>>,
  last_wal_seq: Option<Arc<AtomicU64>>,
) -> ProcessExit {
  if let Err(e) = behavior.init(checkpoint).await {
    warn!(pid = %pid, error = %e, "process init failed");
    return ProcessExit {
      pid,
      reason: Reason::Custom(format!("init failed: {e}")),
    };
  }

  debug!(pid = %pid, "process started");

  let mut suspended = false;
  let mut exit_reason = Reason::Normal;
  let mut control_closed = false;
  let mut user_closed = false;

  loop {
    tokio::select! {
      biased;

      _ = kill_token.cancelled() => {
        exit_reason = Reason::Kill;
        break;
      }

      msg = mailbox.control_rx.recv(), if !control_closed => {
        match msg {
          Some(sys) => match sys {
            SystemMsg::ExitLinked(_from, reason) => {
              if reason.is_abnormal() {
                exit_reason = reason;
                break;
              }
            }
            SystemMsg::MonitorDown(_from, _reason) => {}
            SystemMsg::Suspend => { suspended = true; }
            SystemMsg::Resume => { suspended = false; }
            SystemMsg::GetState(tx) => {
              let info = ProcessInfo {
                pid,
                status: if suspended {
                  ProcessStatus::Suspended
                } else {
                  ProcessStatus::Running
                },
                mailbox_depth: 0,
              };
              let _ = tx.send(info);
            }
          },
          None => {
            control_closed = true;
            if user_closed {
              break;
            }
          }
        }
      }

      msg = mailbox.user_rx.recv(), if !suspended && !user_closed => {
        match msg {
          Some(m) => match behavior.handle(m).await {
            Action::Continue => {
              if let Some(ref store) = checkpoint_store {
                if behavior.should_checkpoint() {
                  if let Some(data) = behavior.checkpoint() {
                    let wal_seq = last_wal_seq
                      .as_ref()
                      .map(|a| a.load(Ordering::SeqCst))
                      .unwrap_or(0);
                    let encoded =
                      crate::durability::recovery::encode_checkpoint(wal_seq, &data);
                    if let Err(e) = store.save(pid, &encoded).await {
                      warn!(pid = %pid, error = %e, "checkpoint save failed");
                    }
                  }
                }
              }
            }
            Action::Stop(reason) => {
              exit_reason = reason;
              break;
            }
            Action::Checkpoint => {
              if let Some(ref store) = checkpoint_store {
                if let Some(data) = behavior.checkpoint() {
                  let wal_seq = last_wal_seq
                    .as_ref()
                    .map(|a| a.load(Ordering::SeqCst))
                    .unwrap_or(0);
                  let encoded =
                    crate::durability::recovery::encode_checkpoint(wal_seq, &data);
                  if let Err(e) = store.save(pid, &encoded).await {
                    warn!(pid = %pid, error = %e, "checkpoint save failed");
                  }
                }
              }
            }
          },
          None => {
            user_closed = true;
            if control_closed {
              break;
            }
          }
        }
      }

      else => break,
    }
  }

  behavior.terminate(&exit_reason).await;
  debug!(pid = %pid, reason = %exit_reason, "process exited");

  ProcessExit {
    pid,
    reason: exit_reason,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    behavior::Behavior,
    error::{Action, Message, Reason, UserPayload},
  };
  use async_trait::async_trait;
  use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
  };

  struct CountBehavior {
    count: Arc<AtomicU32>,
  }

  #[async_trait]
  impl Behavior for CountBehavior {
    async fn init(
      &mut self,
      _cp: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
      Ok(())
    }
    async fn handle(&mut self, _msg: Message) -> Action {
      self.count.fetch_add(1, Ordering::Relaxed);
      Action::Continue
    }
    async fn terminate(&mut self, _reason: &Reason) {}
  }

  #[tokio::test]
  async fn test_process_handles_messages() {
    let count = Arc::new(AtomicU32::new(0));
    let behavior = CountBehavior {
      count: count.clone(),
    };
    let (_pid, handle, join) = spawn_process(behavior, 16, None, None, None);

    handle.send_user(Message::text("a")).await.unwrap();
    handle.send_user(Message::text("b")).await.unwrap();
    handle.send_user(Message::text("c")).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    handle.kill();
    let exit = join.await.unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 3);
    assert!(matches!(exit.reason, Reason::Kill));
  }

  #[tokio::test]
  async fn test_process_kill_is_immediate() {
    let count = Arc::new(AtomicU32::new(0));
    let behavior = CountBehavior {
      count: count.clone(),
    };
    let (_pid, handle, join) = spawn_process(behavior, 16, None, None, None);

    handle.kill();
    let exit = join.await.unwrap();
    assert!(matches!(exit.reason, Reason::Kill));
  }

  struct StopAfterOneBehavior;

  #[async_trait]
  impl Behavior for StopAfterOneBehavior {
    async fn init(
      &mut self,
      _cp: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
      Ok(())
    }
    async fn handle(&mut self, _msg: Message) -> Action {
      Action::Stop(Reason::Normal)
    }
    async fn terminate(&mut self, _reason: &Reason) {}
  }

  #[tokio::test]
  async fn test_process_stops_on_action_stop() {
    let behavior = StopAfterOneBehavior;
    let (_pid, handle, join) = spawn_process(behavior, 16, None, None, None);

    handle.send_user(Message::text("bye")).await.unwrap();
    let exit = join.await.unwrap();
    assert!(matches!(exit.reason, Reason::Normal));
  }

  #[tokio::test]
  async fn test_process_exits_when_senders_dropped() {
    let count = Arc::new(AtomicU32::new(0));
    let behavior = CountBehavior {
      count: count.clone(),
    };
    let (_pid, handle, join) = spawn_process(behavior, 16, None, None, None);

    drop(handle);
    let _exit = join.await.unwrap();
  }

  #[tokio::test]
  async fn test_process_checkpoint_on_action() {
    use crate::durability::{
      checkpoint::SqliteCheckpointStore, recovery::decode_checkpoint,
    };

    struct CheckpointBehavior {
      state: String,
    }

    #[async_trait]
    impl Behavior for CheckpointBehavior {
      async fn init(
        &mut self,
        cp: Option<Vec<u8>>,
      ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(data) = cp {
          self.state = String::from_utf8(data).unwrap();
        }
        Ok(())
      }
      async fn handle(&mut self, msg: Message) -> Action {
        match msg {
          Message::User(UserPayload::Text(ref t)) if t == "checkpoint" => {
            self.state = "checkpointed".into();
            Action::Checkpoint
          }
          Message::User(UserPayload::Text(ref t)) if t == "stop" => {
            Action::Stop(Reason::Normal)
          }
          _ => Action::Continue,
        }
      }
      async fn terminate(&mut self, _reason: &Reason) {}
      fn checkpoint(&self) -> Option<Vec<u8>> {
        Some(self.state.as_bytes().to_vec())
      }
    }

    let store = Arc::new(SqliteCheckpointStore::new(":memory:").unwrap());
    let behavior = CheckpointBehavior {
      state: "initial".into(),
    };
    let (pid, handle, join) = spawn_process(behavior, 16, None, Some(store.clone()), None);

    // Send checkpoint message
    handle.send_user(Message::text("checkpoint")).await.unwrap();
    // Give time to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify checkpoint was saved
    let saved = store.load(pid).await.unwrap().unwrap();
    let (seq, state) = decode_checkpoint(&saved).unwrap();
    assert_eq!(seq, 0);
    assert_eq!(state, b"checkpointed");

    // Stop the process
    handle.send_user(Message::text("stop")).await.unwrap();
    let exit = join.await.unwrap();
    assert!(matches!(exit.reason, Reason::Normal));
  }

  #[tokio::test]
  async fn test_process_should_checkpoint_auto() {
    use crate::durability::checkpoint::SqliteCheckpointStore;

    struct AutoCheckpointBehavior {
      msg_count: u32,
    }

    #[async_trait]
    impl Behavior for AutoCheckpointBehavior {
      async fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
      ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
      }
      async fn handle(&mut self, _msg: Message) -> Action {
        self.msg_count += 1;
        Action::Continue
      }
      async fn terminate(&mut self, _reason: &Reason) {}
      fn should_checkpoint(&self) -> bool {
        self.msg_count % 3 == 0 // checkpoint every 3 messages
      }
      fn checkpoint(&self) -> Option<Vec<u8>> {
        Some(self.msg_count.to_be_bytes().to_vec())
      }
    }

    let store = Arc::new(SqliteCheckpointStore::new(":memory:").unwrap());
    let behavior = AutoCheckpointBehavior { msg_count: 0 };
    let (pid, handle, join) = spawn_process(behavior, 16, None, Some(store.clone()), None);

    // Send 4 messages — checkpoint should trigger after msg 3
    for i in 0..4 {
      handle
        .send_user(Message::text(format!("msg-{i}")))
        .await
        .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify checkpoint exists (saved after 3rd message)
    let saved = store.load(pid).await.unwrap();
    assert!(saved.is_some());

    handle.kill();
    let _ = join.await;
  }
}
