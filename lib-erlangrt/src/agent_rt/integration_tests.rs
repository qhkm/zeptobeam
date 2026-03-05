#[cfg(test)]
mod tests {
  use crate::agent_rt::{
    config::load_config_from_str,
    process::*, scheduler::AgentScheduler, startup::spawn_configured_agents,
    supervision::*, types::*,
  };
  use std::{any::Any, sync::Arc};

  // A behavior that counts messages and stops after N
  struct CounterState {
    count: usize,
    stop_at: usize,
  }

  impl AgentState for CounterState {
    fn as_any(&self) -> &dyn Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
      self
    }
  }

  struct CounterBehavior {
    stop_at: usize,
  }

  impl AgentBehavior for CounterBehavior {
    fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(CounterState {
        count: 0,
        stop_at: self.stop_at,
      }))
    }

    fn handle_message(&self, _msg: Message, state: &mut dyn AgentState) -> Action {
      let s = state.as_any_mut().downcast_mut::<CounterState>().unwrap();
      s.count += 1;
      if s.count >= s.stop_at {
        Action::Stop(Reason::Normal)
      } else {
        Action::Continue
      }
    }

    fn handle_exit(
      &self,
      _from: AgentPid,
      _reason: Reason,
      _state: &mut dyn AgentState,
    ) -> Action {
      Action::Continue
    }

    fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
  }

  #[test]
  fn test_full_supervisor_lifecycle() {
    let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 3 });

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForOne,
      max_restarts: 10,
      max_seconds: 60,
      backoff: BackoffStrategy::None,
      children: vec![ChildSpec {
        id: "counter".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      }],
    };

    let mut sched = AgentScheduler::new();
    let mut sup = Supervisor::start(&mut sched, spec).unwrap();

    let child_pid = sup.children[0].pid;

    // Send 3 messages — child will count to 3 and stop
    for _ in 0..3 {
      sched
        .send(child_pid, Message::Json(serde_json::json!("tick")))
        .unwrap();
    }
    sched.enqueue(child_pid);

    // Run scheduler ticks until child processes all
    // messages and returns Action::Stop
    for _ in 0..10 {
      sched.tick();
    }

    // Child should have been terminated by Action::Stop
    assert!(
      sched.registry.lookup(&child_pid).is_none(),
      "Child should be terminated after stop_at=3"
    );

    // Supervisor should restart permanent child
    sup.handle_child_exit(&mut sched, child_pid, Reason::Normal);

    assert_eq!(sup.children.len(), 1);
    assert_ne!(
      sup.children[0].pid, child_pid,
      "Permanent child should get a new PID"
    );

    // New child should exist in registry
    let new_pid = sup.children[0].pid;
    assert!(
      sched.registry.lookup(&new_pid).is_some(),
      "Restarted child should exist in registry"
    );
  }

  #[test]
  fn test_multiple_processes_fair_scheduling() {
    let mut sched = AgentScheduler::new();
    let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });

    // Spawn 3 processes, each with 100 messages
    let mut pids = Vec::new();
    for _ in 0..3 {
      let pid = sched
        .registry
        .spawn(behavior.clone(), serde_json::Value::Null)
        .unwrap();
      for i in 0..100 {
        sched
          .send(pid, Message::Json(serde_json::json!(i)))
          .unwrap();
      }
      sched.enqueue(pid);
      pids.push(pid);
    }

    // Run 10 ticks — each process should get some time
    for _ in 0..10 {
      sched.tick();
    }

    // All processes should have made progress.
    // With 200 reductions and dispatch cost of 1 per
    // message, each tick processes up to 200 messages.
    // After 10 ticks with round-robin on 3 normal
    // priority processes, all should be fully drained.
    for pid in &pids {
      if let Some(proc) = sched.registry.lookup(pid) {
        assert!(
          !proc.has_messages(),
          "Process {:?} should have drained mailbox",
          pid
        );
      }
    }
  }

  #[test]
  fn test_supervisor_restart_cycle() {
    let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1 });

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForOne,
      max_restarts: 3,
      max_seconds: 60,
      backoff: BackoffStrategy::None,
      children: vec![ChildSpec {
        id: "fast_crash".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      }],
    };

    let mut sched = AgentScheduler::new();
    let mut sup = Supervisor::start(&mut sched, spec).unwrap();

    // Crash the child 4 times — exceeds max_restarts=3
    for round in 0..4 {
      if sup.is_shutdown() {
        break;
      }
      let pid = sup.children[0].pid;

      // Send message, tick to trigger Action::Stop
      sched.send(pid, Message::Text("crash".into())).unwrap();
      sched.enqueue(pid);
      sched.tick();

      // Notify supervisor of the exit
      sup.handle_child_exit(&mut sched, pid, Reason::Custom(format!("crash_{}", round)));
    }

    assert!(
      sup.is_shutdown(),
      "Supervisor should shut down after exceeding \
       max_restarts"
    );
  }

  #[tokio::test]
  async fn test_orchestrator_agent_chat_roundtrip() {
    use crate::agent_rt::{
      bridge::create_bridge, bridge_metrics::BridgeMetrics,
      orchestration::OrchestratorBehavior, tool_factory::ToolFactory,
    };
    use std::collections::HashMap;
    use zeptoclaw::{
      error::Result as ZeptoResult,
      providers::{ChatOptions, LLMProvider, LLMResponse, ToolDefinition},
      session::Message as ZeptoMessage,
      tools::Tool,
    };

    // Mock provider that returns predictable responses
    struct RoundtripMockProvider;

    #[async_trait::async_trait]
    impl LLMProvider for RoundtripMockProvider {
      async fn chat(
        &self,
        _messages: Vec<ZeptoMessage>,
        _tools: Vec<ToolDefinition>,
        _model: Option<&str>,
        _options: ChatOptions,
      ) -> ZeptoResult<LLMResponse> {
        Ok(LLMResponse::text("roundtrip complete"))
      }
      fn default_model(&self) -> &str {
        "mock"
      }
      fn name(&self) -> &str {
        "mock"
      }
    }

    struct NoopFactory;
    impl ToolFactory for NoopFactory {
      fn build_tools(&self, _whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>> {
        vec![]
      }
    }

    // Create bridge with mock provider
    let (handle, mut worker) = create_bridge();
    let mut providers = HashMap::new();
    providers.insert(
      "openai".to_string(),
      Arc::new(RoundtripMockProvider) as Arc<dyn LLMProvider + Send + Sync>,
    );
    // Also add "default" as an alias so the worker's AgentChat
    // provider lookup succeeds regardless of key used.
    providers.insert(
      "default".to_string(),
      Arc::new(RoundtripMockProvider) as Arc<dyn LLMProvider + Send + Sync>,
    );
    worker.set_agent_provider_registry(Arc::new(providers));
    worker.set_agent_tool_factory(Arc::new(NoopFactory) as Arc<dyn ToolFactory>);
    worker.set_agent_metrics(Arc::new(BridgeMetrics::new()));

    // Spawn bridge worker in background
    let bridge_handle = tokio::spawn(async move {
      worker.run().await;
    });

    // Create scheduler with bridge
    let mut sched = AgentScheduler::new();
    sched.set_bridge(handle);

    // Spawn orchestrator
    let orch_behavior = OrchestratorBehavior {
      max_concurrency: 1,
      checkpoint_store: None,
      ..Default::default()
    };
    let orch_pid = sched
      .registry
      .spawn(
        Arc::new(orch_behavior),
        serde_json::json!({
          "self_pid": null,
        }),
      )
      .unwrap();
    sched.enqueue(orch_pid);

    // Send goal to orchestrator, including self_pid so
    // the orchestrator knows its own pid for spawning
    // workers that can send results back.
    sched
      .send(
        orch_pid,
        Message::Json(serde_json::json!({
          "goal": "test roundtrip",
          "requester_pid": null,
          "self_pid": orch_pid.raw(),
        })),
      )
      .unwrap();

    // Tick through the flow:
    // 1. Orchestrator receives goal, emits Custom(decompose_goal)
    // 2. Bridge returns placeholder response
    // 3. Orchestrator receives IoResponse, extract_tasks fallback
    //    creates a single task, spawns a WorkerBehavior
    // 4. Worker receives run_task, emits AgentChat
    // 5. Bridge processes AgentChat with mock provider
    // 6. Worker receives IoResponse, sends worker_result to orch
    // 7. Orchestrator receives result, sends shutdown_worker
    // 8. Worker receives shutdown, stops (Normal)
    // 9. Orchestrator receives DOWN, detects completion, stops
    let mut completed = false;
    for _ in 0..200 {
      sched.tick();
      // Small delay to let bridge process async ops
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;

      // Check if orchestrator completed
      if sched.registry.lookup(&orch_pid).is_none() {
        completed = true;
        break;
      }
    }

    assert!(
      completed,
      "orchestrator should have completed the roundtrip within 200 ticks"
    );

    // Clean up
    drop(sched);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), bridge_handle).await;
  }

  #[test]
  fn test_linked_processes_cascade_through_supervisor() {
    let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForAll,
      max_restarts: 5,
      max_seconds: 60,
      backoff: BackoffStrategy::None,
      children: vec![
        ChildSpec {
          id: "worker_a".to_string(),
          behavior: behavior.clone(),
          args: serde_json::Value::Null,
          restart: ChildRestart::Permanent,
          priority: Priority::Normal,
        },
        ChildSpec {
          id: "worker_b".to_string(),
          behavior: behavior.clone(),
          args: serde_json::Value::Null,
          restart: ChildRestart::Permanent,
          priority: Priority::Normal,
        },
      ],
    };

    let mut sched = AgentScheduler::new();
    let mut sup = Supervisor::start(&mut sched, spec).unwrap();

    let pid_a = sup.children[0].pid;
    let pid_b = sup.children[1].pid;

    // Kill worker_a — OneForAll should restart both
    sup.handle_child_exit(&mut sched, pid_a, Reason::Custom("crash".into()));

    assert_eq!(sup.children.len(), 2);
    let new_a = sup.children[0].pid;
    let new_b = sup.children[1].pid;
    assert_ne!(new_a, pid_a);
    assert_ne!(new_b, pid_b);

    // New processes should be runnable
    assert_eq!(
      sched.registry.lookup(&new_a).unwrap().status,
      ProcessStatus::Runnable
    );
    assert_eq!(
      sched.registry.lookup(&new_b).unwrap().status,
      ProcessStatus::Runnable
    );
  }

  // ===========================================================================
  // Phase 6: Advanced Orchestration Integration Tests
  // ===========================================================================

  use crate::agent_rt::{
    task_graph::{TaskGraph, TaskStatus},
    retry_policy::{RetryStrategy, RetryTracker},
    resource_budget::{ResourceBudget, TokenPricing, ModelPrice},
    result_aggregator::{ResultAggregator, ConcatAggregator, VoteAggregator, MergeAggregator},
    approval_gate::{ApprovalRegistry, ApprovalDecision, task_requires_approval, approval_message},
  };

  #[test]
  fn test_dag_orchestration_end_to_end() {
    // Test that TaskGraph correctly handles dependencies
    let mut graph = TaskGraph::new();
    
    // Create a diamond dependency pattern
    graph.add_tasks(vec![
      serde_json::json!({"task_id": "start", "prompt": "start task"}),
      serde_json::json!({"task_id": "left", "prompt": "left branch", "depends_on": ["start"]}),
      serde_json::json!({"task_id": "right", "prompt": "right branch", "depends_on": ["start"]}),
      serde_json::json!({"task_id": "merge", "prompt": "merge results", "depends_on": ["left", "right"]}),
    ]).unwrap();

    // Initially only "start" is ready
    let ready = graph.ready_tasks();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0]["task_id"], "start");

    // Complete "start", now "left" and "right" should be ready
    graph.mark_running("start");
    graph.mark_completed("start");
    
    let ready = graph.ready_tasks();
    assert_eq!(ready.len(), 2);
    let ids: Vec<_> = ready.iter().map(|t| t["task_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"left"));
    assert!(ids.contains(&"right"));

    // Complete "left", "right" still ready, "merge" still waits for "right"
    graph.mark_running("left");
    graph.mark_completed("left");
    assert_eq!(graph.ready_tasks().len(), 1); // "right" still ready

    // Complete "right", now "merge" should be ready
    graph.mark_running("right");
    graph.mark_completed("right");
    
    let ready = graph.ready_tasks();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0]["task_id"], "merge");

    // Complete "merge", graph should be complete
    graph.mark_running("merge");
    graph.mark_completed("merge");
    assert!(graph.is_complete());
  }

  #[test]
  fn test_retry_policy_end_to_end() {
    let mut tracker = RetryTracker::new(RetryStrategy::Immediate { max_attempts: 2 });
    
    // First two retries should succeed
    assert_eq!(tracker.should_retry("task-1"), Some(0));
    assert_eq!(tracker.attempt_count("task-1"), 1);
    
    assert_eq!(tracker.should_retry("task-1"), Some(0));
    assert_eq!(tracker.attempt_count("task-1"), 2);
    
    // Third retry should fail (exhausted) - count still increments on exhaustion check
    assert_eq!(tracker.should_retry("task-1"), None);
    assert_eq!(tracker.attempt_count("task-1"), 3);
  }

  #[test]
  fn test_backoff_retry_policy() {
    let mut tracker = RetryTracker::new(RetryStrategy::Backoff {
      max_attempts: 4,
      base_ms: 100,
      max_ms: 500,
    });
    
    // Exponential backoff: 100, 200, 400, capped at 500
    assert_eq!(tracker.should_retry("task-1"), Some(100));
    assert_eq!(tracker.should_retry("task-1"), Some(200));
    assert_eq!(tracker.should_retry("task-1"), Some(400));
    assert_eq!(tracker.should_retry("task-1"), Some(500)); // capped
    assert_eq!(tracker.should_retry("task-1"), None); // exhausted
  }

  #[test]
  fn test_budget_stops_orchestration() {
    // Create budget with max 1000 tokens
    let mut budget = ResourceBudget::new(Some(1000), None, TokenPricing::default());
    
    // First usage within budget
    let exhausted = budget.record_usage(&crate::agent_rt::resource_budget::UsageReport {
      input_tokens: 300,
      output_tokens: 200,
      model: None,
    });
    assert!(!exhausted);
    assert!(!budget.is_exhausted());
    assert_eq!(budget.tokens_used(), 500);
    
    // Second usage exceeds budget
    let exhausted = budget.record_usage(&crate::agent_rt::resource_budget::UsageReport {
      input_tokens: 400,
      output_tokens: 300,
      model: None,
    });
    assert!(exhausted);
    assert!(budget.is_exhausted());
    assert_eq!(budget.tokens_used(), 1200);
  }

  #[test]
  fn test_budget_cost_tracking_with_model_pricing() {
    let mut pricing = TokenPricing::default();
    pricing.model_prices.insert("gpt-4".into(), ModelPrice {
      input_per_1k: 0.03,
      output_per_1k: 0.06,
    });
    
    // Budget of $1.00
    let mut budget = ResourceBudget::new(None, Some(1.0), pricing);
    
    // Use 10k input ($0.30) + 5k output ($0.30) = $0.60
    budget.record_usage(&crate::agent_rt::resource_budget::UsageReport {
      input_tokens: 10_000,
      output_tokens: 5_000,
      model: Some("gpt-4".into()),
    });
    
    assert!(!budget.is_exhausted());
    let cost = budget.cost_usd();
    assert!(cost >= 0.59 && cost <= 0.61, "Cost should be ~$0.60, got ${}", cost);
    
    // Use another 10k input ($0.30) + 5k output ($0.30) = $1.20 total > $1.00
    let exhausted = budget.record_usage(&crate::agent_rt::resource_budget::UsageReport {
      input_tokens: 10_000,
      output_tokens: 5_000,
      model: Some("gpt-4".into()),
    });
    
    assert!(exhausted);
    assert!(budget.is_exhausted());
  }

  #[test]
  fn test_result_aggregator_concat() {
    let agg = ConcatAggregator;
    let results = vec![
      serde_json::json!({"result": "a"}),
      serde_json::json!({"result": "b"}),
      serde_json::json!({"result": "c"}),
    ];
    
    let output = agg.aggregate(&results);
    assert_eq!(output, serde_json::json!([{"result": "a"}, {"result": "b"}, {"result": "c"}]));
  }

  #[test]
  fn test_result_aggregator_vote() {
    let agg = VoteAggregator { field: "decision".into() };
    let results = vec![
      serde_json::json!({"decision": "approve"}),
      serde_json::json!({"decision": "reject"}),
      serde_json::json!({"decision": "approve"}),
      serde_json::json!({"decision": "approve"}),
    ];
    
    let output = agg.aggregate(&results);
    assert_eq!(output["winner"], "approve");
    assert_eq!(output["votes"]["approve"], 3);
    assert_eq!(output["votes"]["reject"], 1);
  }

  #[test]
  fn test_result_aggregator_merge() {
    let agg = MergeAggregator;
    let results = vec![
      serde_json::json!({"name": "alice", "tags": ["rust"], "score": 95}),
      serde_json::json!({"name": "bob", "tags": ["python"], "score": 87}),
    ];
    
    let output = agg.aggregate(&results);
    // Last value wins for non-array fields
    assert_eq!(output["name"], "bob");
    assert_eq!(output["score"], 87);
    // Arrays are concatenated
    assert_eq!(output["tags"], serde_json::json!(["rust", "python"]));
  }

  #[test]
  fn test_approval_gate_registry() {
    let mut registry = ApprovalRegistry::new();
    
    // Request approval for a task
    registry.request_approval(
      "orch-1",
      "task-deploy",
      "Approve deployment to production?",
      serde_json::json!({"type": "deploy", "target": "prod"}),
    );
    
    assert!(registry.has_pending("orch-1", "task-deploy"));
    assert_eq!(registry.pending_requests().len(), 1);
    
    // Submit approval
    let ok = registry.submit_decision("orch-1", "task-deploy", ApprovalDecision::Approved);
    assert!(ok);
    assert!(!registry.has_pending("orch-1", "task-deploy"));
    
    // Take the decision
    let decision = registry.take_decision("orch-1", "task-deploy");
    assert_eq!(decision, Some(ApprovalDecision::Approved));
    
    // Decision is consumed
    assert!(registry.take_decision("orch-1", "task-deploy").is_none());
  }

  #[test]
  fn test_approval_gate_rejection() {
    let mut registry = ApprovalRegistry::new();
    
    registry.request_approval("orch-2", "task-delete", "Delete database?", serde_json::json!({}));
    
    // Reject with reason
    registry.submit_decision(
      "orch-2",
      "task-delete",
      ApprovalDecision::Rejected { reason: "too risky".into() },
    );
    
    let decision = registry.take_decision("orch-2", "task-delete").unwrap();
    assert_eq!(decision, ApprovalDecision::Rejected { reason: "too risky".into() });
  }

  #[test]
  fn test_task_requires_approval_helper() {
    assert!(task_requires_approval(&serde_json::json!({"requires_approval": true})));
    assert!(!task_requires_approval(&serde_json::json!({"requires_approval": false})));
    assert!(!task_requires_approval(&serde_json::json!({"task_id": "t1"})));
  }

  #[test]
  fn test_approval_message_helper() {
    assert_eq!(
      approval_message(&serde_json::json!({"approval_message": "Ship to prod?"})),
      "Ship to prod?"
    );
    assert_eq!(approval_message(&serde_json::json!({})), "Approval required");
  }

  #[test]
  fn test_cycle_detection_in_task_graph() {
    let mut graph = TaskGraph::new();
    
    // Try to add tasks with a cycle: a -> b -> c -> a
    let result = graph.add_tasks(vec![
      serde_json::json!({"task_id": "a", "depends_on": ["c"]}),
      serde_json::json!({"task_id": "b", "depends_on": ["a"]}),
      serde_json::json!({"task_id": "c", "depends_on": ["b"]}),
    ]);
    
    assert!(result.is_err(), "Should detect cycle");
  }

  #[test]
  fn test_fail_dependents_cascade_in_graph() {
    let mut graph = TaskGraph::new();
    
    graph.add_tasks(vec![
      serde_json::json!({"task_id": "root"}),
      serde_json::json!({"task_id": "child1", "depends_on": ["root"]}),
      serde_json::json!({"task_id": "child2", "depends_on": ["root"]}),
      serde_json::json!({"task_id": "grandchild", "depends_on": ["child1"]}),
    ]).unwrap();
    
    // Fail root
    graph.mark_running("root");
    graph.mark_failed("root");
    graph.fail_dependents("root");
    
    // All dependents should be skipped
    assert_eq!(graph.task_status("child1"), Some(&TaskStatus::Skipped));
    assert_eq!(graph.task_status("child2"), Some(&TaskStatus::Skipped));
    assert_eq!(graph.task_status("grandchild"), Some(&TaskStatus::Skipped));
  }

  #[test]
  fn test_config_driven_agent_spawn_and_message() {
    let toml_str = r#"
[[agents]]
name = "helper"
provider = "openrouter"
model = "anthropic/claude-sonnet-4"
system_prompt = "You are helpful."
tools = ["web_fetch"]
auto_start = true

[[agents]]
name = "disabled"
provider = "openai"
auto_start = false
"#;
    let config = load_config_from_str(toml_str).unwrap();
    let mut scheduler = AgentScheduler::new();

    let spawned = spawn_configured_agents(&mut scheduler, &config.agents).unwrap();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].1, "helper");

    // Verify process exists and is registered
    let pid = scheduler.registry.lookup_name("helper").unwrap();
    assert!(scheduler.registry.lookup(&pid).is_some());

    // Disabled agent should not be spawned
    assert!(scheduler.registry.lookup_name("disabled").is_none());

    // Send a message to the agent
    scheduler.enqueue(pid);
    let send_result = scheduler.send(pid, Message::Text("hello".into()));
    assert!(send_result.is_ok());

    // Tick the scheduler — agent should process message and emit IoRequest
    let did_work = scheduler.tick();
    assert!(did_work);

    // Agent should still be alive
    assert!(scheduler.registry.lookup(&pid).is_some());
  }

  #[test]
  fn test_orchestrator_decomposes_goal_into_task_graph() {
    use crate::agent_rt::decomposition::parse_decomposition_response;

    let goal = serde_json::json!("Build a REST API for todo items");

    // Simulate an LLM response with decomposed tasks
    let llm_response = r#"{"tasks": [
        {"task_id": "design", "goal": "Design the API schema and endpoints"},
        {"task_id": "implement", "goal": "Implement CRUD endpoints", "depends_on": ["design"]},
        {"task_id": "test", "goal": "Write integration tests", "depends_on": ["implement"]}
    ]}"#;

    let result = parse_decomposition_response(llm_response, &goal);
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0]["task_id"], "design");
    assert_eq!(tasks[1]["task_id"], "implement");
    assert_eq!(tasks[2]["task_id"], "test");

    // Verify dependency chain
    let deps = tasks[1]["depends_on"].as_array().unwrap();
    assert_eq!(deps[0], "design");
  }

  #[test]
  fn test_orchestrator_fallback_on_bad_decomposition() {
    use crate::agent_rt::decomposition::parse_decomposition_response;

    let goal = serde_json::json!("Do something complex");

    let bad_response = "Sorry, I can't help with that.";
    let result = parse_decomposition_response(bad_response, &goal);
    let tasks = result["tasks"].as_array().unwrap();

    // Should fall back to single task
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["goal"], "Do something complex");
  }
}
