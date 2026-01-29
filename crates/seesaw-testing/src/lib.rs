//! Testing utilities for seesaw machines and workflows.
//!
//! This module provides ergonomic helpers for testing state machines,
//! including macros for concise transition tests and fluent builders
//! for complex test scenarios.
//!
//! # Feature Flag
//!
//! This module is only available with the `testing` feature:
//!
//! ```toml
//! [dev-dependencies]
//! seesaw = { version = "0.1", features = ["testing"] }
//! ```
//!
//! # Quick Start
//!
//! ## Using `assert_workflow!` Macro
//!
//! ```ignore
//! use seesaw_core::testing::assert_workflow;
//!
//! let mut machine = MyMachine::new();
//!
//! assert_workflow!(
//!     machine,
//!     Event::Start => Some(Command::Begin),
//!     Event::Step { n: 1 } => Some(Command::Process { n: 1 }),
//!     Event::Done => None,
//! );
//! ```
//!
//! ## Using Fluent Builder
//!
//! ```ignore
//! use seesaw_core::testing::WorkflowTest;
//!
//! WorkflowTest::new(MyMachine::new())
//!     .given(Event::Start)
//!     .expect_some()
//!     .then(Event::Step { n: 1 })
//!     .expect_command(|cmd| matches!(cmd, Some(Command::Process { n: 1 })))
//!     .then(Event::Done)
//!     .expect_none()
//!     .assert_state(|m| m.steps_completed == 2);
//! ```
//!
//! ## Using `EventLatch` for Fan-Out Tests
//!
//! ```ignore
//! use seesaw_core::testing::EventLatch;
//!
//! let latch = EventLatch::new(3);  // Expect 3 events
//!
//! bus.tap::<NotificationEvent>(|_| latch.dec());
//!
//! engine.emit(trigger_event);
//!
//! latch.await_zero().await;  // Wait for all 3 events
//! assert!(all_notifications_sent());
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use uuid::Uuid;

use seesaw_core::{JobQueue, JobSpec, Machine};

/// Asserts a sequence of event → command transitions for a machine.
///
/// This macro provides a concise way to test state machine transitions.
/// Each line specifies an event and the expected command result.
///
/// # Syntax
///
/// ```ignore
/// assert_workflow!(
///     machine_instance,
///     event1 => expected_command1,
///     event2 => expected_command2,
///     // ...
/// );
/// ```
///
/// # Example
///
/// ```ignore
/// use seesaw_core::testing::assert_workflow;
///
/// #[test]
/// fn test_bake_workflow() {
///     let mut machine = BakeMachine::new();
///     let deck_id = Uuid::new_v4();
///     let recipe_id = Uuid::new_v4();
///
///     assert_workflow!(
///         machine,
///         BakeEvent::Requested { deck_id, recipe_id } =>
///             Some(BakeCommand::SetupLoaf { deck_id, recipe_id }),
///         BakeEvent::LoafReady { loaf_id: Uuid::new_v4() } =>
///             Some(BakeCommand::GenerateCards { loaf_id: _ }),
///         BakeEvent::Complete { deck_id } =>
///             None,
///     );
/// }
/// ```
///
/// # Panics
///
/// Panics if any transition doesn't match the expected command.
#[macro_export]
macro_rules! assert_workflow {
    ($machine:expr, $($event:expr => $expected:expr),+ $(,)?) => {
        $(
            let actual = $machine.decide(&$event);
            assert_eq!(
                actual, $expected,
                "Unexpected command for event {:?}\n  expected: {:?}\n  actual: {:?}",
                $event, $expected, actual
            );
        )+
    };
}

/// Fluent test builder for machine workflows.
///
/// Provides a chainable API for testing state machine transitions with
/// support for complex assertions and state inspection.
///
/// # Example
///
/// ```ignore
/// use seesaw_core::testing::WorkflowTest;
///
/// WorkflowTest::new(NotificationMachine::new())
///     // Test initial event
///     .given(NotificationEvent::Created { id, user_id })
///     .expect_some()
///     .expect_command(|cmd| {
///         matches!(cmd, Some(NotificationCommand::Enrich { .. }))
///     })
///
///     // Test enrichment completion
///     .then(NotificationEvent::Enriched { id, data })
///     .expect_command(|cmd| {
///         matches!(cmd, Some(NotificationCommand::Deliver { .. }))
///     })
///
///     // Test delivery
///     .then(NotificationEvent::Delivered { id })
///     .expect_none()
///
///     // Verify final state
///     .assert_state(|m| m.pending.is_empty());
/// ```
pub struct WorkflowTest<M>
where
    M: Machine,
{
    machine: M,
    last_command: Option<M::Command>,
}

