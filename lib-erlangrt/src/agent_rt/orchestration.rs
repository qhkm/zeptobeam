use std::{
  collections::{HashMap, VecDeque},
  sync::Arc,
};

use crate::agent_rt::{checkpoint::CheckpointStore, types::*};

const DECOMPOSE_KIND: &str = "decompose_goal";
const DEFAULT_LLM_PROVIDER: &str = "openai";
const DEFAULT_CHECKPOINT_KEY: &str = "orchestrator-default";

/// Orchestrator process behavior.
///
/// This implementation drives task decomposition and worker
/// spawning using existing runtime primitives. It intentionally
/// emits one scheduler action per handled message.
pub struct OrchestratorBehavior {
  pub max_concurrency: usize,
  pub checkpoint_store: Option<Arc<dyn CheckpointStore>>,
}

impl Default for OrchestratorBehavior {
  fn default() -> Self {
    Self {
      max_concurrency: 1,
      checkpoint_store: None,
    }
  }
}

/// Mutable state for the orchestrator process.
pub struct OrchestratorState {
  pending_tasks: VecDeque<serde_json::Value>,
  pending_spawn_tasks: VecDeque<serde_json::Value>,
  active_workers: HashMap<u64, String>,
  worker_monitors: HashMap<u64, u64>,
  inflight_tasks: HashMap<String, serde_json::Value>,
  results: Vec<serde_json::Value>,
  requester: Option<AgentPid>,
  goal: serde_json::Value,
  self_pid: Option<AgentPid>,
  checkpoint_key: String,
  resumed_from_checkpoint: bool,
  awaiting_decomposition: bool,
  decomposition_done: bool,
}

impl OrchestratorState {
  fn can_spawn_more(&self, max_concurrency: usize) -> bool {
    if !self.decomposition_done || self.awaiting_decomposition {
      return false;
    }
    if self.pending_tasks.is_empty() {
      return false;
    }
    let active = self.active_workers.len();
    let inflight_spawns = self.pending_spawn_tasks.len();
    let cap = max_concurrency.max(1);
    active + inflight_spawns < cap
  }

  fn is_complete(&self) -> bool {
    self.decomposition_done
      && !self.awaiting_decomposition
      && self.pending_tasks.is_empty()
      && self.pending_spawn_tasks.is_empty()
      && self.active_workers.is_empty()
  }
}

impl AgentState for OrchestratorState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for OrchestratorBehavior {
  fn init(&self, args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    let requester = parse_pid(args.get("requester_pid"));
    let self_pid = parse_pid(args.get("self_pid"));
    let checkpoint_key = args
      .get("checkpoint_key")
      .and_then(|v| v.as_str())
      .unwrap_or(DEFAULT_CHECKPOINT_KEY)
      .to_string();

    if let Some((goal, completed_results, pending_tasks)) =
      self.load_checkpoint(&checkpoint_key)
    {
      return Ok(Box::new(OrchestratorState {
        pending_tasks,
        pending_spawn_tasks: VecDeque::new(),
        active_workers: HashMap::new(),
        worker_monitors: HashMap::new(),
        inflight_tasks: HashMap::new(),
        results: completed_results,
        requester,
        goal,
        self_pid,
        checkpoint_key,
        resumed_from_checkpoint: true,
        awaiting_decomposition: false,
        decomposition_done: true,
      }));
    }

    let goal = args.get("goal").cloned().unwrap_or(serde_json::Value::Null);
    Ok(Box::new(OrchestratorState {
      pending_tasks: VecDeque::new(),
      pending_spawn_tasks: VecDeque::new(),
      active_workers: HashMap::new(),
      worker_monitors: HashMap::new(),
      inflight_tasks: HashMap::new(),
      results: Vec::new(),
      requester,
      goal,
      self_pid,
      checkpoint_key,
      resumed_from_checkpoint: false,
      awaiting_decomposition: false,
      decomposition_done: false,
    }))
  }

  fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<OrchestratorState>()
      .expect("orchestrator state type");
    match msg {
      Message::Json(payload) => {
        if let Some(result) = parse_worker_result(&payload) {
          s.results.push(result.as_json);
          if let Some(task_id) = result.task_id {
            s.inflight_tasks.remove(&task_id);
          }
          self.persist_checkpoint(s);
          if let Some(worker_pid) = result.worker_pid {
            Action::Send {
              to: AgentPid::from_raw(worker_pid),
              msg: Message::Json(serde_json::json!({
                "type": "shutdown_worker",
              })),
            }
          } else {
            self.next_or_finalize(s)
          }
        } else {
          let incoming_goal = payload.get("goal").cloned().unwrap_or(payload.clone());
          if s.resumed_from_checkpoint
            && incoming_goal == s.goal
            && payload.get("restart").and_then(|v| v.as_bool()) != Some(true)
          {
            s.requester = parse_pid(payload.get("requester_pid")).or(s.requester);
            s.self_pid = parse_pid(payload.get("self_pid")).or(s.self_pid);
            s.resumed_from_checkpoint = false;
            self.persist_checkpoint(s);
            return self.next_or_finalize(s);
          }
          // Treat JSON payload as a new orchestration goal.
          s.requester = parse_pid(payload.get("requester_pid")).or(s.requester);
          s.self_pid = parse_pid(payload.get("self_pid")).or(s.self_pid);
          s.goal = incoming_goal;
          s.pending_tasks.clear();
          s.pending_spawn_tasks.clear();
          s.active_workers.clear();
          s.worker_monitors.clear();
          s.inflight_tasks.clear();
          s.results.clear();
          s.resumed_from_checkpoint = false;
          s.awaiting_decomposition = true;
          s.decomposition_done = false;
          self.persist_checkpoint(s);
          Action::IoRequest(IoOp::Custom {
            kind: DECOMPOSE_KIND.to_string(),
            payload: serde_json::json!({
              "goal": s.goal.clone(),
            }),
          })
        }
      }
      Message::System(SystemMsg::IoResponse {
        correlation_id: _,
        result,
      }) => {
        s.awaiting_decomposition = false;
        s.decomposition_done = true;
        for task in extract_tasks(&s.goal, &result) {
          s.pending_tasks.push_back(task);
        }
        self.persist_checkpoint(s);
        self.next_or_finalize(s)
      }
      Message::System(SystemMsg::SpawnResult {
        child_pid,
        monitor_ref,
      }) => {
        let task = match s.pending_spawn_tasks.pop_front() {
          Some(t) => t,
          None => return self.next_or_finalize(s),
        };
        let task_id = extract_task_id(&task, &format!("task-{}", child_pid));
        s.active_workers.insert(child_pid, task_id.clone());
        s.inflight_tasks.insert(task_id.clone(), task.clone());
        if let Some(mon_ref) = monitor_ref {
          s.worker_monitors.insert(child_pid, mon_ref);
        }
        self.persist_checkpoint(s);
        Action::Send {
          to: AgentPid::from_raw(child_pid),
          msg: Message::Json(serde_json::json!({
            "type": "run_task",
            "worker_pid": child_pid,
            "monitor_ref": monitor_ref,
            "task_id": task_id,
            "task": task,
          })),
        }
      }
      Message::System(SystemMsg::Down {
        monitor_ref: _,
        pid,
        reason,
      }) => {
        let removed = s.active_workers.remove(&pid);
        s.worker_monitors.remove(&pid);
        if let Some(task_id) = removed {
          s.inflight_tasks.remove(&task_id);
          if !matches!(reason, Reason::Normal) {
            s.results.push(serde_json::json!({
              "type": "worker_error",
              "worker_pid": pid,
              "task_id": task_id,
              "reason": reason_to_value(reason),
            }));
          }
        }
        self.persist_checkpoint(s);
        self.next_or_finalize(s)
      }
      _ => Action::Continue,
    }
  }

  fn handle_exit(
    &self,
    from: AgentPid,
    reason: Reason,
    state: &mut dyn AgentState,
  ) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<OrchestratorState>()
      .expect("orchestrator state type");
    if let Some(task_id) = s.active_workers.remove(&from.raw()) {
      s.inflight_tasks.remove(&task_id);
      if !matches!(reason, Reason::Normal) {
        s.results.push(serde_json::json!({
          "type": "worker_error",
          "worker_pid": from.raw(),
          "task_id": task_id,
          "reason": reason_to_value(reason),
        }));
      }
    }
    self.persist_checkpoint(s);
    self.next_or_finalize(s)
  }

  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

