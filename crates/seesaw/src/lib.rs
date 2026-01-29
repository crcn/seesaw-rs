//! # Seesaw
//!
//! A deterministic, event-driven coordination layer where machines decide,
//! effects execute, and transactions define authority.
//!
//! ## Core Concepts
//!
//! Seesaw separates **facts** from **intent**:
//! - [`Event`] = Facts (what happened)
//! - [`Command`] = Intent (requests for IO with transaction authority)
//!
//! The key principle: **One Command = One Effect = One Transaction**.
//! If multiple writes must be atomic, they belong in one command handled by one effect.
//!
//! ## Architecture
//!
//! ```text
//! Edge (API/WebSocket)
//!     │
//!     ▼ emit()
//! EventBus ──────────────────────────────────────┐
//!     │                                          │
//!     ▼ subscribe()                              │
//! Runtime.run() loop                             │
//!     │                                          │
//!     ├─► Machine A.decide() ─► Some(CmdA) ──┐   │
//!     │                                      │   │
//!     ├─► Machine B.decide() ─► None         │   │
//!     │                                      │   │
//!     └─► Machine C.decide() ─► Some(CmdC) ──┤   │
//!                                            │   │
//!                                            ▼   │
//!                                     Dispatcher │
//!                                            │   │
//!                     ┌──────────────────────┴───┤
//!                     │                          │
//!         Inline ─────┤                          │
//!                     ▼                          │
//!               Effect.execute()                 │
//!                     │                          │
//!                     └─► return Event ──────────┘
//! ```
//!
//! ## Key Invariants
//!
//! 1. **Events are facts** - Immutable, describe what happened, no IO
//! 2. **Commands are intent** - Request for IO with transaction authority
//! 3. **One Command = One Transaction** - Authority boundaries are explicit
//! 4. **Machines are pure** - No IO, no async, state is internal
//! 5. **Effects are stateless** - Commands carry all needed data
//! 6. **At-most-once delivery** - In-memory events, use status fields for durability
//!
//! ## Guarantees
//!
//! - **At-most-once delivery**: Slow receivers may miss events
//! - **In-memory only**: Events are not persisted by seesaw
//! - **No replay**: Lagged receivers get errors
//!
//! For durability, use:
//! - Entity status fields for workflow state
//! - Jobs for durable command execution
//! - Reapers for crash recovery
//!
//! ## Example
//!
//! ```ignore
//! use seesaw::{Event, Command, Machine, Effect, EffectContext, EventBus, RuntimeBuilder};
//! use std::collections::HashSet;
//! use uuid::Uuid;
//!
//! // 1. Define events (facts)
//! #[derive(Debug, Clone)]
//! enum BakeEvent {
//!     Requested { deck_id: Uuid, recipe_id: Uuid },
//!     LoafReady { loaf_id: Uuid },
//!     GenerationComplete { loaf_id: Uuid },
//! }
//! impl Event for BakeEvent {}
//!
//! // 2. Define commands (intent)
//! #[derive(Debug, Clone)]
//! enum BakeCommand {
//!     SetupLoaf { deck_id: Uuid, recipe_id: Uuid },
//!     GenerateCards { loaf_id: Uuid },
//! }
//! impl Command for BakeCommand {}
//!
//! // 3. Define machine (state inside, pure decisions)
//! struct BakeMachine {
//!     pending: HashSet<Uuid>,
//! }
//!
//! impl Machine for BakeMachine {
//!     type Event = BakeEvent;
//!     type Command = BakeCommand;
//!
//!     fn decide(&mut self, event: &BakeEvent) -> Option<BakeCommand> {
//!         match event {
//!             BakeEvent::Requested { deck_id, recipe_id } => {
//!                 self.pending.insert(*deck_id);
//!                 Some(BakeCommand::SetupLoaf {
//!                     deck_id: *deck_id,
//!                     recipe_id: *recipe_id,
//!                 })
//!             }
//!             BakeEvent::LoafReady { loaf_id } => {
//!                 Some(BakeCommand::GenerateCards { loaf_id: *loaf_id })
//!             }
//!             _ => None,
//!         }
//!     }
//! }
//!
//! // 4. Define effects (IO + return events)
//! struct SetupEffect;
//!
//! #[async_trait::async_trait]
//! impl Effect<BakeCommand, MyDeps> for SetupEffect {
//!     type Event = BakeEvent;
//!
//!     async fn execute(&self, cmd: BakeCommand, ctx: EffectContext<MyDeps>) -> anyhow::Result<BakeEvent> {
//!         if let BakeCommand::SetupLoaf { deck_id, recipe_id } = cmd {
//!             let loaf_id = ctx.deps().db.transaction(|tx| async {
//!                 // Create loaf in transaction
//!             }).await?;
//!
//!             Ok(BakeEvent::LoafReady { loaf_id })
//!         } else {
//!             unreachable!()
//!         }
//!     }
//! }
//!
//! // 5. Wire together and run
//! let (runtime, bus) = RuntimeBuilder::new(my_deps)
//!     .with_machine(BakeMachine { pending: HashSet::new() })
//!     .with_effect::<BakeCommand, _>(SetupEffect)
//!     .build();
//!
//! tokio::spawn(runtime.run());
//!
//! // 6. Emit events to trigger workflows
//! bus.emit(BakeEvent::Requested { deck_id, recipe_id });
//! ```
//!
//! ## What This Is Not
//!
//! Seesaw is **not**:
//! - Full event sourcing
//! - A saga engine
//! - An actor framework
//! - A job system replacement
//!
//! Seesaw **is**:
//! > A deterministic, event-driven coordination layer where machines decide,
//! > effects execute, and transactions define authority.