impl<M> WorkflowTest<M>
where
    M: Machine,
    M::Event: std::fmt::Debug,
    M::Command: std::fmt::Debug + PartialEq,
{
    /// Create a new workflow test with the given machine.
    pub fn new(machine: M) -> Self {
        Self {
            machine,
            last_command: None,
        }
    }

    /// Process an initial event and capture the command.
    ///
    /// Use this as the first step in a test chain.
    pub fn given(mut self, event: M::Event) -> Self {
        self.last_command = self.machine.decide(&event);
        self
    }

    /// Process a subsequent event and capture the command.
    ///
    /// Use this after `given()` for additional events in the sequence.
    pub fn then(mut self, event: M::Event) -> Self {
        self.last_command = self.machine.decide(&event);
        self
    }

    /// Assert the last command matches the expected value.
    pub fn expect(self, expected: Option<M::Command>) -> Self {
        assert_eq!(
            self.last_command, expected,
            "Command mismatch\n  expected: {:?}\n  actual: {:?}",
            expected, self.last_command
        );
        self
    }

    /// Assert the last command was `Some(_)`.
    pub fn expect_some(self) -> Self {
        assert!(
            self.last_command.is_some(),
            "Expected Some command, got None"
        );
        self
    }

    /// Assert the last command was `None`.
    pub fn expect_none(self) -> Self {
        assert!(
            self.last_command.is_none(),
            "Expected None, got {:?}",
            self.last_command
        );
        self
    }

    /// Assert the last command matches a predicate.
    ///
    /// # Example
    ///
    /// ```ignore
    /// test.expect_command(|cmd| {
    ///     matches!(cmd, Some(Command::Process { n }) if *n > 0)
    /// });
    /// ```
    pub fn expect_command<F>(self, predicate: F) -> Self
    where
        F: FnOnce(&Option<M::Command>) -> bool,
    {
        assert!(
            predicate(&self.last_command),
            "Command predicate failed for {:?}",
            self.last_command
        );
        self
    }

    /// Assert the machine state matches a predicate.
    ///
    /// # Example
    ///
    /// ```ignore
    /// test.assert_state(|m| m.pending.len() == 1);
    /// ```
    pub fn assert_state<F>(self, predicate: F) -> Self
    where
        F: FnOnce(&M) -> bool,
    {
        assert!(predicate(&self.machine), "State predicate failed");
        self
    }

    /// Get a reference to the machine for custom assertions.
    pub fn machine(&self) -> &M {
        &self.machine
    }

    /// Get a mutable reference to the machine.
    pub fn machine_mut(&mut self) -> &mut M {
        &mut self.machine
    }

    /// Get the last command result.
    pub fn last_command(&self) -> &Option<M::Command> {
        &self.last_command
    }

    /// Consume the test and return the machine.
    pub fn into_machine(self) -> M {
        self.machine
    }
}

/// Extension trait for machines to enable fluent testing.
///
/// # Example
///
/// ```ignore
/// use seesaw_core::testing::MachineTestExt;
///
/// MyMachine::new()
///     .test()
///     .given(Event::Start)
///     .expect_some();
/// ```
pub trait MachineTestExt: Machine + Sized
where
    Self::Event: std::fmt::Debug,
    Self::Command: std::fmt::Debug + PartialEq,
{
    /// Create a workflow test builder for this machine.
    fn test(self) -> WorkflowTest<Self> {
        WorkflowTest::new(self)
    }
}

// Blanket implementation for all compatible machines
impl<M> MachineTestExt for M
where
    M: Machine,
    M::Event: std::fmt::Debug,
    M::Command: std::fmt::Debug + PartialEq,
{
}

// =============================================================================
// Event Latch
// =============================================================================

/// Synchronization primitive for waiting on a specific number of events.
///
/// `EventLatch` enables deterministic testing of fan-out scenarios where
/// multiple events are expected. Instead of using sleeps or yields, tests
/// can wait for an exact number of events to occur.
///
/// # Philosophy
///
/// Tests should wait for meaning, not time.
///
/// # Example
///
/// ```ignore
/// use seesaw_core::testing::EventLatch;
///
/// #[tokio::test]
/// async fn test_notification_fan_out() {
///     let latch = EventLatch::new(3);
///
///     // Register tap before emitting
///     bus.tap::<NotificationEvent>({
///         let latch = latch.clone();
///         move |_| latch.dec()
///     });
///
///     // Emit event that triggers 3 notifications
///     engine.emit(UserCreated { id: user_id });
///
///     // Wait for all 3 notifications (no sleep!)
///     latch.await_zero().await;
///
///     // Now safe to assert
///     assert_eq!(notification_count(), 3);
/// }
/// ```
///
/// # Timeout Safety
///
/// For tests that might hang, use `tokio::time::timeout`:
///
/// ```ignore
/// use std::time::Duration;
/// use tokio::time::timeout;
///
/// timeout(Duration::from_secs(5), latch.await_zero())
///     .await
///     .expect("latch timed out");
/// ```
#[derive(Debug)]
pub struct EventLatch {
    remaining: AtomicUsize,
    notify: Notify,
}

impl EventLatch {
    /// Create a new latch expecting `expected` events.
    pub fn new(expected: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(expected),
            notify: Notify::new(),
        }
    }

    /// Decrement the remaining count.
    ///
    /// Call this when an expected event occurs. When the count reaches zero,
    /// all waiters are notified.
    ///
    /// # Panics
    ///
    /// Panics if called more times than expected (underflow protection).
    pub fn dec(&self) {
        let prev = self.remaining.fetch_sub(1, Ordering::AcqRel);
        if prev == 0 {
            panic!("EventLatch decremented below zero - more events than expected");
        }
        if prev == 1 {
            // We just hit zero
            self.notify.notify_waiters();
        }
    }

    /// Wait for the count to reach zero.
    ///
    /// Returns immediately if the count is already zero.
    pub async fn await_zero(&self) {
        loop {
            // Register for notification BEFORE checking count
            let notified = self.notify.notified();

            if self.remaining.load(Ordering::Acquire) == 0 {
                return;
            }

            // Wait for notification, then loop back to recheck
            notified.await;
        }
    }

    /// Get the current remaining count.
    ///
    /// Useful for debugging or assertions.
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }

    /// Check if the latch has reached zero.
    pub fn is_complete(&self) -> bool {
        self.remaining() == 0
    }
}

