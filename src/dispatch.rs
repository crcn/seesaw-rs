//! Command dispatcher for routing commands to effects.
//!
//! The `Dispatcher` is responsible for:
//! 1. Routing commands to the correct effect handler
//! 2. Handling inline vs background execution
//! 3. Providing the effect context

use std::any::TypeId;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use uuid::Uuid;

use crate::bus::EventBus;
use crate::core::{AnyCommand, Command, CorrelationId, ExecutionMode, JobSpec};
use crate::effect_impl::{AnyEffect, Effect, EffectContext, EffectWrapper};
use crate::engine::{InflightBatch, InflightTracker};
use crate::error::{BatchOutcome, CommandFailed, SeesawError};
use tracing::error;

/// Job queue trait for background and scheduled command execution.
///
/// Implement this trait to integrate with your job system (e.g., PostgreSQL-based queue).
/// The job queue receives pre-serialized command payloads along with their metadata.
///
/// # Example Implementation
///
/// ```ignore
/// use seesaw::{JobQueue, JobSpec};
/// use chrono::{DateTime, Utc};
/// use uuid::Uuid;
///
/// struct MyJobQueue { /* ... */ }
///
/// #[async_trait]
/// impl JobQueue for MyJobQueue {
///     async fn enqueue(&self, payload: serde_json::Value, spec: JobSpec) -> Result<Uuid> {
///         // Insert into jobs table
///         let job_id = self.db.insert_job(
///             spec.job_type,
///             payload,
///             spec.idempotency_key,
///             spec.priority,
///             spec.max_retries,
///         ).await?;
///
///         Ok(job_id)
///     }
///
///     async fn schedule(&self, payload: serde_json::Value, spec: JobSpec, run_at: DateTime<Utc>) -> Result<Uuid> {
///         // Similar to enqueue but with run_at timestamp
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait JobQueue: Send + Sync + 'static {
    /// Enqueue a command for immediate background execution.
    ///
    /// # Arguments
    ///
    /// * `payload` - The serialized command payload (JSON)
    /// * `spec` - Job metadata (type, idempotency key, retries, etc.)
    ///
    /// # Returns
    ///
    /// The job ID of the enqueued command.
    async fn enqueue(&self, payload: serde_json::Value, spec: JobSpec) -> Result<Uuid>;

    /// Schedule a command for execution at a specific time.
    ///
    /// The job queue implementation is responsible for executing the command at or after
    /// the specified time.
    ///
    /// # Arguments
    ///
    /// * `payload` - The serialized command payload (JSON)
    /// * `spec` - Job metadata (type, idempotency key, retries, etc.)
    /// * `run_at` - When to execute the command
    ///
    /// # Returns
    ///
    /// The job ID of the scheduled command.
    async fn schedule(
        &self,
        payload: serde_json::Value,
        spec: JobSpec,
        run_at: DateTime<Utc>,
    ) -> Result<Uuid>;
}

/// A no-op job queue that rejects all background and scheduled commands.
///
/// Use this when you don't need background or scheduled command execution.
pub struct NoOpJobQueue;

#[async_trait::async_trait]
impl JobQueue for NoOpJobQueue {
    async fn enqueue(&self, _payload: serde_json::Value, _spec: JobSpec) -> Result<Uuid> {
        Err(anyhow!(
            "background commands not supported: no job queue configured"
        ))
    }

    async fn schedule(
        &self,
        _payload: serde_json::Value,
        _spec: JobSpec,
        _run_at: DateTime<Utc>,
    ) -> Result<Uuid> {
        Err(anyhow!(
            "scheduled commands not supported: no job queue configured"
        ))
    }
}

/// Command dispatcher for routing commands to effects.
///
/// The `Dispatcher` maintains a registry of effects keyed by command type.
/// When a command is dispatched, the dispatcher:
/// 1. Looks up the effect for the command type
/// 2. Checks the command's execution mode
/// 3. Either executes inline or enqueues for background processing
///
/// # Example
///
/// ```ignore
/// let dispatcher = Dispatcher::new(deps, bus)
///     .with_effect::<CreateUserCommand, _>(CreateUserEffect)
///     .with_effect::<DeleteUserCommand, _>(DeleteUserEffect);
///
/// // Dispatch a command
/// let cmd = Box::new(CreateUserCommand { email: "test@example.com".into() });
/// dispatcher.dispatch(cmd).await?;
/// ```
pub struct Dispatcher<D> {
    effects: HashMap<TypeId, Box<dyn AnyEffect<D>>>,
    deps: Arc<D>,
    bus: EventBus,
    job_queue: Arc<dyn JobQueue>,
}

