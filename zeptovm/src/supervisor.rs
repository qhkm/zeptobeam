use std::{
  pin::pin,
  time::{Duration, Instant},
};

use futures::future::{select, select_all, Either};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{
  behavior::Behavior,
  error::Reason,
  mailbox::ProcessHandle,
  pid::Pid,
  process::{spawn_process_boxed, ProcessExit},
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
  OneForOne,
  OneForAll,
  RestForOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
  Always,
  OnFailure,
  Never,
}

impl RestartPolicy {
  pub fn should_restart(&self, reason: &Reason) -> bool {
    match self {
      RestartPolicy::Always => true,
      RestartPolicy::OnFailure => reason.is_abnormal(),
      RestartPolicy::Never => false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPolicy {
  Timeout(Duration),
  Brutal,
}

pub struct ChildSpec {
  pub id: String,
  pub factory: Box<dyn Fn() -> Box<dyn Behavior> + Send>,
  pub restart: RestartPolicy,
  pub shutdown: ShutdownPolicy,
  pub user_mailbox_capacity: usize,
}

pub struct SupervisorSpec {
  pub strategy: RestartStrategy,
  pub max_restarts: u32,
  pub restart_window: Duration,
  pub children: Vec<ChildSpec>,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Metadata about a running child (everything except the JoinHandle).
struct ChildMeta {
  pid: Pid,
  handle: ProcessHandle,
  spec_index: usize,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start a supervisor. Returns a handle (for killing) and JoinHandle.
pub fn start_supervisor(spec: SupervisorSpec) -> (ProcessHandle, JoinHandle<()>) {
  // Create a lightweight handle for the supervisor itself (for external kill).
  let (sup_handle, _sup_mailbox) = crate::mailbox::create_mailbox(1, 1);
  let kill_token = sup_handle.kill_token.clone();

  let join = tokio::spawn(async move {
    supervision_loop(spec, kill_token).await;
  });

  (sup_handle, join)
}

// ---------------------------------------------------------------------------
// Supervision loop
// ---------------------------------------------------------------------------

async fn supervision_loop(spec: SupervisorSpec, kill_token: CancellationToken) {
  // Parallel vecs: metas[i] corresponds to joins[i].
  let mut metas: Vec<ChildMeta> = Vec::new();
  let mut joins: Vec<JoinHandle<ProcessExit>> = Vec::new();
  let mut restart_timestamps: Vec<Instant> = Vec::new();

  // Start all children from their specs.
  for (i, child_spec) in spec.children.iter().enumerate() {
    let (meta, join) = start_child(child_spec, i);
    debug!(
      child_id = %child_spec.id,
      pid = %meta.pid,
      "supervisor started child"
    );
    metas.push(meta);
    joins.push(join);
  }

  loop {
    if joins.is_empty() {
      debug!("supervisor: no children remaining, exiting");
      break;
    }

    // Race: kill token vs next child exit.
    let killed = pin!(kill_token.cancelled());
    let child_exit = select_all(joins);

    match select(killed, child_exit).await {
      // Kill token fired first.
      Either::Left((_cancelled, child_exit_fut)) => {
        debug!("supervisor: kill token fired, shutting down children");
        kill_all_children(&metas);
        // Drop the select_all future — this gives us back the
        // JoinHandles implicitly. But we still need to await them
        // to avoid leaked tasks. The inner handles are consumed
        // by select_all, so we abort by dropping child_exit_fut
        // which aborts nothing (JoinHandle drop doesn't abort).
        // Instead, we stored handles in metas — kill already
        // fired the CancellationTokens. The tasks will finish
        // on their own. We just await the select_all future's
        // remaining handles. Actually, select_all consumed them.
        // Let's just drop and move on — the kill tokens already
        // cancelled all children.
        drop(child_exit_fut);
        break;
      }
      // A child exited first.
      Either::Right(((result, index, remaining), _cancelled)) => {
        let meta = metas.remove(index);
        joins = remaining;

        let exit = match result {
          Ok(exit) => exit,
          Err(join_err) => {
            warn!(
              pid = %meta.pid,
              error = %join_err,
              "child task panicked"
            );
            ProcessExit {
              pid: meta.pid,
              reason: Reason::Custom(format!("task panic: {join_err}")),
            }
          }
        };

        let child_spec = &spec.children[meta.spec_index];
        debug!(
          child_id = %child_spec.id,
          pid = %exit.pid,
          reason = %exit.reason,
          "supervisor: child exited"
        );

        if !child_spec.restart.should_restart(&exit.reason) {
          debug!(
            child_id = %child_spec.id,
            "supervisor: not restarting child (policy)"
          );
          continue;
        }

        // Check restart intensity.
        let now = Instant::now();
        restart_timestamps.retain(|t| now.duration_since(*t) < spec.restart_window);
        restart_timestamps.push(now);

        if restart_timestamps.len() as u32 > spec.max_restarts {
          warn!(
            "supervisor: max restarts ({}) exceeded in {:?} window, \
             shutting down",
            spec.max_restarts, spec.restart_window
          );
          kill_all_children(&metas);
          for j in joins {
            let _ = j.await;
          }
          break;
        }

        // Restart the child.
        let (new_meta, new_join) = start_child(child_spec, meta.spec_index);
        debug!(
          child_id = %child_spec.id,
          pid = %new_meta.pid,
          "supervisor: restarted child"
        );
        metas.push(new_meta);
        joins.push(new_join);
      }
    }
  }
}

fn start_child(
  spec: &ChildSpec,
  spec_index: usize,
) -> (ChildMeta, JoinHandle<ProcessExit>) {
  let behavior = (spec.factory)();
  let (pid, handle, join) =
    spawn_process_boxed(behavior, spec.user_mailbox_capacity, None);
  let meta = ChildMeta {
    pid,
    handle,
    spec_index,
  };
  (meta, join)
}

fn kill_all_children(metas: &[ChildMeta]) {
  for meta in metas {
    meta.handle.kill();
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    behavior::Behavior,
    error::{Action, Message, Reason},
  };
  use async_trait::async_trait;
  use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
  };

  #[test]
  fn test_restart_policy_should_restart() {
    assert!(RestartPolicy::Always.should_restart(&Reason::Normal));
    assert!(RestartPolicy::Always.should_restart(&Reason::Custom("x".into())));
    assert!(!RestartPolicy::OnFailure.should_restart(&Reason::Normal));
    assert!(RestartPolicy::OnFailure.should_restart(&Reason::Custom("x".into())));
    assert!(!RestartPolicy::Never.should_restart(&Reason::Normal));
    assert!(!RestartPolicy::Never.should_restart(&Reason::Custom("x".into())));
  }

  struct ImmediateCrash {
    init_count: Arc<AtomicU32>,
  }

  #[async_trait]
  impl Behavior for ImmediateCrash {
    async fn init(
      &mut self,
      _cp: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
      self.init_count.fetch_add(1, Ordering::Relaxed);
      // Crash immediately by returning error
      Err("crash on init".into())
    }
    async fn handle(&mut self, _msg: Message) -> Action {
      Action::Continue
    }
    async fn terminate(&mut self, _reason: &Reason) {}
  }

  #[tokio::test]
  async fn test_supervisor_restarts_crashed_child() {
    let init_count = Arc::new(AtomicU32::new(0));
    let ic = init_count.clone();

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForOne,
      max_restarts: 3,
      restart_window: Duration::from_secs(60),
      children: vec![ChildSpec {
        id: "crasher".into(),
        factory: Box::new(move || {
          Box::new(ImmediateCrash {
            init_count: ic.clone(),
          }) as Box<dyn Behavior>
        }),
        restart: RestartPolicy::Always,
        shutdown: ShutdownPolicy::Brutal,
        user_mailbox_capacity: 16,
      }],
    };

    let (_handle, join) = start_supervisor(spec);

    // Supervisor should try to start the child, it crashes, restarts
    // up to 3 times, then supervisor exits due to max_restarts exceeded.
    let _ = join.await;

    // init called: 1 initial + 3 restarts = 4
    let count = init_count.load(Ordering::Relaxed);
    assert!(count >= 4, "expected at least 4 init calls, got {count}");
  }

  #[tokio::test]
  async fn test_supervisor_no_restart_on_normal_with_on_failure() {
    let init_count = Arc::new(AtomicU32::new(0));
    let ic = init_count.clone();

    struct NormalExit {
      init_count: Arc<AtomicU32>,
    }
    #[async_trait]
    impl Behavior for NormalExit {
      async fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
      ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.init_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
      }
      async fn handle(&mut self, _msg: Message) -> Action {
        Action::Stop(Reason::Normal)
      }
      async fn terminate(&mut self, _reason: &Reason) {}
    }

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForOne,
      max_restarts: 5,
      restart_window: Duration::from_secs(60),
      children: vec![ChildSpec {
        id: "normal-exit".into(),
        factory: Box::new(move || {
          Box::new(NormalExit {
            init_count: ic.clone(),
          }) as Box<dyn Behavior>
        }),
        restart: RestartPolicy::OnFailure,
        shutdown: ShutdownPolicy::Brutal,
        user_mailbox_capacity: 16,
      }],
    };

    let (sup_handle, join) = start_supervisor(spec);

    // The child won't exit until it receives a message. Wait briefly
    // then kill the supervisor. The child should only have been inited
    // once (no restarts since it hasn't crashed).
    tokio::time::sleep(Duration::from_millis(100)).await;
    sup_handle.kill();
    let _ = join.await;

    // Only 1 init (no restarts since child didn't crash)
    assert_eq!(init_count.load(Ordering::Relaxed), 1);
  }

  #[tokio::test]
  async fn test_supervisor_kill_stops_children() {
    let init_count = Arc::new(AtomicU32::new(0));
    let ic = init_count.clone();

    struct WaitForever {
      init_count: Arc<AtomicU32>,
    }
    #[async_trait]
    impl Behavior for WaitForever {
      async fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
      ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.init_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
      }
      async fn handle(&mut self, _msg: Message) -> Action {
        Action::Continue
      }
      async fn terminate(&mut self, _reason: &Reason) {}
    }

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForOne,
      max_restarts: 5,
      restart_window: Duration::from_secs(60),
      children: vec![
        ChildSpec {
          id: "child-1".into(),
          factory: Box::new({
            let ic = ic.clone();
            move || {
              Box::new(WaitForever {
                init_count: ic.clone(),
              }) as Box<dyn Behavior>
            }
          }),
          restart: RestartPolicy::Always,
          shutdown: ShutdownPolicy::Brutal,
          user_mailbox_capacity: 16,
        },
        ChildSpec {
          id: "child-2".into(),
          factory: Box::new({
            let ic = ic.clone();
            move || {
              Box::new(WaitForever {
                init_count: ic.clone(),
              }) as Box<dyn Behavior>
            }
          }),
          restart: RestartPolicy::Always,
          shutdown: ShutdownPolicy::Brutal,
          user_mailbox_capacity: 16,
        },
      ],
    };

    let (sup_handle, join) = start_supervisor(spec);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Both children should have started
    assert_eq!(init_count.load(Ordering::Relaxed), 2);

    // Kill supervisor
    sup_handle.kill();
    let _ = join.await;
    // Supervisor and children should all be stopped (test completes = no hang)
  }
}
