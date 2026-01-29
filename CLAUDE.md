# Seesaw Architecture Guidelines

**Mental Model**: Seesaw machines are pure, replayable decision functions that turn facts into intent, and nothing else.

## What Seesaw Is

Seesaw is a **deterministic decision engine** for event-driven systems.

It sits between:
- **Facts** (events that already happened)
- **Intent** (commands describing what should happen next)

**It does not perform actions.** It decides what should be attempted next, given what has occurred so far.

Think: *"Given everything that has happened, what is the single correct next command?"*

### Key Properties

- **Is**: Coordination kernel for event → decision → command → IO cycles
- **Is**: A pure decision function with replayable state
- **Is Not**: Event sourcing, distributed actors, retry engine, saga orchestrator, workflow engine

## Core Primitives

### Event

A **fact**. Something that already happened. Immutable. Past-tense by convention.

```rust
#[derive(Debug, Clone)]
enum ScrapeEvent {
    SourceRequested { source_id: Uuid },
    SourceScraped { source_id: Uuid, data: String },
    NeedsExtracted { source_id: Uuid },
    AuthorizationDenied { source_id: Uuid },
}
// Event is auto-implemented for Clone + Send + Sync + 'static
```

Events may come from:
- User actions
- Background jobs
- External systems
- Failed attempts

### Command

An **intent**. A request to attempt an action. Not guaranteed to succeed.

```rust
#[derive(Debug, Clone)]
enum ScrapeCommand {
    ScrapeSource { source_id: Uuid },
    ExtractNeeds { source_id: Uuid },
    UpdateNeedStatus { need_id: Uuid, status: Status },
}
impl Command for ScrapeCommand {}
```

A command:
- May succeed → emits one or more events
- May fail → emits failure events
- May emit nothing (noop, dedupe, denied)

Execution modes: `Inline` (default), `Background`, `Scheduled { run_at }`.
Background/Scheduled commands need `job_spec()` and `serialize_to_json()`.

### Machine

A **pure decision function**. Holds derived state only. Maps `Event → Option<Command>`.

```rust
impl Machine for ScrapeMachine {
    type Event = ScrapeEvent;
    type Command = ScrapeCommand;

    fn decide(&mut self, event: &ScrapeEvent) -> Option<ScrapeCommand> {
        // Update internal state, return Option<Command>
        match event {
            ScrapeEvent::SourceRequested { source_id } => {
                self.pending.insert(*source_id);
                Some(ScrapeCommand::ScrapeSource { source_id: *source_id })
            }
            ScrapeEvent::SourceScraped { source_id, .. } => {
                self.pending.remove(source_id);
                Some(ScrapeCommand::ExtractNeeds { source_id: *source_id })
            }
            _ => None
        }
    }
}
```

The machine:
- **Never** performs IO
- **Never** blocks
- **Never** retries
- **Never** schedules
- **Never** looks outside itself

### Effect

Stateless command handlers. Execute IO, emit events.

```rust
#[async_trait]
impl Effect<ScrapeCommand, Deps> for ScrapeEffect {
    type Event = ScrapeEvent;

    async fn execute(&self, cmd: ScrapeCommand, ctx: EffectContext<Deps>) -> Result<ScrapeEvent> {
        match cmd {
            ScrapeCommand::ScrapeSource { source_id } => {
                let data = ctx.deps().scraper.scrape(source_id).await?;
                Ok(ScrapeEvent::SourceScraped { source_id, data })
            }
            _ => bail!("unhandled command")
        }
    }
}
```

EffectContext provides:
- `deps()` — shared dependencies
- `signal(event)` — fire-and-forget UI notifications
- `tool_context()` — context for interactive tool execution
- `outbox_correlation_id()` — for outbox writes
- `correlation_id()` — get correlation ID for this execution

### EventTap

Observe committed facts after effects. No decisions, no mutations, no emit.

```rust
#[async_trait]
impl EventTap<ScrapeEvent> for ScrapeTap {
    async fn on_event(&self, event: &ScrapeEvent, ctx: &TapContext) -> Result<()> {
        // Publish to NATS, webhooks, metrics, audit logging
        match event {
            ScrapeEvent::SourceScraped { source_id, .. } => {
                ctx.nats.publish("scrapes", source_id).await?;
            }
            _ => {}
        }
        Ok(())
    }
}
```

## Execution Model

This is how Seesaw actually runs:

1. **An event arrives**
   - The engine rehydrates the machine from its snapshot
   - Calls `decide(event)`

2. **The machine may emit a command**
   - Zero or one (by default)
   - Deterministic

3. **The command is executed elsewhere**
   - By a handler, worker, or adapter
   - Outside the machine

4. **The result comes back as an event**
   - Success → domain event
   - Failure → failure event
   - Authorization → authorization event

5. **Repeat**

**The machine advances only by consuming events.**

⚠️ **Machines never "wait"**. Waiting is modeled as "no command emitted until another event arrives."

## The Seven Hard Rules

**Do not violate these.** These are the Seesaw laws.

