use std::{
  collections::{HashMap, HashSet},
  sync::Arc,
  time::Duration,
};

use crate::agent_rt::{
  bridge::{
    create_bridge_pool, create_bridge_pool_with_mcp_servers, BridgeHandle, BridgeWorker,
  },
  config::McpServerEntry,
  scheduler::AgentScheduler,
  types::*,
};
use tracing::warn;

/// Multi-scheduler runtime with round-robin process
/// placement and cross-partition message routing.
pub struct SchedulerCluster {
  schedulers: Vec<AgentScheduler>,
  pid_owner: HashMap<AgentPid, usize>,
  next_spawn_partition: usize,
  next_tick_partition: usize,
  shared_bridge_shutdown_handle: Option<BridgeHandle>,
  shared_bridge_workers: usize,
}

impl SchedulerCluster {
  pub fn new(partitions: usize) -> Self {
    let count = partitions.max(1);
    let mut schedulers = Vec::with_capacity(count);
    for _ in 0..count {
      let mut scheduler = AgentScheduler::new();
      scheduler.set_external_routing(true);
      schedulers.push(scheduler);
    }
    Self {
      schedulers,
      pid_owner: HashMap::new(),
      next_spawn_partition: 0,
      next_tick_partition: 0,
      shared_bridge_shutdown_handle: None,
      shared_bridge_workers: 0,
    }
  }

  pub fn scheduler_count(&self) -> usize {
    self.schedulers.len()
  }

  pub fn scheduler(&self, index: usize) -> Option<&AgentScheduler> {
    self.schedulers.get(index)
  }

  pub fn scheduler_mut(&mut self, index: usize) -> Option<&mut AgentScheduler> {
    self.schedulers.get_mut(index)
  }

  /// Attach one shared bridge handle to all partitions.
  /// Scheduler-local clones have shutdown disabled so
  /// pool shutdown is coordinated at cluster level.
  pub fn attach_shared_bridge(&mut self, bridge: BridgeHandle) {
    self.shared_bridge_shutdown_handle = Some(bridge.clone());
    if self.shared_bridge_workers == 0 {
      self.shared_bridge_workers = 1;
    }
    for scheduler in &mut self.schedulers {
      scheduler.set_bridge(bridge.clone().with_shutdown_disabled());
    }
  }

  /// Create and attach a shared bridge worker pool used
  /// by all partitions. Returns workers to run on Tokio.
  pub fn create_shared_bridge_pool(&mut self, worker_count: usize) -> Vec<BridgeWorker> {
    let worker_count = worker_count.max(1);
    let (handle, workers) = create_bridge_pool(worker_count);
    self.shared_bridge_workers = workers.len();
    self.attach_shared_bridge(handle);
    workers
  }

  /// Create and attach a shared bridge worker pool pre-configured with
  /// external MCP server definitions.
  pub fn create_shared_bridge_pool_with_mcp_servers(
    &mut self,
    worker_count: usize,
    server_configs: Vec<McpServerEntry>,
  ) -> Vec<BridgeWorker> {
    let worker_count = worker_count.max(1);
    let (handle, workers) =
      create_bridge_pool_with_mcp_servers(worker_count, server_configs);
    self.shared_bridge_workers = workers.len();
    self.attach_shared_bridge(handle);
    workers
  }

  pub fn owner_of(&self, pid: AgentPid) -> Option<usize> {
    self.pid_owner.get(&pid).copied()
  }

  pub fn spawn(
    &mut self,
    behavior: Arc<dyn AgentBehavior>,
    args: serde_json::Value,
  ) -> Result<AgentPid, Reason> {
    let idx = self.next_spawn_partition % self.schedulers.len();
    self.next_spawn_partition = (self.next_spawn_partition + 1) % self.schedulers.len();
    self.spawn_on(idx, behavior, args)
  }

  pub fn spawn_on(
    &mut self,
    partition: usize,
    behavior: Arc<dyn AgentBehavior>,
    args: serde_json::Value,
  ) -> Result<AgentPid, Reason> {
    let scheduler = self
      .schedulers
      .get_mut(partition)
      .ok_or_else(|| Reason::Custom(format!("invalid partition {}", partition)))?;
    let pid = scheduler.registry.spawn(behavior, args)?;
    self.pid_owner.insert(pid, partition);
    Ok(pid)
  }

