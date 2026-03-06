//! Polling-based config file watcher for hot-reload.
//!
//! Periodically re-reads the TOML config and compares the config hash.
//! When the hash changes, a `ConfigChange` is emitted through an mpsc
//! channel so the daemon loop can add/remove agents accordingly.

use std::collections::HashSet;
use std::time::Duration;

use tracing::{debug, info, warn};

use erlangrt::agent_rt::config::load_config;

use crate::agent_adapter::AgentAdapterConfig;
use crate::config_loader::resolve_agents_from_config;

/// The result of a config reload check.
#[derive(Debug)]
pub struct ConfigChange {
  pub new_agents: Vec<AgentAdapterConfig>,
  pub config_hash: u64,
}

/// Describes what agents need to be added or removed.
#[derive(Debug)]
pub struct AgentDiff {
  pub to_add: Vec<AgentAdapterConfig>,
  pub to_remove: Vec<String>,
}

/// Watches a config file for changes by polling and comparing config hashes.
pub struct ConfigWatcher {
  config_path: String,
  poll_interval: Duration,
  last_hash: u64,
}

impl ConfigWatcher {
  pub fn new(
    config_path: String,
    poll_interval: Duration,
    initial_hash: u64,
  ) -> Self {
    Self {
      config_path,
      poll_interval,
      last_hash: initial_hash,
    }
  }

  /// Check if config has changed. Returns `Some(ConfigChange)` if it has.
  pub fn check_for_changes(&mut self) -> Option<ConfigChange> {
    let config = match load_config(&self.config_path) {
      Ok(c) => c,
      Err(e) => {
        warn!(
          path = %self.config_path,
          error = %e,
          "failed to reload config"
        );
        return None;
      }
    };

    let resolved = match resolve_agents_from_config(&config) {
      Ok(r) => r,
      Err(errors) => {
        for e in &errors {
          warn!(error = %e, "config validation error on reload");
        }
        return None;
      }
    };

    if resolved.config_hash == self.last_hash {
      return None;
    }

    info!(
      old_hash = self.last_hash,
      new_hash = resolved.config_hash,
      agent_count = resolved.agents.len(),
      "config change detected"
    );
    self.last_hash = resolved.config_hash;

    Some(ConfigChange {
      new_agents: resolved.agents,
      config_hash: resolved.config_hash,
    })
  }

  /// Compute the diff between currently running agents and new config.
  pub fn diff_agents(
    running: &[String],
    new_agents: &[AgentAdapterConfig],
  ) -> AgentDiff {
    let new_names: HashSet<&str> =
      new_agents.iter().map(|a| a.name.as_str()).collect();
    let running_names: HashSet<&str> =
      running.iter().map(|s| s.as_str()).collect();

    let to_add: Vec<AgentAdapterConfig> = new_agents
      .iter()
      .filter(|a| !running_names.contains(a.name.as_str()))
      .cloned()
      .collect();

    let to_remove: Vec<String> = running
      .iter()
      .filter(|name| !new_names.contains(name.as_str()))
      .cloned()
      .collect();

    AgentDiff { to_add, to_remove }
  }
}

/// Spawn a background task that polls the config file for changes.
/// Returns a join handle and a receiver that emits `ConfigChange`
/// whenever the config hash differs from the previous check.
pub fn spawn_config_watcher(
  config_path: String,
  poll_interval: Duration,
  initial_hash: u64,
) -> (
  tokio::task::JoinHandle<()>,
  tokio::sync::mpsc::Receiver<ConfigChange>,
) {
  let (tx, rx) = tokio::sync::mpsc::channel(4);
  let handle = tokio::spawn(async move {
    let mut watcher =
      ConfigWatcher::new(config_path, poll_interval, initial_hash);
    loop {
      tokio::time::sleep(watcher.poll_interval).await;
      if let Some(change) = watcher.check_for_changes() {
        if tx.send(change).await.is_err() {
          debug!("config watcher channel closed, stopping");
          break;
        }
      }
    }
  });
  (handle, rx)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn test_config(name: &str) -> AgentAdapterConfig {
    AgentAdapterConfig {
      name: name.into(),
      provider: "test".into(),
      model: None,
      system_prompt: None,
      tools: vec![],
      max_iterations: None,
      timeout_ms: None,
    }
  }

  #[test]
  fn test_diff_no_changes() {
    let running = vec!["agent1".into(), "agent2".into()];
    let new_agents =
      vec![test_config("agent1"), test_config("agent2")];
    let diff = ConfigWatcher::diff_agents(&running, &new_agents);
    assert!(diff.to_add.is_empty());
    assert!(diff.to_remove.is_empty());
  }

  #[test]
  fn test_diff_add_agent() {
    let running = vec!["agent1".into()];
    let new_agents =
      vec![test_config("agent1"), test_config("agent2")];
    let diff = ConfigWatcher::diff_agents(&running, &new_agents);
    assert_eq!(diff.to_add.len(), 1);
    assert_eq!(diff.to_add[0].name, "agent2");
    assert!(diff.to_remove.is_empty());
  }

  #[test]
  fn test_diff_remove_agent() {
    let running = vec!["agent1".into(), "agent2".into()];
    let new_agents = vec![test_config("agent1")];
    let diff = ConfigWatcher::diff_agents(&running, &new_agents);
    assert!(diff.to_add.is_empty());
    assert_eq!(diff.to_remove.len(), 1);
    assert_eq!(diff.to_remove[0], "agent2");
  }

  #[test]
  fn test_diff_add_and_remove() {
    let running = vec!["agent1".into(), "agent2".into()];
    let new_agents =
      vec![test_config("agent1"), test_config("agent3")];
    let diff = ConfigWatcher::diff_agents(&running, &new_agents);
    assert_eq!(diff.to_add.len(), 1);
    assert_eq!(diff.to_add[0].name, "agent3");
    assert_eq!(diff.to_remove.len(), 1);
    assert_eq!(diff.to_remove[0], "agent2");
  }

  #[test]
  fn test_diff_all_new() {
    let running: Vec<String> = vec![];
    let new_agents =
      vec![test_config("agent1"), test_config("agent2")];
    let diff = ConfigWatcher::diff_agents(&running, &new_agents);
    assert_eq!(diff.to_add.len(), 2);
    assert!(diff.to_remove.is_empty());
  }

  #[test]
  fn test_diff_all_removed() {
    let running = vec!["agent1".into(), "agent2".into()];
    let new_agents: Vec<AgentAdapterConfig> = vec![];
    let diff = ConfigWatcher::diff_agents(&running, &new_agents);
    assert!(diff.to_add.is_empty());
    assert_eq!(diff.to_remove.len(), 2);
  }

  #[test]
  fn test_watcher_no_change_on_same_hash() {
    // check_for_changes will fail to load the non-existent file and
    // return None — verifying graceful error handling.
    let mut watcher = ConfigWatcher::new(
      "nonexistent.toml".into(),
      Duration::from_secs(5),
      12345,
    );
    assert!(watcher.check_for_changes().is_none());
  }
}