impl Clone for EventLatch {
    fn clone(&self) -> Self {
        // Note: This creates a new latch with the current count,
        // sharing the same underlying atomic. For proper sharing,
        // wrap in Arc.
        Self {
            remaining: AtomicUsize::new(self.remaining.load(Ordering::Acquire)),
            notify: Notify::new(),
        }
    }
}

/// Arc-wrapped EventLatch for easy sharing across closures.
///
/// This is the typical way to use EventLatch in tests:
///
/// ```ignore
/// let latch = SharedEventLatch::new(2);
///
/// bus.tap::<MyEvent>({
///     let latch = latch.clone();
///     move |_| latch.dec()
/// });
///
/// engine.emit(event);
/// latch.await_zero().await;
/// ```
pub type SharedEventLatch = std::sync::Arc<EventLatch>;

/// Create a shared event latch.
///
/// Convenience function for `Arc::new(EventLatch::new(expected))`.
pub fn shared_latch(expected: usize) -> SharedEventLatch {
    std::sync::Arc::new(EventLatch::new(expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use seesaw_core::Command;

    // Test types
    #[derive(Debug, Clone, PartialEq)]
    enum TestEvent {
        Start,
        Step { n: i32 },
        Done,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum TestCommand {
        Begin,
        Process { n: i32 },
        Finalize,
    }
    impl Command for TestCommand {}

    struct TestMachine {
        steps: Vec<i32>,
        done: bool,
    }

    impl TestMachine {
        fn new() -> Self {
            Self {
                steps: Vec::new(),
                done: false,
            }
        }
    }

    impl Machine for TestMachine {
        type Event = TestEvent;
        type Command = TestCommand;

        fn decide(&mut self, event: &TestEvent) -> Option<TestCommand> {
            match event {
                TestEvent::Start => Some(TestCommand::Begin),
                TestEvent::Step { n } => {
                    self.steps.push(*n);
                    Some(TestCommand::Process { n: *n })
                }
                TestEvent::Done => {
                    self.done = true;
                    if self.steps.is_empty() {
                        None
                    } else {
                        Some(TestCommand::Finalize)
                    }
                }
            }
        }
    }

    #[test]
    fn test_assert_workflow_macro() {
        let mut machine = TestMachine::new();

        assert_workflow!(
            machine,
            TestEvent::Start => Some(TestCommand::Begin),
            TestEvent::Step { n: 1 } => Some(TestCommand::Process { n: 1 }),
            TestEvent::Step { n: 2 } => Some(TestCommand::Process { n: 2 }),
            TestEvent::Done => Some(TestCommand::Finalize),
        );

        assert!(machine.done);
        assert_eq!(machine.steps, vec![1, 2]);
    }

    #[test]
    fn test_workflow_test_builder() {
        WorkflowTest::new(TestMachine::new())
            .given(TestEvent::Start)
            .expect(Some(TestCommand::Begin))
            .then(TestEvent::Step { n: 1 })
            .expect_some()
            .expect_command(|cmd| matches!(cmd, Some(TestCommand::Process { n: 1 })))
            .then(TestEvent::Done)
            .expect_some()
            .assert_state(|m| m.done)
            .assert_state(|m| m.steps == vec![1]);
    }

    #[test]
    fn test_workflow_test_expect_none() {
        // Test that Done returns None when steps is empty
        // (Start doesn't add steps, so Done should return None)
        WorkflowTest::new(TestMachine::new())
            .given(TestEvent::Start)
            .expect_some() // Start returns Begin
            .then(TestEvent::Done)
            .expect_none(); // Done returns None when steps is empty
    }

    #[test]
    fn test_machine_test_ext() {
        TestMachine::new()
            .test()
            .given(TestEvent::Start)
            .expect_some();
    }

    #[test]
    fn test_workflow_test_into_machine() {
        let test = WorkflowTest::new(TestMachine::new()).given(TestEvent::Step { n: 42 });

        let machine = test.into_machine();
        assert_eq!(machine.steps, vec![42]);
    }

    #[test]
    fn test_workflow_test_machine_access() {
        let test = WorkflowTest::new(TestMachine::new()).given(TestEvent::Step { n: 1 });

        assert_eq!(test.machine().steps, vec![1]);
        assert_eq!(test.last_command(), &Some(TestCommand::Process { n: 1 }));
    }

    // =========================================================================
    // EventLatch Tests
    // =========================================================================

    use super::{shared_latch, EventLatch};

    #[test]
    fn test_event_latch_basic() {
        let latch = EventLatch::new(2);
        assert_eq!(latch.remaining(), 2);
        assert!(!latch.is_complete());

        latch.dec();
        assert_eq!(latch.remaining(), 1);
        assert!(!latch.is_complete());

        latch.dec();
        assert_eq!(latch.remaining(), 0);
        assert!(latch.is_complete());
    }

    #[test]
    #[should_panic(expected = "decremented below zero")]
    fn test_event_latch_underflow_panics() {
        let latch = EventLatch::new(1);
        latch.dec();
        latch.dec(); // Should panic
    }

    #[tokio::test]
    async fn test_event_latch_await_immediate() {
        let latch = EventLatch::new(0);
        latch.await_zero().await; // Should return immediately
    }

    #[tokio::test]
    async fn test_event_latch_await_after_dec() {
        let latch = std::sync::Arc::new(EventLatch::new(2));

        let latch_clone = latch.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            latch_clone.dec();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            latch_clone.dec();
        });

        latch.await_zero().await;
        assert!(latch.is_complete());
    }

    #[test]
    fn test_shared_latch() {
        let latch = shared_latch(3);
        assert_eq!(latch.remaining(), 3);

        let latch2 = latch.clone();
        latch2.dec();

        // Both point to the same underlying latch
        assert_eq!(latch.remaining(), 2);
        assert_eq!(latch2.remaining(), 2);
    }
}

// =============================================================================
// Spy Job Queue
// =============================================================================

/// A job that was enqueued to the spy queue.
#[derive(Debug, Clone)]
pub struct EnqueuedJob {
    /// The job ID (synthetic, generated by the spy).
    pub id: Uuid,
    /// The job type from the spec.
    pub job_type: String,
    /// The serialized command payload.
    pub payload: serde_json::Value,
    /// The full job specification.
    pub spec: JobSpec,
    /// When the job is scheduled to run (None for immediate background jobs).
    pub scheduled_at: Option<DateTime<Utc>>,
    /// When the job was enqueued.
    pub enqueued_at: DateTime<Utc>,
}

/// Spy job queue that records enqueued commands for test assertions.
///
/// This queue does NOT execute jobs - it only records them for assertions.
/// Use this in tests to verify that commands are being routed to the job
/// queue correctly, without needing to set up actual job infrastructure.
///
/// # Example
///
/// ```ignore
/// use seesaw_core::testing::SpyJobQueue;
///
/// let spy = SpyJobQueue::new();
/// let dispatcher = Dispatcher::with_job_queue(deps, bus, Arc::new(spy.clone()));
///
/// // Emit an event that triggers a background command
/// engine.emit(MyEvent::Trigger);
///
/// // Assert the job was enqueued
/// assert!(spy.was_enqueued("my_job_type"));
///
/// let jobs = spy.jobs_of_type("my_job_type");
/// assert_eq!(jobs.len(), 1);
/// assert_eq!(jobs[0].spec.idempotency_key, Some("expected-key".to_string()));
/// ```
#[derive(Debug, Clone, Default)]
pub struct SpyJobQueue {
    enqueued: Arc<Mutex<Vec<EnqueuedJob>>>,
}

impl SpyJobQueue {
    /// Create a new empty spy queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a job of the given type was enqueued.
    pub fn was_enqueued(&self, job_type: &str) -> bool {
        self.enqueued
            .lock()
            .unwrap()
            .iter()
            .any(|j| j.job_type == job_type)
    }

    /// Get all jobs of a specific type.
    pub fn jobs_of_type(&self, job_type: &str) -> Vec<EnqueuedJob> {
        self.enqueued
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.job_type == job_type)
            .cloned()
            .collect()
    }

    /// Get all enqueued jobs.
    pub fn all_jobs(&self) -> Vec<EnqueuedJob> {
        self.enqueued.lock().unwrap().clone()
    }

    /// Get the count of enqueued jobs.
    pub fn job_count(&self) -> usize {
        self.enqueued.lock().unwrap().len()
    }

    /// Clear all recorded jobs.
    ///
    /// Useful for resetting between test cases.
    pub fn clear(&self) {
        self.enqueued.lock().unwrap().clear();
    }

    /// Get the most recent job of a given type.
    pub fn last_job_of_type(&self, job_type: &str) -> Option<EnqueuedJob> {
        self.enqueued
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|j| j.job_type == job_type)
            .cloned()
    }

    /// Assert a job was enqueued with a specific idempotency key.
    ///
    /// # Panics
    ///
    /// Panics if no job with the given type and idempotency key was enqueued.
    pub fn assert_enqueued_with_key(&self, job_type: &str, idempotency_key: &str) {
        let jobs = self.jobs_of_type(job_type);
        let found = jobs
            .iter()
            .any(|j| j.spec.idempotency_key.as_deref() == Some(idempotency_key));
        assert!(
            found,
            "Expected job '{}' with idempotency_key '{}' to be enqueued. Found {} jobs of this type: {:?}",
            job_type,
            idempotency_key,
            jobs.len(),
            jobs.iter().map(|j| &j.spec.idempotency_key).collect::<Vec<_>>()
        );
    }

    /// Assert no jobs of the given type were enqueued.
    ///
    /// # Panics
    ///
    /// Panics if any job of the given type was enqueued.
    pub fn assert_not_enqueued(&self, job_type: &str) {
        let jobs = self.jobs_of_type(job_type);
        assert!(
            jobs.is_empty(),
            "Expected no '{}' jobs to be enqueued, but found {}",
            job_type,
            jobs.len()
        );
    }

    /// Check if a job was scheduled for a specific reference ID and job type.
    /// Assert the exact count of jobs of a given type.
    ///
    /// # Panics
    ///
    /// Panics if the actual count doesn't match the expected count.
    pub fn assert_job_count(&self, job_type: &str, expected: usize) {
        let actual = self.jobs_of_type(job_type).len();
        assert_eq!(
            actual, expected,
            "Expected {} jobs of type '{}', found {}",
            expected, job_type, actual
        );
    }

    /// Assert the total number of enqueued jobs.
    ///
    /// # Panics
    ///
    /// Panics if the actual total doesn't match the expected count.
    pub fn assert_total_count(&self, expected: usize) {
        let actual = self.job_count();
        assert_eq!(
            actual, expected,
            "Expected {} total jobs, found {}",
            expected, actual
        );
    }

    /// Assert a job was scheduled (not immediately enqueued).
    ///
    /// # Panics
    ///
    /// Panics if no scheduled job of this type exists.
    pub fn assert_was_scheduled(&self, job_type: &str) {
        let jobs = self.jobs_of_type(job_type);
        let found = jobs.iter().any(|j| j.scheduled_at.is_some());
        assert!(
            found,
            "Expected job '{}' to be scheduled (have scheduled_at), but found {} jobs with no scheduled_at",
            job_type,
            jobs.iter().filter(|j| j.scheduled_at.is_none()).count()
        );
    }

    /// Assert a job was scheduled to run at or after a specific time.
    ///
    /// # Panics
    ///
    /// Panics if no job with the given type was scheduled at or after the specified time.
    pub fn assert_scheduled_at_or_after(&self, job_type: &str, min_time: DateTime<Utc>) {
        let jobs = self.jobs_of_type(job_type);
        let found = jobs
            .iter()
            .any(|j| j.scheduled_at.map_or(false, |t| t >= min_time));
        assert!(
            found,
            "Expected job '{}' to be scheduled at or after {}, found: {:?}",
            job_type,
            min_time,
            jobs.iter().map(|j| j.scheduled_at).collect::<Vec<_>>()
        );
    }
}

