use std::collections::HashMap;
use std::sync::Arc;

use crate::agent_rt::process::AgentProcess;
use crate::agent_rt::types::*;

pub struct AgentRegistry {
  processes: HashMap<AgentPid, AgentProcess>,
  names: HashMap<String, AgentPid>,
}

impl AgentRegistry {
  pub fn new() -> Self {
    Self {
      processes: HashMap::new(),
      names: HashMap::new(),
    }
  }

  pub fn spawn(
    &mut self,
    behavior: Arc<dyn AgentBehavior>,
    args: serde_json::Value,
  ) -> Result<AgentPid, Reason> {
    let proc = AgentProcess::new(behavior, args)?;
    let pid = proc.pid;
    self.processes.insert(pid, proc);
    Ok(pid)
  }

  pub fn lookup(&self, pid: &AgentPid) -> Option<&AgentProcess> {
    self.processes.get(pid)
  }

  pub fn lookup_mut(
    &mut self,
    pid: &AgentPid,
  ) -> Option<&mut AgentProcess> {
    self.processes.get_mut(pid)
  }

  pub fn remove(
    &mut self,
    pid: &AgentPid,
  ) -> Option<AgentProcess> {
    let proc = self.processes.remove(pid);
    if proc.is_some() {
      self.names.retain(|_, v| v != pid);
    }
    proc
  }

  pub fn register_name(
    &mut self,
    name: &str,
    pid: AgentPid,
  ) {
    self.names.insert(name.to_string(), pid);
  }

  pub fn unregister_name(&mut self, name: &str) {
    self.names.remove(name);
  }

  pub fn lookup_name(&self, name: &str) -> Option<AgentPid> {
    self.names.get(name).copied()
  }

  pub fn count(&self) -> usize {
    self.processes.len()
  }

  pub fn pids(&self) -> Vec<AgentPid> {
    self.processes.keys().copied().collect()
  }
}