impl OrchestratorBehavior {
  fn load_checkpoint(
    &self,
    checkpoint_key: &str,
  ) -> Option<(
    serde_json::Value,
    Vec<serde_json::Value>,
    VecDeque<serde_json::Value>,
  )> {
    let store = self.checkpoint_store.as_ref()?;
    let value = store.load(checkpoint_key).ok().flatten()?;
    let goal = value.get("goal")?.clone();
    let completed_results = value
      .get("completed_results")
      .and_then(|v| v.as_array())
      .cloned()
      .unwrap_or_default();
    let pending_tasks = value
      .get("pending_tasks")
      .and_then(|v| v.as_array())
      .cloned()
      .unwrap_or_default()
      .into_iter()
      .collect::<VecDeque<_>>();
    Some((goal, completed_results, pending_tasks))
  }

  fn persist_checkpoint(&self, state: &OrchestratorState) {
    let store = match self.checkpoint_store.as_ref() {
      Some(s) => s,
      None => return,
    };

    let mut pending_tasks: Vec<serde_json::Value> =
      state.pending_tasks.iter().cloned().collect();
    pending_tasks.extend(state.pending_spawn_tasks.iter().cloned());
    pending_tasks.extend(state.inflight_tasks.values().cloned());

    let checkpoint = serde_json::json!({
      "goal": state.goal,
      "completed_results": state.results,
      "pending_tasks": pending_tasks,
    });
    let _ = store.save(&state.checkpoint_key, &checkpoint);
  }

  fn clear_checkpoint(&self, key: &str) {
    if let Some(store) = self.checkpoint_store.as_ref() {
      let _ = store.delete(key);
    }
  }

  fn next_or_finalize(&self, state: &mut OrchestratorState) -> Action {
    if state.can_spawn_more(self.max_concurrency) {
      let task = match state.pending_tasks.pop_front() {
        Some(t) => t,
        None => return Action::Continue,
      };
      let fallback = format!(
        "task-{}",
        state.active_workers.len() + state.pending_spawn_tasks.len()
      );
      let task_id = extract_task_id(&task, &fallback);
      state.pending_spawn_tasks.push_back(task);
      self.persist_checkpoint(state);
      let parent_pid = state.self_pid.unwrap_or_default().raw();
      return Action::Spawn {
        behavior: Arc::new(WorkerBehavior),
        args: serde_json::json!({
          "parent_pid": parent_pid,
          "task_id": task_id,
        }),
        link: false,
        monitor: true,
      };
    }

    if state.is_complete() {
      self.clear_checkpoint(&state.checkpoint_key);
      let summary = serde_json::json!({
        "type": "orchestration_complete",
        "goal": state.goal,
        "results": state.results,
      });
      if let Some(requester) = state.requester {
        return Action::Send {
          to: requester,
          msg: Message::Json(summary),
        };
      }
      return Action::Stop(Reason::Normal);
    }

    Action::Continue
  }
}

/// Worker process behavior for one subtask.
pub struct WorkerBehavior;

pub struct WorkerState {
  parent: AgentPid,
  task_id: String,
  awaiting_result: bool,
  worker_pid: Option<u64>,
  max_turns: usize,
  turn_count: usize,
}

