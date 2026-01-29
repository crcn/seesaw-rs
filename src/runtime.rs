//! Runtime for coordinating machines, dispatcher, and event bus.
//!
//! The `Runtime` is the main entry point for running a seesaw application.
//! It coordinates:
//! - Multiple machines observing the event stream
//! - A dispatcher for routing commands to effects
//! - An event bus for broadcasting events

use std::any::TypeId;
use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error, info, trace, warn};

use crate::bus::EventBus;
use crate::dispatch::{Dispatcher, JobQueue};
use crate::engine::{InflightGuard, InflightTracker};
use crate::machine::{Machine, MachineRunner};
use crate::tap::TapRegistry;

#[cfg(debug_assertions)]
use crate::audit::{AuditEntryBuilder, AuditLog, SharedAuditLog};

/// Runtime for coordinating seesaw components.
///
/// The runtime:
/// 1. Subscribes to the event bus
/// 2. For each event, calls `decide` on all machines
/// 3. Dispatches any resulting commands to effects
/// 4. Effects emit new events, continuing the loop
///
/// # Example
///
/// ```ignore
/// // Build components
/// let bus = EventBus::new();
/// let dispatcher = Dispatcher::new(deps, bus.clone())
///     .with_effect::<SetupLoafCommand, _>(SetupEffect)
///     .with_effect::<GenerateCardsCommand, _>(GenerateEffect);
///
/// // Create runtime with machines
/// let runtime = Runtime::new(dispatcher, bus.clone())
///     .with_machine(BakeMachine::new())
///     .with_machine(AuditMachine::new())
///     .with_machine(CacheMachine::new());
///
/// // Run the runtime (spawned in background)
/// tokio::spawn(runtime.run());
///
/// // Emit events to trigger the workflow
/// bus.emit(BakeEvent::Requested { deck_id, recipe_id });
/// ```
pub struct Runtime<D> {
    machines: Vec<MachineRunner>,
    dispatcher: Dispatcher<D>,
    bus: EventBus,
    /// Optional inflight tracker for correlation-based await.
    inflight: Option<Arc<InflightTracker>>,
    /// Event taps for observing committed facts.
    taps: TapRegistry,
    /// Debug-only audit log for event visibility.
    #[cfg(debug_assertions)]
    audit_log: SharedAuditLog,
}

