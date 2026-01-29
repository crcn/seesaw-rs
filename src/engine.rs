//! Seesaw Engine - unified orchestration for machines, effects, and event bus.
//!
//! The Engine is the central coordinator for the seesaw architecture:
//!
//! ```text
//! Events → Machines → Commands → Dispatcher → Effects → Events
//!    ↑                                                      │
//!    └──────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use seesaw::{EngineBuilder, EventBus};
//!
//! let engine = EngineBuilder::new(deps)
//!     .with_machine(MyMachine)
//!     .with_effect::<MyCommand, _>(MyEffect)
//!     .build();
//!
//! // Start the engine (runs in background)
//! let handle = engine.start();
//!
//! // Emit events to trigger the flow
//! handle.emit(MyEvent::Started);
//!
//! // Or emit and wait for all inline work to complete
//! handle.emit_and_await(MyEvent::Started).await?;
//! ```
//!
//! # Correlation and Await
//!
//! Each `emit_and_await` call generates a unique correlation ID that propagates
//! through the entire chain of events and commands. This enables:
//! - Tracking which work belongs to which API request
//! - Awaiting completion of all inline work
//! - Error propagation back to the caller
//!
//! ```ignore
//! pub async fn create_entry(input: CreateEntry, handle: &EngineHandle) -> Result<Entry> {
//!     handle.emit_and_await(EntryEvent::CreateRequested { input }).await?;
//!     // All inline DB transactions committed, all cascading inline work done
//!     Ok(entry)
//! }
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use dashmap::DashMap;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::bus::EventBus;
use crate::core::{CorrelationId, Event};
use crate::dispatch::Dispatcher;
use crate::effect_impl::Effect;
use crate::error::{BatchOutcome, SeesawError};
use crate::machine::Machine;
use crate::runtime::Runtime;
use crate::tap::{EventTap, TapRegistry};
use crate::Command;

// =============================================================================
// Inflight Tracking
// =============================================================================

/// Entry tracking inflight work for a single correlation ID.
pub(crate) struct InflightEntry {
    /// Count of pending work items (events being processed + inline commands executing)
    count: AtomicUsize,
    /// Number of tasks waiting for completion (via wait_zero)
    waiters: AtomicUsize,
    /// Notifier for waiters when count hits zero
    notify: Notify,
    /// First error encountered (if any)
    first_error: Mutex<Option<anyhow::Error>>,
}

impl InflightEntry {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            notify: Notify::new(),
            first_error: Mutex::new(None),
        }
    }
}

/// RAII guard that tracks waiters for an InflightEntry.
///
/// Increments the waiter count on creation, decrements on drop.
/// This ensures proper cleanup even if the async task is cancelled.
pub(crate) struct WaiterGuard {
    entry: Option<Arc<InflightEntry>>,
}

impl WaiterGuard {
    fn new(entry: Option<Arc<InflightEntry>>) -> Self {
        if let Some(ref e) = entry {
            e.waiters.fetch_add(1, Ordering::AcqRel);
        }
        Self { entry }
    }
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Some(ref entry) = self.entry {
            entry.waiters.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Tracks inflight work per correlation ID.
///
/// Used by the runtime and dispatcher to track pending inline work,
/// and by `EngineHandle` to await completion.
#[derive(Default)]
pub struct InflightTracker {
    entries: DashMap<CorrelationId, Arc<InflightEntry>>,
}

impl InflightTracker {
    /// Create a new inflight tracker.
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Get or create an entry for the given correlation ID.
    pub(crate) fn get_or_create(&self, cid: CorrelationId) -> Arc<InflightEntry> {
        self.entries
            .entry(cid)
            .or_insert_with(|| Arc::new(InflightEntry::new()))
            .clone()
    }

    /// Increment the inflight count for a correlation ID.
    ///
    /// Called when:
    /// - An event is about to be emitted (by edge or effect)
    /// - Inline commands are about to be executed
    pub fn inc(&self, cid: CorrelationId, n: usize) {
        let entry = self.get_or_create(cid);
        entry.count.fetch_add(n, Ordering::AcqRel);
    }

    /// Decrement the inflight count for a correlation ID.
    ///
    /// When the count hits zero:
    /// - Notifies all waiters
    /// - If no error or no waiters, removes the entry immediately
    /// - If error AND waiters exist, leaves entry for `wait_zero` to clean up
    ///
    /// Called when:
    /// - Event processing completes
    /// - Inline command batch completes
    pub fn dec(&self, cid: CorrelationId, n: usize) {
        if let Some(entry) = self.entries.get(&cid) {
            let prev = entry.count.fetch_sub(n, Ordering::AcqRel);
            if prev == n {
                // Hit zero - notify waiters
                entry.notify.notify_waiters();

                // Check if anyone is waiting for this result
                let has_waiters = entry.waiters.load(Ordering::Acquire) > 0;

                // Only keep error entries if someone is waiting to receive the error.
                // This prevents memory leaks from fire-and-forget emits that error.
                // Handle poisoned mutex gracefully - treat as "has error" to avoid data loss
                let has_error = entry
                    .first_error
                    .lock()
                    .map(|guard| guard.is_some())
                    .unwrap_or(true); // Poisoned = treat as error

                drop(entry); // Release read lock before potentially removing

                // Remove if: no error, OR no one is waiting for the error
                if !has_error || !has_waiters {
                    self.entries.remove(&cid);
                }
            }
        }
    }

    /// Record an error for a correlation ID.
    ///
    /// Only the first error is recorded (subsequent errors are ignored).
    /// The error will be returned by `wait_zero`.
    pub fn record_error(&self, cid: CorrelationId, err: anyhow::Error) {
        if let Some(entry) = self.entries.get(&cid) {
            // Handle poisoned mutex - recover the guard and continue
            let mut guard = match entry.first_error.lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    warn!(cid = %cid, "mutex was poisoned, recovering");
                    poisoned.into_inner()
                }
            };
            if guard.is_none() {
                warn!(cid = %cid, error = %err, "recording error for correlation");
                *guard = Some(err);
            }
        }
    }

