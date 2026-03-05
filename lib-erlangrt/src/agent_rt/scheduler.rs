use std::collections::VecDeque;

use crate::agent_rt::bridge::BridgeHandle;
use crate::agent_rt::process::{Priority, ProcessStatus};
use crate::agent_rt::registry::AgentRegistry;
use crate::agent_rt::types::*;

const DEFAULT_REDUCTIONS: u32 = 200;
const NORMAL_ADVANTAGE: usize = 8;

pub struct AgentScheduler {
  pub registry: AgentRegistry,
  queue_high: VecDeque<AgentPid>,
  queue_normal: VecDeque<AgentPid>,
  queue_low: VecDeque<AgentPid>,
  advantage_count: usize,
  bridge: Option<BridgeHandle>,
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
    }
  }

  pub fn set_bridge(&mut self, bridge: BridgeHandle) {
    self.bridge = bridge.into();
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
      Priority::Normal => {
        self.queue_normal.push_back(pid)
      }
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
      if self.advantage_count < NORMAL_ADVANTAGE
        || self.queue_low.is_empty()
      {
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
  pub fn send(
    &mut self,
    to: AgentPid,
    msg: Message,
  ) -> Result<(), String> {
    let was_waiting = {
      let proc = self
        .registry
        .lookup_mut(&to)
        .ok_or_else(|| {
          format!("Process {:?} not found", to)
        })?;
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

    let pid = match self.next_runnable() {
      Some(p) => p,
      None => return false,
    };

    // Verify process still exists and is runnable
    {
      let proc = match self.registry.lookup(&pid) {
        Some(p) => p,
        None => return false,
      };
      if proc.status != ProcessStatus::Runnable {
        return false;
      }
    }

    // Reset reductions
    if let Some(proc) = self.registry.lookup_mut(&pid) {
      proc.reductions = DEFAULT_REDUCTIONS;
    }

    let mut pending_sends: Vec<(AgentPid, Message)> =
      Vec::new();
    let mut pending_spawns: Vec<(
      std::sync::Arc<dyn AgentBehavior>,
      serde_json::Value,
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
        let proc =
          match self.registry.lookup_mut(&pid) {
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
      // handle_message, then put state back
      let action = {
        let proc =
          match self.registry.lookup_mut(&pid) {
            Some(p) => p,
            None => break,
          };
        proc.reductions =
          proc.reductions.saturating_sub(1);
        let behavior = proc.behavior.clone();
        let mut state = match proc.state.take() {
          Some(s) => s,
          None => break,
        };
        let action = behavior
          .handle_message(msg, state.as_mut());
        proc.state = Some(state);
        action
      };

      // Handle the action
      match action {
        Action::Continue => {}
        Action::Send { to, msg } => {
          if let Some(proc) =
            self.registry.lookup_mut(&pid)
          {
            proc.reductions =
              proc.reductions.saturating_sub(2);
          }
          pending_sends.push((to, msg));
        }
        Action::Stop(reason) => {
          should_stop = Some(reason);
          break;
        }
        Action::IoRequest(op) => {
          if let Some(proc) =
            self.registry.lookup_mut(&pid)
          {
            proc.reductions =
              proc.reductions.saturating_sub(1);
            proc.status = ProcessStatus::Waiting;
          }
          // Submit to bridge if available
          if let Some(ref mut bridge) = self.bridge {
            if let Err(err) = bridge.submit(pid, op) {
              // Bridge overflow/disconnect — wake
              // process with error
              if let Some(proc) =
                self.registry.lookup_mut(&pid)
              {
                proc.deliver_message(
                  Message::Text(err),
                );
                proc.status = ProcessStatus::Runnable;
              }
              self.enqueue(pid);
            }
          }
          break;
        }
        Action::Spawn { behavior, args } => {
          if let Some(proc) =
            self.registry.lookup_mut(&pid)
          {
            proc.reductions =
              proc.reductions.saturating_sub(10);
          }
          pending_spawns.push((behavior, args));
        }
      }
    }

    // Post-dispatch: handle deferred operations
    if should_wait {
      if let Some(proc) =
        self.registry.lookup_mut(&pid)
      {
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
    for (behavior, args) in pending_spawns {
      if let Ok(child_pid) =
        self.registry.spawn(behavior, args)
      {
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
  pub fn terminate_process(
    &mut self,
    pid: AgentPid,
    reason: Reason,
  ) {
    // Collect links before terminating
    let links: Vec<AgentPid> = self
      .registry
      .lookup(&pid)
      .map(|p| p.links.clone())
      .unwrap_or_default();

    // Call terminate on the process
    if let Some(proc) =
      self.registry.lookup_mut(&pid)
    {
      proc.terminate(reason.clone());
    }

    // Remove from registry BEFORE notifying links
    // to prevent infinite recursion on mutual links
    self.registry.remove(&pid);

    // Notify linked processes
    let mut pids_to_cascade: Vec<AgentPid> =
      Vec::new();
    let mut pids_to_wake: Vec<AgentPid> =
      Vec::new();

    for linked_pid in &links {
      let trap = self
        .registry
        .lookup(linked_pid)
        .map(|p| p.trap_exit)
        .unwrap_or(false);
      let exists =
        self.registry.lookup(linked_pid).is_some();
      if !exists {
        continue;
      }

      if trap {
        // Deliver Exit as mailbox message
        let exit_msg =
          Message::System(SystemMsg::Exit {
            from: pid.raw(),
            reason: reason.clone(),
          });
        if let Some(lp) =
          self.registry.lookup_mut(linked_pid)
        {
          let was_waiting =
            lp.status == ProcessStatus::Waiting;
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

    // Wake processes that were Waiting
    for wake_pid in pids_to_wake {
      self.enqueue(wake_pid);
    }

    // Cascade termination after the loop
    for cascade_pid in pids_to_cascade {
      self.terminate_process(
        cascade_pid,
        reason.clone(),
      );
    }
  }
}
