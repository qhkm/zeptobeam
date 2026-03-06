//! End-to-end integration tests for the v4 (ZeptoBeam Integration) stage.
//!
//! Verifies that the full pipeline — config loading, agent adapter creation,
//! zeptovm process spawning, swarm orchestration, and checkpoint round-trips —
//! works together.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use erlangrt::agent_rt::config::{AgentConfig, AppConfig};
use zeptobeam::agent_adapter::{create_agent_adapter, AgentAdapterConfig};
use zeptobeam::config_loader::{resolve_agents, resolve_agents_from_config};
use zeptobeam::swarm::{run_dag, run_sequential, validate_dag, SwarmStep};
use zeptovm::durability::{decode_checkpoint, CheckpointStore, SqliteCheckpointStore};
use zeptovm::error::{Message, Reason};
use zeptovm::process::spawn_process;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_adapter_config(name: &str) -> AgentAdapterConfig {
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

fn make_agent(name: &str, provider: &str, auto_start: bool) -> AgentConfig {
  AgentConfig {
    name: name.into(),
    provider: provider.into(),
    model: Some("test-model".into()),
    system_prompt: Some("You are a test agent.".into()),
    tools: vec!["tool_a".into()],
    auto_start,
    max_iterations: Some(10),
    timeout_ms: Some(30000),
  }
}

fn stop_message() -> Message {
  Message::json(json!({"command": "stop"}))
}

fn make_step(id: &str, depends_on: Vec<&str>) -> SwarmStep {
  SwarmStep {
    id: id.into(),
    config: test_adapter_config(&format!("agent-{id}")),
    depends_on: depends_on.into_iter().map(String::from).collect(),
  }
}

// ---------------------------------------------------------------------------
// 1. Agent adapter lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_agent_adapter_lifecycle() {
  let config = AgentAdapterConfig {
    name: "lifecycle-agent".into(),
    provider: "test".into(),
    model: Some("test-model".into()),
    system_prompt: Some("You are a lifecycle test.".into()),
    tools: vec!["tool1".into()],
    max_iterations: Some(5),
    timeout_ms: Some(10000),
  };
  let adapter = create_agent_adapter(config);
  let (_pid, handle, join) = spawn_process(adapter, 64, None, None, None);

  // Send a few text messages
  handle.send_user(Message::text("hello")).await.unwrap();
  handle.send_user(Message::text("world")).await.unwrap();

  // Send stop command
  handle
    .send_user(Message::json(json!({"command": "stop"})))
    .await
    .unwrap();

  let exit = join.await.unwrap();
  assert!(
    matches!(&exit.reason, Reason::Custom(s) if s.contains("requested")),
    "expected Custom stop reason, got: {:?}",
    exit.reason
  );
}

// ---------------------------------------------------------------------------
// 2. Config resolve and spawn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_config_resolve_and_spawn() {
  let mut config = AppConfig::default();
  config.agents = vec![
    make_agent("agent1", "test", true),
    make_agent("agent2", "test", false),
  ];

  let resolved = resolve_agents_from_config(&config).unwrap();
  // Only auto_start agents are included
  assert_eq!(resolved.agents.len(), 1);
  assert_eq!(resolved.agents[0].name, "agent1");

  // Spawn the resolved agent as a zeptovm process
  let adapter = create_agent_adapter(resolved.agents[0].clone());
  let (_pid, handle, join) = spawn_process(adapter, 64, None, None, None);

  handle.send_user(Message::text("hi from config")).await.unwrap();
  handle.send_user(stop_message()).await.unwrap();

  let exit = join.await.unwrap();
  assert!(
    matches!(&exit.reason, Reason::Custom(_)),
    "expected Custom reason, got: {:?}",
    exit.reason
  );
}

// ---------------------------------------------------------------------------
// 3. Swarm sequential with two agents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_swarm_sequential_two_agents() {
  let steps = vec![make_step("step1", vec![]), make_step("step2", vec![])];

  let results = run_sequential(steps, stop_message()).await;
  assert_eq!(results.len(), 2);
  assert_eq!(results[0].step_id, "step1");
  assert_eq!(results[1].step_id, "step2");

  for r in &results {
    assert!(
      matches!(&r.exit.reason, Reason::Custom(_)),
      "step {} should exit with Custom, got: {:?}",
      r.step_id,
      r.exit.reason
    );
  }
}