    /// Wait for the inflight count to reach zero.
    ///
    /// Returns `Ok(())` if all work completed successfully.
    /// Returns `Err` if any inline command failed.
    ///
    /// Handles edge cases:
    /// - Entry already removed (work completed): returns Ok
    /// - Notify is edge-triggered: loops with recheck
    pub async fn wait_zero(&self, cid: CorrelationId) -> Result<()> {
        // Track that we're waiting, so error entries aren't cleaned up prematurely.
        // The WaiterGuard decrements on drop, ensuring cleanup even on cancel/panic.
        let entry_for_guard = self.entries.get(&cid).map(|e| e.clone());
        let _guard = WaiterGuard::new(entry_for_guard);

        loop {
            // Clone the Arc so we own the entry independent of the map
            let entry = match self.entries.get(&cid) {
                None => {
                    // Entry was removed - work completed successfully
                    return Ok(());
                }
                Some(entry_ref) => entry_ref.clone(),
            };

            // Register for notification BEFORE checking count
            // This prevents race where dec() notifies between our check and await
            let notified = entry.notify.notified();

            if entry.count.load(Ordering::Acquire) == 0 {
                // Count is zero - check for error before returning
                // Handle poisoned mutex gracefully
                let err = match entry.first_error.lock() {
                    Ok(mut guard) => guard.take(),
                    Err(poisoned) => poisoned.into_inner().take(),
                };
                self.entries.remove(&cid); // Clean up entry
                return match err {
                    Some(e) => Err(e),
                    None => Ok(()),
                };
            }

            // Count > 0, wait for notification
            notified.await;
            // Loop back to recheck (Notify is edge-triggered)
        }
    }

    /// Check if there's pending work (count > 0) for a correlation ID.
    ///
    /// Returns `true` if the cid has an entry with count > 0.
    /// Returns `false` if no entry exists or count is 0.
    pub fn has_pending_work(&self, cid: CorrelationId) -> bool {
        self.entries
            .get(&cid)
            .map(|e| e.count.load(Ordering::Acquire) > 0)
            .unwrap_or(false)
    }

    /// Get the number of active correlations (for debugging).
    pub fn active_count(&self) -> usize {
        self.entries.len()
    }

    /// Register a waiter for a correlation ID.
    ///
    /// Call this BEFORE emitting an event if you plan to call wait_zero.
    /// Returns a guard that decrements the waiter count on drop.
    pub fn register_waiter(&self, cid: CorrelationId) -> WaiterGuard {
        let entry = self.get_or_create(cid);
        WaiterGuard::new(Some(entry))
    }

    /// Begin tracking a batch of commands.
    ///
    /// Increments the inflight count by `size` and returns an `InflightBatch`
    /// receipt that must be completed with the actual outcome. The completion
    /// will decrement based on how many commands actually succeeded.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let batch = inflight.begin_batch(cid, cmds.len());
    /// let outcome = effect.execute_batch(cmds, ctx).await;
    /// let outcome = batch.complete(outcome);
    /// ```
    pub fn begin_batch(self: &Arc<Self>, cid: CorrelationId, size: usize) -> InflightBatch {
        self.inc(cid, size);
        InflightBatch {
            tracker: self.clone(),
            cid,
            size,
            completed: false,
        }
    }
}

impl std::fmt::Debug for InflightTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InflightTracker")
            .field("active_correlations", &self.entries.len())
            .finish()
    }
}

// =============================================================================
// Inflight Guard (RAII)
// =============================================================================

