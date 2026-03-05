use std::{collections::VecDeque, sync::Arc};

use crate::agent_rt::{
  bridge::BridgeHandle,
  observability::{ProcessSnapshot, RuntimeMetrics, RuntimeMetricsSnapshot},
  process::{Priority, ProcessStatus},
  registry::AgentRegistry,
  types::*,
};
use tracing::{debug, info, warn};

const DEFAULT_REDUCTIONS: u32 = 200;
const NORMAL_ADVANTAGE: usize = 8;
type ExitHandler = Box<dyn FnMut(&mut AgentScheduler, AgentPid, Reason)>;

/// Reduction-counting preemptive scheduler with
/// three priority queues (High > Normal > Low) and
/// optional Tokio bridge for async I/O.
pub struct AgentScheduler {
  pub registry: AgentRegistry,
  queue_high: VecDeque<AgentPid>,
  queue_normal: VecDeque<AgentPid>,
  queue_low: VecDeque<AgentPid>,
  allow_external_routing: bool,
  outbound_messages: Vec<(AgentPid, Message)>,
  advantage_count: usize,
  bridge: Option<BridgeHandle>,
  metrics: Arc<RuntimeMetrics>,
  exit_handlers: Vec<ExitHandler>,
}

impl AgentScheduler {
  pub fn new() -> Self {
    Self {
      registry: AgentRegistry::new(),
      queue_high: VecDeque::new(),
      queue_normal: VecDeque::new(),
      queue_low: VecDeque::new(),
      allow_external_routing: false,
      outbound_messages: Vec::new(),
      advantage_count: 0,
      bridge: None,
      metrics: Arc::new(RuntimeMetrics::new()),
      exit_handlers: Vec::new(),
    }
  }

  pub fn metrics(&self) -> Arc<RuntimeMetrics> {
    self.metrics.clone()
  }

  pub fn metrics_snapshot(&self) -> RuntimeMetricsSnapshot {
    self.metrics.snapshot(self.registry.count())
  }

  pub fn list_processes(&self) -> Vec<ProcessSnapshot> {
    let mut snapshots: Vec<ProcessSnapshot> = self
      .registry
      .pids()
      .into_iter()
      .filter_map(|pid| self.registry.lookup(&pid))
      .map(|proc| ProcessSnapshot {
        pid: proc.pid.raw(),
        status: proc.status,
        priority: proc.priority,
        mailbox_depth: proc.mailbox.len(),
        link_count: proc.links.len(),
        monitor_count: proc.monitors.len(),
        monitored_by_count: proc.monitored_by.len(),
        trap_exit: proc.trap_exit,
      })
      .collect();
    snapshots.sort_by_key(|s| s.pid);
    snapshots
  }

  pub fn set_bridge(&mut self, bridge: BridgeHandle) {
    self.bridge = bridge.into();
  }

  /// Enable or disable external routing mode.
  /// When enabled, messages addressed to unknown local
  /// PIDs are queued for external delivery instead of
  /// returning an error.
  pub fn set_external_routing(&mut self, enabled: bool) {
    self.allow_external_routing = enabled;
  }

  /// Drain messages that were addressed to unknown local
  /// PIDs while external routing mode was enabled.
  pub fn drain_outbound_messages(&mut self) -> Vec<(AgentPid, Message)> {
    std::mem::take(&mut self.outbound_messages)
  }

  /// Shut down runtime resources in a bounded time.
  /// This stops bridge intake, drains pending I/O
  /// responses, then terminates all remaining
  /// processes with Reason::Shutdown.
  pub fn graceful_shutdown(&mut self, timeout: std::time::Duration) {
    let started = std::time::Instant::now();
    info!(
      timeout_ms = timeout.as_millis() as u64,
      "scheduler graceful shutdown start"
    );

    if let Some(bridge) = self.bridge.as_mut() {
      if let Err(err) = bridge.shutdown(timeout) {
        warn!(error = err, "bridge shutdown did not complete cleanly");
      }

      let responses = bridge.drain_responses();
      for (pid, msg) in responses {
        let _ = self.send(pid, msg);
      }
    }

    let all_pids = self.registry.pids();
    for pid in all_pids {
      if self.registry.lookup(&pid).is_some() {
        self.terminate_process(pid, Reason::Shutdown);
      }
    }

    self.bridge = None;
    self.queue_high.clear();
    self.queue_normal.clear();
    self.queue_low.clear();

    info!(
      elapsed_ms = started.elapsed().as_millis() as u64,
      remaining_processes = self.registry.count() as u64,
      "scheduler graceful shutdown complete"
    );
  }

