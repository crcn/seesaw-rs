# Seesaw

An event-driven architecture framework that separates **facts** (Events) from **intent** (Commands).

Named after the playground equipment that balances back and forth — representing the balance between events flowing in and commands flowing out.

## Workspace Structure

This repository is organized as a Cargo workspace:

- **[seesaw-core](./crates/seesaw)** - Core event-driven coordination framework
- **[seesaw-job-postgres](./crates/seesaw-job-postgres)** - PostgreSQL job queue implementation
- **[seesaw-outbox](./crates/seesaw-outbox)** - Transactional outbox pattern for durable events
- **[seesaw-persistence](./crates/seesaw-persistence)** - Machine state persistence for crash recovery
- **[seesaw-testing](./crates/seesaw-testing)** - Testing utilities for state machine workflows

## Core Principle

**One Command = One Transaction.** If multiple writes must be atomic, they belong in one command handled by one effect.

## What Seesaw Is / Is Not

Seesaw **is**:

> A deterministic, event-driven coordination layer where machines decide,
> effects execute, and transactions define authority.

Seesaw is **not**:

- Full event sourcing
- A saga engine
- An actor framework
- A job system replacement

## Features

- **Event/Command Separation**: Events are facts, Commands are intent
- **State Machines**: Machines own state and make pure decisions
- **Effect Handlers**: Stateless IO execution with narrow context
- **Event Taps**: Fire-and-forget observation for publishing, metrics, and logging
- **Type-Erased Bus**: Broadcast events across heterogeneous machines
- **Execution Modes**: Inline, Background, and Scheduled command execution
- **Correlation Tracking**: `emit_and_await` for waiting on inline work completion
- **Request/Response Pattern**: `dispatch_request` for edge code that needs responses
- **Job Queue Integration**: Plug in your own durable job system
- **Durable Event Outbox**: Opt-in same-transaction event persistence for at-least-once delivery
- **Testing Utilities**: Ergonomic test helpers for state machine workflows

## Architecture

```
EventBus → Machine.decide() → Command → Dispatcher → Effect.execute() → Event → Runtime emits → EventBus
                                                                                       ↓
                                                                                  EventTaps
```

## Quick Start

```rust
use seesaw_core::{
    Command, Machine, Effect, EffectContext,
    EngineBuilder,
};
use anyhow::Result;
use std::collections::HashSet;
use uuid::Uuid;

// Events are facts - what happened
// Note: Event trait is auto-implemented for Clone + Send + Sync + 'static
#[derive(Debug, Clone)]
enum OrderEvent {
    Placed { order_id: Uuid },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

// Commands are intent - what we want to do
#[derive(Debug, Clone)]
enum OrderCommand {
    Ship { order_id: Uuid },
    NotifyCustomer { order_id: Uuid, message: String },
}
impl Command for OrderCommand {}

// Machines make decisions based on events
struct OrderMachine {
    pending: HashSet<Uuid>,
}

impl Machine for OrderMachine {
    type Event = OrderEvent;
    type Command = OrderCommand;

    fn decide(&mut self, event: &OrderEvent) -> Option<OrderCommand> {
        match event {
            OrderEvent::Placed { order_id } => {
                self.pending.insert(*order_id);
                Some(OrderCommand::Ship { order_id: *order_id })
            }
            OrderEvent::Shipped { order_id } => {
                self.pending.remove(order_id);
                Some(OrderCommand::NotifyCustomer {
                    order_id: *order_id,
                    message: "Your order has shipped!".into(),
                })
            }
            _ => None,
        }
    }
}

// Effects execute IO and return events (the Runtime emits them)
struct OrderEffect;

#[async_trait]
impl Effect<OrderCommand, MyDeps> for OrderEffect {
    type Event = OrderEvent;

    async fn execute(&self, cmd: OrderCommand, ctx: EffectContext<MyDeps>) -> Result<OrderEvent> {
        match cmd {
            OrderCommand::Ship { order_id } => {
                ctx.deps().shipping_api.ship(order_id).await?;
                Ok(OrderEvent::Shipped { order_id })
            }
            OrderCommand::NotifyCustomer { order_id, message } => {
                ctx.deps().email_service.send(order_id, &message).await?;
                Ok(OrderEvent::Delivered { order_id })
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let engine = EngineBuilder::new(MyDeps::new())
        .with_machine(OrderMachine { pending: HashSet::new() })
        .with_effect::<OrderCommand, _>(OrderEffect)
        .build();

    // Start the engine (runs in background)
    let handle = engine.start();

    // Fire-and-forget
    handle.emit(OrderEvent::Placed { order_id: Uuid::new_v4() });

    // Or wait for all inline work to complete
    handle.emit_and_await(OrderEvent::Placed { order_id: Uuid::new_v4() }).await?;

    Ok(())
}
```