// Core modules
mod bus;
mod command_macro;
mod core;
mod dispatch;
mod effect_impl;
mod engine;
mod error;
mod machine;
mod request;
mod runtime;
mod tap;

// Job interfaces (policy-light)
pub mod job;

// Debug auditing for event visibility
#[cfg(debug_assertions)]
pub mod audit;

// Testing utilities are in the separate seesaw-testing crate

// Code smell tests (test-only)
#[cfg(test)]
mod codesmell_tests;

// Stress tests (test-only)
#[cfg(test)]
mod stress_tests;

// Serde auto-serialization tests (test-only)
#[cfg(test)]
mod serde_auto_tests;

// Re-export core traits
pub use crate::core::{
    AnyCommand, Command, CorrelationId, EnvelopeMatch, Event, EventEnvelope, EventRole,
    ExecutionMode, JobSpec, MatchChain, SerializableCommand,
};

// Re-export request helpers (syntactic sugar over event bus)
pub use request::{dispatch_request, dispatch_request_timeout, DEFAULT_REQUEST_TIMEOUT};

// Re-export error types
pub use crate::error::{
    BatchOutcome, Categorizable, CommandFailed, SafeErrorCategory, SeesawError,
};

// Re-export machine types
pub use machine::Machine;

// Re-export effect types
pub use effect_impl::{Effect, EffectContext, ToolContext};

// Re-export tap types (event observation)
pub use tap::{EventTap, TapContext};

// Re-export bus types
pub use bus::EventBus;

// Re-export dispatcher types
pub use dispatch::{Dispatcher, JobQueue, NoOpJobQueue};

// Re-export job types (policy-light interfaces)
pub use job::{ClaimedJob, CommandRegistry, DeserializationError, FailureKind, JobStore};

// Re-export runtime types
pub use runtime::{Runtime, RuntimeBuilder};

// Re-export engine types (primary entry point)
pub use engine::{Engine, EngineBuilder, EngineHandle, InflightBatch, InflightTracker};

// Re-export commonly used external types
pub use async_trait::async_trait;
