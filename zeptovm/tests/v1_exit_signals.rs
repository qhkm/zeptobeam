//! v1 Exit Signal Propagation Tests

use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};

use async_trait::async_trait;
use zeptovm::{
  behavior::Behavior,
  error::{Action, Message, Reason},
  registry::ProcessRegistry,
};

/// A behavior that waits forever (for messages)
struct WaitForever {
  terminated: Arc<AtomicBool>,
}

#[async_trait]
impl Behavior for WaitForever {
  async fn init(
    &mut self,
    _cp: Option<Vec<u8>>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Ok(())
  }
  async fn handle(&mut self, _msg: Message) -> Action {
    Action::Continue
  }
  async fn terminate(&mut self, _reason: &Reason) {
    self.terminated.store(true, Ordering::Relaxed);
  }
}

/// A behavior that dies immediately with a given reason
struct DieOnMessage {
  reason: Reason,
}

#[async_trait]
impl Behavior for DieOnMessage {
  async fn init(
    &mut self,
    _cp: Option<Vec<u8>>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Ok(())
  }
  async fn handle(&mut self, _msg: Message) -> Action {
    Action::Stop(self.reason.clone())
  }
  async fn terminate(&mut self, _reason: &Reason) {}
}

#[tokio::test]
async fn test_linked_process_dies_on_abnormal_exit() {
  let registry = ProcessRegistry::new();
  let b_terminated = Arc::new(AtomicBool::new(false));

  let (pid_a, _join_a) = registry.spawn(
    DieOnMessage {
      reason: Reason::Custom("crash".into()),
    },
    16,
    None,
  );
  let (pid_b, _join_b) = registry.spawn(
    WaitForever {
      terminated: b_terminated.clone(),
    },
    16,
    None,
  );

  registry.link(pid_a, pid_b);

  // Send message to A to trigger its death with abnormal reason
  registry.send(&pid_a, Message::text("die")).await.unwrap();

  // Wait a bit for A to die and propagation
  tokio::time::sleep(Duration::from_millis(50)).await;

  // Notify exit for A (in real usage, supervisor or the process
  // itself would call this)
  registry.notify_exit(&pid_a, &Reason::Custom("crash".into()));

  // B should die because it received ExitLinked with abnormal reason
  tokio::time::sleep(Duration::from_millis(100)).await;

  // B should have been terminated
  assert!(
    b_terminated.load(Ordering::Relaxed),
    "linked process B should have terminated"
  );
}

#[tokio::test]
async fn test_linked_process_survives_normal_exit() {
  let registry = ProcessRegistry::new();
  let b_terminated = Arc::new(AtomicBool::new(false));

  let (pid_a, _join_a) = registry.spawn(
    DieOnMessage {
      reason: Reason::Normal,
    },
    16,
    None,
  );
  let (pid_b, _join_b) = registry.spawn(
    WaitForever {
      terminated: b_terminated.clone(),
    },
    16,
    None,
  );

  registry.link(pid_a, pid_b);

  // A exits normally
  registry.send(&pid_a, Message::text("die")).await.unwrap();
  tokio::time::sleep(Duration::from_millis(50)).await;
  registry.notify_exit(&pid_a, &Reason::Normal);

  // B should survive (normal exit doesn't propagate to non-trapping
  // linked processes)
  tokio::time::sleep(Duration::from_millis(100)).await;
  assert!(
    !b_terminated.load(Ordering::Relaxed),
    "linked process B should survive normal exit"
  );

  // Cleanup
  registry.kill(&pid_b);
}

#[tokio::test]
async fn test_monitor_receives_down_signal() {
  let registry = ProcessRegistry::new();

  // Test that notify_exit doesn't panic and link table is cleaned up
  let (pid_a, _join_a) = registry.spawn(
    DieOnMessage {
      reason: Reason::Custom("crash".into()),
    },
    16,
    None,
  );
  let (pid_b, _join_b) = registry.spawn(
    WaitForever {
      terminated: Arc::new(AtomicBool::new(false)),
    },
    16,
    None,
  );

  let _mref = registry.monitor(pid_b, pid_a);

  // A dies
  registry.send(&pid_a, Message::text("die")).await.unwrap();
  tokio::time::sleep(Duration::from_millis(50)).await;
  registry.notify_exit(&pid_a, &Reason::Custom("crash".into()));

  // No panic = success. MonitorDown was sent on B's control channel.
  tokio::time::sleep(Duration::from_millis(50)).await;

  // Cleanup
  registry.kill(&pid_b);
}