#[async_trait::async_trait]
impl JobQueue for SpyJobQueue {
    async fn enqueue(&self, payload: serde_json::Value, spec: JobSpec) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let job = EnqueuedJob {
            id,
            job_type: spec.job_type.to_string(),
            payload,
            spec,
            scheduled_at: None,
            enqueued_at: Utc::now(),
        };

        self.enqueued.lock().unwrap().push(job);
        Ok(id)
    }

    async fn schedule(
        &self,
        payload: serde_json::Value,
        spec: JobSpec,
        run_at: DateTime<Utc>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let job = EnqueuedJob {
            id,
            job_type: spec.job_type.to_string(),
            payload,
            spec,
            scheduled_at: Some(run_at),
            enqueued_at: Utc::now(),
        };
        self.enqueued.lock().unwrap().push(job);
        Ok(id)
    }
}

// =============================================================================
// Mock Job Store
// =============================================================================

/// A job that was recorded in the mock store.
#[derive(Debug, Clone)]
pub struct RecordedJob {
    /// The job ID.
    pub id: Uuid,
    /// The job type.
    pub job_type: String,
    /// The serialized payload.
    pub payload: serde_json::Value,
    /// Payload version.
    pub version: i32,
    /// Current attempt number.
    pub attempt: i32,
    /// Job status.
    pub status: JobStatus,
    /// Error message if failed.
    pub error: Option<String>,
    /// When the job should run (for scheduled jobs).
    pub run_at: Option<DateTime<Utc>>,
}

