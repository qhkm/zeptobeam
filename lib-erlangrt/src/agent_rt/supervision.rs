use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc, time::Instant};

use crate::agent_rt::{process::Priority, scheduler::AgentScheduler, types::*};
use tracing::{info, warn};

/// Strategy for restarting children when one dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
  OneForOne,
  OneForAll,
  RestForOne,
}

/// When a child should be restarted after exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildRestart {
  Permanent,
  Transient,
  Temporary,
}

/// Strategy for delaying child restarts.
#[derive(Debug, Clone)]
pub enum BackoffStrategy {
  /// Restart immediately.
  None,
  /// Restart after a fixed delay for every attempt.
  Fixed { delay_ms: u64 },
  /// Restart with exponential delay:
  /// base_ms * multiplier^attempt, capped at max_ms.
  Exponential {
    base_ms: u64,
    max_ms: u64,
    multiplier: f64,
  },
}

impl BackoffStrategy {
  /// Return delay in milliseconds for restart attempt.
  pub fn delay_for_attempt(&self, attempt: u32) -> u64 {
    match self {
      BackoffStrategy::None => 0,
      BackoffStrategy::Fixed { delay_ms } => *delay_ms,
      BackoffStrategy::Exponential {
        base_ms,
        max_ms,
        multiplier,
      } => {
        let delay = (*base_ms as f64) * multiplier.powi(attempt as i32);
        let clamped = delay.min(*max_ms as f64) as u64;
        clamped.min(*max_ms)
      }
    }
  }
}

impl Default for BackoffStrategy {
  fn default() -> Self {
    BackoffStrategy::None
  }
}

/// Specification for a supervised child process.
pub struct ChildSpec {
  pub id: String,
  pub behavior: Arc<dyn AgentBehavior>,
  pub args: serde_json::Value,
  pub restart: ChildRestart,
  pub priority: Priority,
}

impl ChildSpec {
  pub fn clone_spec(&self) -> Self {
    ChildSpec {
      id: self.id.clone(),
      behavior: self.behavior.clone(),
      args: self.args.clone(),
      restart: self.restart,
      priority: self.priority,
    }
  }
}

/// Configuration for starting a supervisor with its
/// children and restart intensity limits.
pub struct SupervisorSpec {
  pub strategy: RestartStrategy,
  pub max_restarts: u32,
  pub max_seconds: u32,
  pub children: Vec<ChildSpec>,
  pub backoff: BackoffStrategy,
}

/// A currently running supervised child.
pub struct RunningChild {
  pub id: String,
  pub pid: AgentPid,
  pub spec: ChildSpec,
}

/// Manages a set of child processes with automatic
/// restart according to the chosen strategy and
/// restart intensity limits.
pub struct Supervisor {
  pub strategy: RestartStrategy,
  pub max_restarts: u32,
  pub max_seconds: u32,
  pub children: Vec<RunningChild>,
  restart_timestamps: Vec<Instant>,
  shutdown: bool,
  backoff: BackoffStrategy,
  restart_counts: HashMap<String, u32>,
}

impl Supervisor {
  /// Start a supervisor and auto-wire it to the
  /// scheduler's exit handler so child deaths are
  /// automatically routed to handle_child_exit.
  /// Returns an Rc<RefCell<Supervisor>> for shared
  /// ownership between caller and exit handler closure.
  pub fn start_linked(
    sched: &mut AgentScheduler,
    spec: SupervisorSpec,
  ) -> Result<Rc<RefCell<Self>>, Reason> {
    let sup = Rc::new(RefCell::new(Self::start(sched, spec)?));
    let sup_ref = sup.clone();
    sched.add_exit_handler(move |sched, pid, reason| {
      sup_ref.borrow_mut().handle_child_exit(sched, pid, reason);
    });
    Ok(sup)
  }