  pub fn enqueue(&mut self, pid: AgentPid) -> Result<(), String> {
    self.reconcile_pid_map();
    let idx = self
      .pid_owner
      .get(&pid)
      .copied()
      .ok_or_else(|| format!("Process {:?} not found in cluster", pid))?;
    self.schedulers[idx].enqueue(pid);
    Ok(())
  }

  pub fn send(&mut self, to: AgentPid, msg: Message) -> Result<(), String> {
    self.reconcile_pid_map();
    let idx = self
      .pid_owner
      .get(&to)
      .copied()
      .ok_or_else(|| format!("Process {:?} not found in cluster", to))?;
    self.schedulers[idx].send(to, msg)
  }

  /// Execute one scheduling round across partitions.
  /// Returns true if any scheduler did work or any
  /// cross-scheduler message was routed.
  pub fn tick(&mut self) -> bool {
    let mut did_work = false;
    let count = self.schedulers.len();
    for offset in 0..count {
      let idx = (self.next_tick_partition + offset) % count;
      if self.schedulers[idx].tick() {
        did_work = true;
      }
    }
    self.next_tick_partition = (self.next_tick_partition + 1) % count;

    let routed = self.route_outbound_messages();
    self.reconcile_pid_map();
    did_work || routed > 0
  }

  pub fn graceful_shutdown(&mut self, timeout: Duration) {
    if let Some(mut bridge) = self.shared_bridge_shutdown_handle.take() {
      let rounds = self.shared_bridge_workers.max(1);
      for _ in 0..rounds {
        if bridge.shutdown(timeout).is_err() {
          break;
        }
      }
      for scheduler in &mut self.schedulers {
        scheduler.clear_bridge();
      }
      self.shared_bridge_workers = 0;
    }

    for scheduler in &mut self.schedulers {
      scheduler.graceful_shutdown(timeout);
    }
    self.pid_owner.clear();
  }

  fn route_outbound_messages(&mut self) -> usize {
    let mut outbound: Vec<(AgentPid, Message)> = Vec::new();
    for scheduler in &mut self.schedulers {
      outbound.extend(scheduler.drain_outbound_messages());
    }

    let mut routed = 0;
    for (to, msg) in outbound {
      match self.send(to, msg) {
        Ok(()) => routed += 1,
        Err(err) => warn!(to = to.raw(), error = err, "dropping unroutable message"),
      }
    }
    routed
  }

  fn reconcile_pid_map(&mut self) {
    let mut seen: HashSet<AgentPid> = HashSet::new();
    for (idx, scheduler) in self.schedulers.iter().enumerate() {
      for pid in scheduler.registry.pids() {
        self.pid_owner.insert(pid, idx);
        seen.insert(pid);
      }
    }
    self.pid_owner.retain(|pid, _| seen.contains(pid));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::types::{Action, AgentState, IoOp, Message, Reason, SystemMsg};

  struct EmptyState;

  impl AgentState for EmptyState {
    fn as_any(&self) -> &dyn std::any::Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
      self
    }
  }

  struct NopBehavior;

