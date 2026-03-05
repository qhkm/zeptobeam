#[cfg(test)]
mod tests {
  use crate::agent_rt::{
    process::*, scheduler::AgentScheduler, supervision::*, types::*,
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
}