impl<D: Send + Sync + 'static> Dispatcher<D> {
    /// Create a new dispatcher without a job queue.
    ///
    /// Background commands will fail with an error.
    pub fn new(deps: D, bus: EventBus) -> Self {
        Self {
            effects: HashMap::new(),
            deps: Arc::new(deps),
            bus,
            job_queue: Arc::new(NoOpJobQueue),
        }
    }

    /// Create a new dispatcher with pre-wrapped Arc dependencies.
    ///
    /// Use this when you need to share the deps with other parts of the system.
    pub fn from_arc(deps: Arc<D>, bus: EventBus) -> Self {
        Self {
            effects: HashMap::new(),
            deps,
            bus,
            job_queue: Arc::new(NoOpJobQueue),
        }
    }

    /// Create a new dispatcher with a job queue for background execution.
    pub fn with_job_queue(deps: D, bus: EventBus, job_queue: Arc<dyn JobQueue>) -> Self {
        Self {
            effects: HashMap::new(),
            deps: Arc::new(deps),
            bus,
            job_queue,
        }
    }

    /// Create a new dispatcher with pre-wrapped Arc dependencies and a job queue.
    pub fn from_arc_with_job_queue(
        deps: Arc<D>,
        bus: EventBus,
        job_queue: Arc<dyn JobQueue>,
    ) -> Self {
        Self {
            effects: HashMap::new(),
            deps,
            bus,
            job_queue,
        }
    }

    /// Register an effect handler for a command type.
    ///
    /// # Panics
    ///
    /// Panics if an effect is already registered for this command type.
    /// Use `try_with_effect` for a non-panicking version, or
    /// `with_effect_replace` if you want to replace an existing effect.
    pub fn with_effect<C, E>(self, effect: E) -> Self
    where
        C: Command,
        E: Effect<C, D>,
    {
        self.try_with_effect::<C, E>(effect).unwrap_or_else(|e| {
            panic!("{}", e);
        })
    }

    /// Register an effect handler for a command type, returning an error if
    /// an effect is already registered.
    ///
    /// This is the non-panicking version of `with_effect`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let dispatcher = Dispatcher::new(deps, bus)
    ///     .try_with_effect::<CreateCommand, _>(CreateEffect)?
    ///     .try_with_effect::<DeleteCommand, _>(DeleteEffect)?;
    /// ```
    pub fn try_with_effect<C, E>(mut self, effect: E) -> Result<Self>
    where
        C: Command,
        E: Effect<C, D>,
    {
        let type_id = TypeId::of::<C>();
        if self.effects.contains_key(&type_id) {
            return Err(SeesawError::EffectAlreadyRegistered {
                type_name: std::any::type_name::<C>(),
            }
            .into());
        }
        self.effects
            .insert(type_id, Box::new(EffectWrapper::new(effect)));
        Ok(self)
    }

    /// Register an effect handler, replacing any existing handler.
    pub fn with_effect_replace<C, E>(mut self, effect: E) -> Self
    where
        C: Command,
        E: Effect<C, D>,
    {
        let type_id = TypeId::of::<C>();
        self.effects
            .insert(type_id, Box::new(EffectWrapper::new(effect)));
        self
    }

    /// Dispatch a batch of commands of the same type.
    ///
    /// Routes the commands to the appropriate effect based on their type.
    /// All commands in the batch must have the same `TypeId`.
    ///
    /// - Single command: calls `execute` directly
    /// - Multiple commands: calls `execute_batch` and handles `BatchOutcome`
    ///
    /// # Returns
    ///
    /// - `Ok(())` if all commands succeeded
    /// - `Err` if any command failed (error contains context about which failed)
    ///
    /// # Panics
    ///
    /// Panics if commands have different `TypeId`s (runtime guarantees this).
    pub async fn dispatch(&self, commands: Vec<Box<dyn AnyCommand>>) -> Result<()> {
        if commands.is_empty() {
            return Ok(());
        }

        let type_id = commands[0].command_type_id();
        let effect = self.effects.get(&type_id).ok_or_else(|| {
            SeesawError::NoEffectRegistered {
                type_id,
                type_name: "unknown", // TypeId doesn't preserve type name at runtime
            }
        })?;

        let ctx = EffectContext::new(self.deps.clone(), self.bus.clone());

        if commands.len() == 1 {
            // Single command: direct path, no batch overhead
            let command = commands.into_iter().next().unwrap();
            let envelope = effect.execute_any(command.into_any(), ctx).await?;
            // Runtime is the sole emitter
            self.bus.emit_envelope(envelope);
            Ok(())
        } else {
            // Batch: delegate to execute_any_batch
            let commands_any: Vec<_> = commands.into_iter().map(|c| c.into_any()).collect();
            let envelopes = effect.execute_any_batch(commands_any, ctx).await?;
            // Runtime is the sole emitter - emit all returned events
            for envelope in envelopes {
                self.bus.emit_envelope(envelope);
            }
            Ok(())
        }
    }

    /// Dispatch a single command.
    ///
    /// Convenience method that wraps the command in a vec.
    /// Handles execution mode routing (inline, background, scheduled).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A background/scheduled command doesn't provide a `job_spec()`
    /// - A background/scheduled command doesn't provide `serialize_to_json()`
    /// - The job queue returns an error
    /// - The inline effect returns an error
    pub async fn dispatch_one(&self, command: Box<dyn AnyCommand>) -> Result<()> {
        let mode = command.get_execution_mode();

        match mode {
            ExecutionMode::Inline => self.dispatch(vec![command]).await,
            ExecutionMode::Background => {
                let spec = command.get_job_spec().ok_or_else(|| {
                    anyhow!(
                        "command with TypeId {:?} uses Background execution mode but did not provide job_spec()",
                        command.command_type_id()
                    )
                })?;
                let payload = command.get_serialize_to_json().ok_or_else(|| {
                    anyhow!(
                        "command with TypeId {:?} uses Background execution mode but could not be serialized. \
                         Add #[derive(Serialize, Deserialize)] to your command struct.",
                        command.command_type_id()
                    )
                })?;
                self.job_queue.enqueue(payload, spec).await.map(|_| ())
            }
            ExecutionMode::Scheduled { run_at } => {
                let spec = command.get_job_spec().ok_or_else(|| {
                    anyhow!(
                        "command with TypeId {:?} uses Scheduled execution mode but did not provide job_spec()",
                        command.command_type_id()
                    )
                })?;
                let payload = command.get_serialize_to_json().ok_or_else(|| {
                    anyhow!(
                        "command with TypeId {:?} uses Scheduled execution mode but could not be serialized. \
                         Add #[derive(Serialize, Deserialize)] to your command struct.",
                        command.command_type_id()
                    )
                })?;
                self.job_queue
                    .schedule(payload, spec, run_at)
                    .await
                    .map(|_| ())
            }
        }
    }

    /// Dispatch a batch of commands with correlation tracking.
    ///
    /// This method uses the receipt pattern for accurate inflight tracking:
    /// 1. Creates an `InflightBatch` receipt (increments count)
    /// 2. Creates an EffectContext with the correlation ID
    /// 3. Dispatches the commands
    /// 4. Completes the receipt with the outcome (decrements based on success)
    ///
    /// # Partial Success Handling
    ///
    /// When a batch partially succeeds, only the succeeded count is decremented.
    /// This enables accurate `emit_and_await` behavior and proper retry semantics.
    ///
    /// # Panic Safety
    ///
    /// If execution panics before completion, the `InflightBatch` logs a warning
    /// on drop. The timeout backstop in `emit_and_await` handles cleanup.
    pub async fn dispatch_with_correlation(
        &self,
        commands: Vec<Box<dyn AnyCommand>>,
        cid: CorrelationId,
        inflight: Option<&Arc<InflightTracker>>,
    ) -> Result<()> {
        if commands.is_empty() {
            return Ok(());
        }

        let batch_size = commands.len();
        let type_id = commands[0].command_type_id();

        let effect = self
            .effects
            .get(&type_id)
            .ok_or_else(|| SeesawError::NoEffectRegistered {
                type_id,
                type_name: "unknown",
            })?;

        // Create context with correlation ID for event propagation
        let ctx = EffectContext::with_correlation(
            self.deps.clone(),
            self.bus.clone(),
            cid,
            inflight.cloned(),
        );

        // Use receipt pattern for batch tracking (if inflight tracker provided)
        let batch: Option<InflightBatch> =
            inflight.map(|tracker| tracker.begin_batch(cid, batch_size));

        if commands.len() == 1 {
            // Single command: direct path
            let command = commands.into_iter().next().unwrap();

            // Wrap effect execution with panic catching
            // AssertUnwindSafe is required because effect/ctx are not UnwindSafe
            let result = AssertUnwindSafe(effect.execute_any(command.into_any(), ctx))
                .catch_unwind()
                .await;

            // Convert panic to error
            let result = match result {
                Ok(inner) => inner,
                Err(panic_info) => {
                    let panic_msg = extract_panic_message(&panic_info);
                    error!(%cid, panic = %panic_msg, "effect panicked");
                    Err(anyhow::anyhow!("effect panicked: {}", panic_msg))
                }
            };

            // Complete batch with synthetic outcome
            if let Some(batch) = batch {
                let outcome = match &result {
                    Ok(_) => BatchOutcome::Complete,
                    Err(e) => BatchOutcome::Partial {
                        succeeded: 0,
                        failed_at: 0,
                        error: anyhow::anyhow!("{}", e),
                    },
                };
                batch.complete(outcome);
            }

            match result {
                Ok(envelope) => {
                    // Runtime is the sole emitter
                    self.bus.emit_envelope(envelope);
                    Ok(())
                }
                Err(e) => {
                    // Log raw error for developers (before sanitization)
                    error!(%cid, error = ?e, "effect failed");

                    // Record error for correlation tracking (so emit_and_await returns it)
                    if let Some(tracker) = inflight {
                        tracker.record_error(cid, anyhow::anyhow!("{}", e));
                    }

                    // Emit sanitized CommandFailed event with same correlation ID
                    // so dispatch_request can match it
                    let failed = CommandFailed::from_error(&e, "unknown", cid);
                    self.bus.emit_with_correlation(failed, cid);
                    Ok(())
                }
            }
        } else {
            // Batch: delegate to execute_any_batch
            let commands_any: Vec<_> = commands.into_iter().map(|c| c.into_any()).collect();

            // Wrap effect execution with panic catching
            let result = AssertUnwindSafe(effect.execute_any_batch(commands_any, ctx))
                .catch_unwind()
                .await;

            // Convert panic to error
            let result = match result {
                Ok(inner) => inner,
                Err(panic_info) => {
                    let panic_msg = extract_panic_message(&panic_info);
                    error!(%cid, panic = %panic_msg, "batch effect panicked");
                    Err(anyhow::anyhow!("batch effect panicked: {}", panic_msg))
                }
            };

            // Complete batch with outcome based on success/failure
            if let Some(batch) = batch {
                let outcome = match &result {
                    Ok(_) => {
                        // All succeeded
                        BatchOutcome::Complete
                    }
                    Err(e) => BatchOutcome::Partial {
                        succeeded: 0, // Conservative: assume failure at start
                        failed_at: 0,
                        error: anyhow::anyhow!("{}", e),
                    },
                };
                batch.complete(outcome);
            }

            match result {
                Ok(envelopes) => {
                    // Runtime is the sole emitter - emit all returned events
                    for envelope in envelopes {
                        self.bus.emit_envelope(envelope);
                    }
                    Ok(())
                }
                Err(e) => {
                    // Log raw error for developers (before sanitization)
                    error!(%cid, error = ?e, "batch effect failed");

                    // Record error for correlation tracking
                    if let Some(tracker) = inflight {
                        tracker.record_error(cid, anyhow::anyhow!("batch failed: {}", e));
                    }

                    // Emit sanitized CommandFailed event with same correlation ID
                    // so dispatch_request can match it
                    let failed = CommandFailed::from_error(&e, "unknown", cid);
                    self.bus.emit_with_correlation(failed, cid);
                    Ok(())
                }
            }
        }
    }

    /// Check if an effect is registered for a command type.
    pub fn has_effect<C: Command>(&self) -> bool {
        self.effects.contains_key(&TypeId::of::<C>())
    }

    /// Returns the number of registered effects.
    pub fn effect_count(&self) -> usize {
        self.effects.len()
    }

    /// Get access to the dependencies.
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Get access to the event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }
}