  /// Register an optional process-exit hook.
  /// The hook can drive supervision by reacting to
  /// process terminations and restarting children.
  pub fn add_exit_handler<F>(&mut self, handler: F)
  where
    F: FnMut(&mut AgentScheduler, AgentPid, Reason) + 'static,
  {
    self.exit_handlers.push(Box::new(handler));
  }

  pub fn monitor(
    &mut self,
    watcher: AgentPid,
    target: AgentPid,
  ) -> Result<MonitorRef, String> {
    if self.registry.lookup(&watcher).is_none() {
      return Err(format!("Watcher {:?} not found", watcher));
    }
    if self.registry.lookup(&target).is_none() {
      return Err(format!("Target {:?} not found", target));
    }
    let mon_ref = MonitorRef::new();
    if let Some(watcher_proc) = self.registry.lookup_mut(&watcher) {
      watcher_proc.monitors.push((mon_ref, target));
    }
    if let Some(target_proc) = self.registry.lookup_mut(&target) {
      target_proc.monitored_by.push((mon_ref, watcher));
    }
    Ok(mon_ref)
  }

  pub fn demonitor(&mut self, watcher: AgentPid, mon_ref: MonitorRef) -> bool {
    let target = {
      let watcher_proc = match self.registry.lookup_mut(&watcher) {
        Some(proc) => proc,
        None => return false,
      };
      let pos = match watcher_proc
        .monitors
        .iter()
        .position(|(r, _)| *r == mon_ref)
      {
        Some(i) => i,
        None => return false,
      };
      watcher_proc.monitors.swap_remove(pos).1
    };
    if let Some(target_proc) = self.registry.lookup_mut(&target) {
      target_proc
        .monitored_by
        .retain(|(r, w)| !(*r == mon_ref && *w == watcher));
    }
    true
  }

  /// Set a receive timeout on a process. If the process
  /// is Waiting and the deadline expires, the scheduler
  /// delivers a ReceiveTimeout message and wakes it.
  /// Returns an error if the PID is unknown or the
  /// duration overflows Instant.
  pub fn set_receive_timeout(
    &mut self,
    pid: AgentPid,
    duration: std::time::Duration,
  ) -> Result<(), String> {
    let proc = self
      .registry
      .lookup_mut(&pid)
      .ok_or_else(|| format!("Process {:?} not found", pid))?;
    let deadline = std::time::Instant::now()
      .checked_add(duration)
      .ok_or_else(|| "timeout duration overflow".to_string())?;
    proc.receive_timeout = Some(deadline);
    Ok(())
  }

  /// Add a process to the appropriate priority queue
  /// based on its Priority field.
  pub fn enqueue(&mut self, pid: AgentPid) {
    let priority = match self.registry.lookup(&pid) {
      Some(proc) => proc.priority,
      None => return,
    };
    match priority {
      Priority::High => self.queue_high.push_back(pid),
      Priority::Normal => self.queue_normal.push_back(pid),
      Priority::Low => self.queue_low.push_back(pid),
    }
    debug!(pid = pid.raw(), ?priority, "scheduler enqueue");
  }

  /// Pick the next runnable process from priority queues.
  /// High always first, then Normal has advantage over
  /// Low (NORMAL_ADVANTAGE ticks before Low gets a turn).
  fn next_runnable(&mut self) -> Option<AgentPid> {
    if let Some(pid) = self.queue_high.pop_front() {
      return Some(pid);
    }
    if !self.queue_normal.is_empty() {
      self.advantage_count += 1;
      if self.advantage_count < NORMAL_ADVANTAGE || self.queue_low.is_empty() {
        return self.queue_normal.pop_front();
      }
      // Give low priority a turn
      self.advantage_count = 0;
      if let Some(pid) = self.queue_low.pop_front() {
        return Some(pid);
      }
      return self.queue_normal.pop_front();
    }
    self.queue_low.pop_front()
  }