// ---------------------------------------------------------------------------
// 4. Swarm DAG with dependencies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_swarm_dag_with_dependencies() {
  let steps = vec![
    make_step("A", vec![]),
    make_step("B", vec![]),
    make_step("C", vec!["A", "B"]),
  ];

  // Validate structure first
  validate_dag(&steps).unwrap();

  // Execute
  let results = run_dag(steps, stop_message()).await;
  assert_eq!(results.len(), 3);

  // C must come after both A and B in the results
  let pos_a = results.iter().position(|r| r.step_id == "A").unwrap();
  let pos_b = results.iter().position(|r| r.step_id == "B").unwrap();
  let pos_c = results.iter().position(|r| r.step_id == "C").unwrap();
  assert!(pos_c > pos_a, "C should complete after A");
  assert!(pos_c > pos_b, "C should complete after B");

  for r in &results {
    assert!(
      matches!(&r.exit.reason, Reason::Custom(_)),
      "step {} should exit with Custom, got: {:?}",
      r.step_id,
      r.exit.reason
    );
  }
}

// ---------------------------------------------------------------------------
// 5. Checkpoint round-trip through adapter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_checkpoint_roundtrip_through_adapter() {
  let store = Arc::new(SqliteCheckpointStore::new(":memory:").unwrap());
  let config = test_adapter_config("checkpoint-agent");
  let adapter = create_agent_adapter(config);
  let (pid, handle, join) =
    spawn_process(adapter, 64, None, Some(store.clone()), None);

  // Send 10 text messages to trigger should_checkpoint (every 10 messages)
  for i in 0..10 {
    handle
      .send_user(Message::text(format!("msg-{i}")))
      .await
      .unwrap();
  }

  // Give the process time to handle all messages and write the checkpoint
  tokio::time::sleep(Duration::from_millis(200)).await;

  // Verify checkpoint was saved
  let saved = store.load(pid).await.unwrap();
  assert!(saved.is_some(), "checkpoint should have been saved after 10 messages");

  // Decode and verify the checkpoint payload is valid conversation history
  let raw = saved.unwrap();
  let (_seq, state_bytes) = decode_checkpoint(&raw).unwrap();
  let history: Vec<String> = serde_json::from_slice(state_bytes).unwrap();
  assert_eq!(history.len(), 10);
  assert_eq!(history[0], "msg-0");
  assert_eq!(history[9], "msg-9");

  // Stop the process
  handle.send_user(stop_message()).await.unwrap();
  let exit = join.await.unwrap();
  assert!(
    matches!(&exit.reason, Reason::Custom(_)),
    "expected Custom stop reason, got: {:?}",
    exit.reason
  );
}

// ---------------------------------------------------------------------------
// 6. Config validation rejects invalid
// ---------------------------------------------------------------------------

#[test]
fn test_config_validation_rejects_invalid() {
  // Empty name
  let agents = vec![AgentConfig {
    name: "".into(),
    provider: "test".into(),
    model: None,
    system_prompt: None,
    tools: vec![],
    auto_start: true,
    max_iterations: None,
    timeout_ms: None,
  }];
  let result = resolve_agents(&agents);
  assert!(result.is_err());
  let errors = result.unwrap_err();
  assert!(
    errors.iter().any(|e| e.message.contains("name")),
    "should report name validation error"
  );

  // Empty provider
  let agents = vec![AgentConfig {
    name: "valid-name".into(),
    provider: "  ".into(),
    model: None,
    system_prompt: None,
    tools: vec![],
    auto_start: false,
    max_iterations: None,
    timeout_ms: None,
  }];
  let result = resolve_agents(&agents);
  assert!(result.is_err());
  let errors = result.unwrap_err();
  assert!(
    errors.iter().any(|e| e.message.contains("provider")),
    "should report provider validation error"
  );

  // Zero max_iterations
  let agents = vec![AgentConfig {
    name: "valid-name".into(),
    provider: "test".into(),
    model: None,
    system_prompt: None,
    tools: vec![],
    auto_start: true,
    max_iterations: Some(0),
    timeout_ms: None,
  }];
  let result = resolve_agents(&agents);
  assert!(result.is_err());
  let errors = result.unwrap_err();
  assert!(
    errors.iter().any(|e| e.message.contains("max_iterations")),
    "should report max_iterations validation error"
  );
}
