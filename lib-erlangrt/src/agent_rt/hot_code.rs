use std::collections::HashMap;
use std::sync::Arc;

use crate::agent_rt::error::AgentRtError;
use crate::agent_rt::types::{AgentBehavior, AgentPid};

pub type BehaviorFactory = Arc<dyn Fn() -> Arc<dyn AgentBehavior> + Send + Sync>;

pub struct BehaviorVersion {
  pub name: String,
  pub version: String,
  pub factory: BehaviorFactory,
}

pub struct HotCodeRegistry {
  behaviors: HashMap<String, Vec<BehaviorVersion>>,
  process_versions: HashMap<AgentPid, (String, String)>,
  version_history: HashMap<AgentPid, Vec<String>>,
}

impl HotCodeRegistry {
  pub fn new() -> Self {
    Self {
      behaviors: HashMap::new(),
      process_versions: HashMap::new(),
      version_history: HashMap::new(),
    }
  }

  pub fn register(
    &mut self,
    name: &str,
    version: &str,
    factory: BehaviorFactory,
  ) {
    let entry = BehaviorVersion {
      name: name.to_string(),
      version: version.to_string(),
      factory,
    };
    self
      .behaviors
      .entry(name.to_string())
      .or_default()
      .push(entry);
  }

  pub fn list_versions(&self, name: &str) -> Vec<String> {
    self
      .behaviors
      .get(name)
      .map(|versions| versions.iter().map(|v| v.version.clone()).collect())
      .unwrap_or_default()
  }

  pub fn get_factory(
    &self,
    name: &str,
    version: &str,
  ) -> Result<&BehaviorFactory, AgentRtError> {
    let versions = self.behaviors.get(name).ok_or_else(|| {
      AgentRtError::HotCode(format!("behavior '{}' not found", name))
    })?;
    versions
      .iter()
      .find(|v| v.version == version)
      .map(|v| &v.factory)
      .ok_or_else(|| {
        AgentRtError::HotCode(format!(
          "version '{}' of '{}' not found",
          version, name
        ))
      })
  }

  pub fn set_process_version(
    &mut self,
    pid: AgentPid,
    name: &str,
    version: &str,
  ) {
    self
      .process_versions
      .insert(pid, (name.to_string(), version.to_string()));
  }

  pub fn current_version(&self, pid: AgentPid) -> Option<(String, String)> {
    self.process_versions.get(&pid).cloned()
  }

  pub fn remove_process(&mut self, pid: AgentPid) {
    self.process_versions.remove(&pid);
    self.version_history.remove(&pid);
  }

  pub fn pids_on_version(
    &self,
    name: &str,
    version: &str,
  ) -> Vec<AgentPid> {
    self
      .process_versions
      .iter()
      .filter(|(_, (n, v))| n == name && v == version)
      .map(|(pid, _)| *pid)
      .collect()
  }

  pub fn pids_for_behavior(
    &self,
    name: &str,
  ) -> Vec<(AgentPid, String)> {
    self
      .process_versions
      .iter()
      .filter(|(_, (n, _))| n == name)
      .map(|(pid, (_, v))| (*pid, v.clone()))
      .collect()
  }

  pub fn prepare_upgrade(
    &self,
    pid: AgentPid,
    target_version: &str,
  ) -> Result<(String, BehaviorFactory), AgentRtError> {
    let (name, current_vsn) = self
      .process_versions
      .get(&pid)
      .ok_or_else(|| AgentRtError::HotCode("process not tracked".into()))?;
    let factory = self.get_factory(name, target_version)?.clone();
    Ok((current_vsn.clone(), factory))
  }

  pub fn push_version_history(&mut self, pid: AgentPid, version: &str) {
    self
      .version_history
      .entry(pid)
      .or_default()
      .push(version.to_string());
  }

  pub fn prepare_rollback(
    &self,
    pid: AgentPid,
  ) -> Result<(String, BehaviorFactory), AgentRtError> {
    let (name, _) = self
      .process_versions
      .get(&pid)
      .ok_or_else(|| AgentRtError::HotCode("process not tracked".into()))?;
    let history = self
      .version_history
      .get(&pid)
      .ok_or_else(|| AgentRtError::HotCode("no version history".into()))?;
    let prev_vsn = history
      .last()
      .ok_or_else(|| AgentRtError::HotCode("no previous version".into()))?;
    let factory = self.get_factory(name, prev_vsn)?.clone();
    Ok((prev_vsn.clone(), factory))
  }

  pub fn stale_pids(&self, name: &str) -> Vec<AgentPid> {
    let versions = match self.behaviors.get(name) {
      Some(v) if v.len() >= 3 => v,
      _ => return Vec::new(),
    };
    let oldest = &versions[0].version;
    self.pids_on_version(name, oldest)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::types::*;

  struct SimpleState(#[allow(dead_code)] i64);

  impl AgentState for SimpleState {
    fn as_any(&self) -> &dyn std::any::Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
      self
    }
  }

  struct EchoBehavior;

  impl AgentBehavior for EchoBehavior {
    fn init(
      &self,
      _args: serde_json::Value,
    ) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(SimpleState(0)))
    }

    fn handle_message(
      &self,
      _msg: Message,
      _state: &mut dyn AgentState,
    ) -> Action {
      Action::Continue
    }