  impl AgentBehavior for NopBehavior {
    fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(EmptyState))
    }
    fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
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
    fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
  }

  struct SenderState {
    target: AgentPid,
  }

  impl AgentState for SenderState {
    fn as_any(&self) -> &dyn std::any::Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
      self
    }
  }

  struct SenderBehavior;

  impl AgentBehavior for SenderBehavior {
    fn init(&self, args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
      let target = args
        .get("target")
        .and_then(|v| v.as_u64())
        .map(AgentPid::from_raw)
        .ok_or_else(|| Reason::Custom("missing target".to_string()))?;
      Ok(Box::new(SenderState { target }))
    }

    fn handle_message(&self, _msg: Message, state: &mut dyn AgentState) -> Action {
      let s = state.as_any_mut().downcast_mut::<SenderState>().unwrap();
      Action::Send {
        to: s.target,
        msg: Message::Text("cross-partition".to_string()),
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

  struct TimerOnceState {
    requested: bool,
    completed: bool,
  }

  impl AgentState for TimerOnceState {
    fn as_any(&self) -> &dyn std::any::Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
      self
    }
  }

  struct TimerOnceBehavior {
    duration: Duration,
  }

  impl AgentBehavior for TimerOnceBehavior {
    fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(TimerOnceState {
        requested: false,
        completed: false,
      }))
    }

    fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
      let s = state.as_any_mut().downcast_mut::<TimerOnceState>().unwrap();
      match msg {
        Message::Text(_) if !s.requested => {
          s.requested = true;
          Action::IoRequest(IoOp::Timer {
            duration: self.duration,
          })
        }
        Message::System(SystemMsg::IoResponse { .. }) => {
          s.completed = true;
          Action::Continue
        }
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

  #[test]
  fn test_cluster_spawn_round_robin_across_partitions() {
    let mut cluster = SchedulerCluster::new(2);

    let p1 = cluster
      .spawn(Arc::new(NopBehavior), serde_json::Value::Null)
      .unwrap();
    let p2 = cluster
      .spawn(Arc::new(NopBehavior), serde_json::Value::Null)
      .unwrap();
    let p3 = cluster
      .spawn(Arc::new(NopBehavior), serde_json::Value::Null)
      .unwrap();

    assert_eq!(cluster.owner_of(p1), Some(0));
    assert_eq!(cluster.owner_of(p2), Some(1));
    assert_eq!(cluster.owner_of(p3), Some(0));
  }

  #[test]
  fn test_cluster_routes_cross_scheduler_messages() {
    let mut cluster = SchedulerCluster::new(2);
    let receiver = cluster
      .spawn(Arc::new(NopBehavior), serde_json::Value::Null)
      .unwrap();
    let sender = cluster
      .spawn(
        Arc::new(SenderBehavior),
        serde_json::json!({ "target": receiver.raw() }),
      )
      .unwrap();

    cluster.send(sender, Message::Text("go".into())).unwrap();
    cluster.enqueue(sender).unwrap();
    cluster.tick();

    let receiver_partition = cluster.owner_of(receiver).unwrap();
    let receiver_proc = cluster
      .scheduler(receiver_partition)
      .unwrap()
      .registry
      .lookup(&receiver)
      .unwrap();
    assert_eq!(receiver_proc.mailbox.len(), 1);
    assert!(matches!(
      receiver_proc.mailbox.front(),
      Some(Message::Text(text)) if text == "cross-partition"
    ));
  }

  #[tokio::test]
  async fn test_cluster_shared_bridge_pool_handles_io_across_partitions() {
    let mut cluster = SchedulerCluster::new(2);
    let workers = cluster.create_shared_bridge_pool(2);
    let mut worker_handles = Vec::new();
    for worker in workers {
      worker_handles.push(tokio::spawn(async move {
        worker.run().await;
      }));
    }

    let p1 = cluster
      .spawn(
        Arc::new(TimerOnceBehavior {
          duration: Duration::from_millis(15),
        }),
        serde_json::Value::Null,
      )
      .unwrap();
    let p2 = cluster
      .spawn(
        Arc::new(TimerOnceBehavior {
          duration: Duration::from_millis(15),
        }),
        serde_json::Value::Null,
      )
      .unwrap();

    cluster.send(p1, Message::Text("go".into())).unwrap();
    cluster.send(p2, Message::Text("go".into())).unwrap();
    cluster.enqueue(p1).unwrap();
    cluster.enqueue(p2).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
      cluster.tick();
      if timer_completed(&cluster, p1) && timer_completed(&cluster, p2) {
        break;
      }
      if std::time::Instant::now() >= deadline {
        panic!("timed out waiting for shared bridge pool io responses");
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }

    cluster.graceful_shutdown(Duration::from_millis(200));
    for handle in worker_handles {
      let _ = handle.await;
    }
  }

  fn timer_completed(cluster: &SchedulerCluster, pid: AgentPid) -> bool {
    let Some(partition) = cluster.owner_of(pid) else {
      return false;
    };
    let Some(proc) = cluster
      .scheduler(partition)
      .and_then(|s| s.registry.lookup(&pid))
    else {
      return false;
    };
    proc
      .state
      .as_ref()
      .and_then(|s| s.as_any().downcast_ref::<TimerOnceState>())
      .map(|s| s.completed)
      .unwrap_or(false)
  }
}