### Rule 1: No IO in machines

Ever.

❌ **No**:
- DB queries
- Network calls
- Time (`now()`)
- Randomness
- Config lookups
- Environment variables

If it isn't in the event or the machine's state, it doesn't exist.

### Rule 2: Machines are deterministic

Given:
- The same prior events
- The same new event

You **must** get the same command.

If replaying events could produce different commands → **broken**.

### Rule 3: State must be derived, not authoritative

Machine state:
- Is reconstructed from events
- May be thrown away at any time
- Must not be the source of truth

✅ **Good state**:
```rust
pending_scrapes: HashSet<Uuid>
in_flight_jobs: HashMap<Uuid, JobId>
dedupe_flags: HashSet<String>
workflow_stage: WorkflowStage
```

❌ **Bad state**:
```rust
organization_name: String     // Should come from DB
user_email: String            // Should come from DB
need_description: String      // Should come from DB
```

**If losing it would corrupt reality, it doesn't belong in the machine.**

### Rule 4: Commands are requests, not guarantees

A command:
- May be denied
- May fail
- May partially succeed
- May emit nothing

**Never assume success in the machine.** Success must be confirmed by a subsequent event.

❌ **Wrong**:
```rust
emit(CreatePost);
mark_as_created();  // NO! Haven't received PostCreated yet
```

✅ **Correct**:
```rust
// In decide():
ScrapeEvent::SourceRequested => Some(ScrapeCommand::ScrapeSource),
ScrapeEvent::SourceScraped => {
    self.completed.insert(source_id);  // Only after success event
    Some(NextCommand)
}
```

### Rule 5: Events close loops

Every long-running workflow must have:
- A success terminal event **or**
- A failure terminal event

Otherwise you get:
- Permanent "in-flight" state
- Silent deadlocks
- Ghost workflows

### Rule 6: Machines don't branch on time

❌ **No**:
- `now()`
- "if older than 5 minutes"
- timeouts

If time matters:
- Model it as an event (`JobTimedOut`)
- Or let an external scheduler emit events

### Rule 7: Machines don't coordinate each other

One machine:
- Decides only for its domain
- Emits commands only for its handlers

Cross-domain coordination happens via **events**, not direct calls.

## Role Matrix

| Role    | Decide? | Mutate? | Emit? |
| ------- | ------- | ------- | ----- |
| Machine | Yes     | No      | No    |
| Effect  | No      | Yes     | Yes   |
| Tap     | No      | No      | No    |

## What Seesaw Is Not

### ❌ Not a workflow engine
- No DAGs
- No BPMN
- No retries
- No timers

Workflows **emerge** from event sequences.

### ❌ Not CQRS (exactly)

It overlaps, but:
- Seesaw doesn't require read models
- It doesn't enforce command/event segregation at the system level

It's closer to **event-driven decision modeling**.

### ❌ Not a state machine in the classical sense

There are no explicit "states".

State is:
- Implicit
- Derived
- Reconstructable

You don't "enter" a state. You observe that certain events have occurred.

### ❌ Not business logic execution

The machine decides **what should happen**, not **how it happens**.

Execution lives elsewhere (in Effects).

## Common Failure Modes

### 1. Smuggling IO through events

❌ **Bad**:
```rust
Event::UserRequested { user_email: String }  // Email might change!
```

✅ **Better**:
```rust
Event::UserRequested { user_id: Uuid }  // Immutable reference
```

Events should reference facts, not embed volatile data.

### 2. Letting machines become mini-services

If your machine:
- Has dozens of fields
- Mirrors database rows
- Knows "too much"

You're leaking domain state into the decision layer.

### 3. Assuming commands succeed

Classic bug:
```rust
emit(CreatePost)
mark_as_created()  // NO! Only mark after PostCreated event
```

### 4. Encoding business rules in handlers

If rules live in handlers instead of machines:
- ❌ You lose replayability
- ❌ You lose auditability
- ❌ You lose testability

**Handlers execute. Machines decide.**

### 5. Using machines for orchestration across domains

If a machine is coordinating:
- Payments
- Emails
- Search indexing
- Analytics

It's probably too big. Split by domain.

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

## Outbox Pattern

For durable events (external side effects), write to outbox in same transaction:

```rust
let mut tx = ctx.deps().db.begin().await?;
let entity = Entity::create(&cmd, &mut tx).await?;
writer.write_event(&EntityCreated { id }, ctx.outbox_correlation_id()).await?;
tx.commit().await?;
```

## Architecture Flow

```
EventBus → Machine.decide() → Command → Dispatcher → Effect.execute() → Runtime → EventBus
                                                                                        ↓
                                                                                   EventTaps
```

## Design Principles Summary

1. **Effects are reactive, not decisional** — Execute IO, emit ONE event
2. **Events are facts, not commands** — `UserCreated`, not `CreateUser`
3. **State drives behavior in machines** — Track state, make decisions
4. **EffectContext is narrow** — Only `deps()` and `signal()`
5. **One Command = One Transaction** — Multiple atomic writes belong in one command
