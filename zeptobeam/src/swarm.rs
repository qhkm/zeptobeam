//! Swarm orchestration for multi-agent workflows.
//!
//! Supports three execution strategies:
//! - Sequential: agents run one after another, each receiving the same message
//! - Parallel: all agents run concurrently, results collected
//! - DAG: directed acyclic graph with dependency edges

use std::collections::{HashMap, HashSet, VecDeque};
use tracing::{debug, info, warn};

use zeptovm::error::Message;
use zeptovm::process::{spawn_process, ProcessExit};

use crate::agent_adapter::{create_agent_adapter, AgentAdapterConfig};

/// A step in the swarm — one agent invocation.
#[derive(Debug, Clone)]
pub struct SwarmStep {
  /// Unique identifier for this step.
  pub id: String,
  /// Agent config for this step.
  pub config: AgentAdapterConfig,
  /// IDs of steps that must complete before this one starts (for DAG mode).
  pub depends_on: Vec<String>,
}

/// Result of a single swarm step.
#[derive(Debug)]
pub struct StepResult {
  pub step_id: String,
  pub exit: ProcessExit,
}

/// Strategy for executing a swarm.
#[derive(Debug, Clone)]
pub enum SwarmStrategy {
  Sequential,
  Parallel,
  Dag,
}

/// Execute a swarm of agents sequentially.
/// Each agent receives the initial message.
/// Returns results in execution order.
pub async fn run_sequential(
  steps: Vec<SwarmStep>,
  message: Message,
) -> Vec<StepResult> {
  let mut results = Vec::new();

  for step in steps {
    info!(step_id = %step.id, "starting sequential step");
    let adapter = create_agent_adapter(step.config);
    let (_pid, handle, join) = spawn_process(adapter, 64, None, None, None);

    if let Err(e) = handle.send_user(message.clone()).await {
      warn!(step_id = %step.id, error = %e, "failed to send message");
      handle.kill();
    }

    // Drop the handle so the process can exit if it doesn't stop on its own
    drop(handle);

    // Wait for the process to finish
    match join.await {
      Ok(exit) => {
        debug!(step_id = %step.id, reason = %exit.reason, "step completed");
        results.push(StepResult {
          step_id: step.id,
          exit,
        });
      }
      Err(e) => {
        warn!(step_id = %step.id, error = %e, "step join failed");
      }
    }
  }

  results
}

/// Execute a swarm of agents in parallel.
/// All agents receive the same message and run concurrently.
/// Returns results (order not guaranteed).
pub async fn run_parallel(
  steps: Vec<SwarmStep>,
  message: Message,
) -> Vec<StepResult> {
  let mut join_handles = Vec::new();

  for step in &steps {
    info!(step_id = %step.id, "starting parallel step");
    let adapter = create_agent_adapter(step.config.clone());
    let (_pid, proc_handle, join) = spawn_process(adapter, 64, None, None, None);

    if let Err(e) = proc_handle.send_user(message.clone()).await {
      warn!(step_id = %step.id, error = %e, "failed to send message");
      proc_handle.kill();
    }

    // Drop the handle so the process can exit cleanly
    drop(proc_handle);

    join_handles.push((step.id.clone(), join));
  }

  let mut results = Vec::new();
  for (step_id, join) in join_handles {
    match join.await {
      Ok(exit) => {
        debug!(step_id = %step_id, reason = %exit.reason, "parallel step completed");
        results.push(StepResult { step_id, exit });
      }
      Err(e) => {
        warn!(step_id = %step_id, error = %e, "parallel step join failed");
      }
    }
  }

  results
}