/// Job status in the mock store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Job is pending execution.
    Pending,
    /// Job has been claimed by a worker.
    Claimed,
    /// Job completed successfully.
    Succeeded,
    /// Job failed (retryable).
    Failed,
    /// Job permanently failed (dead-letter).
    DeadLetter,
}

/// Mock job store for testing the claim/execute/mark flow.
///
/// This implementation stores jobs in memory and tracks their state transitions.
/// It's useful for testing job workers and the full job lifecycle.
///
/// # Example
///
/// ```ignore
/// use seesaw_core::testing::MockJobStore;
/// use seesaw_core::job::{JobStore, ClaimedJob, FailureKind};
///
/// let store = MockJobStore::new();
///
/// // Seed a job (simulating what the job queue would have persisted)
/// let job_id = store.seed_job("email:send", json!({"user_id": "123"}), 1);
///
/// // Claim it
/// let jobs = store.claim_ready("worker-1", 10).await?;
/// assert_eq!(jobs.len(), 1);
///
/// // Execute and mark success
/// store.mark_succeeded(job_id).await?;
///
/// // Verify state
/// assert!(store.job_succeeded(job_id));
/// ```
#[derive(Debug, Clone, Default)]
pub struct MockJobStore {
    jobs: Arc<Mutex<Vec<RecordedJob>>>,
    heartbeats: Arc<Mutex<Vec<(Uuid, DateTime<Utc>)>>>,
}

impl MockJobStore {
    /// Create a new empty mock store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a job into the store (simulating persistence from job queue).
    ///
    /// Returns the job ID for later reference.
    pub fn seed_job(
        &self,
        job_type: impl Into<String>,
        payload: serde_json::Value,
        version: i32,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let job = RecordedJob {
            id,
            job_type: job_type.into(),
            payload,
            version,
            attempt: 0,
            status: JobStatus::Pending,
            error: None,
            run_at: None,
        };
        self.jobs.lock().unwrap().push(job);
        id
    }

    /// Seed a scheduled job with a specific run_at time.
    pub fn seed_scheduled_job(
        &self,
        job_type: impl Into<String>,
        payload: serde_json::Value,
        version: i32,
        run_at: DateTime<Utc>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let job = RecordedJob {
            id,
            job_type: job_type.into(),
            payload,
            version,
            attempt: 0,
            status: JobStatus::Pending,
            error: None,
            run_at: Some(run_at),
        };
        self.jobs.lock().unwrap().push(job);
        id
    }

