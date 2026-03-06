//! v3 Gate Test: control plane — budget gate, provider gate, admission control.

use zeptovm::control::{AdmissionController, BudgetGate, Priority, ProviderGate};

// ---------------------------------------------------------------------------
// Test 1: budget precheck blocks exhausted agent
// ---------------------------------------------------------------------------

#[test]
fn gate_v3_budget_blocks_exhausted_agent() {
    let gate = BudgetGate::new(Some(1000), Some(5000));

    // Precheck with 500 tokens should pass (0 + 500 <= 1000)
    assert!(gate.precheck(500), "precheck(500) should pass on fresh gate");

    // Commit 800 tokens, 3000 cost
    gate.commit(800, 3000);

    // 800 + 200 = 1000, within limit
    assert!(
        gate.precheck(200),
        "precheck(200) should pass (800 + 200 = 1000)"
    );

    // 800 + 201 = 1001, exceeds token limit
    assert!(
        !gate.precheck(201),
        "precheck(201) should fail (800 + 201 > 1000)"
    );

    // Commit 200 tokens, 2000 cost -> total: 1000 tokens, 5000 cost
    gate.commit(200, 2000);

    // Cost is now at limit (5000 >= 5000), should fail even for 1 token
    assert!(
        !gate.precheck(1),
        "precheck(1) should fail (cost limit 5000 reached)"
    );

    // Verify counters
    assert_eq!(gate.tokens_used(), 1000);
    assert_eq!(gate.cost_used(), 5000);
}

// ---------------------------------------------------------------------------
// Test 2: provider gate enforces RPM
// ---------------------------------------------------------------------------

#[test]
fn gate_v3_provider_gate_enforces_rpm() {
    let gate = ProviderGate::new(3, 10_000, 100);

    // Acquire 3 permits (all should succeed)
    let p1 = gate.try_acquire(100).expect("1st acquire should succeed");
    let p2 = gate.try_acquire(100).expect("2nd acquire should succeed");
    let p3 = gate.try_acquire(100).expect("3rd acquire should succeed");

    assert_eq!(gate.in_flight(), 3);
    assert_eq!(gate.available_rpm(), 0);

    // 4th should fail (RPM exhausted)
    assert!(
        gate.try_acquire(100).is_none(),
        "4th acquire should fail (RPM exhausted)"
    );

    // Drop one permit — in-flight decreases but RPM is NOT refilled
    drop(p1);
    assert_eq!(gate.in_flight(), 2);
    assert_eq!(
        gate.available_rpm(),
        0,
        "RPM should still be 0 after drop (permits are consumed)"
    );

    // Try again — should still fail (RPM permits consumed for the minute)
    assert!(
        gate.try_acquire(100).is_none(),
        "acquire should still fail after drop (RPM not returned on drop)"
    );

    // Refill RPM (simulates new minute)
    gate.refill();
    assert_eq!(gate.available_rpm(), 3);

    // Now acquire should succeed
    let p4 = gate
        .try_acquire(100)
        .expect("acquire should succeed after refill");
    assert_eq!(gate.in_flight(), 3); // p2, p3 still held + p4

    drop(p2);
    drop(p3);
    drop(p4);
    assert_eq!(gate.in_flight(), 0);
}

// ---------------------------------------------------------------------------
// Test 3: admission priority ordering (weighted fair queuing)
// ---------------------------------------------------------------------------

#[test]
fn gate_v3_admission_priority_ordering() {
    let mut ac = AdmissionController::new();

    // Enqueue 10 of each priority
    for i in 0..10 {
        ac.enqueue(format!("high-{i}"), Priority::High);
    }
    for i in 0..10 {
        ac.enqueue(format!("normal-{i}"), Priority::Normal);
    }
    for i in 0..10 {
        ac.enqueue(format!("low-{i}"), Priority::Low);
    }

    assert_eq!(ac.len(), 30);

    // Dequeue 7 items (one full weighted cycle: 4 High, 2 Normal, 1 Low)
    let mut high_count = 0;
    let mut normal_count = 0;
    let mut low_count = 0;

    for _ in 0..7 {
        let req = ac.dequeue().expect("should have items to dequeue");
        match req.priority {
            Priority::High => high_count += 1,
            Priority::Normal => normal_count += 1,
            Priority::Low => low_count += 1,
        }
    }

    assert_eq!(high_count, 4, "expected 4 High in one cycle");
    assert_eq!(normal_count, 2, "expected 2 Normal in one cycle");
    assert_eq!(low_count, 1, "expected 1 Low in one cycle");

    // 23 items should remain
    assert_eq!(ac.len(), 23);
}

// ---------------------------------------------------------------------------
// Test 4: full pipeline simulation (budget + provider + admission)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gate_v3_full_pipeline_simulation() {
    // 1. Create all three gates
    let budget = BudgetGate::new(Some(1000), None);
    let provider = ProviderGate::new(10, 5000, 5);
    let mut admission = AdmissionController::new();

    // 2. Enqueue a request with Normal priority
    admission.enqueue("llm-request-1", Priority::Normal);
    assert_eq!(admission.len(), 1);

    // 3. Dequeue it
    let request = admission
        .dequeue()
        .expect("should dequeue the request");
    assert_eq!(request.data, "llm-request-1");
    assert_eq!(request.priority, Priority::Normal);
    assert!(admission.is_empty());

    // 4. Budget precheck (estimated 100 tokens)
    assert!(
        budget.precheck(100),
        "budget precheck should pass for 100 tokens"
    );

    // 5. Acquire provider permit (estimated 100 tokens)
    let permit = provider
        .try_acquire(100)
        .expect("provider acquire should succeed");
    assert_eq!(provider.in_flight(), 1);

    // 6. Simulate LLM call
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // 7. Commit actual usage (slightly less than estimated)
    budget.commit(95, 50);

    // 8. Drop provider permit (request complete)
    drop(permit);

    // 9. Verify final state
    assert_eq!(budget.tokens_used(), 95, "tokens_used should be 95");
    assert_eq!(budget.cost_used(), 50, "cost_used should be 50");
    assert_eq!(
        provider.in_flight(),
        0,
        "in_flight should be 0 after dropping permit"
    );
    assert_eq!(
        provider.remaining_tpm(),
        4900,
        "TPM should be 5000 - 100 = 4900"
    );

    // Budget should still allow more requests
    assert!(
        budget.precheck(905),
        "precheck(905) should pass (95 + 905 = 1000)"
    );
    assert!(
        !budget.precheck(906),
        "precheck(906) should fail (95 + 906 > 1000)"
    );
}
