//! Effect trait and context for command execution.
//!
//! Effects are command handlers that execute IO and return events.
//! They are **stateless** - commands carry all needed data.
//!
//! # Key Properties
//!
//! - **One Command = One Effect = One Event**
//! - **Stateless**: No access to machine state, commands carry data
//! - **Return events**: Effects return events describing outcomes (Runtime emits)
//! - **Narrow context**: Only `deps()` and `signal()` available

use std::any::Any;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::bus::EventBus;
use crate::core::{Command, CorrelationId, Event, EventEnvelope};
use crate::engine::InflightTracker;
use crate::error::SeesawError;

/// Context passed to effect handlers.
///
/// # Immutability Invariant (CRITICAL)
///
/// `EffectContext` is **immutable and cheap to clone**.
/// Clones MUST be semantically identical.
///
/// This invariant is critical for batch operations where `ctx.clone()` is used:
/// ```ignore
/// for command in commands {
///     events.push(self.execute(command, ctx.clone()).await?);
/// }
/// ```
///
/// **DO NOT add to EffectContext:**
/// - Counters or timers
/// - "First command" flags
/// - Retry metadata
/// - Any per-command mutable state
///
/// Such additions would silently break batch semantics.
///
/// # Narrow API
///
/// `EffectContext` is intentionally narrow to prevent effects from
/// accumulating too much power. Effects should only:
/// 1. Access dependencies via `deps()`
/// 2. Return events (Runtime emits)
/// 3. Optionally signal UI progress via `signal()`
///
/// Effects do NOT have access to:
/// - The raw EventBus (removed)
/// - Machine state
/// - Spawning their own emitters
///
/// # Correlation Propagation
///
/// When an effect returns an event, the correlation ID from the original
/// command is automatically propagated. This enables `emit_and_await`
/// to track all cascading work.
///
/// # Example
///
/// ```ignore
/// async fn execute(&self, cmd: CreateUserCommand, ctx: EffectContext<MyDeps>) -> Result<UserEvent> {
///     // Access dependencies
///     let user = ctx.deps().db.transaction(|tx| async {
///         User::create(cmd.email, cmd.name, tx).await
///     }).await?;
///
///     // Return event (Runtime emits with correlation ID)
///     Ok(UserEvent::Created { user_id: user.id })
/// }
/// ```
pub struct EffectContext<D> {
    deps: Arc<D>,
    bus: EventBus,
    /// Correlation ID to propagate on emit (None for fire-and-forget)
    cid: Option<CorrelationId>,
    /// Inflight tracker for increment-before-emit
    inflight: Option<Arc<InflightTracker>>,
}

impl<D> EffectContext<D> {
    /// Create a new effect context without correlation (fire-and-forget).
    ///
    /// Use this for edge functions and other contexts outside the seesaw
    /// dispatch loop where correlation tracking is not needed.
    pub fn new(deps: Arc<D>, bus: EventBus) -> Self {
        Self {
            deps,
            bus,
            cid: None,
            inflight: None,
        }
    }

    /// Create a new effect context with correlation tracking.
    ///
    /// Events emitted will carry the same correlation ID, enabling
    /// `emit_and_await` to track cascading work.
    pub(crate) fn with_correlation(
        deps: Arc<D>,
        bus: EventBus,
        cid: CorrelationId,
        inflight: Option<Arc<InflightTracker>>,
    ) -> Self {
        Self {
            deps,
            bus,
            cid: Some(cid),
            inflight,
        }
    }

    /// Get shared dependencies.
    ///
    /// Dependencies typically include:
    /// - Database connection pool
    /// - External API clients
    /// - Configuration
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Get the correlation ID for outbox writes.
    ///
    /// Returns the `CorrelationId` suitable for use with `OutboxWriter::write_event`.
    /// If no correlation ID is set, returns `CorrelationId::NONE`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn execute(&self, cmd: CreateCmd, ctx: EffectContext<Deps>) -> Result<MyEvent> {
    ///     let mut tx = ctx.deps().db.begin().await?;
    ///
    ///     // Business write
    ///     let entity = Entity::create(&cmd, &mut tx).await?;
    ///
    ///     // Outbox write with correlation
    ///     let mut writer = PgOutboxWriter::new(&mut tx);
    ///     writer.write_event(&EntityCreated { id: entity.id }, ctx.outbox_correlation_id()).await?;
    ///
    ///     tx.commit().await?;
    ///     Ok(MyEvent::EntityCreated { id: entity.id })
    /// }
    /// ```
    pub fn outbox_correlation_id(&self) -> CorrelationId {
        self.cid.unwrap_or(CorrelationId::NONE)
    }

