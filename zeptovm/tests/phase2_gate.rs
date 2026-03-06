//! Phase 2 gate tests: verify timer, supervision, and effect hardening.

use zeptovm::core::behavior::StepBehavior;
use zeptovm::core::effect::{
    CompensationSpec, EffectId, EffectKind, EffectResult, EffectStatus,
};
use zeptovm::core::message::{Envelope, Signal};
use zeptovm::core::step_result::StepResult;
use zeptovm::core::supervisor::{
  BackoffPolicy, RestartStrategy, SupervisionStrategy, SupervisorSpec,
};
use zeptovm::core::timer::{TimerKind, TimerSpec};
use zeptovm::core::turn_context::TurnContext;
use zeptovm::durability::idempotency::IdempotencyStore;
use zeptovm::error::Reason;
use zeptovm::kernel::compensation::{CompensationEntry, CompensationLog};
use zeptovm::kernel::metrics::Metrics;
use zeptovm::kernel::supervisor_behavior::SupervisorBehavior;
use zeptovm::kernel::timer_wheel::TimerWheel;
use zeptovm::pid::Pid;

/// Gate test: timer wheel fires at correct time.
#[test]
fn phase2_gate_timer_wheel() {
    let mut wheel = TimerWheel::new();
    let pid = Pid::from_raw(1);

    // Schedule 3 timers at different deadlines
    wheel.schedule(TimerSpec::new(pid, TimerKind::SleepUntil, 100));
    wheel.schedule(TimerSpec::new(pid, TimerKind::Timeout, 200));
    wheel.schedule(TimerSpec::new(pid, TimerKind::RetryBackoff, 300));

    assert_eq!(wheel.len(), 3);

    // Tick at 150ms — only first timer should fire
    let fired = wheel.tick(150);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].kind, TimerKind::SleepUntil);

    // Tick at 300ms — remaining two fire
    let fired = wheel.tick(300);
    assert_eq!(fired.len(), 2);
    assert!(wheel.is_empty());
}

/// Gate test: supervisor detects max restarts exceeded.
#[test]
fn phase2_gate_supervisor_escalation() {
    let mut sup = SupervisorBehavior::new(SupervisorSpec {
        max_restarts: 2,
        restart_window_ms: 5000,
        backoff: BackoffPolicy::Immediate,
        strategy: SupervisionStrategy::OneForOne,
    });
    let sup_pid = Pid::from_raw(1);
    let child = Pid::from_raw(10);
    sup.register_child("w1".into(), child, RestartStrategy::Permanent);
    sup.init(None);

    // First crash — restart
    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
        sup_pid,
        Signal::ChildExited {
            child_pid: child,
            reason: Reason::Custom("crash".into()),
        },
    );
    let r = sup.handle(msg, &mut ctx);
    assert!(matches!(r, StepResult::Continue));

    // Second crash — exceeds max_restarts=2, supervisor shuts down
    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
        sup_pid,
        Signal::ChildExited {
            child_pid: child,
            reason: Reason::Custom("crash".into()),
        },
    );
    let r = sup.handle(msg, &mut ctx);
    assert!(matches!(r, StepResult::Fail(Reason::Shutdown)));
}

/// Gate test: compensation rollback in reverse order.
#[test]
fn phase2_gate_compensation_rollback() {
    let mut log = CompensationLog::new();
    let pid = Pid::from_raw(1);

    // Record 3 compensatable steps
    for i in 1..=3 {
        log.record(
            pid,
            CompensationEntry {
                effect_id: EffectId::new(),
                spec: CompensationSpec {
                    undo_kind: EffectKind::Http,
                    undo_input: serde_json::json!({"step": i}),
                },
                completed_at_ms: i as u64 * 1000,
            },
        );
    }

    let rollbacks = log.rollback_all(pid);
    assert_eq!(rollbacks.len(), 3);
    // Reverse order: step 3, 2, 1
    assert_eq!(rollbacks[0].input["step"], 3);
    assert_eq!(rollbacks[1].input["step"], 2);
    assert_eq!(rollbacks[2].input["step"], 1);
}

/// Gate test: metrics are thread-safe and accurate.
#[test]
fn phase2_gate_metrics_concurrent() {
    use std::sync::Arc;

    let m = Arc::new(Metrics::new());
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                for _ in 0..250 {
                    m.inc("scheduler.ticks");
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(m.counter("scheduler.ticks"), 1000);
}

/// Gate test: idempotency store deduplicates effects.
#[test]
fn phase2_gate_idempotency() {
    let store = IdempotencyStore::open_in_memory().unwrap();
    let id = EffectId::new();

    // First check — miss
    assert!(store.check(&id).unwrap().is_none());

    // Record
    let result = EffectResult::success(id, serde_json::json!("response"));
    store.record(&id, &result).unwrap();

    // Second check — hit
    let cached = store.check(&id).unwrap().unwrap();
    assert_eq!(cached.status, EffectStatus::Succeeded);
    assert_eq!(cached.effect_id, id);
}