## Core Concepts

### Events

Events are immutable facts describing what happened. The `Event` trait is **auto-implemented** for any type that is `Clone + Send + Sync + 'static`.

```rust
#[derive(Debug, Clone)]
enum UserEvent {
    // Input - requests from edges
    CreateRequested { email: String },
    // Fact - what actually happened
    Created { user_id: Uuid, email: String },
    Verified { user_id: Uuid },
    Deleted { user_id: Uuid },
}
// Event is automatically implemented!
```

**Event Roles:**

| Role   | Description                           | Example           |
| ------ | ------------------------------------- | ----------------- |
| Input  | Edge-originated requests              | `CreateRequested` |
| Fact   | Effect-produced ground truth          | `Created`         |
| Signal | Ephemeral UI updates (via `signal()`) | Typing indicators |

### Commands

Commands are requests for IO with transaction authority. Each command maps to exactly one effect execution.

```rust
#[derive(Debug, Clone)]
enum UserCommand {
    Create { email: String, name: String },
    SendVerificationEmail { user_id: Uuid },
    Delete { user_id: Uuid },
}

impl Command for UserCommand {}
```

### Execution Modes

Commands specify when they should execute:

```rust
impl Command for MyCommand {
    fn execution_mode(&self) -> ExecutionMode {
        match self {
            // Execute immediately (default)
            Self::FastOperation { .. } => ExecutionMode::Inline,

            // Queue for background execution
            Self::SlowOperation { .. } => ExecutionMode::Background,

            // Execute at a specific time
            Self::ScheduledTask { run_at, .. } => ExecutionMode::Scheduled { run_at: *run_at },
        }
    }
}
```

### Machines

Machines are pure state machines that interpret events and decide on commands. State lives inside the machine.

```rust
struct RegistrationMachine {
    pending_verifications: HashSet<Uuid>,
}

impl Machine for RegistrationMachine {
    type Event = UserEvent;
    type Command = UserCommand;

    fn decide(&mut self, event: &UserEvent) -> Option<UserCommand> {
        match event {
            UserEvent::Created { user_id, .. } => {
                self.pending_verifications.insert(*user_id);
                Some(UserCommand::SendVerificationEmail { user_id: *user_id })
            }
            UserEvent::Verified { user_id } => {
                self.pending_verifications.remove(user_id);
                None
            }
            _ => None,
        }
    }
}
```

Key properties:

- **State is internal**: Each machine owns its state via `&mut self`
- **Pure decisions**: No IO, no async, just state transitions
- **One event → one command**: Returns `Option<Command>`, not `Vec<Command>`
- **Fan-out via multiple machines**: Same event can be observed by many machines

### Effects

Effects are stateless command handlers that execute IO and return events.

```rust
struct CreateUserEffect;

#[async_trait]
impl Effect<UserCommand, MyDeps> for CreateUserEffect {
    type Event = UserEvent;

    async fn execute(&self, cmd: UserCommand, ctx: EffectContext<MyDeps>) -> Result<UserEvent> {
        match cmd {
            UserCommand::Create { email, name } => {
                // One transaction
                let user = ctx.deps().db.transaction(|tx| async {
                    let user = User::create(&email, &name, tx).await?;
                    UserProfile::create(user.id, tx).await?;
                    Ok(user)
                }).await?;

                // Return fact
                Ok(UserEvent::Created { user_id: user.id, email })
            }
            // ... other commands
        }
    }
}
```

Key properties:

- **Stateless**: Commands carry all needed data
- **Return events**: Effects return events; the Runtime emits them
- **Narrow context**: Only `deps()`, `signal()`, and `tool_context()` available
- **One Command = One Transaction**: Authority boundaries
- **Batch support**: Override `execute_batch` for optimized bulk operations

