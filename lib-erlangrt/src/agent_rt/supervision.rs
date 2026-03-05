use std::sync::Arc;
use std::time::Instant;

use crate::agent_rt::process::Priority;
use crate::agent_rt::scheduler::AgentScheduler;
use crate::agent_rt::types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
  OneForOne,
  OneForAll,
  RestForOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildRestart {
  Permanent,
  Transient,
  Temporary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPolicy {
  Timeout(u64),
  Brutal,
  Infinity,
}

pub struct ChildSpec {
  pub id: String,
  pub behavior: Arc<dyn AgentBehavior>,
  pub args: serde_json::Value,
  pub restart: ChildRestart,
  pub shutdown: ShutdownPolicy,
  pub priority: Priority,
}

impl ChildSpec {
  pub fn clone_spec(&self) -> Self {
    ChildSpec {
      id: self.id.clone(),
      behavior: self.behavior.clone(),
      args: self.args.clone(),
      restart: self.restart,
      shutdown: self.shutdown,
      priority: self.priority,
    }
  }
}

pub struct SupervisorSpec {
  pub strategy: RestartStrategy,
  pub max_restarts: u32,
  pub max_seconds: u32,
  pub children: Vec<ChildSpec>,
}

pub struct RunningChild {
  pub id: String,
  pub pid: AgentPid,
  pub spec: ChildSpec,
}

pub struct Supervisor {
  pub strategy: RestartStrategy,
  pub max_restarts: u32,
  pub max_seconds: u32,
  pub children: Vec<RunningChild>,
  restart_timestamps: Vec<Instant>,
  shutdown: bool,
}

impl Supervisor {
  /// Start a supervisor by spawning all children from
  /// the spec.
  pub fn start(
    sched: &mut AgentScheduler,
    spec: SupervisorSpec,
  ) -> Result<Self, Reason> {
    let mut children = Vec::new();
    for child_spec in spec.children {
      let pid = sched.registry.spawn(
        child_spec.behavior.clone(),
        child_spec.args.clone(),
      )?;
      // Set priority on the spawned process
      if let Some(proc) =
        sched.registry.lookup_mut(&pid)
      {
        proc.priority = child_spec.priority;
      }
      sched.enqueue(pid);
      children.push(RunningChild {
        id: child_spec.id.clone(),
        pid,
        spec: child_spec,
      });
    }
    Ok(Self {
      strategy: spec.strategy,
      max_restarts: spec.max_restarts,
      max_seconds: spec.max_seconds,
      children,
      restart_timestamps: Vec::new(),
      shutdown: false,
    })
  }

  /// Handle a child exit. Depending on the restart
  /// policy and strategy, restart one or more children.
  pub fn handle_child_exit(
    &mut self,
    sched: &mut AgentScheduler,
    pid: AgentPid,
    reason: Reason,
  ) {
    if self.shutdown {
      return;
    }

    // Find the index of the dead child
    let idx = match self
      .children
      .iter()
      .position(|c| c.pid == pid)
    {
      Some(i) => i,
      None => return,
    };

    let should_restart = match self.children[idx]
      .spec
      .restart
    {
      ChildRestart::Permanent => true,
      ChildRestart::Transient => {
        !matches!(
          reason,
          Reason::Normal | Reason::Shutdown
        )
      }
      ChildRestart::Temporary => false,
    };

    if !should_restart {
      // Remove the child without restarting
      self.children.remove(idx);
      return;
    }

    // Check restart intensity
    if self.exceeds_max_restarts() {
      self.shutdown = true;
      return;
    }

    match self.strategy {
      RestartStrategy::OneForOne => {
        self.restart_one(sched, idx);
      }
      RestartStrategy::OneForAll => {
        self.restart_all(sched, idx);
      }
      RestartStrategy::RestForOne => {
        self.restart_rest(sched, idx);
      }
    }
  }

  pub fn is_shutdown(&self) -> bool {
    self.shutdown
  }

  pub fn children_pids(&self) -> Vec<AgentPid> {
    self.children.iter().map(|c| c.pid).collect()
  }

  /// Check whether the restart intensity has been
  /// exceeded (more than max_restarts in the last
  /// max_seconds window).
  fn exceeds_max_restarts(&mut self) -> bool {
    let now = Instant::now();
    self.restart_timestamps.push(now);

    let window = std::time::Duration::from_secs(
      self.max_seconds as u64,
    );
    self
      .restart_timestamps
      .retain(|t| now.duration_since(*t) <= window);

    self.restart_timestamps.len() as u32
      > self.max_restarts
  }

  /// OneForOne: restart only the dead child at idx.
  fn restart_one(
    &mut self,
    sched: &mut AgentScheduler,
    idx: usize,
  ) {
    let dead = self.children.remove(idx);
    // Dead child is already terminated — don't call
    // terminate_process again (avoids duplicate exit
    // notifications to linked processes).
    if let Some(new) =
      self.spawn_child(sched, &dead.spec)
    {
      self.children.insert(idx, new);
    }
  }

  /// OneForAll: terminate all children and restart
  /// them all from their specs.
  fn restart_all(
    &mut self,
    sched: &mut AgentScheduler,
    dead_idx: usize,
  ) {
    let dead_pid = self.children[dead_idx].pid;

    // Collect specs before draining children
    let specs: Vec<ChildSpec> = self
      .children
      .iter()
      .map(|c| c.spec.clone_spec())
      .collect();

    // Terminate all current children except the
    // already-dead one
    for child in self.children.drain(..) {
      if child.pid != dead_pid {
        sched.terminate_process(
          child.pid,
          Reason::Shutdown,
        );
      }
    }

    // Respawn all from specs
    for spec in specs {
      if let Some(new) =
        self.spawn_child(sched, &spec)
      {
        self.children.push(new);
      }
    }
  }

  /// RestForOne: terminate children after the dead one
  /// (inclusive), then restart those.
  fn restart_rest(
    &mut self,
    sched: &mut AgentScheduler,
    dead_idx: usize,
  ) {
    let dead_pid = self.children[dead_idx].pid;

    // Collect specs for children from dead_idx onward
    let specs: Vec<ChildSpec> = self.children
      [dead_idx..]
      .iter()
      .map(|c| c.spec.clone_spec())
      .collect();

    // Terminate and remove from dead_idx onward,
    // skipping the already-dead child
    let rest: Vec<RunningChild> =
      self.children.drain(dead_idx..).collect();
    for child in rest {
      if child.pid != dead_pid {
        sched.terminate_process(
          child.pid,
          Reason::Shutdown,
        );
      }
    }

    // Respawn the collected specs
    for spec in specs {
      if let Some(new) =
        self.spawn_child(sched, &spec)
      {
        self.children.push(new);
      }
    }
  }

  /// Spawn a single child from a ChildSpec, returning
  /// a RunningChild on success.
  fn spawn_child(
    &self,
    sched: &mut AgentScheduler,
    spec: &ChildSpec,
  ) -> Option<RunningChild> {
    let pid = sched
      .registry
      .spawn(spec.behavior.clone(), spec.args.clone())
      .ok()?;
    if let Some(proc) =
      sched.registry.lookup_mut(&pid)
    {
      proc.priority = spec.priority;
    }
    sched.enqueue(pid);
    Some(RunningChild {
      id: spec.id.clone(),
      pid,
      spec: spec.clone_spec(),
    })
  }
}
