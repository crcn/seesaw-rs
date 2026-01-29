//! Persistence for machines that exist across time.
//!
//! # Overview
//!
//! A [`PersistentMachine`] is a [`Machine`] that survives process restarts,
//! deploys, and node failures. Its state is durable.
//!
//! # The Contract
//!
//! 1. **Load before decide.** For every event routed to a persistent machine,
//!    the engine loads the current snapshot (or creates a new instance).
//!
//! 2. **Decide is unchanged.** `decide(&mut self, event)` runs exactly as it
//!    does for in-memory machines. Persistence is invisible to decision logic.
//!
//! 3. **Save before dispatch.** If state changed, the snapshot is persisted
//!    *before* any command is dispatched. This is non-negotiable.
//!
//! 4. **Skip save if unchanged.** If `changed()` returns false, no write occurs.
//!    The revision does not advance.
//!
//! # What This Guarantees
//!
//! - **Durable intent, not durable execution.** Once save succeeds, the machine's
//!   decision is recorded. Command dispatch is best-effort.
//!
//! - **State is authoritative.** If state says "step 3 complete," that intent
//!   was recorded. Whether the effect ran is a separate question.
//!
//! - **Crash = pause, not failure.** Restart reloads state. The machine is in
//!   a known position.
//!
//! # What This Does NOT Guarantee
//!
//! - No automatic retries. Handle failure events explicitly.
//! - No exactly-once effects. Idempotency is required.
//! - No compensation. Write an undo command if you need rollback.

use std::hash::Hash;

use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use smallvec::SmallVec;

use crate::core::Event;
use crate::machine::Machine;

// =============================================================================
// Store Error
// =============================================================================

/// Errors from machine state storage.
///
/// This distinction is critical for correct behavior:
/// - [`StoreError::Conflict`] means another writer modified the machine.
///   The event should be rejected or reprocessed.
/// - [`StoreError::Backend`] means storage failed (timeout, connection, etc).
///   This is a system-level failure.
///
/// Treating them the same breaks determinism.
#[derive(Debug)]
pub enum StoreError {
    /// Another writer modified the machine since we loaded it.
    ///
    /// This is expected under concurrency. The event should be rejected
    /// or the caller should retry with fresh state.
    Conflict,

    /// Storage backend failed (timeout, connection, serialization).
    ///
    /// This is a system-level failure, not a concurrency issue.
    Backend(anyhow::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Conflict => write!(f, "revision conflict: state was modified concurrently"),
            StoreError::Backend(e) => write!(f, "storage backend error: {}", e),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Conflict => None,
            StoreError::Backend(e) => Some(e.as_ref()),
        }
    }
}

impl From<anyhow::Error> for StoreError {
    fn from(err: anyhow::Error) -> Self {
        StoreError::Backend(err)
    }
}

// =============================================================================
// Revision
// =============================================================================

/// Revision for optimistic concurrency control.
///
/// Each save operation must provide the expected revision. If the stored
/// revision doesn't match, the save fails with [`StoreError::Conflict`].
///
/// # Semantics
///
/// - [`Revision::NONE`] indicates a new machine (never been saved).
/// - After each successful save, the revision advances.
/// - Two no-op events (where `changed() == false`) may race without conflict
///   because neither advances the revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Revision(pub u64);

impl Revision {
    /// Sentinel value for a machine that has never been saved.
    pub const NONE: Revision = Revision(0);

    /// Create a new revision with a specific value.
    pub fn new(value: u64) -> Self {
        Revision(value)
    }

    /// Get the next revision (for save operations).
    pub fn next(self) -> Self {
        Revision(self.0.saturating_add(1))
    }

    /// Check if this is the NONE sentinel.
    pub fn is_none(&self) -> bool {
        self.0 == 0
    }

    /// Get the inner value.
    pub fn value(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_none() {
            write!(f, "NONE")
        } else {
            write!(f, "r{}", self.0)
        }
    }
}

// =============================================================================
// Router
// =============================================================================

/// Extracts machine identity from events.
///
/// This is infrastructure, not business logic. It runs before machine load
/// to determine which machine instance(s) should process an event.
///
/// # Fan-out
///
/// A single event can route to multiple machine instances. This preserves
/// Seesaw's support for multiple machines reacting to one event.
pub trait Router: Send + Sync + 'static {
    /// The event type this router handles.
    type Event: Event;

    /// The machine identity type.
    type Id: Clone + Eq + Hash + Send + Sync + 'static;

    /// Route an event to zero or more machine instances.
    ///
    /// Returns empty if the event doesn't target any persistent machine.
    /// Returns multiple IDs for fan-out scenarios.
    fn route(&self, event: &Self::Event) -> SmallVec<[Self::Id; 1]>;
}

