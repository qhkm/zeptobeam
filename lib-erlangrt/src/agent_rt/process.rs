use std::{collections::VecDeque, sync::Arc};

use crate::agent_rt::types::*;

/// A running agent process with its own mailbox,
/// state, and scheduling metadata.
pub struct AgentProcess {
  pub pid: AgentPid,
  pub behavior: Arc<dyn AgentBehavior>,
  pub state: Option<Box<dyn AgentState>>,
  pub mailbox: VecDeque<Message>,
  pub reductions: u32,
  pub status: ProcessStatus,
  pub priority: Priority,
  pub links: Vec<AgentPid>,
  pub monitors: Vec<(MonitorRef, AgentPid)>,
  pub monitored_by: Vec<(MonitorRef, AgentPid)>,
  pub supervisor: Option<AgentPid>,
  pub trap_exit: bool,
}

/// Scheduling status of an agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
  Runnable,
  Waiting,
  Suspended,
  Exiting,
}

/// Scheduling priority level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
  High,
  Normal,
  Low,
}

impl Default for Priority {
  fn default() -> Self {
    Self::Normal
  }
}

impl AgentProcess {
  pub fn new(
    behavior: Arc<dyn AgentBehavior>,
    args: serde_json::Value,
  ) -> Result<Self, Reason> {
    let pid = AgentPid::new();
    let state = behavior.init(args)?;
    Ok(Self {
      pid,
      behavior,
      state: Some(state),
      mailbox: VecDeque::new(),
      reductions: 0,
      status: ProcessStatus::Runnable,
      priority: Priority::Normal,
      links: Vec::new(),
      monitors: Vec::new(),
      monitored_by: Vec::new(),
      supervisor: None,
      trap_exit: false,
    })
  }

  pub fn deliver_message(&mut self, msg: Message) {
    self.mailbox.push_back(msg);
    if self.status == ProcessStatus::Waiting {
      self.status = ProcessStatus::Runnable;
    }
  }

  pub fn has_messages(&self) -> bool {
    !self.mailbox.is_empty()
  }

  pub fn next_message(&mut self) -> Option<Message> {
    self.mailbox.pop_front()
  }

  pub fn terminate(&mut self, reason: Reason) {
    self.status = ProcessStatus::Exiting;
    if let Some(ref mut state) = self.state {
      let behavior = self.behavior.clone();
      let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        behavior.terminate(reason, state.as_mut());
      }));
    }
    self.state = None;
  }
}