    /// Get the correlation ID.
    ///
    /// Returns the correlation ID for this effect execution.
    /// If no correlation ID is set, returns `CorrelationId::NONE`.
    pub fn correlation_id(&self) -> CorrelationId {
        self.cid.unwrap_or(CorrelationId::NONE)
    }

    /// Fire-and-forget signal for UI observability.
    ///
    /// Signals are NOT fact events - they are transient UI updates
    /// like typing indicators and progress notifications.
    ///
    /// # Constraints (Frozen)
    ///
    /// - **Not persisted** - signals are ephemeral
    /// - **Not replayed** - excluded from event streams
    /// - **Not observed by machines** - machines only react to fact events
    /// - **Not used for correctness** - system behavior unchanged if dropped
    /// - **Cannot trigger commands** - directly or indirectly (no "reactive UI machines")
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn execute(&self, cmd: ProcessCommand, ctx: EffectContext<Deps>) -> Result<ProcessEvent> {
    ///     // Signal progress started (UI-only, not a fact)
    ///     ctx.signal(ProgressSignal { task_id: cmd.task_id, percent: 0 });
    ///
    ///     // Do the actual work...
    ///     let result = process_data(...).await?;
    ///
    ///     // Return the fact event
    ///     Ok(ProcessEvent::Completed { task_id: cmd.task_id, result })
    /// }
    /// ```
    pub fn signal<E: Event>(&self, event: E) {
        // Signals use random correlation (fire-and-forget, no tracking)
        self.bus.emit(event);
    }
}

/// Context for interactive tool execution.
///
/// This is the blessed way to provide context to agent tools that need
/// to perform interactive operations (calling `dispatch_request`, etc.).
///
/// # When to Use
///
/// Use `ToolContext` when:
/// - Agent tools need to call `dispatch_request` for interactive actions
/// - Tools need access to the kernel/dependencies AND the event bus
///
/// # Example
///
/// ```ignore
/// let tool_ctx = ctx.tool_context();
/// let agent_config = registry
///     .select_for_container(&container, agent, tool_ctx.kernel.clone(), tool_ctx.bus.clone())
///     .await?;
/// ```
pub struct ToolContext<D> {
    /// Shared dependencies (kernel, database, etc.)
    pub deps: Arc<D>,
    /// Event bus for interactive dispatch_request calls.
    pub bus: EventBus,
}

impl<D> Clone for ToolContext<D> {
    fn clone(&self) -> Self {
        Self {
            deps: self.deps.clone(),
            bus: self.bus.clone(),
        }
    }
}

impl<D> EffectContext<D> {
    /// Get a context suitable for interactive tool execution.
    ///
    /// This provides the deps and bus needed by agent tools that call
    /// `dispatch_request` during their execution.
    pub fn tool_context(&self) -> ToolContext<D> {
        ToolContext {
            deps: self.deps.clone(),
            bus: self.bus.clone(),
        }
    }
}

impl<D> Clone for EffectContext<D> {
    fn clone(&self) -> Self {
        Self {
            deps: self.deps.clone(),
            bus: self.bus.clone(),
            cid: self.cid,
            inflight: self.inflight.clone(),
        }
    }
}

impl<D> std::fmt::Debug for EffectContext<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectContext").finish_non_exhaustive()
    }
}

