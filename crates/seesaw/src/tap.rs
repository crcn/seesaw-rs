//! Event Taps - observe facts without making decisions or mutating state.
//!
//! Taps provide a first-class way to observe events for:
//! - Publishing to external systems (NATS, webhooks)
//! - Analytics and metrics
//! - Logging and auditing
//!
//! # Architecture Role
//!
//! Seesaw has three roles:
//!
//! | Role    | Purpose            | Can decide? | Can mutate? | Can emit? |
//! |---------|--------------------|-------------|-------------|-----------|
//! | Machine | Decide intent      | ✅          | ❌          | ❌        |
//! | Effect  | Execute intent     | ❌          | ✅          | ✅        |
//! | Tap     | Observe facts      | ❌          | ❌          | ❌        |
//!
//! # Execution Order
//!
//! Taps run **after** effects:
//!
//! ```text
//! Event
//!  → Machines (decide)
//!  → Commands
//!  → Effects (execute, may emit new events)
//!  → Events
//!  → Taps   ← here (observe committed facts)
//! ```
//!
//! This guarantees:
//! - Authorization already passed
//! - Data committed to DB
//! - No rollback ambiguity
//! - Observability correctness
//!
//! # Example
//!
//! ```ignore
//! use seesaw::{EventTap, TapContext};
//!
//! pub struct NatsPublishTap {
//!     client: async_nats::Client,
//! }
//!
//! #[async_trait]
//! impl EventTap<EntryEvent> for NatsPublishTap {
//!     async fn on_event(&self, event: &EntryEvent, ctx: &TapContext) -> Result<()> {
//!         let payload = serde_json::to_vec(event)?;
//!         self.client.publish("entry.events", payload.into()).await?;
//!         Ok(())
//!     }
//! }
//! ```

use std::any::TypeId;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use tracing::warn;

use crate::core::{CorrelationId, Event};

// =============================================================================
// Tap Context
// =============================================================================

/// Context provided to event taps.
///
/// Intentionally minimal - taps observe, they don't act.
/// No `emit()`, no `deps()`, no state mutation.
pub struct TapContext {
    /// Correlation ID for the event (NONE if uncorrelated).
    pub correlation_id: CorrelationId,
    /// When this tap execution started.
    pub timestamp: Instant,
}

impl TapContext {
    /// Create a new tap context.
    pub fn new(correlation_id: CorrelationId) -> Self {
        Self {
            correlation_id,
            timestamp: Instant::now(),
        }
    }

    /// Check if this event has a real correlation ID (not NONE).
    pub fn has_correlation(&self) -> bool {
        self.correlation_id.is_some()
    }
}

// =============================================================================
// Event Tap Trait
// =============================================================================

/// Trait for observing events without side effects.
///
/// Taps are called after effects complete. They:
/// - Receive committed facts
/// - Cannot emit new events
/// - Cannot access deps (use closure capture if needed)
/// - Run concurrently by default
///
/// Use taps for:
/// - Publishing to NATS/Kafka
/// - Sending webhooks
/// - Recording metrics
/// - Audit logging
#[async_trait]
pub trait EventTap<E: Event>: Send + Sync + 'static {
    /// Called when an event of type E is observed.
    ///
    /// Errors are logged but do not affect the main flow.
    /// Taps should be fire-and-forget - don't rely on their success.
    async fn on_event(&self, event: &E, ctx: &TapContext) -> Result<()>;
}

// =============================================================================
// Tap Runner (Type-Erased)
// =============================================================================

/// Type-erased tap runner that can handle any event type.
pub(crate) struct TapRunner {
    event_type: TypeId,
    run_fn: Box<dyn Fn(&dyn std::any::Any, CorrelationId) + Send + Sync>,
    name: &'static str,
}