  /// Deliver a message to a process. If the process was
  /// Waiting, wake it and re-enqueue it.
  pub fn send(&mut self, to: AgentPid, msg: Message) -> Result<(), String> {
    let msg_kind = message_kind(&msg);
    let was_waiting = if let Some(proc) = self.registry.lookup_mut(&to) {
      let was = proc.status == ProcessStatus::Waiting;
      proc.deliver_message(msg);
      was
    } else if self.allow_external_routing {
      self.outbound_messages.push((to, msg));
      debug!(
        to = to.raw(),
        msg_kind = msg_kind,
        "scheduler queued outbound message for external routing"
      );
      return Ok(());
    } else {
      return Err(format!("Process {:?} not found", to));
    };
    self.metrics.record_message_sent();
    debug!(
      to = to.raw(),
      msg_kind = msg_kind,
      woke_waiting = was_waiting,
      "scheduler send"
    );
    if was_waiting {
      self.enqueue(to);
    }
    Ok(())
  }

  /// Run one scheduling cycle. Returns true if work was
  /// done, false if no runnable process was found.
  pub fn tick(&mut self) -> bool {
    // Drain bridge responses and deliver to processes
    let responses = self
      .bridge
      .as_ref()
      .map(|b| b.drain_responses())
      .unwrap_or_default();
    if !responses.is_empty() {
      debug!(
        count = responses.len(),
        "scheduler drained bridge responses"
      );
    }
    for (resp_pid, msg) in responses {
      let _ = self.send(resp_pid, msg);
    }

    // Check receive timeouts on Waiting processes
    let now = std::time::Instant::now();
    let timed_out: Vec<AgentPid> = self
      .registry
      .pids()
      .into_iter()
      .filter(|pid| {
        self
          .registry
          .lookup(pid)
          .map(|p| {
            p.status == ProcessStatus::Waiting
              && p.receive_timeout.map_or(false, |d| now >= d)
          })
          .unwrap_or(false)
      })
      .collect();
    for pid in timed_out {
      if let Some(proc) = self.registry.lookup_mut(&pid) {
        proc.receive_timeout = None;
      }
      debug!(pid = pid.raw(), "scheduler receive timeout fired");
      let _ = self.send(pid, Message::System(SystemMsg::ReceiveTimeout));
    }

    // Skip stale/non-runnable queue entries until we
    // find runnable work or run out of queued processes.
    let pid = loop {
      let candidate = match self.next_runnable() {
        Some(p) => p,
        None => return false,
      };
      let is_runnable = self
        .registry
        .lookup(&candidate)
        .map(|p| p.status == ProcessStatus::Runnable)
        .unwrap_or(false);
      if is_runnable {
        debug!(pid = candidate.raw(), "scheduler selected runnable process");
        break candidate;
      }
    };

    // Reset reductions
    if let Some(proc) = self.registry.lookup_mut(&pid) {
      proc.reductions = DEFAULT_REDUCTIONS;
    }

    let mut pending_sends: Vec<(AgentPid, Message)> = Vec::new();
    let mut pending_spawns: Vec<(
      std::sync::Arc<dyn AgentBehavior>,
      serde_json::Value,
      bool,
      bool,
    )> = Vec::new();
    let mut should_stop: Option<Reason> = None;
    let mut should_wait = false;
    let mut out_of_reductions = false;

    // Dispatch loop
    loop {
      // Check reductions
      {
        let proc = match self.registry.lookup(&pid) {
          Some(p) => p,
          None => break,
        };
        if proc.reductions == 0 {
          out_of_reductions = true;
          break;
        }
      }

      // Get next message
      let msg = {
        let proc = match self.registry.lookup_mut(&pid) {
          Some(p) => p,
          None => break,
        };
        proc.next_message()
      };

      let msg = match msg {
        Some(m) => m,
        None => {
          // Mailbox empty -> set Waiting
          should_wait = true;
          break;
        }
      };
      self.metrics.record_message_processed();

      // Dispatch message: get behavior + state, call
      // the appropriate callback, then put state back.
      // Wrapped in catch_unwind for BEAM-style process
      // isolation — a panicking behavior terminates the
      // process instead of crashing the scheduler.
      let action = {
        let proc = match self.registry.lookup_mut(&pid) {
          Some(p) => p,
          None => break,
        };
        proc.reductions = proc.reductions.saturating_sub(1);
        let behavior = proc.behavior.clone();
        let mut state = match proc.state.take() {
          Some(s) => s,
          None => break,
        };
        let result =
          std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
            Message::System(SystemMsg::Exit { from, reason }) => {
              behavior.handle_exit(AgentPid::from_raw(from), reason, state.as_mut())
            }
            other => behavior.handle_message(other, state.as_mut()),
          }));
        match result {
          Ok(action) => {
            if let Some(proc) = self.registry.lookup_mut(&pid) {
              proc.state = Some(state);
            }
            action
          }
          Err(panic_info) => {
            // State may be corrupted — drop it
            drop(state);
            let panic_msg = panic_info
              .downcast_ref::<String>()
              .cloned()
              .or_else(|| panic_info.downcast_ref::<&str>().map(|s| s.to_string()))
              .unwrap_or_else(|| "unknown panic".to_string());
            Action::Stop(Reason::Custom(format!("panic: {}", panic_msg)))
          }
        }
      };

      // Handle the action
      match action {
        Action::Continue => {}
        Action::Send { to, msg } => {
          if let Some(proc) = self.registry.lookup_mut(&pid) {
            proc.reductions = proc.reductions.saturating_sub(2);
          }
          pending_sends.push((to, msg));
        }
        Action::Stop(reason) => {
          debug!(pid = pid.raw(), ?reason, "process requested stop");
          should_stop = Some(reason);
          break;
        }
        Action::IoRequest(op) => {
          debug!(
            pid = pid.raw(),
            op_kind = io_op_kind(&op),
            "process requested i/o"
          );
          if let Some(proc) = self.registry.lookup_mut(&pid) {
            proc.reductions = proc.reductions.saturating_sub(1);
            proc.status = ProcessStatus::Waiting;
          }
          // Submit to bridge if available
          if let Some(ref mut bridge) = self.bridge {
            if let Err(err) = bridge.submit(pid, op) {
              // Bridge overflow/disconnect — wake
              // process with error
              if let Some(proc) = self.registry.lookup_mut(&pid) {
                proc.deliver_message(Message::Text(err));
                proc.status = ProcessStatus::Runnable;
              }
              self.enqueue(pid);
            }
          } else {
            // No bridge configured — deliver error
            // back to process so it doesn't get stuck
            if let Some(proc) = self.registry.lookup_mut(&pid) {
              proc.deliver_message(Message::Text("no I/O bridge configured".to_string()));
              proc.status = ProcessStatus::Runnable;
            }
            self.enqueue(pid);
          }
          break;
        }
        Action::Spawn {
          behavior,
          args,
          link,
          monitor,
        } => {
          if let Some(proc) = self.registry.lookup_mut(&pid) {
            proc.reductions = proc.reductions.saturating_sub(10);
          }
          pending_spawns.push((behavior, args, link, monitor));
        }
      }
    }

    // Post-dispatch: handle deferred operations
    if should_wait {
      if let Some(proc) = self.registry.lookup_mut(&pid) {
        proc.status = ProcessStatus::Waiting;
      }
    }

    if out_of_reductions {
      self.enqueue(pid);
    }

    // Process pending sends
    for (to, msg) in pending_sends {
      let _ = self.send(to, msg);
    }

    // Process pending spawns
    for (behavior, args, link, monitor) in pending_spawns {
      if let Ok(child_pid) = self.registry.spawn(behavior, args) {
        if link {
          // Auto-link parent <-> child
          if let Some(parent_proc) = self.registry.lookup_mut(&pid) {
            parent_proc.links.push(child_pid);
          }
          if let Some(child_proc) = self.registry.lookup_mut(&child_pid) {
            child_proc.links.push(pid);
          }
        }

        let monitor_ref = if monitor {
          self.monitor(pid, child_pid).ok().map(|r| r.raw())
        } else {
          None
        };

        // Notify parent with SpawnResult
        let spawn_msg = Message::System(SystemMsg::SpawnResult {
          child_pid: child_pid.raw(),
          monitor_ref,
        });
        let _ = self.send(pid, spawn_msg);
        debug!(
          parent_pid = pid.raw(),
          child_pid = child_pid.raw(),
          link,
          monitor,
          monitor_ref = ?monitor_ref,
          "spawn completed"
        );

        self.enqueue(child_pid);
      }
    }

    // Handle stop
    if let Some(reason) = should_stop {
      self.terminate_process(pid, reason);
    }

    true
  }

  /// Terminate a process: call its terminate callback,
  /// remove from registry, then notify linked processes.
  /// If a linked process has trap_exit=true, deliver Exit
  /// as a mailbox message. If trap_exit=false and reason
  /// is not Normal, cascade termination. Normal exits
  /// with trap_exit=false do not propagate.
  pub fn terminate_process(&mut self, pid: AgentPid, reason: Reason) {
    info!(pid = pid.raw(), ?reason, "terminating process");
    // Collect links and monitor relations before terminating
    let (links, monitored_by, monitors): (
      Vec<AgentPid>,
      Vec<(MonitorRef, AgentPid)>,
      Vec<(MonitorRef, AgentPid)>,
    ) = self
      .registry
      .lookup(&pid)
      .map(|p| (p.links.clone(), p.monitored_by.clone(), p.monitors.clone()))
      .unwrap_or_default();

    // Call terminate on the process
    if let Some(proc) = self.registry.lookup_mut(&pid) {
      proc.terminate(reason.clone());
    }

    // Remove from registry BEFORE notifying links
    // to prevent infinite recursion on mutual links
    self.registry.remove(&pid);

    // Remove reverse monitor records for monitors owned by
    // this process.
    for (mon_ref, target_pid) in monitors {
      if let Some(target_proc) = self.registry.lookup_mut(&target_pid) {
        target_proc
          .monitored_by
          .retain(|(r, w)| !(*r == mon_ref && *w == pid));
      }
    }

    // Notify linked processes
    let mut pids_to_cascade: Vec<AgentPid> = Vec::new();
    let mut pids_to_wake: Vec<AgentPid> = Vec::new();
    let mut down_to_send: Vec<(AgentPid, Message)> = Vec::new();

    for linked_pid in &links {
      let trap = self
        .registry
        .lookup(linked_pid)
        .map(|p| p.trap_exit)
        .unwrap_or(false);
      let exists = self.registry.lookup(linked_pid).is_some();
      if !exists {
        continue;
      }

      if trap {
        // Deliver Exit as mailbox message
        let exit_msg = Message::System(SystemMsg::Exit {
          from: pid.raw(),
          reason: reason.clone(),
        });
        if let Some(lp) = self.registry.lookup_mut(linked_pid) {
          let was_waiting = lp.status == ProcessStatus::Waiting;
          lp.deliver_message(exit_msg);
          if was_waiting {
            pids_to_wake.push(*linked_pid);
          }
        }
      } else if !matches!(reason, Reason::Normal) {
        // Cascade death for non-normal exits
        pids_to_cascade.push(*linked_pid);
      }
      // Normal + no trap_exit = do nothing
    }

    // Notify monitor watchers via DOWN messages
    for (mon_ref, watcher_pid) in monitored_by {
      if let Some(watcher_proc) = self.registry.lookup_mut(&watcher_pid) {
        watcher_proc
          .monitors
          .retain(|(r, t)| !(*r == mon_ref && *t == pid));
      }
      down_to_send.push((
        watcher_pid,
        Message::System(SystemMsg::Down {
          monitor_ref: mon_ref.raw(),
          pid: pid.raw(),
          reason: reason.clone(),
        }),
      ));
    }

    // Wake processes that were Waiting
    for wake_pid in pids_to_wake {
      self.enqueue(wake_pid);
    }

    // Send DOWN notifications after monitor cleanup.
    for (watcher_pid, down_msg) in down_to_send {
      let _ = self.send(watcher_pid, down_msg);
    }

    // Cascade termination after the loop
    for cascade_pid in pids_to_cascade {
      self.terminate_process(cascade_pid, reason.clone());
    }

    // Notify optional exit handlers (for supervisor
    // integration) after termination is complete.
    if !self.exit_handlers.is_empty() {
      let mut handlers = std::mem::take(&mut self.exit_handlers);
      for handler in &mut handlers {
        handler(self, pid, reason.clone());
      }
      self.exit_handlers = handlers;
    }
    self.metrics.record_process_termination();
  }
}

impl Default for AgentScheduler {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for AgentScheduler {
  fn drop(&mut self) {
    if self.registry.count() == 0 && self.bridge.is_none() {
      return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      self.graceful_shutdown(std::time::Duration::from_millis(50));
    }));
  }
}

fn message_kind(msg: &Message) -> &'static str {
  match msg {
    Message::Text(_) => "Text",
    Message::Json(_) => "Json",
    Message::System(_) => "System",
  }
}

fn io_op_kind(op: &IoOp) -> &'static str {
  match op {
    IoOp::HttpRequest { .. } => "HttpRequest",
    IoOp::LlmRequest { .. } => "LlmRequest",
    IoOp::Timer { .. } => "Timer",
    IoOp::Custom { .. } => "Custom",
  }
}