### EffectContext

`EffectContext` provides a narrow API to effects:

```rust
// Access shared dependencies (database, APIs, config)
ctx.deps()

// Fire-and-forget signal for UI observability (typing indicators, progress)
ctx.signal(MySignalEvent::Progress { percent: 50 });

// Get correlation ID for outbox writes
ctx.outbox_correlation_id()

// Get context for interactive tools (dispatch_request, etc.)
// Returns ToolContext { deps, bus } for agent tools
let tool_ctx = ctx.tool_context();

// Get correlation ID directly
ctx.correlation_id()
```

### Event Taps

Taps observe **committed facts** after effects complete. They run fire-and-forget and cannot emit new events.

```rust
use seesaw_core::{EventTap, TapContext};

pub struct NatsPublishTap {
    client: async_nats::Client,
}

#[async_trait]
impl EventTap<EntryEvent> for NatsPublishTap {
    async fn on_event(&self, event: &EntryEvent, ctx: &TapContext) -> Result<()> {
        let payload = serde_json::to_vec(event)?;
        self.client.publish("entry.events", payload.into()).await?;
        Ok(())
    }
}
```

Use taps for:

- Publishing to NATS/Kafka
- Sending webhooks
- Recording metrics
- Audit logging

**Roles:**

| Role    | Decide? | Mutate? | Emit? |
| ------- | ------- | ------- | ----- |
| Machine | Yes     | No      | No    |
| Effect  | No      | Yes     | Yes   |
| Tap     | No      | No      | No    |

## Engine Usage

The `EngineBuilder` is the primary way to wire up seesaw:

```rust
let engine = EngineBuilder::new(deps)
    .with_machine(OrderMachine::new())
    .with_machine(AuditMachine::new())
    .with_effect::<OrderCommand, _>(OrderEffect)
    .with_effect::<AuditCommand, _>(AuditEffect)
    .with_event_tap::<OrderEvent, _>(NatsPublishTap::new(client))
    .build();

let handle = engine.start();

// Fire-and-forget
handle.emit(OrderEvent::Placed { order_id });

// Wait for all inline work to complete
handle.emit_and_await(OrderEvent::Placed { order_id }).await?;
```

Other builder methods:

- `.with_bus(bus)` — Use an existing EventBus
- `.with_inflight(tracker)` — Use an existing InflightTracker
- `.with_arc(deps)` — Use Arc-wrapped dependencies
- `.with_job_queue(queue)` — Enable background command execution

## Request/Response Pattern

For edge code that needs a response, use `dispatch_request`:

```rust
use seesaw_core::{dispatch_request, EnvelopeMatch};

let entry = dispatch_request(
    EntryRequestEvent::Create { ... },
    &bus,
    |m| m.try_match(|e: &EntryEvent| match e {
        EntryEvent::Created { entry } => Some(Ok(entry.clone())),
        _ => None,
    })
    .or_try(|denied: &AuthDenied| Some(Err(anyhow!("denied"))))
    .result()
).await?;
```

This does NOT guarantee a response exists—it emits an event and waits until a correlated event matches the extractor, or times out (default: 30 seconds).

## Background Jobs

Commands with `Background`/`Scheduled` execution modes need:

- `fn execution_mode() -> ExecutionMode`
- `fn job_spec() -> Option<JobSpec>`
- `auto_serialize!()` macro (one-liner for serde serialization)

```rust
use seesaw_core::{Command, ExecutionMode, JobSpec, auto_serialize};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendEmailCommand {
    user_id: Uuid,
    template: String,
}

impl Command for SendEmailCommand {
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Background
    }

    fn job_spec(&self) -> Option<JobSpec> {
        Some(JobSpec::new("email:send")
            .with_idempotency_key(format!("email:{}:{}", self.user_id, self.template)))
    }

    // Just one line - serde derive does the rest!
    auto_serialize!();
}
```

Wire up via `.with_job_queue(queue)` on EngineBuilder.

## Scheduled Commands

Schedule commands to execute at a specific time:

```rust
use chrono::{DateTime, Utc, Duration};

#[derive(Debug, Clone)]
struct ReminderCommand {
    user_id: Uuid,
    message: String,
    remind_at: DateTime<Utc>,
}

impl Command for ReminderCommand {
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Scheduled { run_at: self.remind_at }
    }

    fn job_spec(&self) -> Option<JobSpec> {
        Some(JobSpec::new("reminder:send"))
    }
}

// In a machine
fn decide(&mut self, event: &TaskEvent) -> Option<ReminderCommand> {
    match event {
        TaskEvent::Created { task_id, due_at } => {
            // Schedule reminder 1 hour before due
            Some(ReminderCommand {
                user_id: self.owner_id,
                message: format!("Task {} is due soon!", task_id),
                remind_at: *due_at - Duration::hours(1),
            })
        }
        _ => None,
    }
}
```

## Durable Event Outbox

For events that must survive crashes, use the transactional outbox pattern:

```rust
use seesaw_outbox::{OutboxEvent, OutboxWriter, CorrelationId};

// 1. Mark event for outbox persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderPlaced {
    pub order_id: Uuid,
    pub customer_id: Uuid,
}

impl OutboxEvent for OrderPlaced {
    fn event_type() -> &'static str { "order.placed.v1" }
}

// 2. Write to outbox in same transaction as business data
async fn execute(&self, cmd: CreateOrderCmd, ctx: EffectContext<Deps>) -> Result<OrderEvent> {
    let mut tx = ctx.deps().db.begin().await?;

    // Business write
    let order = Order::create(&cmd, &mut tx).await?;

    // Outbox write (same transaction) - survives crashes
    let mut writer = PgOutboxWriter::new(&mut tx);
    writer.write_event(
        &OrderPlaced { order_id: order.id, customer_id: cmd.customer_id },
        ctx.outbox_correlation_id(),
    ).await?;

    tx.commit().await?;
    Ok(OrderEvent::Created { order })
}
```

**Key differences from in-memory events:**

| Aspect      | Effect return         | Outbox                |
| ----------- | --------------------- | --------------------- |
| Durability  | Lost on crash         | Survives crash        |
| Delivery    | At-most-once          | At-least-once         |
| Performance | Immediate             | Poll-based latency    |
| Use case    | Internal coordination | External side effects |

## Design Philosophy

1. **Events are Facts, Commands are Intent**: Clear separation between what happened and what should happen
2. **One Command = One Transaction**: Authority boundaries prevent transaction sprawl
3. **Machines are Pure**: No IO, predictable, testable
4. **Effects are Narrow**: Only deps, no state accumulation
5. **Fan-out via Machines**: Multiple machines can observe the same event independently
6. **Taps Observe, Don't Act**: Fire-and-forget for external publishing

## Guarantees

- **At-most-once delivery**: Slow receivers may miss events
- **In-memory only**: Events are not persisted by seesaw
- **No replay**: Lagged receivers get errors

For durability, use:

- Entity status fields for workflow state
- Jobs for durable command execution
- Reapers for crash recovery

## Testing

Test machines by calling `decide` directly:

```rust
#[test]
fn test_machine_state_transitions() {
    let mut machine = OrderMachine::new();

    let cmd = machine.decide(&OrderEvent::Placed { order_id });
    assert!(cmd.is_some());
    assert!(machine.pending.contains(&order_id));
}
```

### Testing Utilities

The `seesaw-testing` crate provides ergonomic test helpers:

```toml
[dev-dependencies]
seesaw-core = { version = "0.1" }
seesaw-testing = { version = "0.1" }
```

**Using `assert_workflow!` macro:**

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

**Using fluent builder:**

```rust
use seesaw_testing::{WorkflowTest, MachineTestExt};

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

**Using `EventLatch` for fan-out tests:**

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

**Using `SpyJobQueue` for background job assertions:**

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

**Using `MockJobStore` for job lifecycle tests:**

```rust
use seesaw_testing::{MockJobStore, JobStatus};
use seesaw_core::job::JobStore;

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

**Test effects with mock dependencies:**

```rust
#[tokio::test]
async fn test_effect_returns_event() {
    let deps = Arc::new(MockDeps::new());
    let bus = EventBus::new();

    let ctx = EffectContext::new(deps, bus);
    let effect = CreateUserEffect;

    let event = effect.execute(
        UserCommand::Create { email: "test@example.com".into(), name: "Test".into() },
        ctx,
    ).await.unwrap();

    assert!(matches!(event, UserEvent::Created { .. }));
}
```

## License

MIT