    /// Get a job by ID.
    pub fn get_job(&self, job_id: Uuid) -> Option<RecordedJob> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == job_id)
            .cloned()
    }

    /// Check if a job succeeded.
    pub fn job_succeeded(&self, job_id: Uuid) -> bool {
        self.get_job(job_id)
            .map(|j| j.status == JobStatus::Succeeded)
            .unwrap_or(false)
    }

    /// Check if a job failed (retryable).
    pub fn job_failed(&self, job_id: Uuid) -> bool {
        self.get_job(job_id)
            .map(|j| j.status == JobStatus::Failed)
            .unwrap_or(false)
    }

    /// Check if a job is in dead-letter.
    pub fn job_dead_letter(&self, job_id: Uuid) -> bool {
        self.get_job(job_id)
            .map(|j| j.status == JobStatus::DeadLetter)
            .unwrap_or(false)
    }

    /// Get the number of heartbeats recorded for a job.
    pub fn heartbeat_count(&self, job_id: Uuid) -> usize {
        self.heartbeats
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == job_id)
            .count()
    }

    /// Get all jobs in a specific status.
    pub fn jobs_with_status(&self, status: JobStatus) -> Vec<RecordedJob> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.status == status)
            .cloned()
            .collect()
    }

    /// Get total job count.
    pub fn job_count(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    /// Clear all jobs and heartbeats.
    pub fn clear(&self) {
        self.jobs.lock().unwrap().clear();
        self.heartbeats.lock().unwrap().clear();
    }

    /// Get the error message for a failed job.
    pub fn job_error(&self, job_id: Uuid) -> Option<String> {
        self.get_job(job_id).and_then(|j| j.error)
    }

    /// Get the attempt count for a job.
    pub fn job_attempt(&self, job_id: Uuid) -> Option<i32> {
        self.get_job(job_id).map(|j| j.attempt)
    }
}

#[async_trait::async_trait]
impl seesaw_core::JobStore for MockJobStore {
    async fn claim_ready(
        &self,
        _worker_id: &str,
        limit: i64,
    ) -> Result<Vec<seesaw_core::ClaimedJob>> {
        let now = Utc::now();
        let mut jobs = self.jobs.lock().unwrap();
        let mut claimed = Vec::new();

        for job in jobs.iter_mut() {
            if claimed.len() >= limit as usize {
                break;
            }

            // Only claim pending jobs that are ready to run
            if job.status == JobStatus::Pending {
                // For scheduled jobs, check if run_at has passed
                if let Some(run_at) = job.run_at {
                    if run_at > now {
                        continue;
                    }
                }

                job.status = JobStatus::Claimed;
                job.attempt += 1;

                claimed.push(seesaw_core::ClaimedJob {
                    id: job.id,
                    job_type: job.job_type.clone(),
                    payload: job.payload.clone(),
                    version: job.version,
                    attempt: job.attempt,
                });
            }
        }

        Ok(claimed)
    }

    async fn mark_succeeded(&self, job_id: Uuid) -> Result<()> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
            job.status = JobStatus::Succeeded;
            Ok(())
        } else {
            Err(anyhow::anyhow!("job not found: {}", job_id))
        }
    }

    async fn mark_failed(
        &self,
        job_id: Uuid,
        error: &str,
        kind: seesaw_core::FailureKind,
    ) -> Result<()> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
            job.error = Some(error.to_string());
            match kind {
                seesaw_core::FailureKind::Retryable => {
                    job.status = JobStatus::Failed;
                    // Reset to pending for retry (simplified - real impl would have backoff)
                    job.status = JobStatus::Pending;
                }
                seesaw_core::FailureKind::NonRetryable => {
                    job.status = JobStatus::DeadLetter;
                }
            }
            Ok(())
        } else {
            Err(anyhow::anyhow!("job not found: {}", job_id))
        }
    }

    async fn heartbeat(&self, job_id: Uuid) -> Result<()> {
        // Verify job exists and is claimed
        {
            let jobs = self.jobs.lock().unwrap();
            let job = jobs
                .iter()
                .find(|j| j.id == job_id)
                .ok_or_else(|| anyhow::anyhow!("job not found: {}", job_id))?;
            if job.status != JobStatus::Claimed {
                return Err(anyhow::anyhow!("job not claimed: {}", job_id));
            }
        }

        self.heartbeats.lock().unwrap().push((job_id, Utc::now()));
        Ok(())
    }
}

#[cfg(test)]
mod mock_store_tests {
    use super::*;
    use seesaw_core::{FailureKind, JobStore};

    #[test]
    fn test_mock_store_seed_job() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("test:job", serde_json::json!({"key": "value"}), 1);