/// Execute a swarm as a DAG.
/// Steps with no dependencies start first. As they complete,
/// dependent steps become ready and start.
/// Returns results in completion order.
pub async fn run_dag(
  steps: Vec<SwarmStep>,
  message: Message,
) -> Vec<StepResult> {
  // Build adjacency: which steps depend on which
  let step_map: HashMap<String, SwarmStep> =
    steps.into_iter().map(|s| (s.id.clone(), s)).collect();

  // Track completed steps
  let mut completed: HashSet<String> = HashSet::new();
  let mut results = Vec::new();

  // Find ready steps (no unmet dependencies)
  let mut pending: HashSet<String> = step_map.keys().cloned().collect();

  while !pending.is_empty() {
    // Find all steps whose dependencies are satisfied
    let ready: Vec<String> = pending
      .iter()
      .filter(|id| {
        let step = &step_map[*id];
        step.depends_on.iter().all(|dep| completed.contains(dep))
      })
      .cloned()
      .collect();

    if ready.is_empty() {
      warn!(
        pending = ?pending,
        "DAG deadlock: no ready steps but pending remain"
      );
      break;
    }

    // Execute all ready steps in parallel
    let mut wave_handles = Vec::new();
    for step_id in &ready {
      pending.remove(step_id);
      let step = &step_map[step_id];
      info!(step_id = %step_id, "starting DAG step");

      let adapter = create_agent_adapter(step.config.clone());
      let (_pid, proc_handle, join) =
        spawn_process(adapter, 64, None, None, None);

      if let Err(e) = proc_handle.send_user(message.clone()).await {
        warn!(step_id = %step_id, error = %e, "failed to send message");
        proc_handle.kill();
      }

      // Drop the handle so the process can exit cleanly
      drop(proc_handle);

      wave_handles.push((step_id.clone(), join));
    }

    // Wait for this wave to complete
    for (step_id, join) in wave_handles {
      match join.await {
        Ok(exit) => {
          debug!(step_id = %step_id, reason = %exit.reason, "DAG step completed");
          completed.insert(step_id.clone());
          results.push(StepResult { step_id, exit });
        }
        Err(e) => {
          warn!(step_id = %step_id, error = %e, "DAG step join failed");
          // Mark as completed so dependents can still proceed
          completed.insert(step_id.clone());
        }
      }
    }
  }

  results
}

