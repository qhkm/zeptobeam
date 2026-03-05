//! Startup helpers for spawning agents from config.

use std::sync::Arc;

use tracing::info;

use crate::agent_rt::{
  agent_worker::AgentWorkerBehavior,
  config::AgentConfig,
  scheduler::AgentScheduler,
  types::AgentPid,
};

/// Spawn agent processes for each `auto_start = true` entry in config.
/// Returns a vec of (pid, name) for successfully spawned agents.
pub fn spawn_configured_agents(
  scheduler: &mut AgentScheduler,
  agents: &[AgentConfig],
) -> Result<Vec<(AgentPid, String)>, String> {
  let mut spawned = Vec::new();

  for agent_cfg in agents {
    if !agent_cfg.auto_start {
      continue;
    }

    let behavior = Arc::new(AgentWorkerBehavior::new(
      agent_cfg.name.clone(),
      agent_cfg.provider.clone(),
      agent_cfg.model.clone(),
      agent_cfg.system_prompt.clone(),
      agent_cfg.tools.clone(),
      agent_cfg.max_iterations,
      agent_cfg.timeout_ms,
    ));

    let pid = scheduler
      .registry
      .spawn(behavior, serde_json::json!({}))
      .map_err(|r| format!("Failed to spawn agent '{}': {:?}", agent_cfg.name, r))?;

    scheduler
      .registry
      .register_name(&agent_cfg.name, pid)
      .map_err(|e| format!("Failed to register agent '{}': {}", agent_cfg.name, e))?;

    info!(
      name = %agent_cfg.name,
      pid = pid.raw(),
      provider = %agent_cfg.provider,
      "spawned agent from config"
    );

    spawned.push((pid, agent_cfg.name.clone()));
  }

  Ok(spawned)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::{
    config::AgentConfig, process::ProcessStatus, scheduler::AgentScheduler,
  };

  #[test]
  fn test_spawn_configured_agents_empty() {
    let mut scheduler = AgentScheduler::new();
    let result = spawn_configured_agents(&mut scheduler, &[]);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 0);
  }

  #[test]
  fn test_spawn_configured_agents_auto_start_true() {
    let mut scheduler = AgentScheduler::new();
    let agents = vec![AgentConfig {
      name: "test-agent".into(),
      provider: "openrouter".into(),
      model: None,
      system_prompt: None,
      tools: vec![],
      auto_start: true,
      max_iterations: None,
      timeout_ms: None,
    }];
    let result = spawn_configured_agents(&mut scheduler, &agents).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, "test-agent");
    // Verify process is registered by name
    let pid = scheduler.registry.lookup_name("test-agent");
    assert!(pid.is_some());
    assert_eq!(pid.unwrap(), result[0].0);
  }

  #[test]
  fn test_spawn_configured_agents_skips_auto_start_false() {
    let mut scheduler = AgentScheduler::new();
    let agents = vec![
      AgentConfig {
        name: "active".into(),
        provider: "openrouter".into(),
        model: None,
        system_prompt: None,
        tools: vec![],
        auto_start: true,
        max_iterations: None,
        timeout_ms: None,
      },
      AgentConfig {
        name: "inactive".into(),
        provider: "openai".into(),
        model: None,
        system_prompt: None,
        tools: vec![],
        auto_start: false,
        max_iterations: None,
        timeout_ms: None,
      },
    ];
    let result = spawn_configured_agents(&mut scheduler, &agents).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, "active");
    assert!(scheduler.registry.lookup_name("inactive").is_none());
  }

  #[test]
  fn test_spawn_configured_agents_enqueues_processes() {
    let mut scheduler = AgentScheduler::new();
    let agents = vec![AgentConfig {
      name: "worker".into(),
      provider: "openrouter".into(),
      model: None,
      system_prompt: None,
      tools: vec![],
      auto_start: true,
      max_iterations: None,
      timeout_ms: None,
    }];
    let spawned = spawn_configured_agents(&mut scheduler, &agents).unwrap();
    let pid = spawned[0].0;
    // Process should be in the registry and runnable
    let proc = scheduler.registry.lookup(&pid).unwrap();
    assert_eq!(proc.status, ProcessStatus::Runnable);
  }
}