/// RAII guard for decrementing inflight count on drop.
///
/// Ensures the count is decremented even if the code panics,
/// preventing hung `emit_and_await` calls.
pub struct InflightGuard {
    tracker: Arc<InflightTracker>,
    cid: CorrelationId,
    count: usize,
}

impl InflightGuard {
    /// Create a guard for inline command execution.
    ///
    /// Increments the count now, decrements on drop.
    pub fn for_commands(tracker: Arc<InflightTracker>, cid: CorrelationId, count: usize) -> Self {
        tracker.inc(cid, count);
        Self {
            tracker,
            cid,
            count,
        }
    }

    /// Create a guard for event processing.
    ///
    /// Does NOT increment (caller already did via emit).
    /// Decrements on drop.
    pub fn for_event(tracker: Arc<InflightTracker>, cid: CorrelationId) -> Self {
        Self {
            tracker,
            cid,
            count: 1,
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.tracker.dec(self.cid, self.count);
    }
}

// =============================================================================
// Inflight Batch (Receipt Pattern)
// =============================================================================

/// Receipt for batch command execution with explicit completion.
///
/// This implements the "receipt pattern" for inflight tracking:
/// 1. Call `begin_batch()` to get a receipt
/// 2. Execute the batch
/// 3. Call `complete()` with the outcome to decrement correctly
///
/// Unlike `InflightGuard`, this does NOT decrement on drop. If dropped
/// without calling `complete()`, it logs a warning but leaves the inflight
/// count elevated (timeout will eventually clean up).
///
/// # Why no automatic cleanup?
///
/// Seesaw's core principle: **expose truth, not timing**.
///
/// If a batch panics mid-execution, we don't know how many commands succeeded.
/// Guessing would lie to the system. Instead:
/// - We warn loudly (aids debugging)
/// - We let the timeout backstop handle it (correct behavior)
/// - `emit_and_await` returns an error after timeout (honest outcome)
///
/// # Example
///
/// ```ignore
/// let batch = inflight.begin_batch(cid, cmds.len());
/// let outcome = effect.execute_batch(cmds, ctx).await;
/// let outcome = batch.complete(outcome);
/// ```
pub struct InflightBatch {
    tracker: Arc<InflightTracker>,
    cid: CorrelationId,
    size: usize,
    completed: bool,
}

impl InflightBatch {
    /// Complete the batch and decrement inflight count.
    ///
    /// This is the only way to properly close a batch. The inflight count
    /// is decremented by the full batch size because all commands have been
    /// processed (even if some failed). The outcome reports what happened
    /// but doesn't affect the decrement - work is done regardless of success.
    ///
    /// Errors should be recorded separately via `InflightTracker::record_error`.
    pub fn complete(mut self, outcome: BatchOutcome) -> BatchOutcome {
        self.completed = true;
        // Decrement by full size: all commands were processed (success or failure)
        self.tracker.dec(self.cid, self.size);
        outcome
    }
}

impl Drop for InflightBatch {
    fn drop(&mut self) {
        if !self.completed {
            warn!(
                cid = %self.cid,
                batch_size = self.size,
                "InflightBatch dropped without complete(); \
                 inflight counts may be stuck until timeout (likely panic)"
            );
        }
    }
}

// =============================================================================
// Engine
// =============================================================================

/// The seesaw Engine coordinates machines, effects, and event routing.
///
/// This is the main entry point for running a seesaw application.
/// Use `EngineBuilder` to construct an engine with machines and effects.
pub struct Engine<D> {
    runtime: Runtime<D>,
    bus: EventBus,
    inflight: Arc<InflightTracker>,
}

impl<D: Send + Sync + 'static> Engine<D> {
    /// Create a new engine builder.
    pub fn builder(deps: D) -> EngineBuilder<D> {
        EngineBuilder::new(deps)
    }

    /// Get the event bus for emitting events.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Get the inflight tracker.
    pub fn inflight(&self) -> &Arc<InflightTracker> {
        &self.inflight
    }

    /// Emit an event to the bus.
    ///
    /// This triggers the machine → command → effect flow.
    pub fn emit<E: Event>(&self, event: E) {
        self.bus.emit(event);
    }

    /// Start the engine, running the runtime in the background.
    ///
    /// Returns a handle that can be used to emit events and wait for completion.
    pub fn start(self) -> EngineHandle {
        info!("starting seesaw engine");

        let handle = tokio::spawn(self.runtime.run());

        EngineHandle {
            bus: self.bus,
            inflight: self.inflight,
            handle,
        }
    }
}

// =============================================================================
// Engine Handle
// =============================================================================

