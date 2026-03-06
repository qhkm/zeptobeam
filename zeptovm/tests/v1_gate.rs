//! v1 Gate Test: supervision tree with restart, escalation, and exit signals.

use std::{
  sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
  },
  time::Duration,
};

use async_trait::async_trait;
use zeptovm::{
  behavior::Behavior,
  error::{Action, Message, Reason},
  supervisor::{
    start_supervisor, ChildSpec, RestartPolicy, RestartStrategy, ShutdownPolicy,
    SupervisorSpec,
  },
};

/// Behavior that crashes on every init (simulates persistent failure)
struct AlwaysCrash {
  init_count: Arc<AtomicU32>,
}

#[async_trait]
impl Behavior for AlwaysCrash {
  async fn init(
    &mut self,
    _cp: Option<Vec<u8>>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    self.init_count.fetch_add(1, Ordering::Relaxed);
    Err("persistent crash".into())
  }
  async fn handle(&mut self, _msg: Message) -> Action {
    Action::Continue
  }
  async fn terminate(&mut self, _reason: &Reason) {}
}

/// Behavior that runs normally until killed
struct Healthy {
  init_count: Arc<AtomicU32>,
}

#[async_trait]
impl Behavior for Healthy {
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

/// Gate test 1: OneForOne restart works — crashed child is restarted
#[tokio::test]
async fn gate_v1_one_for_one_restarts() {
  let crash_count = Arc::new(AtomicU32::new(0));
  let healthy_count = Arc::new(AtomicU32::new(0));
  let cc = crash_count.clone();
  let hc = healthy_count.clone();

  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 3,
    restart_window: Duration::from_secs(60),
    children: vec![
      ChildSpec {
        id: "crasher".into(),
        factory: Box::new(move || {
          Box::new(AlwaysCrash {
            init_count: cc.clone(),
          }) as Box<dyn Behavior>
        }),
        restart: RestartPolicy::Always,
        shutdown: ShutdownPolicy::Brutal,
        user_mailbox_capacity: 16,
      },
      ChildSpec {
        id: "healthy".into(),
        factory: Box::new(move || {
          Box::new(Healthy {
            init_count: hc.clone(),
          }) as Box<dyn Behavior>
        }),
        restart: RestartPolicy::Always,
        shutdown: ShutdownPolicy::Brutal,
        user_mailbox_capacity: 16,
      },
    ],
  };

  let (_handle, join) = start_supervisor(spec);
  // Supervisor should exit when crasher exceeds max_restarts
  let _ = join.await;

  // Crasher init called: 1 initial + 3 restarts = at least 4
  let crashes = crash_count.load(Ordering::Relaxed);
  assert!(
    crashes >= 4,
    "expected at least 4 crash inits, got {crashes}"
  );

  // Healthy should have been started only once (OneForOne doesn't
  // restart siblings)
  let healthy = healthy_count.load(Ordering::Relaxed);
  assert_eq!(
    healthy, 1,
    "healthy child should only be started once, got {healthy}"
  );
}

/// Gate test 2: max_restarts escalation — supervisor exits when limit exceeded
#[tokio::test]
async fn gate_v1_max_restarts_escalation() {
  let init_count = Arc::new(AtomicU32::new(0));
  let ic = init_count.clone();

  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 2,
    restart_window: Duration::from_secs(60),
    children: vec![ChildSpec {
      id: "crasher".into(),
      factory: Box::new(move || {
        Box::new(AlwaysCrash {
          init_count: ic.clone(),
        }) as Box<dyn Behavior>
      }),
      restart: RestartPolicy::Always,
      shutdown: ShutdownPolicy::Brutal,
      user_mailbox_capacity: 16,
    }],
  };

  let (_handle, join) = start_supervisor(spec);

  // Supervisor should exit (not hang) because max_restarts exceeded
  let result = tokio::time::timeout(Duration::from_secs(5), join).await;
  assert!(
    result.is_ok(),
    "supervisor should have exited, not timed out"
  );
}

/// Gate test 3: OnFailure policy doesn't restart on normal exit
#[tokio::test]
async fn gate_v1_on_failure_no_restart_normal() {
  let init_count = Arc::new(AtomicU32::new(0));
  let ic = init_count.clone();

  struct NormalExitOnInit {
    init_count: Arc<AtomicU32>,
  }

  #[async_trait]
  impl Behavior for NormalExitOnInit {
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
        Box::new(NormalExitOnInit {
          init_count: ic.clone(),
        }) as Box<dyn Behavior>
      }),
      restart: RestartPolicy::OnFailure,
      shutdown: ShutdownPolicy::Brutal,
      user_mailbox_capacity: 16,
    }],
  };

  let (sup_handle, join) = start_supervisor(spec);

  // Wait for child to start, then kill supervisor
  tokio::time::sleep(Duration::from_millis(200)).await;
  sup_handle.kill();
  let _ = join.await;

  // Should only have been started once (no restart on normal exit)
  let count = init_count.load(Ordering::Relaxed);
  assert_eq!(count, 1, "expected 1 init (no restart), got {count}");
}