// =============================================================================
// Machine Store
// =============================================================================

/// Persists machine state across time.
///
/// Implementations must provide:
/// - Atomic load/save with revision checking
/// - Conflict detection for concurrent modifications
///
/// # Concurrency
///
/// The store uses optimistic concurrency control via revisions. If two
/// workers try to save the same machine, one will succeed and one will
/// get [`StoreError::Conflict`].
#[async_trait]
pub trait MachineStore<Id, State>: Send + Sync + 'static
where
    Id: Clone + Eq + Hash + Send + Sync + 'static,
    State: Send + Sync,
{
    /// Load state for a machine.
    ///
    /// Returns `None` if the machine has never been saved (new instance).
    /// Returns `Some((state, revision))` if the machine exists.
    async fn load(&self, id: &Id) -> Result<Option<(State, Revision)>, StoreError>;

    /// Save state with optimistic concurrency control.
    ///
    /// The `expected` revision must match the stored revision for the save
    /// to succeed. For new machines, use [`Revision::NONE`].
    ///
    /// Returns the new revision on success.
    /// Returns [`StoreError::Conflict`] if the revision doesn't match.
    async fn save(&self, id: &Id, state: &State, expected: Revision) -> Result<Revision, StoreError>;
}

// =============================================================================
// Persistent Machine
// =============================================================================

/// A Machine that survives process restarts.
///
/// This extends [`Machine`] with lifecycle hooks for persistence. The machine
/// still owns its state via `&mut self`. These hooks serialize/deserialize
/// that internal state.
///
/// # State Ownership
///
/// State remains internal to the machine. `decide(&mut self, event)` is
/// unchanged. Persistence is expressed as lifecycle hooks, not API replacement.
///
/// # Changed Tracking
///
/// The `changed()` method reports whether state has been modified since the
/// last save (or creation). This prevents unnecessary writes.
///
/// If `changed()` returns `false`, the engine skips the save operation and
/// does not advance the revision. This means two no-op events can race
/// without conflict.
pub trait PersistentMachine: Machine {
    /// The serializable snapshot of machine state.
    type Snapshot: Serialize + DeserializeOwned + Send + Sync;

    /// The machine identity type.
    type Id: Clone + Eq + Hash + Send + Sync + 'static;

    /// Create a new machine instance for a never-before-seen ID.
    ///
    /// Called when [`MachineStore::load`] returns `None`.
    /// New machines should set `changed = true` so they get persisted.
    fn create(id: &Self::Id, event: &Self::Event) -> Self;

    /// Reconstruct a machine from a persisted snapshot.
    ///
    /// This is a constructor, not a method. It creates a new machine instance
    /// from serialized state. Restored machines should set `changed = false`.
    fn restore(snapshot: Self::Snapshot) -> Self;

    /// Serialize current state for persistence.
    ///
    /// Called after `decide()` if `changed()` returns `true`.
    fn snapshot(&self) -> Self::Snapshot;

    /// Did state change since last save (or creation)?
    ///
    /// If `false`, the engine skips the save operation.
    fn changed(&self) -> bool;

    /// Mark state as clean after successful save.
    fn mark_clean(&mut self);
}

// =============================================================================
// In-Memory Store (for testing)
// =============================================================================