    fn handle_exit(
      &self,
      _from: AgentPid,
      _reason: Reason,
      _state: &mut dyn AgentState,
    ) -> Action {
      Action::Continue
    }

    fn terminate(
      &self,
      _reason: Reason,
      _state: &mut dyn AgentState,
    ) {
    }
  }

  // Task 8 tests

  #[test]
  fn test_register_behavior() {
    let mut reg = HotCodeRegistry::new();
    reg.register("echo", "v1", Arc::new(|| {
      Arc::new(EchoBehavior) as Arc<dyn AgentBehavior>
    }));
    let versions = reg.list_versions("echo");
    assert_eq!(versions, vec!["v1"]);
  }

  #[test]
  fn test_register_multiple_versions() {
    let mut reg = HotCodeRegistry::new();
    reg.register("echo", "v1", Arc::new(|| Arc::new(EchoBehavior)));
    reg.register("echo", "v2", Arc::new(|| Arc::new(EchoBehavior)));
    let versions = reg.list_versions("echo");
    assert_eq!(versions, vec!["v1", "v2"]);
  }

  #[test]
  fn test_list_versions_unknown() {
    let reg = HotCodeRegistry::new();
    assert!(reg.list_versions("unknown").is_empty());
  }

  #[test]
  fn test_track_process_version() {
    let mut reg = HotCodeRegistry::new();
    let pid = AgentPid::new();
    reg.register("echo", "v1", Arc::new(|| Arc::new(EchoBehavior)));
    reg.set_process_version(pid, "echo", "v1");
    assert_eq!(
      reg.current_version(pid),
      Some(("echo".to_string(), "v1".to_string()))
    );
  }

  #[test]
  fn test_untrack_process() {
    let mut reg = HotCodeRegistry::new();
    let pid = AgentPid::new();
    reg.set_process_version(pid, "echo", "v1");
    reg.remove_process(pid);
    assert_eq!(reg.current_version(pid), None);
  }

  // Task 9 tests

  #[test]
  fn test_upgrade_process() {
    let mut reg = HotCodeRegistry::new();
    let pid = AgentPid::new();
    reg.register("echo", "v1", Arc::new(|| Arc::new(EchoBehavior)));
    reg.register("echo", "v2", Arc::new(|| Arc::new(EchoBehavior)));
    reg.set_process_version(pid, "echo", "v1");

    let report = reg.prepare_upgrade(pid, "v2");
    assert!(report.is_ok());
    let (old_vsn, _factory) = report.unwrap();
    assert_eq!(old_vsn, "v1");
    reg.set_process_version(pid, "echo", "v2");
    assert_eq!(
      reg.current_version(pid),
      Some(("echo".into(), "v2".into()))
    );
  }

  #[test]
  fn test_upgrade_process_unknown_version() {
    let mut reg = HotCodeRegistry::new();
    let pid = AgentPid::new();
    reg.register("echo", "v1", Arc::new(|| Arc::new(EchoBehavior)));
    reg.set_process_version(pid, "echo", "v1");

    let report = reg.prepare_upgrade(pid, "v99");
    assert!(report.is_err());
  }

  #[test]
  fn test_rollback_process() {
    let mut reg = HotCodeRegistry::new();
    let pid = AgentPid::new();
    reg.register("echo", "v1", Arc::new(|| Arc::new(EchoBehavior)));
    reg.register("echo", "v2", Arc::new(|| Arc::new(EchoBehavior)));
    reg.set_process_version(pid, "echo", "v1");
    reg.push_version_history(pid, "v1");
    reg.set_process_version(pid, "echo", "v2");

    let rollback = reg.prepare_rollback(pid);
    assert!(rollback.is_ok());
    let (prev_vsn, _factory) = rollback.unwrap();
    assert_eq!(prev_vsn, "v1");
  }

  #[test]
  fn test_upgrade_all_for_behavior() {
    let mut reg = HotCodeRegistry::new();
    let p1 = AgentPid::new();
    let p2 = AgentPid::new();
    let p3 = AgentPid::new();

    reg.register(
      "worker",
      "v1",
      Arc::new(|| Arc::new(EchoBehavior)),
    );
    reg.register(
      "worker",
      "v2",
      Arc::new(|| Arc::new(EchoBehavior)),
    );
    reg.register(
      "other",
      "v1",
      Arc::new(|| Arc::new(EchoBehavior)),
    );

    reg.set_process_version(p1, "worker", "v1");
    reg.set_process_version(p2, "worker", "v1");
    reg.set_process_version(p3, "other", "v1");

    let pids = reg.pids_for_behavior("worker");
    assert_eq!(pids.len(), 2);
    let worker_pids: Vec<AgentPid> =
      pids.iter().map(|(pid, _)| *pid).collect();
    assert!(worker_pids.contains(&p1));
    assert!(worker_pids.contains(&p2));
  }

  #[test]
  fn test_stale_version_detection() {
    let mut reg = HotCodeRegistry::new();
    let pid = AgentPid::new();
    reg.register("echo", "v1", Arc::new(|| Arc::new(EchoBehavior)));
    reg.register("echo", "v2", Arc::new(|| Arc::new(EchoBehavior)));
    reg.register("echo", "v3", Arc::new(|| Arc::new(EchoBehavior)));
    reg.set_process_version(pid, "echo", "v1");

    let stale = reg.stale_pids("echo");
    assert!(stale.contains(&pid));
  }
}