        let job = store.get_job(job_id).unwrap();
        assert_eq!(job.job_type, "test:job");
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.attempt, 0);
    }

    #[test]
    fn test_mock_store_seed_scheduled_job() {
        let store = MockJobStore::new();
        let run_at = Utc::now() + chrono::Duration::hours(1);
        let job_id = store.seed_scheduled_job("scheduled:job", serde_json::json!({}), 1, run_at);

        let job = store.get_job(job_id).unwrap();
        assert_eq!(job.run_at, Some(run_at));
    }

    #[tokio::test]
    async fn test_mock_store_claim_ready() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("test:job", serde_json::json!({}), 1);

        let claimed = store.claim_ready("worker-1", 10).await.unwrap();

        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, job_id);
        assert_eq!(claimed[0].attempt, 1);

        // Job should now be claimed
        let job = store.get_job(job_id).unwrap();
        assert_eq!(job.status, JobStatus::Claimed);
    }

    #[tokio::test]
    async fn test_mock_store_claim_respects_limit() {
        let store = MockJobStore::new();
        store.seed_job("job:1", serde_json::json!({}), 1);
        store.seed_job("job:2", serde_json::json!({}), 1);
        store.seed_job("job:3", serde_json::json!({}), 1);

        let claimed = store.claim_ready("worker-1", 2).await.unwrap();

        assert_eq!(claimed.len(), 2);
    }

    #[tokio::test]
    async fn test_mock_store_scheduled_job_not_ready() {
        let store = MockJobStore::new();
        let future = Utc::now() + chrono::Duration::hours(1);
        store.seed_scheduled_job("scheduled:job", serde_json::json!({}), 1, future);

        let claimed = store.claim_ready("worker-1", 10).await.unwrap();

        // Scheduled job in the future should not be claimed
        assert_eq!(claimed.len(), 0);
    }

    #[tokio::test]
    async fn test_mock_store_scheduled_job_ready() {
        let store = MockJobStore::new();
        let past = Utc::now() - chrono::Duration::hours(1);
        let job_id = store.seed_scheduled_job("scheduled:job", serde_json::json!({}), 1, past);

        let claimed = store.claim_ready("worker-1", 10).await.unwrap();

        // Scheduled job in the past should be claimed
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, job_id);
    }

    #[tokio::test]
    async fn test_mock_store_mark_succeeded() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("test:job", serde_json::json!({}), 1);

        // Claim first
        store.claim_ready("worker-1", 10).await.unwrap();

        // Mark succeeded
        store.mark_succeeded(job_id).await.unwrap();

        assert!(store.job_succeeded(job_id));
    }

    #[tokio::test]
    async fn test_mock_store_mark_failed_retryable() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("test:job", serde_json::json!({}), 1);

        store.claim_ready("worker-1", 10).await.unwrap();
        store
            .mark_failed(job_id, "transient error", FailureKind::Retryable)
            .await
            .unwrap();

        // Retryable failures go back to pending
        let job = store.get_job(job_id).unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(store.job_error(job_id), Some("transient error".to_string()));
    }

    #[tokio::test]
    async fn test_mock_store_mark_failed_non_retryable() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("test:job", serde_json::json!({}), 1);

        store.claim_ready("worker-1", 10).await.unwrap();
        store
            .mark_failed(job_id, "permanent error", FailureKind::NonRetryable)
            .await
            .unwrap();

        assert!(store.job_dead_letter(job_id));
    }

    #[tokio::test]
    async fn test_mock_store_heartbeat() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("long:job", serde_json::json!({}), 1);

        store.claim_ready("worker-1", 10).await.unwrap();

        // Send heartbeats
        store.heartbeat(job_id).await.unwrap();
        store.heartbeat(job_id).await.unwrap();
        store.heartbeat(job_id).await.unwrap();

        assert_eq!(store.heartbeat_count(job_id), 3);
    }

    #[tokio::test]
    async fn test_mock_store_heartbeat_requires_claimed() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("test:job", serde_json::json!({}), 1);

        // Job is pending, not claimed
        let result = store.heartbeat(job_id).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not claimed"));
    }

    #[test]
    fn test_mock_store_jobs_with_status() {
        let store = MockJobStore::new();
        store.seed_job("job:1", serde_json::json!({}), 1);
        store.seed_job("job:2", serde_json::json!({}), 1);

        let pending = store.jobs_with_status(JobStatus::Pending);
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn test_mock_store_clear() {
        let store = MockJobStore::new();
        store.seed_job("job:1", serde_json::json!({}), 1);
        store.seed_job("job:2", serde_json::json!({}), 1);

        store.clear();

        assert_eq!(store.job_count(), 0);
    }

    #[tokio::test]
    async fn test_mock_store_retry_increments_attempt() {
        let store = MockJobStore::new();
        let job_id = store.seed_job("retry:job", serde_json::json!({}), 1);

        // First attempt
        let claimed = store.claim_ready("worker-1", 10).await.unwrap();
        assert_eq!(claimed[0].attempt, 1);

        // Fail with retryable
        store
            .mark_failed(job_id, "error", FailureKind::Retryable)
            .await
            .unwrap();

        // Second attempt
        let claimed = store.claim_ready("worker-1", 10).await.unwrap();
        assert_eq!(claimed[0].attempt, 2);
    }
}

#[cfg(test)]
mod spy_tests {
    use super::*;