/// In-memory machine store for testing.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory store backed by a HashMap.
    pub struct InMemoryStore<Id, State> {
        data: Mutex<HashMap<Id, (State, Revision)>>,
    }

    impl<Id, State> InMemoryStore<Id, State> {
        pub fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    impl<Id, State> Default for InMemoryStore<Id, State> {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl<Id, State> MachineStore<Id, State> for InMemoryStore<Id, State>
    where
        Id: Clone + Eq + Hash + Send + Sync + 'static,
        State: Clone + Send + Sync + 'static,
    {
        async fn load(&self, id: &Id) -> Result<Option<(State, Revision)>, StoreError> {
            let data = self.data.lock().map_err(|e| {
                StoreError::Backend(anyhow::anyhow!("mutex poisoned: {}", e))
            })?;
            Ok(data.get(id).cloned())
        }

        async fn save(
            &self,
            id: &Id,
            state: &State,
            expected: Revision,
        ) -> Result<Revision, StoreError> {
            let mut data = self.data.lock().map_err(|e| {
                StoreError::Backend(anyhow::anyhow!("mutex poisoned: {}", e))
            })?;

            let current_rev = data.get(id).map(|(_, r)| *r).unwrap_or(Revision::NONE);

            if current_rev != expected {
                return Err(StoreError::Conflict);
            }

            let new_rev = expected.next();
            data.insert(id.clone(), (state.clone(), new_rev));
            Ok(new_rev)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testing::InMemoryStore;
    use crate::Command;
    use smallvec::smallvec;
    use uuid::Uuid;

    // =========================================================================
    // Test Types
    // =========================================================================

    #[derive(Debug, Clone)]
    enum TestEvent {
        Started { id: Uuid },
        StepCompleted { id: Uuid, step: u32 },
    }

    #[derive(Debug, Clone)]
    enum TestCommand {
        DoStep { step: u32 },
        Complete,
    }
    impl Command for TestCommand {}

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    struct TestSnapshot {
        id: Uuid,
        current_step: u32,
        completed: bool,
    }

    struct TestMachine {
        id: Uuid,
        current_step: u32,
        completed: bool,
        changed: bool,
    }

    impl Machine for TestMachine {
        type Event = TestEvent;
        type Command = TestCommand;

        fn decide(&mut self, event: &TestEvent) -> Option<TestCommand> {
            match event {
                TestEvent::Started { .. } => {
                    self.current_step = 1;
                    self.changed = true;
                    Some(TestCommand::DoStep { step: 1 })
                }
                TestEvent::StepCompleted { step, .. } => {
                    if *step < 3 {
                        self.current_step = step + 1;
                        self.changed = true;
                        Some(TestCommand::DoStep { step: step + 1 })
                    } else {
                        self.completed = true;
                        self.changed = true;
                        Some(TestCommand::Complete)
                    }
                }
            }
        }
    }

    impl PersistentMachine for TestMachine {
        type Snapshot = TestSnapshot;
        type Id = Uuid;

        fn create(id: &Uuid, _event: &TestEvent) -> Self {
            TestMachine {
                id: *id,
                current_step: 0,
                completed: false,
                changed: true,
            }
        }

        fn restore(snapshot: TestSnapshot) -> Self {
            TestMachine {
                id: snapshot.id,
                current_step: snapshot.current_step,
                completed: snapshot.completed,
                changed: false,
            }
        }

        fn snapshot(&self) -> TestSnapshot {
            TestSnapshot {
                id: self.id,
                current_step: self.current_step,
                completed: self.completed,
            }
        }

        fn changed(&self) -> bool {
            self.changed
        }

        fn mark_clean(&mut self) {
            self.changed = false;
        }
    }

    struct TestRouter;

    impl Router for TestRouter {
        type Event = TestEvent;
        type Id = Uuid;

        fn route(&self, event: &TestEvent) -> SmallVec<[Uuid; 1]> {
            match event {
                TestEvent::Started { id } => smallvec![*id],
                TestEvent::StepCompleted { id, .. } => smallvec![*id],
            }
        }
    }

    // =========================================================================
    // Revision Tests
    // =========================================================================

    #[test]
    fn test_revision_none() {
        assert!(Revision::NONE.is_none());
        assert_eq!(Revision::NONE.value(), 0);
    }

    #[test]
    fn test_revision_next() {
        let r1 = Revision::NONE;
        let r2 = r1.next();
        let r3 = r2.next();

        assert_eq!(r1.value(), 0);
        assert_eq!(r2.value(), 1);
        assert_eq!(r3.value(), 2);
    }

    #[test]
    fn test_revision_display() {
        assert_eq!(format!("{}", Revision::NONE), "NONE");
        assert_eq!(format!("{}", Revision::new(5)), "r5");
    }

    // =========================================================================
    // Store Error Tests
    // =========================================================================

    #[test]
    fn test_store_error_display() {
        let conflict = StoreError::Conflict;
        assert!(conflict.to_string().contains("conflict"));

        let backend = StoreError::Backend(anyhow::anyhow!("connection failed"));
        assert!(backend.to_string().contains("connection failed"));
    }

    // =========================================================================
    // Router Tests
    // =========================================================================

    #[test]
    fn test_router_routes_events() {
        let router = TestRouter;
        let id = Uuid::new_v4();

        let ids = router.route(&TestEvent::Started { id });
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], id);
    }

    // =========================================================================
    // PersistentMachine Tests
    // =========================================================================

    #[test]
    fn test_persistent_machine_create() {
        let id = Uuid::new_v4();
        let event = TestEvent::Started { id };

        let machine = TestMachine::create(&id, &event);

        assert_eq!(machine.id, id);
        assert_eq!(machine.current_step, 0);
        assert!(!machine.completed);
        assert!(machine.changed);
    }

    #[test]
    fn test_persistent_machine_restore() {
        let snapshot = TestSnapshot {
            id: Uuid::new_v4(),
            current_step: 2,
            completed: false,
        };

        let machine = TestMachine::restore(snapshot.clone());

        assert_eq!(machine.id, snapshot.id);
        assert_eq!(machine.current_step, 2);
        assert!(!machine.changed);
    }

    #[test]
    fn test_persistent_machine_changed_tracking() {
        let id = Uuid::new_v4();
        let event = TestEvent::Started { id };
        let mut machine = TestMachine::create(&id, &event);

        assert!(machine.changed());

        machine.mark_clean();
        assert!(!machine.changed());

        machine.decide(&event);
        assert!(machine.changed());
    }

    // =========================================================================
    // InMemoryStore Tests
    // =========================================================================

    #[tokio::test]
    async fn test_in_memory_store_load_empty() {
        let store: InMemoryStore<Uuid, TestSnapshot> = InMemoryStore::new();
        let id = Uuid::new_v4();

        let result = store.load(&id).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_in_memory_store_save_and_load() {
        let store: InMemoryStore<Uuid, TestSnapshot> = InMemoryStore::new();
        let id = Uuid::new_v4();
        let snapshot = TestSnapshot {
            id,
            current_step: 1,
            completed: false,
        };

        let rev = store.save(&id, &snapshot, Revision::NONE).await.unwrap();
        assert_eq!(rev, Revision::new(1));

        let (loaded, loaded_rev) = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded, snapshot);
        assert_eq!(loaded_rev, Revision::new(1));
    }

    #[tokio::test]
    async fn test_in_memory_store_conflict_detection() {
        let store: InMemoryStore<Uuid, TestSnapshot> = InMemoryStore::new();
        let id = Uuid::new_v4();
        let snapshot = TestSnapshot {
            id,
            current_step: 1,
            completed: false,
        };

        store.save(&id, &snapshot, Revision::NONE).await.unwrap();

        let result = store.save(&id, &snapshot, Revision::NONE).await;
        assert!(matches!(result, Err(StoreError::Conflict)));

        let result = store.save(&id, &snapshot, Revision::new(1)).await;
        assert!(result.is_ok());
    }

    // =========================================================================
    // Integration Test
    // =========================================================================

    #[tokio::test]
    async fn test_full_persistent_machine_flow() {
        let store: InMemoryStore<Uuid, TestSnapshot> = InMemoryStore::new();
        let router = TestRouter;
        let id = Uuid::new_v4();

        // Event 1: Started
        let event1 = TestEvent::Started { id };
        let ids = router.route(&event1);
        assert_eq!(ids.len(), 1);

        // Load (new machine)
        let loaded = store.load(&id).await.unwrap();
        assert!(loaded.is_none());

        // Create and decide
        let mut machine = TestMachine::create(&id, &event1);
        let cmd = machine.decide(&event1);
        assert!(matches!(cmd, Some(TestCommand::DoStep { step: 1 })));
        assert!(machine.changed());

        // Save before dispatch
        let snapshot = machine.snapshot();
        let rev = store.save(&id, &snapshot, Revision::NONE).await.unwrap();
        machine.mark_clean();

        // Event 2: StepCompleted
        let event2 = TestEvent::StepCompleted { id, step: 1 };

        // Load existing
        let (loaded_snapshot, loaded_rev) = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded_rev, rev);

        let mut machine = TestMachine::restore(loaded_snapshot);
        assert!(!machine.changed());

        let cmd = machine.decide(&event2);
        assert!(matches!(cmd, Some(TestCommand::DoStep { step: 2 })));
        assert!(machine.changed());

        // Save
        let snapshot = machine.snapshot();
        let rev = store.save(&id, &snapshot, loaded_rev).await.unwrap();
        assert_eq!(rev, Revision::new(2));
    }
}