/// Effect handler for executing commands.
///
/// Effects are the IO layer of seesaw. They:
/// 1. Receive commands (intent for IO)
/// 2. Execute database transactions, API calls, etc.
/// 3. Return events describing outcomes (Runtime emits them)
///
/// # Key Principles
///
/// - Effects return `Result<Self::Event>`. The Runtime is the only authority that emits facts.
/// - Effects may read state but must not retain state across executions.
/// - This ensures determinism, replayability, and auditability.
///
/// # Transaction Boundaries
///
/// Each effect execution should use exactly one database transaction.
/// If multiple writes must be atomic, do them in the same transaction
/// within the effect.
///
/// # Example
///
/// ```ignore
/// struct CreateUserEffect;
///
/// #[async_trait]
/// impl Effect<CreateUserCommand, MyDeps> for CreateUserEffect {
///     type Event = UserEvent;
///
///     async fn execute(&self, cmd: CreateUserCommand, ctx: EffectContext<MyDeps>) -> Result<UserEvent> {
///         let user = ctx.deps().db.transaction(|tx| async {
///             let user = User::create(&cmd.email, &cmd.name, tx).await?;
///             UserProfile::create(user.id, tx).await?;
///             Ok(user)
///         }).await?;
///
///         Ok(UserEvent::Created { user_id: user.id, email: cmd.email })
///     }
/// }
/// ```
#[async_trait]
pub trait Effect<C: Command, D>: Send + Sync + 'static {
    /// The event type this effect produces.
    type Event: Event;

    /// Execute a command and return an event.
    ///
    /// The Runtime wraps the returned event in an EventEnvelope and emits it.
    /// Effects do not emit directly - they return events.
    ///
    /// # Errors
    ///
    /// Return `Err` if the command execution fails. The Runtime converts
    /// errors to sanitized `CommandFailed` events.
    async fn execute(&self, command: C, ctx: EffectContext<D>) -> Result<Self::Event>;

    /// Execute multiple commands of the same type.
    ///
    /// Returns `Vec<Self::Event>` - one event per command.
    /// The default implementation calls `execute` sequentially with early return on error.
    ///
    /// Override this method to optimize batch operations:
    /// - Bulk database inserts
    /// - Single transaction for multiple writes
    /// - Batch API calls
    ///
    /// # Semantics
    ///
    /// - Commands are processed in order (no reordering)
    /// - Default is fail-fast (first error stops processing)
    /// - Each command produces exactly one event
    ///
    /// # ⚠️ NOT TRANSACTIONAL (Critical)
    ///
    /// `execute_batch` is **NOT atomic**. It provides:
    /// - Sequential execution with early abort on first error
    /// - **NO rollback** of previously executed commands
    /// - Partial success is possible and common
    ///
    /// If 5 commands are submitted and command 3 fails:
    /// - Commands 1-2: **Executed and committed** (side effects persist)
    /// - Command 3: **Failed** (error returned)
    /// - Commands 4-5: **Never attempted**
    ///
    /// **This is intentional.** True atomicity across commands would require:
    /// - Distributed transactions (complexity explosion)
    /// - Command-level rollback logic (violates stateless effects)
    /// - Transaction coordinators (wrong abstraction layer)
    ///
    /// If you need atomicity, use a single command with a single transaction
    /// inside the effect. That's the seesaw model: **One Command = One Transaction**.
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn execute_batch(&self, commands: Vec<C>, ctx: EffectContext<D>) -> Result<Vec<Self::Event>> {
    ///     // Single transaction for all commands
    ///     let ids: Vec<_> = commands.iter().map(|c| c.id).collect();
    ///     ctx.deps().db.bulk_insert(&commands).await?;
    ///     Ok(ids.into_iter().map(|id| MyEvent::Created { id }).collect())
    /// }
    /// ```
    async fn execute_batch(
        &self,
        commands: Vec<C>,
        ctx: EffectContext<D>,
    ) -> Result<Vec<Self::Event>>
    where
        D: Send + Sync + 'static,
    {
        let mut events = Vec::with_capacity(commands.len());
        for command in commands {
            events.push(self.execute(command, ctx.clone()).await?);
        }
        Ok(events)
    }
}

/// Type-erased effect trait for internal use.
///
/// Returns `EventEnvelope` so the Runtime can emit events.
#[async_trait]
pub(crate) trait AnyEffect<D>: Send + Sync {
    /// Execute a type-erased command and return an event envelope.
    ///
    /// The Runtime emits the returned envelope - effects never emit directly.
    async fn execute_any(
        &self,
        command: Box<dyn Any + Send + Sync>,
        ctx: EffectContext<D>,
    ) -> Result<EventEnvelope>;

    /// Execute a batch of type-erased commands and return event envelopes.
    ///
    /// Returns one envelope per command. The Runtime emits all of them.
    async fn execute_any_batch(
        &self,
        commands: Vec<Box<dyn Any + Send + Sync>>,
        ctx: EffectContext<D>,
    ) -> Result<Vec<EventEnvelope>>;
}

/// Wrapper to make concrete effects implement AnyEffect.
pub(crate) struct EffectWrapper<E, C, D>
where
    E: Effect<C, D>,
    C: Command,
    D: Send + Sync + 'static,
{
    effect: E,
    _phantom: std::marker::PhantomData<(C, D)>,
}

