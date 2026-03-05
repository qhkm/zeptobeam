use std::collections::VecDeque;

use crate::agent_rt::{
  bridge::BridgeHandle,
  process::{Priority, ProcessStatus},
  registry::AgentRegistry,
  types::*,
};

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
  advantage_count: usize,
  bridge: Option<BridgeHandle>,
  exit_handlers: Vec<ExitHandler>,
}

impl AgentScheduler {
  pub fn new() -> Self {
    Self {
      registry: AgentRegistry::new(),
      queue_high: VecDeque::new(),
      queue_normal: VecDeque::new(),
      queue_low: VecDeque::new(),
      advantage_count: 0,
      bridge: None,
      exit_handlers: Vec::new(),
    }
  }

  pub fn set_bridge(&mut self, bridge: BridgeHandle) {
    self.bridge = bridge.into();
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

  pub fn demonitor(
    &mut self,
    watcher: AgentPid,
    mon_ref: MonitorRef,
  ) -> bool {
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
    let was_waiting = {
      let proc = self
        .registry
        .lookup_mut(&to)
        .ok_or_else(|| format!("Process {:?} not found", to))?;
      let was = proc.status == ProcessStatus::Waiting;
      proc.deliver_message(msg);
      was
    };
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
    for (resp_pid, msg) in responses {
      let _ = self.send(resp_pid, msg);
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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
          match msg {
            Message::System(SystemMsg::Exit { from, reason }) => {
              behavior.handle_exit(AgentPid::from_raw(from), reason, state.as_mut())
            }
            other => behavior.handle_message(other, state.as_mut()),
          }
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
          should_stop = Some(reason);
          break;
        }
        Action::IoRequest(op) => {
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
              proc.deliver_message(Message::Text(
                "no I/O bridge configured".to_string(),
              ));
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
    // Collect links and monitor relations before terminating
    let (links, monitored_by, monitors): (
      Vec<AgentPid>,
      Vec<(MonitorRef, AgentPid)>,
      Vec<(MonitorRef, AgentPid)>,
    ) = self
      .registry
      .lookup(&pid)
      .map(|p| {
        (
          p.links.clone(),
          p.monitored_by.clone(),
          p.monitors.clone(),
        )
      })
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
      if let Some(target_proc) =
        self.registry.lookup_mut(&target_pid)
      {
        target_proc.monitored_by.retain(|(r, w)| {
          !(*r == mon_ref && *w == pid)
        });
      }
    }

    // Notify linked processes
    let mut pids_to_cascade: Vec<AgentPid> = Vec::new();
    let mut pids_to_wake: Vec<AgentPid> = Vec::new();
    let mut down_to_send: Vec<(AgentPid, Message)> =
      Vec::new();

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
      if let Some(watcher_proc) =
        self.registry.lookup_mut(&watcher_pid)
      {
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
  }
}

impl Default for AgentScheduler {
  fn default() -> Self {
    Self::new()
  }
}
