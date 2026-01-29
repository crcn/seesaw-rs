# Seesaw Architecture Guidelines

Event-driven framework separating **facts** (Events) from **intent** (Commands).

## Core Principle

**One Command = One Transaction.** Multiple atomic writes belong in one command handled by one effect.

## What Seesaw Is / Is Not

- **Is**: Coordination kernel for event → decision → command → IO cycles
- **Is Not**: Event sourcing, distributed actors, retry engine, saga orchestrator

## Architecture

```
EventBus → Machine.decide() → Command → Dispatcher → Effect.execute() → ctx.emit() → EventBus
                                                                              ↓
                                                                         EventTaps
```

## Core Traits

### Event

Immutable facts. Auto-implemented for `Clone + Send + Sync + 'static`.

```rust
#[derive(Debug, Clone)]
enum UserEvent {
    Created { user_id: Uuid },
}
// Event is automatically implemented
```

### Command

Requests for IO. Requires explicit `impl Command`.

```rust
#[derive(Debug, Clone)]
enum UserCommand {
    Create { email: String },
}
impl Command for UserCommand {}
```

Execution modes: `Inline` (default), `Background`, `Scheduled { run_at }`.
Background/Scheduled commands need `job_spec()` and `serialize_to_json()`.

### Machine

Pure state machines that decide on commands. No IO.

```rust
impl Machine for MyMachine {
    type Event = MyEvent;
    type Command = MyCommand;

    fn decide(&mut self, event: &MyEvent) -> Option<MyCommand> {
        // Update internal state, return Option<Command>
    }
}
```

### Effect

Stateless command handlers. Execute IO, emit events.

```rust
#[async_trait]
impl Effect<MyCommand, Deps> for MyEffect {
    async fn execute(&self, cmd: MyCommand, ctx: EffectContext<Deps>) -> Result<()> {
        // Do IO via ctx.deps()
        ctx.emit(MyEvent::Done);
        Ok(())
    }
}
```

EffectContext provides:

- `deps()` — shared dependencies
- `emit(event)` — emit event (correlation propagated automatically)
- `bus()` — migration helper
- `deps_arc()` — migration helper
- `outbox_correlation_id()` — for outbox writes

### EventTap

Observe committed facts after effects. No decisions, no mutations, no emit.

```rust
#[async_trait]
impl EventTap<MyEvent> for MyTap {
    async fn on_event(&self, event: &MyEvent, ctx: &TapContext) -> Result<()> {
        // Publish to NATS, webhooks, metrics, audit logging
    }
}
```

## Roles

| Role    | Decide? | Mutate? | Emit? |
| ------- | ------- | ------- | ----- |
| Machine | Yes     | No      | No    |
| Effect  | No      | Yes     | Yes   |
| Tap     | No      | No      | No    |

## Engine Usage

```rust
let engine = EngineBuilder::new(deps)
    .with_machine(MyMachine::new())
    .with_effect::<MyCommand, _>(MyEffect)
    .with_event_tap::<MyEvent, _>(MyTap)
    .build();

let handle = engine.start();
handle.emit(MyEvent::Started);                    // Fire-and-forget
handle.emit_and_await(MyEvent::Started).await?;   // Wait for completion
```

Other builder methods: `.with_bus()`, `.with_inflight()`, `.with_arc(deps)`

## Request/Response Pattern

For edges that need a response:

```rust
use seesaw::{dispatch_request, EnvelopeMatch};

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

## Design Principles

1. **Effects are reactive, not decisional** — Execute IO, emit ONE event
2. **Events are facts, not commands** — `UserCreated`, not `CreateUser`
3. **State drives behavior in machines** — Track state, make decisions
4. **EffectContext is narrow** — Only `deps()` and `emit()`

## Structural Authorization Pattern

Wrap commands in `Authorize<C>` to enforce auth in the type system:

```
RequestEvent → Machine → Authorize<Cmd> → AuthEffect → Authorized<Cmd> → Forwarder → Cmd → Effect
```

## Workflow Patterns

| Pattern           | Use For                        | State Location    |
| ----------------- | ------------------------------ | ----------------- |
| Enriched Pipeline | Notifications, audit, webhooks | Entity timestamps |
| State Machine     | AI agents, wizards, sessions   | Machine internal  |

## Background Jobs

Commands with `Background`/`Scheduled` need:

- `fn execution_mode() -> ExecutionMode`
- `fn job_spec() -> Option<JobSpec>`
- `fn serialize_to_json() -> Option<serde_json::Value>`

Wire up via `.with_job_queue(queue)` on EngineBuilder or Dispatcher.

## Outbox

For durable events (external side effects), write to outbox in same transaction:

```rust
let mut tx = ctx.deps().db.begin().await?;
let entity = Entity::create(&cmd, &mut tx).await?;
writer.write_event(&EntityCreated { id }, ctx.outbox_correlation_id()).await?;
tx.commit().await?;
```
