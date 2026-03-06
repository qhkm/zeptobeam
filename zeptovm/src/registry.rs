use dashmap::DashMap;
use tokio::task::JoinHandle;

use crate::{
  error::Message,
  mailbox::ProcessHandle,
  pid::Pid,
  process::{spawn_process, ProcessExit},
};

pub struct ProcessRegistry {
  handles: DashMap<Pid, ProcessHandle>,
}

impl ProcessRegistry {
  pub fn new() -> Self {
    Self {
      handles: DashMap::new(),
    }
  }

  pub fn spawn(
    &self,
    behavior: impl crate::behavior::Behavior,
    user_mailbox_capacity: usize,
    checkpoint: Option<Vec<u8>>,
  ) -> (Pid, JoinHandle<ProcessExit>) {
    let (pid, handle, join) = spawn_process(behavior, user_mailbox_capacity, checkpoint);
    self.handles.insert(pid, handle);
    (pid, join)
  }

  pub fn lookup(&self, pid: &Pid) -> Option<ProcessHandle> {
    self.handles.get(pid).map(|r| r.value().clone())
  }

  pub async fn send(&self, pid: &Pid, msg: Message) -> Result<(), String> {
    // Clone handle to release the DashMap guard before awaiting
    let handle = self
      .handles
      .get(pid)
      .map(|r| r.value().clone())
      .ok_or_else(|| format!("process {pid} not found"))?;
    handle
      .send_user(msg)
      .await
      .map_err(|_| format!("process {pid} mailbox closed"))
  }

  pub fn kill(&self, pid: &Pid) {
    if let Some(handle) = self.handles.get(pid) {
      handle.kill();
    }
  }

  pub fn remove(&self, pid: &Pid) {
    self.handles.remove(pid);
  }

  pub fn count(&self) -> usize {
    self.handles.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    behavior::Behavior,
    error::{Action, Message, Reason},
  };
  use async_trait::async_trait;

  struct Echo;

  #[async_trait]
  impl Behavior for Echo {
    async fn init(
      &mut self,
      _cp: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
      Ok(())
    }
    async fn handle(&mut self, _msg: Message) -> Action {
      Action::Continue
    }
    async fn terminate(&mut self, _reason: &Reason) {}
  }

  #[tokio::test]
  async fn test_registry_spawn_and_lookup() {
    let registry = ProcessRegistry::new();
    let (pid, _join) = registry.spawn(Echo, 16, None);
    assert!(registry.lookup(&pid).is_some());
  }

  #[tokio::test]
  async fn test_registry_send_message() {
    let registry = ProcessRegistry::new();
    let (pid, _join) = registry.spawn(Echo, 16, None);
    let result = registry.send(&pid, Message::text("hi")).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_registry_kill_and_remove() {
    let registry = ProcessRegistry::new();
    let (pid, join) = registry.spawn(Echo, 16, None);
    registry.kill(&pid);
    let _ = join.await;
    registry.remove(&pid);
    assert!(registry.lookup(&pid).is_none());
  }

  #[tokio::test]
  async fn test_registry_count() {
    let registry = ProcessRegistry::new();
    assert_eq!(registry.count(), 0);
    let (_pid1, _j1) = registry.spawn(Echo, 16, None);
    let (_pid2, _j2) = registry.spawn(Echo, 16, None);
    assert_eq!(registry.count(), 2);
  }
}