/// Extract a human-readable message from a panic payload.
fn extract_panic_message(panic_info: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic_info.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic_info.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

impl<D> std::fmt::Debug for Dispatcher<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("effect_count", &self.effects.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    // Test types
    #[derive(Debug, Clone)]
    struct TestDeps {
        value: i32,
    }

    #[derive(Debug, Clone)]
    struct CreateCommand {
        name: String,
    }
    impl Command for CreateCommand {}

    #[derive(Debug, Clone)]
    struct DeleteCommand {
        id: u64,
    }
    impl Command for DeleteCommand {}

    #[derive(Debug, Clone, serde::Serialize)]
    struct BackgroundCommand {
        task: String,
    }
    impl Command for BackgroundCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Background
        }

        fn job_spec(&self) -> Option<JobSpec> {
            Some(JobSpec::new("test:background"))
        }

        fn serialize_to_json(&self) -> Option<serde_json::Value> {
            serde_json::to_value(self).ok()
        }
    }

    #[derive(Debug, Clone, serde::Serialize)]
    struct ScheduledCommand {
        task: String,
        run_at: DateTime<Utc>,
    }
    impl Command for ScheduledCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Scheduled {
                run_at: self.run_at,
            }
        }

        fn job_spec(&self) -> Option<JobSpec> {
            Some(JobSpec::new("test:scheduled"))
        }

        fn serialize_to_json(&self) -> Option<serde_json::Value> {
            serde_json::to_value(self).ok()
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    struct TestEvent {
        message: String,
    }
    // Event auto-impl by blanket

    // Test effects
    struct CreateEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<CreateCommand, TestDeps> for CreateEffect {
        type Event = TestEvent;

        async fn execute(
            &self,
            cmd: CreateCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<TestEvent> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(TestEvent {
                message: format!("created {}", cmd.name),
            })
        }
    }

    struct DeleteEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<DeleteCommand, TestDeps> for DeleteEffect {
        type Event = TestEvent;

        async fn execute(
            &self,
            cmd: DeleteCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<TestEvent> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(TestEvent {
                message: format!("deleted {}", cmd.id),
            })
        }
    }

    #[tokio::test]
    async fn test_dispatcher_inline_dispatch() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        let dispatcher = Dispatcher::new(TestDeps { value: 42 }, bus)
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: call_count.clone(),
            });

        let cmd: Box<dyn AnyCommand> = Box::new(CreateCommand {
            name: "test".to_string(),
        });
        dispatcher.dispatch(vec![cmd]).await.unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        let event = receiver.recv().await.unwrap();
        let test_event = event.downcast_ref::<TestEvent>().unwrap();
        assert_eq!(test_event.message, "created test");
    }

    #[tokio::test]
    async fn test_dispatcher_multiple_effects() {
        let create_count = Arc::new(AtomicUsize::new(0));
        let delete_count = Arc::new(AtomicUsize::new(0));

        let bus = EventBus::new();

        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: create_count.clone(),
            })
            .with_effect::<DeleteCommand, _>(DeleteEffect {
                call_count: delete_count.clone(),
            });

        // Dispatch create
        let cmd: Box<dyn AnyCommand> = Box::new(CreateCommand {
            name: "item".to_string(),
        });
        dispatcher.dispatch(vec![cmd]).await.unwrap();

        // Dispatch delete
        let cmd: Box<dyn AnyCommand> = Box::new(DeleteCommand { id: 123 });
        dispatcher.dispatch(vec![cmd]).await.unwrap();

        assert_eq!(create_count.load(Ordering::Relaxed), 1);
        assert_eq!(delete_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_dispatcher_no_effect_registered() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus);

        let cmd: Box<dyn AnyCommand> = Box::new(CreateCommand {
            name: "test".to_string(),
        });
        let result = dispatcher.dispatch(vec![cmd]).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no effect registered"));
    }

    #[tokio::test]
    async fn test_dispatcher_background_no_queue() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus);

        let cmd: Box<dyn AnyCommand> = Box::new(BackgroundCommand {
            task: "process".to_string(),
        });
        let result = dispatcher.dispatch_one(cmd).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no job queue"));
    }

    // Mock job queue for testing
    struct MockJobQueue {
        enqueued: Arc<std::sync::Mutex<Vec<String>>>,
        scheduled: Arc<std::sync::Mutex<Vec<(String, DateTime<Utc>)>>>,
    }

    #[async_trait::async_trait]
    impl JobQueue for MockJobQueue {
        async fn enqueue(&self, _payload: serde_json::Value, spec: JobSpec) -> Result<Uuid> {
            self.enqueued
                .lock()
                .unwrap()
                .push(spec.job_type.to_string());
            Ok(Uuid::new_v4())
        }

        async fn schedule(
            &self,
            _payload: serde_json::Value,
            spec: JobSpec,
            run_at: DateTime<Utc>,
        ) -> Result<Uuid> {
            self.scheduled
                .lock()
                .unwrap()
                .push((spec.job_type.to_string(), run_at));
            Ok(Uuid::new_v4())
        }
    }

    #[tokio::test]
    async fn test_dispatcher_background_with_queue() {
        let enqueued = Arc::new(std::sync::Mutex::new(Vec::new()));
        let scheduled = Arc::new(std::sync::Mutex::new(Vec::new()));
        let job_queue = Arc::new(MockJobQueue {
            enqueued: enqueued.clone(),
            scheduled: scheduled.clone(),
        });

        let bus = EventBus::new();
        let dispatcher = Dispatcher::with_job_queue(TestDeps { value: 0 }, bus, job_queue);

        let cmd: Box<dyn AnyCommand> = Box::new(BackgroundCommand {
            task: "process".to_string(),
        });
        dispatcher.dispatch_one(cmd).await.unwrap();

        assert_eq!(enqueued.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_dispatcher_scheduled_no_queue() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus);

        let run_at = Utc::now() + chrono::Duration::hours(1);
        let cmd: Box<dyn AnyCommand> = Box::new(ScheduledCommand {
            task: "reminder".to_string(),
            run_at,
        });
        let result = dispatcher.dispatch_one(cmd).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("scheduled commands not supported"));
    }

    #[tokio::test]
    async fn test_dispatcher_scheduled_with_queue() {
        let enqueued = Arc::new(std::sync::Mutex::new(Vec::new()));
        let scheduled = Arc::new(std::sync::Mutex::new(Vec::new()));
        let job_queue = Arc::new(MockJobQueue {
            enqueued: enqueued.clone(),
            scheduled: scheduled.clone(),
        });

        let bus = EventBus::new();
        let dispatcher = Dispatcher::with_job_queue(TestDeps { value: 0 }, bus, job_queue);

        let run_at = Utc::now() + chrono::Duration::hours(1);
        let cmd: Box<dyn AnyCommand> = Box::new(ScheduledCommand {
            task: "reminder".to_string(),
            run_at,
        });
        dispatcher.dispatch_one(cmd).await.unwrap();

        let scheduled_items = scheduled.lock().unwrap();
        assert_eq!(scheduled_items.len(), 1);
        assert_eq!(scheduled_items[0].1, run_at);
    }

    #[test]
    fn test_dispatcher_has_effect() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: Arc::new(AtomicUsize::new(0)),
            });

        assert!(dispatcher.has_effect::<CreateCommand>());
        assert!(!dispatcher.has_effect::<DeleteCommand>());
    }

    #[test]
    fn test_dispatcher_effect_count() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: Arc::new(AtomicUsize::new(0)),
            })
            .with_effect::<DeleteCommand, _>(DeleteEffect {
                call_count: Arc::new(AtomicUsize::new(0)),
            });

        assert_eq!(dispatcher.effect_count(), 2);
    }

    #[test]
    #[should_panic(expected = "effect already registered")]
    fn test_dispatcher_duplicate_effect_panics() {
        let bus = EventBus::new();
        let _dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: Arc::new(AtomicUsize::new(0)),
            })
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: Arc::new(AtomicUsize::new(0)),
            });
    }

    #[test]
    fn test_dispatcher_replace_effect() {
        let count1 = Arc::new(AtomicUsize::new(0));
        let count2 = Arc::new(AtomicUsize::new(0));

        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: count1.clone(),
            })
            .with_effect_replace::<CreateCommand, _>(CreateEffect {
                call_count: count2.clone(),
            });

        assert_eq!(dispatcher.effect_count(), 1);
    }

    #[test]
    fn test_dispatcher_deps() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 123 }, bus);

        assert_eq!(dispatcher.deps().value, 123);
    }

    #[test]
    fn test_dispatcher_debug() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus);

        let debug = format!("{:?}", dispatcher);
        assert!(debug.contains("Dispatcher"));
        assert!(debug.contains("effect_count"));
    }

    #[tokio::test]
    async fn test_dispatcher_batch_dispatch() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        let dispatcher = Dispatcher::new(TestDeps { value: 42 }, bus)
            .with_effect::<CreateCommand, _>(CreateEffect {
                call_count: call_count.clone(),
            });

        // Create batch of 3 commands
        let batch: Vec<Box<dyn AnyCommand>> = vec![
            Box::new(CreateCommand {
                name: "a".to_string(),
            }),
            Box::new(CreateCommand {
                name: "b".to_string(),
            }),
            Box::new(CreateCommand {
                name: "c".to_string(),
            }),
        ];
        dispatcher.dispatch(batch).await.unwrap();

        // Default execute_batch calls execute 3 times
        assert_eq!(call_count.load(Ordering::Relaxed), 3);

        // Should have emitted 3 events
        let e1 = receiver.recv().await.unwrap();
        let e2 = receiver.recv().await.unwrap();
        let e3 = receiver.recv().await.unwrap();
        assert_eq!(e1.downcast_ref::<TestEvent>().unwrap().message, "created a");
        assert_eq!(e2.downcast_ref::<TestEvent>().unwrap().message, "created b");
        assert_eq!(e3.downcast_ref::<TestEvent>().unwrap().message, "created c");
    }

    #[tokio::test]
    async fn test_dispatcher_empty_batch() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus);

        // Empty batch should succeed without needing an effect
        let result = dispatcher.dispatch(vec![]).await;
        assert!(result.is_ok());
    }

    // Effect for BackgroundCommand to test inline execution
    struct BackgroundEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<BackgroundCommand, TestDeps> for BackgroundEffect {
        type Event = TestEvent;

        async fn execute(
            &self,
            cmd: BackgroundCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<TestEvent> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(TestEvent {
                message: format!("processed {}", cmd.task),
            })
        }
    }

    /// Tests that dispatch() executes background commands inline via effects.
    ///
    /// This is critical for job workers: when a background command is pulled from
    /// the job queue, the worker should execute it inline via dispatch(), NOT
    /// re-enqueue it via dispatch_one(). This test verifies that dispatch()
    /// ignores execution_mode and runs the effect directly.
    #[tokio::test]
    async fn test_dispatch_executes_background_commands_inline() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        // Create dispatcher with BackgroundEffect registered
        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<BackgroundCommand, _>(BackgroundEffect {
                call_count: call_count.clone(),
            });

        // Create a background command
        let cmd: Box<dyn AnyCommand> = Box::new(BackgroundCommand {
            task: "important-task".to_string(),
        });

        // Use dispatch() directly (not dispatch_one())
        // This should execute the effect inline, NOT check execution_mode
        dispatcher.dispatch(vec![cmd]).await.unwrap();

        // Effect should have been called
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            1,
            "dispatch() should execute background commands inline via effects"
        );

        // Event should have been emitted
        let event = receiver.recv().await.unwrap();
        let test_event = event.downcast_ref::<TestEvent>().unwrap();
        assert_eq!(test_event.message, "processed important-task");
    }

    // ==========================================================================
    // Missing job_spec() and serialize_to_json() Tests
    // ==========================================================================

    /// Background command that doesn't implement job_spec()
    #[derive(Debug, Clone)]
    struct BackgroundNoJobSpecCommand;
    impl Command for BackgroundNoJobSpecCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Background
        }
        // Intentionally missing job_spec()
        fn serialize_to_json(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({"cmd": "test"}))
        }
    }

    /// Background command that doesn't implement serialize_to_json()
    #[derive(Debug, Clone)]
    struct BackgroundNoSerializeCommand;
    impl Command for BackgroundNoSerializeCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Background
        }
        fn job_spec(&self) -> Option<JobSpec> {
            Some(JobSpec::new("test:no_serialize"))
        }
        // Intentionally missing serialize_to_json()
    }

    /// Scheduled command that doesn't implement job_spec()
    #[derive(Debug, Clone)]
    struct ScheduledNoJobSpecCommand {
        run_at: DateTime<Utc>,
    }
    impl Command for ScheduledNoJobSpecCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Scheduled { run_at: self.run_at }
        }
        // Intentionally missing job_spec()
        fn serialize_to_json(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({"cmd": "test"}))
        }
    }

    /// Scheduled command that doesn't implement serialize_to_json()
    #[derive(Debug, Clone)]
    struct ScheduledNoSerializeCommand {
        run_at: DateTime<Utc>,
    }
    impl Command for ScheduledNoSerializeCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Scheduled { run_at: self.run_at }
        }
        fn job_spec(&self) -> Option<JobSpec> {
            Some(JobSpec::new("test:no_serialize"))
        }
        // Intentionally missing serialize_to_json()
    }

    #[tokio::test]
    async fn test_background_command_missing_job_spec() {
        let job_queue = Arc::new(MockJobQueue {
            enqueued: Arc::new(std::sync::Mutex::new(Vec::new())),
            scheduled: Arc::new(std::sync::Mutex::new(Vec::new())),
        });

        let bus = EventBus::new();
        let dispatcher = Dispatcher::with_job_queue(TestDeps { value: 0 }, bus, job_queue);

        let cmd: Box<dyn AnyCommand> = Box::new(BackgroundNoJobSpecCommand);
        let result = dispatcher.dispatch_one(cmd).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("job_spec"),
            "Error should mention missing job_spec(): {}",
            err
        );
    }

    #[tokio::test]
    async fn test_background_command_missing_serialize() {
        let job_queue = Arc::new(MockJobQueue {
            enqueued: Arc::new(std::sync::Mutex::new(Vec::new())),
            scheduled: Arc::new(std::sync::Mutex::new(Vec::new())),
        });

        let bus = EventBus::new();
        let dispatcher = Dispatcher::with_job_queue(TestDeps { value: 0 }, bus, job_queue);

        let cmd: Box<dyn AnyCommand> = Box::new(BackgroundNoSerializeCommand);
        let result = dispatcher.dispatch_one(cmd).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("could not be serialized"),
            "Error should mention serialization failure: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_scheduled_command_missing_job_spec() {
        let job_queue = Arc::new(MockJobQueue {
            enqueued: Arc::new(std::sync::Mutex::new(Vec::new())),
            scheduled: Arc::new(std::sync::Mutex::new(Vec::new())),
        });

        let bus = EventBus::new();
        let dispatcher = Dispatcher::with_job_queue(TestDeps { value: 0 }, bus, job_queue);

        let run_at = Utc::now() + chrono::Duration::hours(1);
        let cmd: Box<dyn AnyCommand> = Box::new(ScheduledNoJobSpecCommand { run_at });
        let result = dispatcher.dispatch_one(cmd).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("job_spec"),
            "Error should mention missing job_spec(): {}",
            err
        );
    }

    #[tokio::test]
    async fn test_scheduled_command_missing_serialize() {
        let job_queue = Arc::new(MockJobQueue {
            enqueued: Arc::new(std::sync::Mutex::new(Vec::new())),
            scheduled: Arc::new(std::sync::Mutex::new(Vec::new())),
        });

        let bus = EventBus::new();
        let dispatcher = Dispatcher::with_job_queue(TestDeps { value: 0 }, bus, job_queue);

        let run_at = Utc::now() + chrono::Duration::hours(1);
        let cmd: Box<dyn AnyCommand> = Box::new(ScheduledNoSerializeCommand { run_at });
        let result = dispatcher.dispatch_one(cmd).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("could not be serialized"),
            "Error should mention serialization failure: {}",
            err
        );
    }

    // ==========================================================================
    // End missing job_spec/serialize_to_json tests
    // ==========================================================================

    /// Tests the contrast: dispatch_one() routes background commands to job queue.
    ///
    /// This shows why job workers must use dispatch() not dispatch_one():
    /// dispatch_one() would re-enqueue the command instead of executing it.
    #[tokio::test]
    async fn test_dispatch_one_enqueues_background_commands() {
        let effect_count = Arc::new(AtomicUsize::new(0));
        let enqueued = Arc::new(std::sync::Mutex::new(Vec::new()));
        let job_queue = Arc::new(MockJobQueue {
            enqueued: enqueued.clone(),
            scheduled: Arc::new(std::sync::Mutex::new(Vec::new())),
        });

        let bus = EventBus::new();

        // Create dispatcher with BOTH effect and job queue
        let dispatcher = Dispatcher::with_job_queue(TestDeps { value: 0 }, bus, job_queue)
            .with_effect::<BackgroundCommand, _>(BackgroundEffect {
            call_count: effect_count.clone(),
        });

        let cmd: Box<dyn AnyCommand> = Box::new(BackgroundCommand {
            task: "task".to_string(),
        });

        // dispatch_one() checks execution_mode and routes to job queue
        dispatcher.dispatch_one(cmd).await.unwrap();

        // Effect should NOT have been called
        assert_eq!(
            effect_count.load(Ordering::Relaxed),
            0,
            "dispatch_one() should NOT execute background commands via effects"
        );

        // Command should have been enqueued instead
        assert_eq!(
            enqueued.lock().unwrap().len(),
            1,
            "dispatch_one() should enqueue background commands to job queue"
        );
    }
}