/// Handle to a running engine.
///
/// The handle provides access to the event bus for emitting events
/// while the engine runs in the background.
///
/// # Fire-and-Forget vs Await
///
/// - `emit()`: Fire-and-forget. Returns immediately, work happens in background.
/// - `emit_and_await()`: Waits for all inline commands to complete before returning.
///
/// Use `emit_and_await` for API endpoints where you need strong consistency
/// (e.g., the data must be in the database before returning 200 OK).
///
/// Use `emit()` for fire-and-forget scenarios (notifications, analytics, etc.).
pub struct EngineHandle {
    bus: EventBus,
    inflight: Arc<InflightTracker>,
    handle: JoinHandle<()>,
}

impl EngineHandle {
    /// Get the event bus for fire-and-forget emission.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Get the inflight tracker (for advanced use cases).
    pub fn inflight(&self) -> &Arc<InflightTracker> {
        &self.inflight
    }

    /// Emit an event to the bus (fire-and-forget).
    ///
    /// Returns immediately. The event will be processed asynchronously.
    /// Use this for notifications, analytics, or other non-critical side effects.
    pub fn emit<E: Event>(&self, event: E) {
        self.bus.emit(event);
    }

    /// Emit an event and wait for all inline commands to complete.
    ///
    /// Uses a default timeout of 30 seconds.
    ///
    /// # Returns
    ///
    /// - `Ok(())` if all inline work completed successfully
    /// - `Err` if any inline command failed, or timeout was reached
    ///
    /// # Example
    ///
    /// ```ignore
    /// pub async fn create_user(input: CreateUser, handle: &EngineHandle) -> Result<User> {
    ///     handle.emit_and_await(UserEvent::CreateRequested { input }).await?;
    ///     // User is now in the database
    ///     Ok(user)
    /// }
    /// ```
    pub async fn emit_and_await<E: Event>(&self, event: E) -> Result<()> {
        self.emit_and_await_timeout(event, Duration::from_secs(30))
            .await
    }

    /// Abort the engine's background task.
    ///
    /// Call this during test teardown to release resources held by the engine.
    /// After calling this, the engine will no longer process events.
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Emit an event and wait for all inline commands to complete, with custom timeout.
    ///
    /// # Returns
    ///
    /// - `Ok(())` if all inline work completed successfully
    /// - `Err` if any inline command failed, or timeout was reached
    pub async fn emit_and_await_timeout<E: Event>(
        &self,
        event: E,
        timeout: Duration,
    ) -> Result<()> {
        let cid = CorrelationId::new();

        // Register waiter BEFORE emitting to avoid race where:
        // 1. Event is emitted
        // 2. Processed quickly, error recorded
        // 3. dec() sees no waiters, cleans up error entry
        // 4. wait_zero finds no entry, returns Ok instead of error
        let _waiter_guard = self.inflight.register_waiter(cid);

        // Increment for the event we're about to emit
        // (Runtime will decrement when it finishes processing)
        self.inflight.inc(cid, 1);

        // Emit the event with correlation
        self.bus.emit_with_correlation(event, cid);

        // Wait for all inline work to complete
        match tokio::time::timeout(timeout, self.inflight.wait_zero(cid)).await {
            Ok(result) => result,
            Err(_) => {
                // Timeout - clean up the entry to prevent leak
                self.inflight.entries.remove(&cid);
                Err(SeesawError::Timeout { duration: timeout }.into())
            }
        }
    }
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle")
            .field("inflight", &self.inflight)
            .finish_non_exhaustive()
    }
}

// =============================================================================
// Engine Builder
// =============================================================================

/// Builder for constructing an Engine with machines, effects, and taps.
///
/// # Example
///
/// ```ignore
/// let engine = EngineBuilder::new(my_deps)
///     .with_bus(shared_bus)  // Optional: use existing bus
///     .with_machine(StateMachine::new())
///     .with_machine(AuditMachine::new())
///     .with_effect::<CreateCommand, _>(CreateEffect)
///     .with_effect::<DeleteCommand, _>(DeleteEffect)
///     .with_event_tap::<MyEvent, _>(NatsPublishTap::new(client))
///     .build();
/// ```
pub struct EngineBuilder<D> {
    deps: Arc<D>,
    bus: EventBus,
    inflight: Arc<InflightTracker>,
    job_queue: Option<Arc<dyn crate::dispatch::JobQueue>>,
    machines: Vec<Box<dyn FnOnce(Runtime<D>) -> Runtime<D> + Send>>,
    effects: Vec<Box<dyn FnOnce(Dispatcher<D>) -> Dispatcher<D> + Send>>,
    taps: TapRegistry,
}