impl TapRunner {
    /// Create a tap runner for a specific tap and event type.
    ///
    /// E must be Clone (which Event types are via the blanket impl).
    pub fn new<E: Event + Clone, T: EventTap<E>>(tap: T, name: &'static str) -> Self {
        let tap = Arc::new(tap);

        Self {
            event_type: TypeId::of::<E>(),
            name,
            run_fn: Box::new(move |any_event, correlation_id| {
                // Downcast and clone the event before spawning
                let Some(event) = any_event.downcast_ref::<E>() else {
                    return;
                };
                let event = event.clone();

                let tap = tap.clone();
                let ctx = TapContext::new(correlation_id);

                // Spawn as fire-and-forget - taps don't block the main flow
                tokio::spawn(async move {
                    if let Err(e) = tap.on_event(&event, &ctx).await {
                        warn!(
                            tap = %std::any::type_name::<T>(),
                            error = %e,
                            "tap failed"
                        );
                    }
                });
            }),
        }
    }

    /// Get the event type this runner handles.
    pub fn event_type(&self) -> TypeId {
        self.event_type
    }

    /// Get the tap name (for debugging).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Run the tap if the event matches.
    pub fn try_run(&self, event: &dyn std::any::Any, correlation_id: CorrelationId) {
        if event.type_id() == self.event_type {
            (self.run_fn)(event, correlation_id);
        }
    }
}

impl std::fmt::Debug for TapRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapRunner")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

// =============================================================================
// Tap Registry
// =============================================================================

/// Registry of event taps.
///
/// Taps are registered per event type and called when events are observed.
#[derive(Default)]
pub(crate) struct TapRegistry {
    taps: Vec<TapRunner>,
}

impl TapRegistry {
    /// Create a new empty tap registry.
    pub fn new() -> Self {
        Self { taps: Vec::new() }
    }

    /// Register a tap for an event type.
    pub fn register<E: Event + Clone, T: EventTap<E>>(&mut self, tap: T, name: &'static str) {
        self.taps.push(TapRunner::new(tap, name));
    }

    /// Run all taps that match the given event.
    pub fn run_all(&self, event: &dyn std::any::Any, correlation_id: Option<CorrelationId>) {
        let cid = correlation_id.unwrap_or(CorrelationId::NONE);
        for tap in &self.taps {
            tap.try_run(event, cid);
        }
    }

    /// Check if any taps are registered.
    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Get the number of registered taps.
    pub fn len(&self) -> usize {
        self.taps.len()
    }
}

impl std::fmt::Debug for TapRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapRegistry")
            .field("tap_count", &self.taps.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Duration;

    #[derive(Debug, Clone)]
    struct TestEvent {
        value: i32,
    }

    struct CountingTap {
        count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl EventTap<TestEvent> for CountingTap {
        async fn on_event(&self, _event: &TestEvent, _ctx: &TapContext) -> Result<()> {
            self.count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_tap_registry_runs_matching_taps() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut registry = TapRegistry::new();

        registry.register(
            CountingTap {
                count: count.clone(),
            },
            "test_tap",
        );

        let event = TestEvent { value: 42 };
        registry.run_all(&event, None);

        // Give the spawned task time to run
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_tap_receives_correlation_id() {
        let received_cid = Arc::new(std::sync::Mutex::new(CorrelationId::NONE));

        struct CorrelationTap {
            received: Arc<std::sync::Mutex<CorrelationId>>,
        }

        #[async_trait]
        impl EventTap<TestEvent> for CorrelationTap {
            async fn on_event(&self, _event: &TestEvent, ctx: &TapContext) -> Result<()> {
                *self.received.lock().unwrap() = ctx.correlation_id;
                Ok(())
            }
        }

        let mut registry = TapRegistry::new();
        registry.register(
            CorrelationTap {
                received: received_cid.clone(),
            },
            "cid_tap",
        );

        let cid = CorrelationId::new();
        let event = TestEvent { value: 42 };
        registry.run_all(&event, Some(cid));

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_eq!(*received_cid.lock().unwrap(), cid);
    }

    #[test]
    fn test_tap_context_has_timestamp() {
        let ctx = TapContext::new(CorrelationId::NONE);
        // Just verify it's created with a reasonable timestamp
        assert!(ctx.timestamp.elapsed().as_secs() < 1);
    }
}