impl<E, C, D> EffectWrapper<E, C, D>
where
    E: Effect<C, D>,
    C: Command,
    D: Send + Sync + 'static,
{
    pub fn new(effect: E) -> Self {
        Self {
            effect,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait]
impl<Eff, C, D> AnyEffect<D> for EffectWrapper<Eff, C, D>
where
    Eff: Effect<C, D>,
    C: Command,
    D: Send + Sync + 'static,
{
    async fn execute_any(
        &self,
        command: Box<dyn Any + Send + Sync>,
        ctx: EffectContext<D>,
    ) -> Result<EventEnvelope> {
        let command = command
            .downcast::<C>()
            .map_err(|c| SeesawError::CommandTypeMismatch {
                expected: std::any::type_name::<C>(),
                actual_type_id: (*c).type_id(),
            })?;
        let cid = ctx.correlation_id();
        let event = self.effect.execute(*command, ctx).await?;
        Ok(EventEnvelope::new(cid, event))
    }

    async fn execute_any_batch(
        &self,
        commands: Vec<Box<dyn Any + Send + Sync>>,
        ctx: EffectContext<D>,
    ) -> Result<Vec<EventEnvelope>> {
        let typed: Result<Vec<C>, _> = commands
            .into_iter()
            .map(|c| {
                c.downcast::<C>()
                    .map(|b| *b)
                    .map_err(|c| SeesawError::CommandTypeMismatch {
                        expected: std::any::type_name::<C>(),
                        actual_type_id: (*c).type_id(),
                    })
            })
            .collect();
        let cid = ctx.correlation_id();
        let events = self.effect.execute_batch(typed?, ctx).await?;
        Ok(events
            .into_iter()
            .map(|e| EventEnvelope::new(cid, e))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Test types
    #[derive(Debug, Clone)]
    struct TestDeps {
        value: i32,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct TestCommand {
        action: String,
    }
    impl Command for TestCommand {}

    #[derive(Debug, Clone, PartialEq)]
    struct TestEvent {
        result: String,
    }
    // Event auto-impl by blanket

    // Test effect
    struct TestEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Effect<TestCommand, TestDeps> for TestEffect {
        type Event = TestEvent;

        async fn execute(
            &self,
            cmd: TestCommand,
            ctx: EffectContext<TestDeps>,
        ) -> Result<TestEvent> {
            self.call_count.fetch_add(1, Ordering::Relaxed);

            // Access deps
            let value = ctx.deps().value;

            // Return event (Runtime emits)
            Ok(TestEvent {
                result: format!("{} with value {}", cmd.action, value),
            })
        }
    }

    #[tokio::test]
    async fn test_effect_context_deps() {
        let deps = Arc::new(TestDeps { value: 42 });
        let bus = EventBus::new();
        let ctx = EffectContext::new(deps, bus);

        assert_eq!(ctx.deps().value, 42);
    }

    #[tokio::test]
    async fn test_effect_execute() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let effect = TestEffect {
            call_count: call_count.clone(),
        };

        let deps = Arc::new(TestDeps { value: 100 });
        let bus = EventBus::new();

        let ctx = EffectContext::new(deps, bus);

        let event = effect
            .execute(
                TestCommand {
                    action: "test".to_string(),
                },
                ctx,
            )
            .await
            .unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        assert_eq!(event.result, "test with value 100");
    }

    #[tokio::test]
    async fn test_effect_wrapper() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let effect = TestEffect {
            call_count: call_count.clone(),
        };
        let wrapper = EffectWrapper::new(effect);

        let deps = Arc::new(TestDeps { value: 50 });
        let bus = EventBus::new();
        let ctx = EffectContext::new(deps, bus);

        let cmd: Box<dyn Any + Send + Sync> = Box::new(TestCommand {
            action: "wrapped".to_string(),
        });

        let envelope = wrapper.execute_any(cmd, ctx).await.unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        // Verify the envelope contains the expected event
        let event = envelope.downcast_ref::<TestEvent>().unwrap();
        assert_eq!(event.result, "wrapped with value 50");
    }

    #[tokio::test]
    async fn test_effect_wrapper_wrong_command_type() {
        let effect = TestEffect {
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        let wrapper = EffectWrapper::new(effect);

        let deps = Arc::new(TestDeps { value: 0 });
        let bus = EventBus::new();
        let ctx = EffectContext::new(deps, bus);

        // Wrong command type
        #[derive(Debug)]
        struct WrongCommand;
        impl Command for WrongCommand {}

        let cmd: Box<dyn Any + Send + Sync> = Box::new(WrongCommand);

        let result = wrapper.execute_any(cmd, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("type mismatch"));
    }

    // Test effect that returns error
    struct FailingEffect;

    #[async_trait]
    impl Effect<TestCommand, TestDeps> for FailingEffect {
        type Event = TestEvent;

        async fn execute(
            &self,
            _cmd: TestCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<TestEvent> {
            // Effects that fail just return Err - Runtime converts to CommandFailed
            Err(anyhow::anyhow!("effect failed"))
        }
    }

    #[tokio::test]
    async fn test_effect_error() {
        let effect = FailingEffect;

        let deps = Arc::new(TestDeps { value: 0 });
        let bus = EventBus::new();

        let ctx = EffectContext::new(deps, bus);

        let result = effect
            .execute(
                TestCommand {
                    action: "fail".to_string(),
                },
                ctx,
            )
            .await;

        // Should return error (Runtime converts to CommandFailed event)
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("effect failed"));
    }

    #[tokio::test]
    async fn test_effect_context_clone() {
        let deps = Arc::new(TestDeps { value: 42 });
        let bus = EventBus::new();
        let ctx1 = EffectContext::new(deps, bus);
        let ctx2 = ctx1.clone();

        assert_eq!(ctx1.deps().value, ctx2.deps().value);
    }

    // Test effect with custom batch implementation
    struct BatchOptimizedEffect {
        individual_calls: Arc<AtomicUsize>,
        batch_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Effect<TestCommand, TestDeps> for BatchOptimizedEffect {
        type Event = TestEvent;

        async fn execute(
            &self,
            cmd: TestCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<TestEvent> {
            self.individual_calls.fetch_add(1, Ordering::Relaxed);
            Ok(TestEvent {
                result: format!("individual: {}", cmd.action),
            })
        }

        async fn execute_batch(
            &self,
            commands: Vec<TestCommand>,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<Vec<TestEvent>> {
            self.batch_calls.fetch_add(1, Ordering::Relaxed);

            // Simulated bulk operation - return one event for the batch
            let actions: Vec<_> = commands.iter().map(|c| c.action.as_str()).collect();
            Ok(vec![TestEvent {
                result: format!("batch: [{}]", actions.join(", ")),
            }])
        }
    }

    #[tokio::test]
    async fn test_effect_batch_override() {
        let individual_calls = Arc::new(AtomicUsize::new(0));
        let batch_calls = Arc::new(AtomicUsize::new(0));

        let effect = BatchOptimizedEffect {
            individual_calls: individual_calls.clone(),
            batch_calls: batch_calls.clone(),
        };

        let deps = Arc::new(TestDeps { value: 0 });
        let bus = EventBus::new();

        let ctx = EffectContext::new(deps, bus);

        let commands = vec![
            TestCommand {
                action: "a".to_string(),
            },
            TestCommand {
                action: "b".to_string(),
            },
            TestCommand {
                action: "c".to_string(),
            },
        ];

        let events = effect.execute_batch(commands, ctx).await.unwrap();

        // Should use batch path, not individual
        assert_eq!(individual_calls.load(Ordering::Relaxed), 0);
        assert_eq!(batch_calls.load(Ordering::Relaxed), 1);

        // Should return single batch event (custom batch can consolidate)
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].result, "batch: [a, b, c]");
    }

    #[tokio::test]
    async fn test_effect_default_batch_uses_execute() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let effect = TestEffect {
            call_count: call_count.clone(),
        };

        let deps = Arc::new(TestDeps { value: 100 });
        let bus = EventBus::new();

        let ctx = EffectContext::new(deps, bus);

        let commands = vec![
            TestCommand {
                action: "x".to_string(),
            },
            TestCommand {
                action: "y".to_string(),
            },
        ];

        let events = effect.execute_batch(commands, ctx).await.unwrap();

        // Default batch impl calls execute for each
        assert_eq!(call_count.load(Ordering::Relaxed), 2);

        // Should return 2 individual events
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].result, "x with value 100");
        assert_eq!(events[1].result, "y with value 100");
    }
}