  /// Start a supervisor by spawning all children from
  /// the spec.
  pub fn start(sched: &mut AgentScheduler, spec: SupervisorSpec) -> Result<Self, Reason> {
    let mut children = Vec::new();
    for child_spec in spec.children {
      let pid = sched
        .registry
        .spawn(child_spec.behavior.clone(), child_spec.args.clone())?;
      // Set priority on the spawned process
      if let Some(proc) = sched.registry.lookup_mut(&pid) {
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
      backoff: spec.backoff,
      restart_counts: HashMap::new(),
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
    info!(
      child_pid = pid.raw(),
      ?reason,
      strategy = ?self.strategy,
      "supervisor.child_exit"
    );
    if self.shutdown {
      return;
    }

    // Find the index of the dead child
    let idx = match self.children.iter().position(|c| c.pid == pid) {
      Some(i) => i,
      None => return,
    };

    let should_restart = match self.children[idx].spec.restart {
      ChildRestart::Permanent => true,
      ChildRestart::Transient => !matches!(reason, Reason::Normal | Reason::Shutdown),
      ChildRestart::Temporary => false,
    };

    if !should_restart {
      // Remove the child without restarting
      self.children.remove(idx);
      return;
    }

    // Check restart intensity
    if self.exceeds_max_restarts() {
      warn!(
        child_pid = pid.raw(),
        max_restarts = self.max_restarts,
        max_seconds = self.max_seconds,
        "supervisor.intensity_exceeded"
      );
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

  /// Return the next restart delay (ms) for a child.
  pub fn pending_restart_delay(&self, child_id: &str) -> u64 {
    let attempt = self.restart_counts.get(child_id).copied().unwrap_or(0);
    self.backoff.delay_for_attempt(attempt)
  }

  /// Check whether the restart intensity has been
  /// exceeded (more than max_restarts in the last
  /// max_seconds window).
  fn exceeds_max_restarts(&mut self) -> bool {
    let now = Instant::now();
    self.restart_timestamps.push(now);

    let window = std::time::Duration::from_secs(self.max_seconds as u64);
    self
      .restart_timestamps
      .retain(|t| now.duration_since(*t) <= window);

    self.restart_timestamps.len() as u32 > self.max_restarts
  }

  /// OneForOne: restart only the dead child at idx.
  fn restart_one(&mut self, sched: &mut AgentScheduler, idx: usize) {
    let dead = self.children.remove(idx);
    info!(
      child_id = dead.id,
      old_pid = dead.pid.raw(),
      strategy = "one_for_one",
      "supervisor.restart"
    );
    self.increment_restart_count(&dead.id);
    // Dead child is already terminated — don't call
    // terminate_process again (avoids duplicate exit
    // notifications to linked processes).
    if let Some(new) = self.spawn_child(sched, &dead.spec) {
      self.children.insert(idx, new);
    }
  }

  /// OneForAll: terminate all children and restart
  /// them all from their specs.
  fn restart_all(&mut self, sched: &mut AgentScheduler, dead_idx: usize) {
    let dead_pid = self.children[dead_idx].pid;

    // Collect specs before draining children
    let specs: Vec<ChildSpec> =
      self.children.iter().map(|c| c.spec.clone_spec()).collect();

    // Terminate all current children except the
    // already-dead one
    for child in self.children.drain(..) {
      if child.pid != dead_pid {
        sched.terminate_process(child.pid, Reason::Shutdown);
      }
    }

    // Respawn all from specs
    for spec in specs {
      self.increment_restart_count(&spec.id);
      if let Some(new) = self.spawn_child(sched, &spec) {
        self.children.push(new);
      }
    }
  }

  /// RestForOne: terminate children after the dead one
  /// (inclusive), then restart those.
  fn restart_rest(&mut self, sched: &mut AgentScheduler, dead_idx: usize) {
    let dead_pid = self.children[dead_idx].pid;

    // Collect specs for children from dead_idx onward
    let specs: Vec<ChildSpec> = self.children[dead_idx..]
      .iter()
      .map(|c| c.spec.clone_spec())
      .collect();

    // Terminate and remove from dead_idx onward,
    // skipping the already-dead child
    let rest: Vec<RunningChild> = self.children.drain(dead_idx..).collect();
    for child in rest {
      if child.pid != dead_pid {
        sched.terminate_process(child.pid, Reason::Shutdown);
      }
    }

    // Respawn the collected specs
    for spec in specs {
      self.increment_restart_count(&spec.id);
      if let Some(new) = self.spawn_child(sched, &spec) {
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
    if let Some(proc) = sched.registry.lookup_mut(&pid) {
      proc.priority = spec.priority;
    }
    sched.enqueue(pid);
    Some(RunningChild {
      id: spec.id.clone(),
      pid,
      spec: spec.clone_spec(),
    })
  }

  fn increment_restart_count(&mut self, child_id: &str) {
    let count = self.restart_counts.entry(child_id.to_owned()).or_insert(0);
    *count += 1;
  }
}