    #[tokio::test]
    async fn test_spy_queue_enqueue() {
        let spy = SpyJobQueue::new();

        let payload = serde_json::json!({ "user_id": "123" });
        let spec = JobSpec::new("email:send").with_idempotency_key("email:123:welcome");

        let id = spy.enqueue(payload.clone(), spec).await.unwrap();

        assert!(spy.was_enqueued("email:send"));
        assert!(!spy.was_enqueued("other:type"));

        let jobs = spy.jobs_of_type("email:send");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].payload, payload);
        assert!(jobs[0].scheduled_at.is_none());
    }

    #[tokio::test]
    async fn test_spy_queue_schedule() {
        let spy = SpyJobQueue::new();

        let payload = serde_json::json!({ "reminder": "test" });
        let spec = JobSpec::new("reminder:send");
        let run_at = Utc::now() + chrono::Duration::hours(1);

        let id = spy.schedule(payload.clone(), spec, run_at).await.unwrap();

        let jobs = spy.all_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].scheduled_at, Some(run_at));
    }

    #[tokio::test]
    async fn test_spy_queue_clear() {
        let spy = SpyJobQueue::new();

        spy.enqueue(serde_json::json!({}), JobSpec::new("test:1"))
            .await
            .unwrap();
        spy.enqueue(serde_json::json!({}), JobSpec::new("test:2"))
            .await
            .unwrap();

        assert_eq!(spy.job_count(), 2);

        spy.clear();

        assert_eq!(spy.job_count(), 0);
        assert!(spy.all_jobs().is_empty());
    }

    #[tokio::test]
    async fn test_spy_queue_last_job_of_type() {
        let spy = SpyJobQueue::new();

        spy.enqueue(
            serde_json::json!({ "n": 1 }),
            JobSpec::new("test").with_idempotency_key("first"),
        )
        .await
        .unwrap();

        spy.enqueue(
            serde_json::json!({ "n": 2 }),
            JobSpec::new("test").with_idempotency_key("second"),
        )
        .await
        .unwrap();

        let last = spy.last_job_of_type("test").unwrap();
        assert_eq!(last.spec.idempotency_key, Some("second".to_string()));
    }

    #[tokio::test]
    async fn test_spy_queue_assert_enqueued_with_key() {
        let spy = SpyJobQueue::new();

        spy.enqueue(
            serde_json::json!({}),
            JobSpec::new("email:send").with_idempotency_key("email:user:123"),
        )
        .await
        .unwrap();

        // Should not panic
        spy.assert_enqueued_with_key("email:send", "email:user:123");
    }

    #[test]
    #[should_panic(expected = "Expected job")]
    fn test_spy_queue_assert_enqueued_with_key_fails() {
        let spy = SpyJobQueue::new();
        spy.assert_enqueued_with_key("missing:type", "some-key");
    }

    #[tokio::test]
    async fn test_spy_queue_assert_not_enqueued() {
        let spy = SpyJobQueue::new();

        spy.enqueue(serde_json::json!({}), JobSpec::new("email:send"))
            .await
            .unwrap();

        // Should not panic - "other:type" was not enqueued
        spy.assert_not_enqueued("other:type");
    }

    #[tokio::test]
    #[should_panic(expected = "Expected no")]
    async fn test_spy_queue_assert_not_enqueued_fails() {
        let spy = SpyJobQueue::new();

        spy.enqueue(serde_json::json!({}), JobSpec::new("email:send"))
            .await
            .unwrap();

        spy.assert_not_enqueued("email:send");
    }

    #[test]
    fn test_spy_queue_clone() {
        let spy1 = SpyJobQueue::new();
        let spy2 = spy1.clone();

        // They share the same underlying storage
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            spy1.enqueue(serde_json::json!({}), JobSpec::new("test"))
                .await
                .unwrap();
        });

        assert_eq!(spy2.job_count(), 1);
    }

    #[tokio::test]
    async fn test_spy_queue_multiple_jobs_same_type() {
        let spy = SpyJobQueue::new();

        for i in 0..5 {
            spy.enqueue(
                serde_json::json!({ "index": i }),
                JobSpec::new("batch:job").with_idempotency_key(format!("key:{}", i)),
            )
            .await
            .unwrap();
        }

        let jobs = spy.jobs_of_type("batch:job");
        assert_eq!(jobs.len(), 5);

        // Verify payloads are unique
        let indices: Vec<i64> = jobs
            .iter()
            .map(|j| j.payload["index"].as_i64().unwrap())
            .collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_spy_queue_job_contains_full_spec() {
        let spy = SpyJobQueue::new();

        let spec = JobSpec::new("full:spec")
            .with_idempotency_key("idem:key")
            .with_max_retries(5)
            .with_priority(10)
            .with_version(2);

        spy.enqueue(serde_json::json!({"data": "test"}), spec)
            .await
            .unwrap();

        let job = spy.last_job_of_type("full:spec").unwrap();

        assert_eq!(job.spec.job_type, "full:spec");
        assert_eq!(job.spec.idempotency_key, Some("idem:key".to_string()));
        assert_eq!(job.spec.max_retries, 5);
        assert_eq!(job.spec.priority, 10);
        assert_eq!(job.spec.version, 2);
    }

    #[tokio::test]
    async fn test_spy_queue_scheduled_at_recorded() {
        let spy = SpyJobQueue::new();
        let run_at = Utc::now() + chrono::Duration::hours(2);

        spy.schedule(serde_json::json!({}), JobSpec::new("delayed:job"), run_at)
            .await
            .unwrap();

        let job = spy.last_job_of_type("delayed:job").unwrap();
        assert_eq!(job.scheduled_at, Some(run_at));
    }

    #[tokio::test]
    async fn test_spy_queue_enqueued_at_recorded() {
        let spy = SpyJobQueue::new();
        let before = Utc::now();

        spy.enqueue(serde_json::json!({}), JobSpec::new("timed:job"))
            .await
            .unwrap();

        let after = Utc::now();
        let job = spy.last_job_of_type("timed:job").unwrap();

        assert!(job.enqueued_at >= before);
        assert!(job.enqueued_at <= after);
    }

    #[tokio::test]
    async fn test_spy_queue_different_job_types() {
        let spy = SpyJobQueue::new();

        spy.enqueue(serde_json::json!({}), JobSpec::new("email:send"))
            .await
            .unwrap();
        spy.enqueue(serde_json::json!({}), JobSpec::new("sms:send"))
            .await
            .unwrap();
        spy.enqueue(serde_json::json!({}), JobSpec::new("push:send"))
            .await
            .unwrap();

        assert!(spy.was_enqueued("email:send"));
        assert!(spy.was_enqueued("sms:send"));
        assert!(spy.was_enqueued("push:send"));
        assert!(!spy.was_enqueued("webhook:send"));

        assert_eq!(spy.job_count(), 3);
    }

    #[test]
    fn test_spy_queue_empty_state() {
        let spy = SpyJobQueue::new();

        assert_eq!(spy.job_count(), 0);
        assert!(spy.all_jobs().is_empty());
        assert!(!spy.was_enqueued("any:type"));
        assert!(spy.last_job_of_type("any:type").is_none());
    }

    #[tokio::test]
    async fn test_spy_queue_unique_job_ids() {
        let spy = SpyJobQueue::new();

        let id1 = spy
            .enqueue(serde_json::json!({}), JobSpec::new("test"))
            .await
            .unwrap();
        let id2 = spy
            .enqueue(serde_json::json!({}), JobSpec::new("test"))
            .await
            .unwrap();
        let id3 = spy
            .schedule(serde_json::json!({}), JobSpec::new("test"), Utc::now())
            .await
            .unwrap();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }
}