impl AgentState for WorkerState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for WorkerBehavior {
  fn init(&self, args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    let parent_pid = parse_pid(args.get("parent_pid"))
      .ok_or_else(|| Reason::Custom("worker init requires parent_pid".into()))?;
    let task_id = args
      .get("task_id")
      .and_then(|v| v.as_str())
      .unwrap_or("task")
      .to_string();
    let max_turns = args.get("max_turns").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    Ok(Box::new(WorkerState {
      parent: parent_pid,
      task_id,
      awaiting_result: false,
      worker_pid: None,
      max_turns,
      turn_count: 0,
    }))
  }

  fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<WorkerState>()
      .expect("worker state type");
    match msg {
      Message::Json(payload) => {
        if payload.get("type").and_then(|v| v.as_str()) == Some("shutdown_worker") {
          return Action::Stop(Reason::Normal);
        }
        if payload.get("type").and_then(|v| v.as_str()) == Some("follow_up") {
          if s.awaiting_result {
            return Action::Send {
              to: s.parent,
              msg: Message::Json(serde_json::json!({
                "type": "worker_busy",
              })),
            };
          }
          // Not busy -- emit AgentChat for follow-up
          let prompt = payload
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
          s.awaiting_result = true;
          return Action::IoRequest(IoOp::AgentChat {
            provider: "default".to_string(),
            model: None,
            system_prompt: None,
            prompt,
            tools: None,
            max_iterations: None,
            timeout_ms: None,
          });
        }
        if payload.get("type").and_then(|v| v.as_str()) != Some("run_task") {
          return Action::Continue;
        }
        s.worker_pid = payload.get("worker_pid").and_then(|v| v.as_u64());
        if let Some(task_id) = payload.get("task_id").and_then(|v| v.as_str()) {
          s.task_id = task_id.to_string();
        }
        let task = payload
          .get("task")
          .cloned()
          .unwrap_or(serde_json::Value::Null);
        s.awaiting_result = true;
        Action::IoRequest(build_llm_request_from_task(&task))
      }
      Message::System(SystemMsg::IoResponse {
        correlation_id: _,
        result,
      }) => {
        if !s.awaiting_result {
          return Action::Continue;
        }
        s.awaiting_result = false;
        s.turn_count += 1;
        if s.turn_count >= s.max_turns {
          return Action::Stop(Reason::Normal);
        }
        Action::Send {
          to: s.parent,
          msg: Message::Json(serde_json::json!({
            "type": "worker_result",
            "worker_pid": s.worker_pid,
            "task_id": s.task_id,
            "result": io_result_to_value(result),
          })),
        }
      }
      Message::System(SystemMsg::ReceiveTimeout) => Action::Stop(Reason::Shutdown),
      _ => Action::Continue,
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

fn parse_pid(v: Option<&serde_json::Value>) -> Option<AgentPid> {
  v.and_then(|value| value.as_u64()).map(AgentPid::from_raw)
}

fn extract_task_id(task: &serde_json::Value, fallback: &str) -> String {
  task
    .get("task_id")
    .and_then(|v| v.as_str())
    .unwrap_or(fallback)
    .to_string()
}

fn extract_tasks(goal: &serde_json::Value, result: &IoResult) -> Vec<serde_json::Value> {
  let tasks_value = match result {
    IoResult::Ok(v) => v
      .get("tasks")
      .cloned()
      .or_else(|| v.get("payload").and_then(|p| p.get("tasks")).cloned()),
    _ => None,
  };
  if let Some(arr) = tasks_value.and_then(|v| v.as_array().cloned()) {
    if !arr.is_empty() {
      return arr;
    }
  }
  vec![serde_json::json!({
    "task_id": "task-0",
    "goal": goal.clone(),
  })]
}

fn io_result_to_value(result: IoResult) -> serde_json::Value {
  match result {
    IoResult::Ok(v) => serde_json::json!({
      "ok": true,
      "value": v,
    }),
    IoResult::Error(err) => serde_json::json!({
      "ok": false,
      "error": err,
    }),
    IoResult::Timeout => serde_json::json!({
      "ok": false,
      "error": "timeout",
    }),
  }
}

fn reason_to_value(reason: Reason) -> serde_json::Value {
  match reason {
    Reason::Normal => serde_json::json!("normal"),
    Reason::Shutdown => serde_json::json!("shutdown"),
    Reason::Custom(s) => serde_json::json!(s),
  }
}

struct WorkerResult {
  worker_pid: Option<u64>,
  task_id: Option<String>,
  as_json: serde_json::Value,
}

fn parse_worker_result(payload: &serde_json::Value) -> Option<WorkerResult> {
  if payload.get("type").and_then(|v| v.as_str()) != Some("worker_result") {
    return None;
  }
  Some(WorkerResult {
    worker_pid: payload.get("worker_pid").and_then(|v| v.as_u64()),
    task_id: payload
      .get("task_id")
      .and_then(|v| v.as_str())
      .map(str::to_string),
    as_json: payload.clone(),
  })
}

fn build_llm_request_from_task(task: &serde_json::Value) -> IoOp {
  let provider = task
    .get("provider")
    .and_then(|v| v.as_str())
    .unwrap_or(DEFAULT_LLM_PROVIDER)
    .to_string();

  let model = task
    .get("model")
    .and_then(|v| v.as_str())
    .map(str::to_string);

  let prompt = task
    .get("prompt")
    .and_then(|v| v.as_str())
    .or_else(|| task.get("goal").and_then(|v| v.as_str()))
    .map(str::to_string)
    .unwrap_or_else(|| task.to_string());

  let system_prompt = task
    .get("system")
    .and_then(|v| v.as_str())
    .map(str::to_string);

  let tools = task.get("tools").and_then(|v| v.as_array()).map(|arr| {
    arr
      .iter()
      .filter_map(|v| v.as_str().map(str::to_string))
      .collect::<Vec<String>>()
  });

  let max_iterations = task
    .get("max_iterations")
    .and_then(|v| v.as_u64())
    .map(|v| v as usize);

  let timeout_ms = task.get("timeout_ms").and_then(|v| v.as_u64());

  IoOp::AgentChat {
    provider,
    model,
    system_prompt,
    prompt,
    tools,
    max_iterations,
    timeout_ms,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::checkpoint::{CheckpointStore, InMemoryCheckpointStore};

  #[test]
  fn test_orchestrator_goal_triggers_decompose_request() {
    let behavior = OrchestratorBehavior {
      max_concurrency: 2,
      checkpoint_store: None,
    };
    let mut state = behavior
      .init(serde_json::json!({
        "self_pid": 0x8000_0001u64,
      }))
      .unwrap();
    let action = behavior.handle_message(
      Message::Json(serde_json::json!({
        "goal": "build release notes",
        "requester_pid": 0x8000_0002u64,
      })),
      state.as_mut(),
    );
    match action {
      Action::IoRequest(IoOp::Custom { kind, payload }) => {
        assert_eq!(kind, DECOMPOSE_KIND);
        assert_eq!(payload["goal"], serde_json::json!("build release notes"));
      }
      _ => panic!("expected decompose IoRequest"),
    }
  }

  #[test]
  fn test_orchestrator_decompose_response_spawns_worker() {
    let behavior = OrchestratorBehavior {
      max_concurrency: 2,
      checkpoint_store: None,
    };
    let mut state = behavior
      .init(serde_json::json!({
        "self_pid": 0x8000_0010u64,
      }))
      .unwrap();
    let _ = behavior.handle_message(
      Message::Json(serde_json::json!({
        "goal": "ship feature",
      })),
      state.as_mut(),
    );
    let action = behavior.handle_message(
      Message::System(SystemMsg::IoResponse {
        correlation_id: 0,
        result: IoResult::Ok(serde_json::json!({
          "tasks": [
            {"task_id": "t1", "prompt": "part 1"},
            {"task_id": "t2", "prompt": "part 2"}
          ]
        })),
      }),
      state.as_mut(),
    );
    match action {
      Action::Spawn {
        args,
        link,
        monitor,
        ..
      } => {
        assert_eq!(args["parent_pid"], 0x8000_0010u64);
        assert_eq!(args["task_id"], "t1");
        assert!(!link);
        assert!(monitor);
      }
      _ => panic!("expected Spawn action"),
    }
  }

  #[test]
  fn test_orchestrator_spawn_result_sends_task_to_child() {
    let behavior = OrchestratorBehavior {
      max_concurrency: 1,
      checkpoint_store: None,
    };
    let mut state = behavior
      .init(serde_json::json!({
        "self_pid": 0x8000_0100u64,
      }))
      .unwrap();
    let _ = behavior.handle_message(
      Message::Json(serde_json::json!({
        "goal": "compose",
      })),
      state.as_mut(),
    );
    let _ = behavior.handle_message(
      Message::System(SystemMsg::IoResponse {
        correlation_id: 0,
        result: IoResult::Ok(serde_json::json!({
          "tasks": [{"task_id": "w1", "prompt": "work"}]
        })),
      }),
      state.as_mut(),
    );
    let action = behavior.handle_message(
      Message::System(SystemMsg::SpawnResult {
        child_pid: 0x8000_0200u64,
        monitor_ref: Some(42),
      }),
      state.as_mut(),
    );
    match action {
      Action::Send { to, msg } => {
        assert_eq!(to.raw(), 0x8000_0200u64);
        match msg {
          Message::Json(v) => {
            assert_eq!(v["type"], "run_task");
            assert_eq!(v["task_id"], "w1");
          }
          _ => panic!("expected json task message"),
        }
      }
      _ => panic!("expected task send to child"),
    }
  }

  #[test]
  fn test_worker_run_task_and_send_result() {
    let behavior = WorkerBehavior;
    let mut state = behavior
      .init(serde_json::json!({
        "parent_pid": 0x8000_0300u64,
        "task_id": "fallback",
      }))
      .unwrap();
    let action = behavior.handle_message(
      Message::Json(serde_json::json!({
        "type": "run_task",
        "worker_pid": 0x8000_0400u64,
        "task_id": "task-42",
        "task": {"prompt": "draft summary"},
      })),
      state.as_mut(),
    );
    match action {
      Action::IoRequest(IoOp::AgentChat {
        provider,
        model,
        prompt,
        ..
      }) => {
        assert_eq!(provider, "openai");
        assert!(model.is_none(), "model should be None when not specified");
        assert_eq!(prompt, "draft summary");
      }
      _ => panic!("expected worker IoRequest with AgentChat"),
    }

    let action = behavior.handle_message(
      Message::System(SystemMsg::IoResponse {
        correlation_id: 1,
        result: IoResult::Ok(serde_json::json!({
          "output": "done",
        })),
      }),
      state.as_mut(),
    );
    match action {
      Action::Send { to, msg } => {
        assert_eq!(to.raw(), 0x8000_0300u64);
        match msg {
          Message::Json(v) => {
            assert_eq!(v["type"], "worker_result");
            assert_eq!(v["worker_pid"], 0x8000_0400u64);
            assert_eq!(v["task_id"], "task-42");
            assert_eq!(v["result"]["ok"], true);
          }
          _ => panic!("expected worker result json"),
        }
      }
      _ => panic!("expected send result to parent"),
    }
  }

  #[test]
  fn test_orchestrator_survives_worker_crash_via_down() {
    let behavior = OrchestratorBehavior {
      max_concurrency: 1,
      checkpoint_store: None,
    };
    let mut state = behavior
      .init(serde_json::json!({
        "self_pid": 0x8000_0555u64,
      }))
      .unwrap();

    let _ = behavior.handle_message(
      Message::Json(serde_json::json!({
        "goal": "recover from crash",
      })),
      state.as_mut(),
    );
    let _ = behavior.handle_message(
      Message::System(SystemMsg::IoResponse {
        correlation_id: 1,
        result: IoResult::Ok(serde_json::json!({
          "tasks": [{"task_id": "crashy"}]
        })),
      }),
      state.as_mut(),
    );
    let _ = behavior.handle_message(
      Message::System(SystemMsg::SpawnResult {
        child_pid: 0x8000_0666u64,
        monitor_ref: Some(999),
      }),
      state.as_mut(),
    );

    let action = behavior.handle_message(
      Message::System(SystemMsg::Down {
        monitor_ref: 999,
        pid: 0x8000_0666u64,
        reason: Reason::Custom("panic".into()),
      }),
      state.as_mut(),
    );

    // No crash / panic path: orchestrator handles DOWN and
    // moves to completion behavior.
    assert!(matches!(action, Action::Stop(Reason::Normal)));
    let s = state.as_any().downcast_ref::<OrchestratorState>().unwrap();
    assert_eq!(s.results.len(), 1);
    assert_eq!(s.results[0]["type"], "worker_error");
  }

  #[test]
  fn test_orchestrator_persists_checkpoint_after_worker_result() {
    let store = InMemoryCheckpointStore::new();
    let behavior = OrchestratorBehavior {
      max_concurrency: 1,
      checkpoint_store: Some(Arc::new(store.clone())),
    };

    let mut state = behavior
      .init(serde_json::json!({
        "self_pid": 0x8000_0777u64,
        "checkpoint_key": "orch-checkpoint-a",
      }))
      .unwrap();

    let _ = behavior.handle_message(
      Message::Json(serde_json::json!({
        "goal": "checkpoint this",
      })),
      state.as_mut(),
    );
    let _ = behavior.handle_message(
      Message::System(SystemMsg::IoResponse {
        correlation_id: 0,
        result: IoResult::Ok(serde_json::json!({
          "tasks": [{"task_id": "cp-1", "prompt": "do work"}]
        })),
      }),
      state.as_mut(),
    );
    let _ = behavior.handle_message(
      Message::System(SystemMsg::SpawnResult {
        child_pid: 0x8000_0888u64,
        monitor_ref: Some(1),
      }),
      state.as_mut(),
    );

    let action = behavior.handle_message(
      Message::Json(serde_json::json!({
        "type": "worker_result",
        "worker_pid": 0x8000_0888u64,
        "task_id": "cp-1",
        "result": {"ok": true, "value": {"status": 200}},
      })),
      state.as_mut(),
    );
    assert!(matches!(action, Action::Send { .. }));

    let checkpoint = store.load("orch-checkpoint-a").unwrap().unwrap();
    assert_eq!(checkpoint["goal"], serde_json::json!("checkpoint this"));
    assert_eq!(
      checkpoint["completed_results"].as_array().unwrap().len(),
      1,
      "worker result should be persisted"
    );
  }

  #[test]
  fn test_orchestrator_resumes_from_checkpoint_and_skips_decompose() {
    let store = InMemoryCheckpointStore::new();
    store
      .save(
        "orch-checkpoint-b",
        &serde_json::json!({
          "goal": "resume-goal",
          "completed_results": [{"type":"worker_result","task_id":"done-1"}],
          "pending_tasks": [{"task_id":"todo-1","prompt":"resume me"}],
        }),
      )
      .unwrap();

    let behavior = OrchestratorBehavior {
      max_concurrency: 1,
      checkpoint_store: Some(Arc::new(store)),
    };
    let mut state = behavior
      .init(serde_json::json!({
        "self_pid": 0x8000_0999u64,
        "checkpoint_key": "orch-checkpoint-b",
      }))
      .unwrap();

    let action = behavior.handle_message(
      Message::Json(serde_json::json!({
        "goal": "resume-goal",
      })),
      state.as_mut(),
    );

    match action {
      Action::Spawn { args, .. } => {
        assert_eq!(args["task_id"], "todo-1");
      }
      _ => panic!("expected Spawn from resumed checkpoint, got non-spawn action"),
    }
  }
}