impl<D: Send + Sync + 'static> Runtime<D> {
    /// Create a new runtime with the given dispatcher and event bus.
    pub fn new(dispatcher: Dispatcher<D>, bus: EventBus) -> Self {
        Self {
            machines: Vec::new(),
            dispatcher,
            bus,
            inflight: None,
            taps: TapRegistry::new(),
            #[cfg(debug_assertions)]
            audit_log: Arc::new(AuditLog::new()),
        }
    }

    /// Set the tap registry for event observation.
    ///
    /// Taps run after effects complete, observing committed facts.
    pub(crate) fn with_taps(mut self, taps: TapRegistry) -> Self {
        self.taps = taps;
        self
    }

    /// Set the inflight tracker for correlation-based await.
    ///
    /// When set, the runtime can track pending work for `emit_and_await`.
    pub fn with_inflight(mut self, inflight: Arc<InflightTracker>) -> Self {
        self.inflight = Some(inflight);
        self
    }

    /// Add a machine to the runtime.
    ///
    /// Machines are called in the order they are added. Each machine
    /// can observe the same event and independently decide whether
    /// to emit a command.
    pub fn with_machine<M: Machine>(mut self, machine: M) -> Self {
        self.machines.push(MachineRunner::new(machine));
        self
    }

    /// Run the runtime, processing events until the bus is closed.
    ///
    /// This method consumes the runtime and runs the main event loop.
    /// It should typically be spawned as a background task:
    ///
    /// ```ignore
    /// tokio::spawn(runtime.run());
    /// ```
    ///
    /// The loop continues until:
    /// - All senders are dropped (bus closed)
    /// - A fatal error occurs
    ///
    /// # Per-Tick Batching
    ///
    /// Commands emitted by machines in response to a single event are
    /// collected and grouped by `(TypeId, CorrelationId)` before dispatch.
    /// This enables effects to optimize batch operations (e.g., bulk inserts)
    /// while maintaining correlation for `emit_and_await`.
    ///
    /// Invariant: Events emitted by effects are not processed until the
    /// next tick (via the event bus). This ensures batching is a semantic
    /// no-op—machines never observe effect completion within the same tick.
    pub async fn run(mut self) {
        info!(
            machine_count = self.machines.len(),
            "seesaw runtime starting"
        );

        let mut receiver = self.bus.subscribe();

        loop {
            match receiver.recv().await {
                Ok(envelope) => {
                    // RAII guard for event processing - decrements on drop even if we panic
                    // Only create guard if:
                    // 1. We have an inflight tracker
                    // 2. There's pending work for this cid (count > 0)
                    // This prevents extra decrements for events emitted by effects
                    // (like CommandFailed) which share the same cid but weren't inc'd separately.
                    let _event_guard = self.inflight.as_ref().and_then(|tracker| {
                        if tracker.has_pending_work(envelope.cid) {
                            Some(InflightGuard::for_event(tracker.clone(), envelope.cid))
                        } else {
                            None
                        }
                    });

                    // 1. Collect commands from all machines for this event
                    //    Group inline commands by (TypeId, CorrelationId) for batching
                    let mut inline_batches: BTreeMap<
                        (TypeId, crate::core::CorrelationId),
                        Vec<Box<dyn crate::core::AnyCommand>>,
                    > = BTreeMap::new();

                    // Debug audit: track which machines observe/emit
                    #[cfg(debug_assertions)]
                    let mut audit_builder = AuditEntryBuilder::with_type_id(
                        envelope.type_id,
                        // We don't have the event type name in the envelope, use a placeholder
                        "unknown",
                    );

                    for machine in &mut self.machines {
                        // Check if this machine handles this event type
                        #[cfg(debug_assertions)]
                        let handles_event = machine.handles_event(envelope.payload.as_ref());

                        // Pass the envelope's payload to machines
                        match machine.decide(envelope.payload.as_ref()) {
                            Ok(Some(cmd)) => {
                                debug!(machine = machine.name(), "machine emitted command");

                                // Record in audit log
                                #[cfg(debug_assertions)]
                                {
                                    audit_builder.observed(machine.name());
                                    audit_builder.emitted(machine.name());
                                }

                                let mode = cmd.get_execution_mode();
                                let type_id = cmd.command_type_id();

                                match mode {
                                    crate::core::ExecutionMode::Inline => {
                                        // Group by (TypeId, cid) to maintain correlation per batch
                                        inline_batches
                                            .entry((type_id, envelope.cid))
                                            .or_default()
                                            .push(cmd);
                                    }
                                    crate::core::ExecutionMode::Background
                                    | crate::core::ExecutionMode::Scheduled { .. } => {
                                        // Background/scheduled: dispatch immediately to job queue
                                        if let Err(e) = self.dispatcher.dispatch_one(cmd).await {
                                            error!(error = %e, "background command dispatch failed");
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                // Machine didn't emit, but may have observed
                                #[cfg(debug_assertions)]
                                if handles_event {
                                    audit_builder.observed(machine.name());
                                }
                            }
                            Err(panic_msg) => {
                                // Machine panicked - record error for correlation tracking
                                if let Some(ref inflight) = self.inflight {
                                    if envelope.cid.is_some() {
                                        inflight
                                            .record_error(envelope.cid, anyhow::anyhow!("{}", panic_msg));
                                    }
                                }
                                // Continue processing other machines - one bad machine
                                // shouldn't stop others from handling this event
                            }
                        }
                    }

                    // Record audit entry
                    #[cfg(debug_assertions)]
                    self.audit_log.record(audit_builder.build());

                    // 2. Dispatch inline batches (deterministic order via BTreeMap)
                    for ((type_id, cid), batch) in inline_batches {
                        let batch_size = batch.len();
                        if batch_size > 1 {
                            debug!(batch_size, ?type_id, %cid, "dispatching command batch");
                        }

                        // Dispatch with correlation for inflight tracking
                        if let Err(e) = self
                            .dispatcher
                            .dispatch_with_correlation(batch, cid, self.inflight.as_ref())
                            .await
                        {
                            error!(error = %e, "batch dispatch failed");
                            // Record error for correlation
                            if let Some(tracker) = &self.inflight {
                                tracker.record_error(cid, e);
                            }
                        }
                    }

                    // 3. Run event taps (after effects complete)
                    // Taps observe committed facts - they run fire-and-forget
                    if !self.taps.is_empty() {
                        self.taps
                            .run_all(envelope.payload.as_ref(), Some(envelope.cid));
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    warn!(missed = n, "event bus lagged, missed events");
                }
                Err(RecvError::Closed) => {
                    info!("event bus closed, runtime shutting down");
                    break;
                }
            }
        }

        info!("seesaw runtime stopped");
    }

    /// Get the number of registered machines.
    pub fn machine_count(&self) -> usize {
        self.machines.len()
    }

    /// Get access to the dispatcher.
    pub fn dispatcher(&self) -> &Dispatcher<D> {
        &self.dispatcher
    }

    /// Get access to the event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Get access to the audit log (debug builds only).
    ///
    /// The audit log tracks which machines observe and emit commands for each event.
    /// Use this to debug wiring issues or find dead machines.
    ///
    /// # Example
    ///
    /// ```ignore
    /// #[cfg(debug_assertions)]
    /// {
    ///     let audit = runtime.audit_log();
    ///     for entry in audit.silent_events() {
    ///         tracing::warn!(
    ///             event_type = entry.event_type_name,
    ///             observers = ?entry.observers,
    ///             "event had no command emitters"
    ///         );
    ///     }
    /// }
    /// ```
    #[cfg(debug_assertions)]
    pub fn audit_log(&self) -> &SharedAuditLog {
        &self.audit_log
    }
}

impl<D> std::fmt::Debug for Runtime<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("machine_count", &self.machines.len())
            .finish_non_exhaustive()
    }
}

/// Builder for constructing a complete seesaw application.
///
/// `RuntimeBuilder` provides a convenient way to assemble all the
/// components of a seesaw application in one place.
///
/// # Example
///
/// ```ignore
/// let (runtime, bus) = RuntimeBuilder::new(deps)
///     .with_machine(BakeMachine::new())
///     .with_machine(AuditMachine::new())
///     .with_effect::<SetupLoafCommand, _>(SetupEffect)
///     .with_effect::<GenerateCardsCommand, _>(GenerateEffect)
///     .build();
///
/// tokio::spawn(runtime.run());
/// bus.emit(StartEvent);
/// ```
pub struct RuntimeBuilder<D> {
    deps: D,
    machines: Vec<MachineRunner>,
    bus: EventBus,
    job_queue: Option<Arc<dyn JobQueue>>,
    effects: Vec<Box<dyn FnOnce(Dispatcher<D>) -> Dispatcher<D>>>,
}

impl<D: Send + Sync + 'static> RuntimeBuilder<D> {
    /// Create a new runtime builder with the given dependencies.
    pub fn new(deps: D) -> Self {
        Self {
            deps,
            machines: Vec::new(),
            bus: EventBus::new(),
            job_queue: None,
            effects: Vec::new(),
        }
    }

    /// Use an existing event bus instead of creating a new one.
    pub fn with_bus(mut self, bus: EventBus) -> Self {
        self.bus = bus;
        self
    }

    /// Set the job queue for background command execution.
    pub fn with_job_queue(mut self, job_queue: Arc<dyn JobQueue>) -> Self {
        self.job_queue = Some(job_queue);
        self
    }

    /// Add a machine to the runtime.
    pub fn with_machine<M: Machine>(mut self, machine: M) -> Self {
        self.machines.push(MachineRunner::new(machine));
        self
    }

    /// Register an effect handler for a command type.
    pub fn with_effect<C, E>(mut self, effect: E) -> Self
    where
        C: crate::core::Command,
        E: crate::effect_impl::Effect<C, D>,
    {
        self.effects.push(Box::new(move |d: Dispatcher<D>| {
            d.with_effect::<C, E>(effect)
        }));
        self
    }

    /// Build the runtime and return it along with the event bus.
    ///
    /// Returns a tuple of (Runtime, EventBus) so you can emit events
    /// to the bus while the runtime processes them.
    pub fn build(self) -> (Runtime<D>, EventBus) {
        let bus = self.bus;

        // Build dispatcher
        let mut dispatcher = match self.job_queue {
            Some(jq) => Dispatcher::with_job_queue(self.deps, bus.clone(), jq),
            None => Dispatcher::new(self.deps, bus.clone()),
        };

        // Apply effects
        for add_effect in self.effects {
            dispatcher = add_effect(dispatcher);
        }

        // Build runtime
        let runtime = Runtime {
            machines: self.machines,
            dispatcher,
            bus: bus.clone(),
            inflight: None,
            taps: TapRegistry::new(),
            #[cfg(debug_assertions)]
            audit_log: Arc::new(AuditLog::new()),
        };

        (runtime, bus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Command, Event};
    use crate::effect_impl::Effect;
    use anyhow::Result;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // Test types
    #[derive(Debug, Clone)]
    struct TestDeps {
        value: i32,
    }

    #[derive(Debug, Clone)]
    enum TestEvent {
        Start,
        Step { n: i32 },
        Done,
    }
    // Event auto-impl by blanket

    #[derive(Debug, Clone)]
    enum TestCommand {
        Process { n: i32 },
        Finish,
    }
    impl Command for TestCommand {}

    // Test machine
    struct TestMachine {
        step_count: i32,
    }

    impl TestMachine {
        fn new() -> Self {
            Self { step_count: 0 }
        }
    }

    impl Machine for TestMachine {
        type Event = TestEvent;
        type Command = TestCommand;

        fn decide(&mut self, event: &TestEvent) -> Option<TestCommand> {
            match event {
                TestEvent::Start => {
                    self.step_count = 1;
                    Some(TestCommand::Process { n: 1 })
                }
                TestEvent::Step { n } => {
                    self.step_count = *n + 1;
                    if *n < 3 {
                        Some(TestCommand::Process { n: *n + 1 })
                    } else {
                        Some(TestCommand::Finish)
                    }
                }
                TestEvent::Done => None,
            }
        }
    }

    // Test effect
    struct TestEffect {
        process_count: Arc<AtomicUsize>,
        finish_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<TestCommand, TestDeps> for TestEffect {
        type Event = TestEvent;

        async fn execute(
            &self,
            cmd: TestCommand,
            _ctx: crate::effect_impl::EffectContext<TestDeps>,
        ) -> Result<TestEvent> {
            match cmd {
                TestCommand::Process { n } => {
                    self.process_count.fetch_add(1, Ordering::Relaxed);
                    Ok(TestEvent::Step { n })
                }
                TestCommand::Finish => {
                    self.finish_count.fetch_add(1, Ordering::Relaxed);
                    Ok(TestEvent::Done)
                }
            }
        }
    }

    #[tokio::test]
    async fn test_runtime_event_to_command_flow() {
        let process_count = Arc::new(AtomicUsize::new(0));
        let finish_count = Arc::new(AtomicUsize::new(0));

        let bus = EventBus::new();

        let dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus.clone())
            .with_effect::<TestCommand, _>(TestEffect {
                process_count: process_count.clone(),
                finish_count: finish_count.clone(),
            });

        let runtime = Runtime::new(dispatcher, bus.clone()).with_machine(TestMachine::new());

        // Spawn runtime
        let handle = tokio::spawn(runtime.run());

        // Give runtime time to subscribe
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Emit start event
        bus.emit(TestEvent::Start);

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check results - should have processed 3 steps and finished
        assert_eq!(process_count.load(Ordering::Relaxed), 3);
        assert_eq!(finish_count.load(Ordering::Relaxed), 1);

        // Cleanup
        drop(bus);
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }

    #[tokio::test]
    async fn test_runtime_multiple_machines() {
        // Two machines, both handle the same event
        #[derive(Debug, Clone)]
        struct SharedEvent;
        // Event auto-impl by blanket

        // NoOp event for effects that have nothing meaningful to return
        #[derive(Debug, Clone)]
        struct NoOpEvent;

        #[derive(Debug, Clone)]
        struct MachineACommand;
        impl Command for MachineACommand {}

        #[derive(Debug, Clone)]
        struct MachineBCommand;
        impl Command for MachineBCommand {}

        struct MachineA {
            count: Arc<AtomicUsize>,
        }
        impl Machine for MachineA {
            type Event = SharedEvent;
            type Command = MachineACommand;
            fn decide(&mut self, _: &SharedEvent) -> Option<MachineACommand> {
                self.count.fetch_add(1, Ordering::Relaxed);
                Some(MachineACommand)
            }
        }

        struct MachineB {
            count: Arc<AtomicUsize>,
        }
        impl Machine for MachineB {
            type Event = SharedEvent;
            type Command = MachineBCommand;
            fn decide(&mut self, _: &SharedEvent) -> Option<MachineBCommand> {
                self.count.fetch_add(1, Ordering::Relaxed);
                Some(MachineBCommand)
            }
        }

        struct NoOpEffectA;
        #[async_trait::async_trait]
        impl Effect<MachineACommand, ()> for NoOpEffectA {
            type Event = NoOpEvent;

            async fn execute(
                &self,
                _: MachineACommand,
                _: crate::effect_impl::EffectContext<()>,
            ) -> Result<NoOpEvent> {
                Ok(NoOpEvent)
            }
        }

        struct NoOpEffectB;
        #[async_trait::async_trait]
        impl Effect<MachineBCommand, ()> for NoOpEffectB {
            type Event = NoOpEvent;

            async fn execute(
                &self,
                _: MachineBCommand,
                _: crate::effect_impl::EffectContext<()>,
            ) -> Result<NoOpEvent> {
                Ok(NoOpEvent)
            }
        }

        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));

        let bus = EventBus::new();
        let dispatcher = Dispatcher::new((), bus.clone())
            .with_effect::<MachineACommand, _>(NoOpEffectA)
            .with_effect::<MachineBCommand, _>(NoOpEffectB);

        let runtime = Runtime::new(dispatcher, bus.clone())
            .with_machine(MachineA {
                count: count_a.clone(),
            })
            .with_machine(MachineB {
                count: count_b.clone(),
            });

        let handle = tokio::spawn(runtime.run());

        // Give runtime time to subscribe
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Emit one event
        bus.emit(SharedEvent);

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Both machines should have processed the event
        assert_eq!(count_a.load(Ordering::Relaxed), 1);
        assert_eq!(count_b.load(Ordering::Relaxed), 1);

        drop(bus);
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
    }

    #[test]
    fn test_runtime_builder() {
        let process_count = Arc::new(AtomicUsize::new(0));
        let finish_count = Arc::new(AtomicUsize::new(0));

        let (runtime, _bus) = RuntimeBuilder::new(TestDeps { value: 42 })
            .with_machine(TestMachine::new())
            .with_effect::<TestCommand, _>(TestEffect {
                process_count: process_count.clone(),
                finish_count: finish_count.clone(),
            })
            .build();

        assert_eq!(runtime.machine_count(), 1);
        assert!(runtime.dispatcher().has_effect::<TestCommand>());
    }

    #[test]
    fn test_runtime_machine_count() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new((), bus.clone());

        let runtime = Runtime::new(dispatcher, bus)
            .with_machine(TestMachine::new())
            .with_machine(TestMachine::new());

        assert_eq!(runtime.machine_count(), 2);
    }

    #[test]
    fn test_runtime_debug() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::new((), bus.clone());
        let runtime = Runtime::new(dispatcher, bus);

        let debug = format!("{:?}", runtime);
        assert!(debug.contains("Runtime"));
        assert!(debug.contains("machine_count"));
    }
}