impl<D: Send + Sync + 'static> EngineBuilder<D> {
    /// Create a new engine builder with the given dependencies.
    pub fn new(deps: D) -> Self {
        Self {
            deps: Arc::new(deps),
            bus: EventBus::new(),
            inflight: Arc::new(InflightTracker::new()),
            job_queue: None,
            machines: Vec::new(),
            effects: Vec::new(),
            taps: TapRegistry::new(),
        }
    }

    /// Create a new engine builder with Arc-wrapped dependencies.
    ///
    /// Use this when you need to share the deps with other parts of the system.
    pub fn with_arc(deps: Arc<D>) -> Self {
        Self {
            deps,
            bus: EventBus::new(),
            inflight: Arc::new(InflightTracker::new()),
            job_queue: None,
            machines: Vec::new(),
            effects: Vec::new(),
            taps: TapRegistry::new(),
        }
    }

    /// Use an existing event bus instead of creating a new one.
    ///
    /// This is useful when you need to share the bus with other systems
    /// or when bridging from a legacy event system.
    pub fn with_bus(mut self, bus: EventBus) -> Self {
        self.bus = bus;
        self
    }

    /// Use an existing inflight tracker instead of creating a new one.
    ///
    /// This is useful when you need to share the tracker with other systems.
    pub fn with_inflight(mut self, inflight: Arc<InflightTracker>) -> Self {
        self.inflight = inflight;
        self
    }

    /// Set a job queue for background command execution.
    ///
    /// When a command has `ExecutionMode::Background` or `ExecutionMode::Scheduled`,
    /// the dispatcher will route it to this job queue instead of executing inline.
    pub fn with_job_queue(mut self, job_queue: Arc<dyn crate::dispatch::JobQueue>) -> Self {
        self.job_queue = Some(job_queue);
        self
    }

    /// Register a machine that listens to events and emits commands.
    ///
    /// Machines are called in the order they are registered.
    /// Each machine can independently decide whether to emit a command
    /// based on the event it receives.
    pub fn with_machine<M>(mut self, machine: M) -> Self
    where
        M: Machine + 'static,
    {
        self.machines
            .push(Box::new(move |runtime| runtime.with_machine(machine)));
        self
    }

    /// Register an effect handler for a command type.
    ///
    /// When a command of type `C` is dispatched, the registered effect
    /// will be called to execute it.
    pub fn with_effect<C, E>(mut self, effect: E) -> Self
    where
        C: Command,
        E: Effect<C, D>,
    {
        self.effects.push(Box::new(move |dispatcher| {
            dispatcher.with_effect::<C, E>(effect)
        }));
        self
    }

    /// Register an event tap for observing events.
    ///
    /// Taps run **after** effects complete. They observe committed facts
    /// and cannot emit new events or access dependencies.
    ///
    /// Use taps for:
    /// - Publishing to NATS/Kafka
    /// - Sending webhooks
    /// - Recording metrics
    /// - Audit logging
    ///
    /// # Example
    ///
    /// ```ignore
    /// .with_event_tap::<EntryEvent, _>(NatsPublishTap::new(client))
    /// .with_event_tap::<DeckEvent, _>(MetricsTap::new())
    /// ```
    pub fn with_event_tap<E, T>(mut self, tap: T) -> Self
    where
        E: Event + Clone,
        T: EventTap<E>,
    {
        self.taps.register::<E, T>(tap, std::any::type_name::<T>());
        self
    }

    /// Build the engine.
    ///
    /// This creates the dispatcher, registers effects, builds the runtime,
    /// and connects everything to the event bus.
    pub fn build(self) -> Engine<D> {
        // Build dispatcher with effects (use from_arc since deps is already Arc)
        // Include job queue if configured for background command execution
        let mut dispatcher = match self.job_queue {
            Some(jq) => Dispatcher::from_arc_with_job_queue(self.deps, self.bus.clone(), jq),
            None => Dispatcher::from_arc(self.deps, self.bus.clone()),
        };
        for add_effect in self.effects {
            dispatcher = add_effect(dispatcher);
        }

        // Build runtime with machines, taps, and inflight tracker
        let mut runtime = Runtime::new(dispatcher, self.bus.clone())
            .with_inflight(self.inflight.clone())
            .with_taps(self.taps);
        for add_machine in self.machines {
            runtime = add_machine(runtime);
        }

        Engine {
            runtime,
            bus: self.bus,
            inflight: self.inflight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect_impl::EffectContext;
    use anyhow::Result;
    use std::time::Duration;

    // ==========================================================================
    // Inflight Tracker Tests
    // ==========================================================================

    #[test]
    fn test_inflight_tracker_basic() {
        let tracker = InflightTracker::new();
        let cid = CorrelationId::new();

        // Create entry and increment
        tracker.inc(cid, 1);

        // Entry should exist with count 1
        let entry = tracker.entries.get(&cid).unwrap();
        assert_eq!(entry.count.load(Ordering::Relaxed), 1);
        drop(entry);

        // Decrement to zero
        tracker.dec(cid, 1);

        // Entry should be removed
        assert!(tracker.entries.get(&cid).is_none());
    }

    #[test]
    fn test_inflight_tracker_multiple_inc_dec() {
        let tracker = InflightTracker::new();
        let cid = CorrelationId::new();

        tracker.inc(cid, 1);
        tracker.inc(cid, 2);

        let entry = tracker.entries.get(&cid).unwrap();
        assert_eq!(entry.count.load(Ordering::Relaxed), 3);
        drop(entry);

        tracker.dec(cid, 2);

        let entry = tracker.entries.get(&cid).unwrap();
        assert_eq!(entry.count.load(Ordering::Relaxed), 1);
        drop(entry);

        tracker.dec(cid, 1);
        assert!(tracker.entries.get(&cid).is_none());
    }

    #[tokio::test]
    async fn test_wait_zero_immediate() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        // Entry doesn't exist - should return immediately
        let result = tracker.wait_zero(cid).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wait_zero_with_work() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        tracker.inc(cid, 1);

        let tracker_clone = tracker.clone();
        let handle = tokio::spawn(async move {
            // Simulate work
            tokio::time::sleep(Duration::from_millis(10)).await;
            tracker_clone.dec(cid, 1);
        });

        let result = tracker.wait_zero(cid).await;
        assert!(result.is_ok());

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_zero_with_error() {
        // This tests the real-world pattern: wait_zero is called BEFORE dec
        // (which is how emit_and_await works - it starts waiting before work completes)
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        tracker.inc(cid, 1);
        tracker.record_error(cid, anyhow::anyhow!("test error"));

        // Start waiting BEFORE dec (this is what emit_and_await does)
        let tracker_clone = tracker.clone();
        let wait_handle = tokio::spawn(async move { tracker_clone.wait_zero(cid).await });

        // Give the waiter time to register
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Now dec - waiter exists, so error entry is preserved
        tracker.dec(cid, 1);

        let result = wait_handle.await.unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test error"));
    }

    #[tokio::test]
    async fn test_wait_zero_without_waiter_cleans_error() {
        // If no one is waiting when dec() hits zero, error is discarded
        // to prevent memory leaks in fire-and-forget patterns
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        tracker.inc(cid, 1);
        tracker.record_error(cid, anyhow::anyhow!("test error"));
        tracker.dec(cid, 1);
        // No waiter existed, so entry is cleaned up (error discarded)

        // wait_zero called AFTER dec: entry gone, returns Ok
        let result = tracker.wait_zero(cid).await;
        assert!(result.is_ok(), "Expected Ok when entry already cleaned up");
    }

    #[test]
    fn test_inflight_guard_for_commands() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        {
            let _guard = InflightGuard::for_commands(tracker.clone(), cid, 3);
            let entry = tracker.entries.get(&cid).unwrap();
            assert_eq!(entry.count.load(Ordering::Relaxed), 3);
        }

        // Guard dropped, entry should be removed
        assert!(tracker.entries.get(&cid).is_none());
    }

    #[test]
    fn test_inflight_guard_for_event() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        // Simulate emit doing the increment
        tracker.inc(cid, 1);

        {
            let _guard = InflightGuard::for_event(tracker.clone(), cid);
            let entry = tracker.entries.get(&cid).unwrap();
            assert_eq!(entry.count.load(Ordering::Relaxed), 1);
        }

        // Guard dropped, entry should be removed
        assert!(tracker.entries.get(&cid).is_none());
    }

    #[test]
    fn test_inflight_guard_panic_safety() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = InflightGuard::for_commands(tracker.clone(), cid, 1);
            panic!("simulated panic");
        }));

        assert!(result.is_err());

        // Entry should still be removed despite panic
        assert!(tracker.entries.get(&cid).is_none());
    }

    #[test]
    fn test_first_error_wins() {
        let tracker = InflightTracker::new();
        let cid = CorrelationId::new();

        tracker.inc(cid, 1);
        tracker.record_error(cid, anyhow::anyhow!("first error"));
        tracker.record_error(cid, anyhow::anyhow!("second error"));

        let entry = tracker.entries.get(&cid).unwrap();
        let err = entry.first_error.lock().expect("mutex not poisoned");
        assert!(err.as_ref().unwrap().to_string().contains("first error"));
    }

    // ==========================================================================
    // Engine Tests
    // ==========================================================================

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
            _ctx: EffectContext<TestDeps>,
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
    async fn test_engine_builder_creates_engine() {
        let process_count = Arc::new(AtomicUsize::new(0));
        let finish_count = Arc::new(AtomicUsize::new(0));

        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_machine(TestMachine { step_count: 0 })
            .with_effect::<TestCommand, _>(TestEffect {
                process_count: process_count.clone(),
                finish_count: finish_count.clone(),
            })
            .build();

        // Start engine
        let handle = engine.start();

        // Give runtime time to subscribe
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Emit start event
        handle.emit(TestEvent::Start);

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check results - should have processed 3 steps and finished
        assert_eq!(process_count.load(Ordering::Relaxed), 3);
        assert_eq!(finish_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_engine_builder_with_arc() {
        let deps = Arc::new(TestDeps { value: 42 });
        let _engine = EngineBuilder::with_arc(deps).build();
    }

    #[test]
    fn test_engine_builder_with_bus() {
        let bus = EventBus::new();
        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_bus(bus.clone())
            .build();

        // The engine's bus should be the same instance
        // (can't directly compare, but we can verify it works)
        bus.emit(TestEvent::Start);
    }

    // ==========================================================================
    // Effect Error Handling Tests
    // ==========================================================================

    // Types for error handling tests
    #[derive(Debug, Clone)]
    struct ErrorTriggerEvent;

    #[derive(Debug, Clone)]
    struct ErrorResultEvent;

    #[derive(Debug, Clone)]
    struct ErrorCommand;
    impl Command for ErrorCommand {}

    // Machine that always emits ErrorCommand when it sees ErrorTriggerEvent
    struct ErrorTriggerMachine;
    impl Machine for ErrorTriggerMachine {
        type Event = ErrorTriggerEvent;
        type Command = ErrorCommand;

        fn decide(&mut self, _event: &ErrorTriggerEvent) -> Option<ErrorCommand> {
            Some(ErrorCommand)
        }
    }

    // Effect that always returns an error
    struct AlwaysFailsEffect;

    #[async_trait::async_trait]
    impl Effect<ErrorCommand, TestDeps> for AlwaysFailsEffect {
        type Event = ErrorResultEvent;

        async fn execute(
            &self,
            _cmd: ErrorCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<ErrorResultEvent> {
            Err(anyhow::anyhow!("authorization denied"))
        }
    }

    /// Test that emit_and_await returns quickly when an effect returns an error.
    ///
    /// This test reproduces a bug where effect errors cause emit_and_await to hang
    /// until timeout because:
    /// 1. The inflight count is incremented for the command batch
    /// 2. When the effect fails, BatchOutcome::Partial { succeeded: 0 } is used
    /// 3. succeeded_count(1) returns 0, so dec(cid, 0) is called
    /// 4. The inflight count never reaches zero
    ///
    /// Expected behavior: emit_and_await should return an error quickly, not timeout.
    #[tokio::test]
    async fn test_emit_and_await_returns_error_on_effect_failure() {
        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_machine(ErrorTriggerMachine)
            .with_effect::<ErrorCommand, _>(AlwaysFailsEffect)
            .build();

        let handle = engine.start();

        // Give runtime time to subscribe
        tokio::time::sleep(Duration::from_millis(10)).await;

        // This should return an error quickly, NOT timeout after 500ms
        let start = std::time::Instant::now();
        let result = handle
            .emit_and_await_timeout(ErrorTriggerEvent, Duration::from_millis(500))
            .await;

        let elapsed = start.elapsed();

        // The operation should complete quickly (< 100ms), not timeout at 500ms
        assert!(
            elapsed < Duration::from_millis(100),
            "emit_and_await took {:?}, expected < 100ms. \
             This indicates the inflight count is not being decremented on error.",
            elapsed
        );

        // And it should return an error (the authorization denied error)
        assert!(
            result.is_err(),
            "Expected an error to be returned, got Ok(())"
        );

        handle.abort();
    }

    // ==========================================================================
    // Batch Error Handling Tests
    // ==========================================================================

    // Types for batch error tests
    #[derive(Debug, Clone)]
    struct BatchTriggerEvent {
        count: usize,
    }

    #[derive(Debug, Clone)]
    struct BatchResultEvent {
        index: usize,
    }

    #[derive(Debug, Clone)]
    struct BatchCommand {
        index: usize,
        should_fail: bool,
    }
    impl Command for BatchCommand {}

    // Machine that emits multiple commands for a single event
    struct BatchMachine {
        fail_at: Option<usize>,
    }

    impl Machine for BatchMachine {
        type Event = BatchTriggerEvent;
        type Command = BatchCommand;

        fn decide(&mut self, event: &BatchTriggerEvent) -> Option<BatchCommand> {
            // This machine only emits one command per decide() call
            // To test batch behavior, we need multiple machines or a different approach
            // For now, emit one command that may or may not fail
            let should_fail = self.fail_at == Some(0);
            Some(BatchCommand {
                index: 0,
                should_fail,
            })
        }
    }

    // Effect that fails based on command's should_fail flag
    struct ConditionalFailEffect {
        executed: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<BatchCommand, TestDeps> for ConditionalFailEffect {
        type Event = BatchResultEvent;

        async fn execute(
            &self,
            cmd: BatchCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<BatchResultEvent> {
            self.executed.fetch_add(1, Ordering::SeqCst);
            if cmd.should_fail {
                Err(anyhow::anyhow!(
                    "command {} failed intentionally",
                    cmd.index
                ))
            } else {
                Ok(BatchResultEvent { index: cmd.index })
            }
        }
    }

    /// Test that emit_and_await works correctly when effect succeeds.
    /// This is the baseline test to ensure success path works.
    #[tokio::test]
    async fn test_emit_and_await_success_baseline() {
        let executed = Arc::new(AtomicUsize::new(0));

        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_machine(BatchMachine { fail_at: None })
            .with_effect::<BatchCommand, _>(ConditionalFailEffect {
                executed: executed.clone(),
            })
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        let start = std::time::Instant::now();
        let result = handle
            .emit_and_await_timeout(BatchTriggerEvent { count: 1 }, Duration::from_millis(500))
            .await;
        let elapsed = start.elapsed();

        // Success case should complete quickly
        assert!(
            elapsed < Duration::from_millis(100),
            "Success case took {:?}, expected < 100ms",
            elapsed
        );
        assert!(result.is_ok(), "Expected Ok, got {:?}", result);
        assert_eq!(executed.load(Ordering::SeqCst), 1);

        handle.abort();
    }

    /// Test that the error message is propagated through emit_and_await.
    #[tokio::test]
    async fn test_emit_and_await_error_message_propagation() {
        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_machine(ErrorTriggerMachine)
            .with_effect::<ErrorCommand, _>(AlwaysFailsEffect)
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        let result = handle
            .emit_and_await_timeout(ErrorTriggerEvent, Duration::from_millis(500))
            .await;

        // When the fix is applied, this should contain the actual error
        // For now, it times out, so we check for timeout error
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        // After fix: should contain "authorization denied" or similar
        // Currently: contains "timed out"
        println!("Error message: {}", err_msg);

        handle.abort();
    }

    /// Test multiple sequential emit_and_await calls with errors.
    /// Ensures error handling doesn't leave state corrupted.
    #[tokio::test]
    async fn test_emit_and_await_multiple_errors_sequential() {
        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_machine(ErrorTriggerMachine)
            .with_effect::<ErrorCommand, _>(AlwaysFailsEffect)
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Run multiple emit_and_await calls sequentially
        for i in 0..3 {
            let start = std::time::Instant::now();
            let result = handle
                .emit_and_await_timeout(ErrorTriggerEvent, Duration::from_millis(200))
                .await;
            let elapsed = start.elapsed();

            assert!(
                elapsed < Duration::from_millis(100),
                "Iteration {} took {:?}, expected < 100ms. State may be corrupted from previous error.",
                i,
                elapsed
            );
            assert!(result.is_err(), "Iteration {} expected error", i);
        }

        handle.abort();
    }

    /// Test that inflight tracker is properly cleaned up after errors.
    #[tokio::test]
    async fn test_inflight_tracker_cleanup_after_error() {
        let inflight = Arc::new(InflightTracker::new());

        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_inflight(inflight.clone())
            .with_machine(ErrorTriggerMachine)
            .with_effect::<ErrorCommand, _>(AlwaysFailsEffect)
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Check initial state
        assert_eq!(
            inflight.active_count(),
            0,
            "Should have no active correlations initially"
        );

        let _ = handle
            .emit_and_await_timeout(ErrorTriggerEvent, Duration::from_millis(200))
            .await;

        // After completion (success or failure), inflight should be cleaned up
        // Give a moment for async cleanup
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            inflight.active_count(),
            0,
            "Should have no active correlations after error. \
             Leaked correlations indicate the count never reached zero."
        );

        handle.abort();
    }

    // ==========================================================================
    // No-Command Path Tests (event with no matching machine)
    // ==========================================================================

    /// Test emit_and_await when no machine handles the event (no command emitted).
    /// This should complete immediately since there's no work to do.
    #[tokio::test]
    async fn test_emit_and_await_no_command_path() {
        // Use an event type that no machine handles
        #[derive(Debug, Clone)]
        struct UnhandledEvent;

        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_machine(ErrorTriggerMachine) // Only handles ErrorTriggerEvent
            .with_effect::<ErrorCommand, _>(AlwaysFailsEffect)
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        let start = std::time::Instant::now();
        let result = handle
            .emit_and_await_timeout(UnhandledEvent, Duration::from_millis(500))
            .await;
        let elapsed = start.elapsed();

        // No machine handles this event, so no command is emitted
        // Should complete quickly with Ok
        assert!(
            elapsed < Duration::from_millis(100),
            "No-command path took {:?}, expected < 100ms",
            elapsed
        );
        assert!(result.is_ok(), "No-command path should succeed");

        handle.abort();
    }
}
