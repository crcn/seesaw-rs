# seesaw-testing

Testing utilities for the Seesaw framework.

## Features

- `assert_workflow!` macro for testing state machine transitions
- Fluent `WorkflowTest` builder API
- `EventLatch` for testing fan-out scenarios
- `SpyJobQueue` for background job assertions
- `MockJobStore` for job lifecycle testing

## Installation

```toml
[dev-dependencies]
seesaw-testing = "0.1"
```

## Usage

### Testing Machine Transitions with `assert_workflow!`

```rust
use seesaw_testing::assert_workflow;

#[test]
fn test_order_workflow() {
    let mut machine = OrderMachine::new();

    assert_workflow!(
        machine,
        OrderEvent::Placed { order_id } => Some(OrderCommand::Ship { order_id }),
        OrderEvent::Shipped { order_id } => Some(OrderCommand::NotifyCustomer { order_id, .. }),
        OrderEvent::Delivered { order_id } => None,
    );
}
```

### Fluent Workflow Testing

```rust
use seesaw_testing::MachineTestExt;

#[test]
fn test_notification_workflow() {
    NotificationMachine::new()
        .test()
        .given(NotificationEvent::Created { id, user_id })
        .expect_some()
        .expect_command(|cmd| matches!(cmd, Some(NotificationCommand::Enrich { .. })))
        .then(NotificationEvent::Enriched { id, data })
        .expect_command(|cmd| matches!(cmd, Some(NotificationCommand::Deliver { .. })))
        .then(NotificationEvent::Delivered { id })
        .expect_none()
        .assert_state(|m| m.delivered_count == 1);
}
```

### Testing Fan-Out with `EventLatch`

```rust
use seesaw_testing::shared_latch;

#[tokio::test]
async fn test_notification_fan_out() {
    let latch = shared_latch(3);  // Expect 3 events

    bus.tap::<NotificationEvent>({
        let latch = latch.clone();
        move |_| latch.dec()
    });

    engine.emit(trigger_event);

    // Wait for all 3 events (no sleep!)
    latch.await_zero().await;
}
```

### Testing Background Jobs with `SpyJobQueue`

```rust
use seesaw_testing::SpyJobQueue;

#[tokio::test]
async fn test_background_job_enqueued() {
    let spy = SpyJobQueue::new();
    let dispatcher = Dispatcher::with_job_queue(deps, bus, Arc::new(spy.clone()));

    engine.emit(MyEvent::Trigger);

    // Assert the job was enqueued
    assert!(spy.was_enqueued("email:send"));
    spy.assert_enqueued_with_key("email:send", "email:123:welcome");
    spy.assert_job_count("email:send", 1);
}
```

### Testing Job Lifecycle with `MockJobStore`

```rust
use seesaw_testing::{MockJobStore, JobStatus};

#[tokio::test]
async fn test_job_claim_and_complete() {
    let store = MockJobStore::new();
    let job_id = store.seed_job("email:send", json!({"user_id": "123"}), 1);

    let jobs = store.claim_ready("worker-1", 10).await?;
    assert_eq!(jobs.len(), 1);

    store.mark_succeeded(job_id).await?;
    assert!(store.job_succeeded(job_id));
}
```

## License

MIT
