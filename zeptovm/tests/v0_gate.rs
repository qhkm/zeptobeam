//! v0 Gate Test: spawn 100 processes, deliver 10k messages, clean shutdown.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use zeptovm::behavior::Behavior;
use zeptovm::error::{Action, Message, Reason};
use zeptovm::registry::ProcessRegistry;

struct CounterBehavior {
    count: Arc<AtomicU32>,
}

#[async_trait]
impl Behavior for CounterBehavior {
    async fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn handle(&mut self, _msg: Message) -> Action {
        self.count.fetch_add(1, Ordering::Relaxed);
        Action::Continue
    }

    async fn terminate(&mut self, _reason: &Reason) {}
}

#[tokio::test]
async fn gate_v0_spawn_100_deliver_10k() {
    let registry = ProcessRegistry::new();
    let total_count = Arc::new(AtomicU32::new(0));

    // Spawn 100 processes
    let mut pids = Vec::new();
    let mut joins = Vec::new();
    for _ in 0..100 {
        let behavior = CounterBehavior {
            count: total_count.clone(),
        };
        let (pid, join) = registry.spawn(behavior, 256, None);
        pids.push(pid);
        joins.push(join);
    }

    assert_eq!(registry.count(), 100);

    // Deliver 10k messages (100 per process)
    for pid in &pids {
        for i in 0..100 {
            registry
                .send(pid, Message::text(format!("msg-{i}")))
                .await
                .unwrap();
        }
    }

    // Wait for all messages to be processed (poll with timeout)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let current = total_count.load(Ordering::Relaxed);
        if current >= 10_000 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for 10000 messages, only {current} processed"
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Kill all processes
    for pid in &pids {
        registry.kill(pid);
    }

    // Wait for all to exit
    for join in joins {
        let exit = join.await.unwrap();
        assert!(matches!(exit.reason, Reason::Kill));
    }

    // Verify all 10k messages were processed
    let total = total_count.load(Ordering::Relaxed);
    assert_eq!(
        total, 10_000,
        "expected 10000 messages processed, got {total}"
    );
}

#[tokio::test]
async fn gate_v0_clean_shutdown_on_drop() {
    let registry = ProcessRegistry::new();
    let count = Arc::new(AtomicU32::new(0));

    let behavior = CounterBehavior {
        count: count.clone(),
    };
    let (_pid, join) = registry.spawn(behavior, 16, None);

    // Drop registry (drops all handles) — process should exit
    drop(registry);

    let _exit = join.await.unwrap();
    // Process exits when all senders are dropped
}
