use std::sync::RwLock;

use dashmap::DashMap;
use tokio::task::JoinHandle;

use crate::{
  error::{Message, Reason, SystemMsg},
  link::{LinkTable, MonitorRef},
  mailbox::ProcessHandle,
  pid::Pid,
  process::{spawn_process, ProcessExit},
};

pub struct ProcessRegistry {
  handles: DashMap<Pid, ProcessHandle>,
  links: RwLock<LinkTable>,
}

impl ProcessRegistry {
  pub fn new() -> Self {
    Self {
      handles: DashMap::new(),
      links: RwLock::new(LinkTable::new()),
    }
  }

  pub fn spawn(
    &self,
    behavior: impl crate::behavior::Behavior,
    user_mailbox_capacity: usize,
    checkpoint: Option<Vec<u8>>,
  ) -> (Pid, JoinHandle<ProcessExit>) {
    let (pid, handle, join) =
      spawn_process(behavior, user_mailbox_capacity, checkpoint, None);
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

  /// Create a bidirectional link between two processes.
  pub fn link(&self, a: Pid, b: Pid) {
    self.links.write().unwrap().link(a, b);
  }

  /// Remove a bidirectional link between two processes.
  pub fn unlink(&self, a: Pid, b: Pid) {
    self.links.write().unwrap().unlink(a, b);
  }

  /// Create a unidirectional monitor: watcher monitors target.
  pub fn monitor(&self, watcher: Pid, target: Pid) -> MonitorRef {
    self.links.write().unwrap().monitor(watcher, target)
  }

  /// Remove a monitor.
  pub fn demonitor(&self, mref: MonitorRef) {
    self.links.write().unwrap().demonitor(mref);
  }

  /// Propagate exit signals to linked and monitoring processes.
  /// Per Erlang semantics, `Kill` is converted to `Killed` to prevent
  /// infinite kill propagation through link chains.
  pub fn notify_exit(&self, pid: &Pid, reason: &Reason) {
    let (linked, monitors) = {
      let mut lt = self.links.write().unwrap();
      let linked = lt.remove_all(pid);
      let monitors = lt.remove_monitors_of(pid);
      (linked, monitors)
    };

    // Convert Kill → Killed for propagation (Erlang semantics)
    let propagated_reason = match reason {
      Reason::Kill => Reason::Custom("killed".into()),
      other => other.clone(),
    };

    for linked_pid in linked {
      if let Some(handle) = self.handles.get(&linked_pid) {
        let _ =
          handle.try_send_control(SystemMsg::ExitLinked(*pid, propagated_reason.clone()));
      }
    }

    for (_mref, watcher_pid) in monitors {
      if let Some(handle) = self.handles.get(&watcher_pid) {
        let _ = handle
          .try_send_control(SystemMsg::MonitorDown(*pid, propagated_reason.clone()));
      }
    }
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