/// Validate a DAG for cycles. Returns Ok(()) if acyclic,
/// Err with cycle info otherwise.
pub fn validate_dag(steps: &[SwarmStep]) -> Result<(), String> {
  let ids: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();

  // Check all dependencies reference valid steps
  for step in steps {
    for dep in &step.depends_on {
      if !ids.contains(dep.as_str()) {
        return Err(format!(
          "step '{}' depends on '{}' which does not exist",
          step.id, dep
        ));
      }
    }
  }

  // Topological sort to detect cycles (Kahn's algorithm)
  let mut in_degree: HashMap<&str, usize> = HashMap::new();
  let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

  for step in steps {
    in_degree.entry(step.id.as_str()).or_insert(0);
    adj.entry(step.id.as_str()).or_default();
    for dep in &step.depends_on {
      adj.entry(dep.as_str()).or_default().push(step.id.as_str());
      *in_degree.entry(step.id.as_str()).or_insert(0) += 1;
    }
  }

  let mut queue: VecDeque<&str> = in_degree
    .iter()
    .filter(|(_, &deg)| deg == 0)
    .map(|(&id, _)| id)
    .collect();

  let mut visited = 0;
  while let Some(node) = queue.pop_front() {
    visited += 1;
    if let Some(neighbors) = adj.get(node) {
      for &next in neighbors {
        let deg = in_degree.get_mut(next).unwrap();
        *deg -= 1;
        if *deg == 0 {
          queue.push_back(next);
        }
      }
    }
  }

  if visited != steps.len() {
    Err("DAG contains a cycle".into())
  } else {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use zeptovm::error::Reason;

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

  fn stop_message() -> Message {
    Message::json(serde_json::json!({"command": "stop"}))
  }

  #[tokio::test]
  async fn test_sequential_runs_in_order() {
    let steps = vec![
      SwarmStep {
        id: "step-1".into(),
        config: test_config("agent-1"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "step-2".into(),
        config: test_config("agent-2"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "step-3".into(),
        config: test_config("agent-3"),
        depends_on: vec![],
      },
    ];

    let results = run_sequential(steps, stop_message()).await;

    assert_eq!(results.len(), 3);
    // Sequential preserves order
    assert_eq!(results[0].step_id, "step-1");
    assert_eq!(results[1].step_id, "step-2");
    assert_eq!(results[2].step_id, "step-3");

    // All should exit with a custom reason (stop command)
    for result in &results {
      assert!(
        matches!(&result.exit.reason, Reason::Custom(_)),
        "expected Custom stop reason for step {}",
        result.step_id
      );
    }
  }

  #[tokio::test]
  async fn test_parallel_runs_all() {
    let steps = vec![
      SwarmStep {
        id: "p-1".into(),
        config: test_config("agent-1"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "p-2".into(),
        config: test_config("agent-2"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "p-3".into(),
        config: test_config("agent-3"),
        depends_on: vec![],
      },
    ];

    let results = run_parallel(steps, stop_message()).await;

    assert_eq!(results.len(), 3);

    // All step IDs should be present (order not guaranteed)
    let ids: HashSet<&str> =
      results.iter().map(|r| r.step_id.as_str()).collect();
    assert!(ids.contains("p-1"));
    assert!(ids.contains("p-2"));
    assert!(ids.contains("p-3"));

    // All exited cleanly
    for result in &results {
      assert!(matches!(&result.exit.reason, Reason::Custom(_)));
    }
  }

  #[tokio::test]
  async fn test_dag_respects_dependencies() {
    // Chain: A -> B -> C
    let steps = vec![
      SwarmStep {
        id: "A".into(),
        config: test_config("agent-a"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "B".into(),
        config: test_config("agent-b"),
        depends_on: vec!["A".into()],
      },
      SwarmStep {
        id: "C".into(),
        config: test_config("agent-c"),
        depends_on: vec!["B".into()],
      },
    ];

    let results = run_dag(steps, stop_message()).await;

    assert_eq!(results.len(), 3);

    // A must complete before B, B before C
    let pos_a = results.iter().position(|r| r.step_id == "A").unwrap();
    let pos_b = results.iter().position(|r| r.step_id == "B").unwrap();
    let pos_c = results.iter().position(|r| r.step_id == "C").unwrap();
    assert!(pos_a < pos_b, "A should complete before B");
    assert!(pos_b < pos_c, "B should complete before C");
  }

  #[tokio::test]
  async fn test_dag_parallel_roots() {
    // A and B have no deps, C depends on both
    let steps = vec![
      SwarmStep {
        id: "A".into(),
        config: test_config("agent-a"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "B".into(),
        config: test_config("agent-b"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "C".into(),
        config: test_config("agent-c"),
        depends_on: vec!["A".into(), "B".into()],
      },
    ];

    let results = run_dag(steps, stop_message()).await;

    assert_eq!(results.len(), 3);

    // C must come after both A and B
    let pos_a = results.iter().position(|r| r.step_id == "A").unwrap();
    let pos_b = results.iter().position(|r| r.step_id == "B").unwrap();
    let pos_c = results.iter().position(|r| r.step_id == "C").unwrap();
    assert!(pos_c > pos_a, "C should complete after A");
    assert!(pos_c > pos_b, "C should complete after B");
  }

  #[test]
  fn test_validate_dag_valid() {
    let steps = vec![
      SwarmStep {
        id: "A".into(),
        config: test_config("a"),
        depends_on: vec![],
      },
      SwarmStep {
        id: "B".into(),
        config: test_config("b"),
        depends_on: vec!["A".into()],
      },
      SwarmStep {
        id: "C".into(),
        config: test_config("c"),
        depends_on: vec!["A".into()],
      },
      SwarmStep {
        id: "D".into(),
        config: test_config("d"),
        depends_on: vec!["B".into(), "C".into()],
      },
    ];

    assert!(validate_dag(&steps).is_ok());
  }

  #[test]
  fn test_validate_dag_cycle() {
    // A -> B -> A (cycle)
    let steps = vec![
      SwarmStep {
        id: "A".into(),
        config: test_config("a"),
        depends_on: vec!["B".into()],
      },
      SwarmStep {
        id: "B".into(),
        config: test_config("b"),
        depends_on: vec!["A".into()],
      },
    ];

    let result = validate_dag(&steps);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("cycle"));
  }

  #[test]
  fn test_validate_dag_missing_dep() {
    let steps = vec![SwarmStep {
      id: "A".into(),
      config: test_config("a"),
      depends_on: vec!["nonexistent".into()],
    }];

    let result = validate_dag(&steps);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("does not exist"));
  }
}
