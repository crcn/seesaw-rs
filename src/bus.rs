//! Type-erased event bus for broadcasting events.
//!
//! # Guarantees
//!
//! - **At-most-once delivery**: Slow receivers may miss events
//! - **In-memory only**: Events are not persisted
//! - **No replay**: Lagged receivers get `RecvError::Lagged`
//!
//! For durability, use:
//! - Entity status fields for workflow state
//! - Jobs for durable command execution
//! - Reapers for crash recovery
//!
//! # Correlation
//!
//! Events can be emitted with a correlation ID for tracking related work.
//! Use `emit_with_correlation` when you need to await completion of all
//! work triggered by an event.

use std::any::Any;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::core::{CorrelationId, Event, EventEnvelope};

/// Default channel capacity for the event bus.
const DEFAULT_CAPACITY: usize = 10000;

/// Type-erased event bus for broadcasting events.
///
/// The `EventBus` is a broadcast channel that allows multiple subscribers
/// to receive events. Events are wrapped in [`EventEnvelope`] which carries
/// correlation metadata for tracking related work.
///
/// # Example
///
/// ```ignore
/// let bus = EventBus::new();
///
/// // Subscribe to events
/// let mut receiver = bus.subscribe();
///
/// // Emit an event (fire-and-forget)
/// bus.emit(UserCreated { user_id: uuid::Uuid::new_v4() });
///
/// // Receive the event
/// let envelope = receiver.recv().await?;
/// if let Some(user_created) = envelope.downcast_ref::<UserCreated>() {
///     println!("User created: {:?}", user_created.user_id);
/// }
/// ```
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<EventEnvelope>,
}

impl EventBus {
    /// Create a new event bus with default capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a new event bus with the specified capacity.
    ///
    /// The capacity determines how many events can be buffered before
    /// slow receivers start lagging.
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Emit an event to all subscribers (fire-and-forget).
    ///
    /// Generates a new random correlation ID. Use `emit_with_correlation`
    /// if you need to track or await the work triggered by this event.
    ///
    /// Returns the number of receivers that received the event.
    pub fn emit<E: Event>(&self, event: E) -> usize {
        let envelope = EventEnvelope::new_random(event);
        self.sender.send(envelope).unwrap_or(0)
    }

    /// Emit an event with a specific correlation ID.
    ///
    /// Use this when you need to:
    /// - Track related work across the system
    /// - Await completion via `EngineHandle::emit_and_await`
    /// - Propagate correlation from an effect to child events
    ///
    /// Returns the number of receivers that received the event.
    pub fn emit_with_correlation<E: Event>(&self, event: E, cid: CorrelationId) -> usize {
        let envelope = EventEnvelope::new(cid, event);
        self.sender.send(envelope).unwrap_or(0)
    }

    /// Emit an event envelope directly.
    ///
    /// This is useful when forwarding envelopes or when you've already
    /// constructed the envelope.
    pub fn emit_envelope(&self, envelope: EventEnvelope) -> usize {
        self.sender.send(envelope).unwrap_or(0)
    }

    /// Emit a type-erased event to all subscribers.
    ///
    /// This wraps the event in an envelope with a random correlation ID.
    pub fn emit_any(&self, event: Arc<dyn Any + Send + Sync>) -> usize {
        let envelope = EventEnvelope {
            cid: CorrelationId::new(),
            type_id: (*event).type_id(),
            payload: event,
        };
        self.sender.send(envelope).unwrap_or(0)
    }

    /// Subscribe to events on this bus.
    ///
    /// Returns a receiver that will receive all event envelopes emitted after
    /// subscription. Events emitted before subscription are not received.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }

    /// Returns the number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscriber_count", &self.subscriber_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct TestEvent {
        value: i32,
    }
    // Event auto-impl by blanket

    #[derive(Debug, Clone, PartialEq)]
    struct OtherEvent {
        message: String,
    }
    // Event auto-impl by blanket

    #[tokio::test]
    async fn test_emit_and_receive() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        bus.emit(TestEvent { value: 42 });

        let envelope = receiver.recv().await.unwrap();
        let test_event = envelope.downcast_ref::<TestEvent>().unwrap();
        assert_eq!(test_event.value, 42);
    }

    #[tokio::test]
    async fn test_emit_with_correlation() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        let cid = CorrelationId::new();
        bus.emit_with_correlation(TestEvent { value: 99 }, cid);

        let envelope = receiver.recv().await.unwrap();
        assert_eq!(envelope.cid, cid);
        assert_eq!(envelope.downcast_ref::<TestEvent>().unwrap().value, 99);
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let bus = EventBus::new();
        let mut receiver1 = bus.subscribe();
        let mut receiver2 = bus.subscribe();

        bus.emit(TestEvent { value: 100 });

        // Both receivers should get the event
        let envelope1 = receiver1.recv().await.unwrap();
        let envelope2 = receiver2.recv().await.unwrap();

        assert_eq!(envelope1.downcast_ref::<TestEvent>().unwrap().value, 100);
        assert_eq!(envelope2.downcast_ref::<TestEvent>().unwrap().value, 100);
        // Same correlation ID
        assert_eq!(envelope1.cid, envelope2.cid);
    }

    #[tokio::test]
    async fn test_different_event_types() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        bus.emit(TestEvent { value: 1 });
        bus.emit(OtherEvent {
            message: "hello".to_string(),
        });
        bus.emit(TestEvent { value: 2 });

        // First event
        let envelope = receiver.recv().await.unwrap();
        assert!(envelope.downcast_ref::<TestEvent>().is_some());

        // Second event (different type)
        let envelope = receiver.recv().await.unwrap();
        assert!(envelope.downcast_ref::<OtherEvent>().is_some());
        assert_eq!(
            envelope.downcast_ref::<OtherEvent>().unwrap().message,
            "hello"
        );

        // Third event
        let envelope = receiver.recv().await.unwrap();
        assert!(envelope.downcast_ref::<TestEvent>().is_some());
    }

    #[tokio::test]
    async fn test_emit_returns_receiver_count() {
        let bus = EventBus::new();

        // No subscribers
        let count = bus.emit(TestEvent { value: 1 });
        assert_eq!(count, 0);

        // One subscriber
        let _receiver1 = bus.subscribe();
        let count = bus.emit(TestEvent { value: 2 });
        assert_eq!(count, 1);

        // Two subscribers
        let _receiver2 = bus.subscribe();
        let count = bus.emit(TestEvent { value: 3 });
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_subscriber_count() {
        let bus = EventBus::new();
        assert_eq!(bus.subscriber_count(), 0);

        let _r1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        let _r2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);

        drop(_r1);
        // Note: subscriber_count may not immediately reflect dropped receivers
        // due to how broadcast channels work
    }

    #[tokio::test]
    async fn test_emit_any() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 99 });
        bus.emit_any(event);

        let envelope = receiver.recv().await.unwrap();
        assert_eq!(envelope.downcast_ref::<TestEvent>().unwrap().value, 99);
    }

    #[tokio::test]
    async fn test_clone_shares_channel() {
        let bus1 = EventBus::new();
        let bus2 = bus1.clone();

        let mut receiver = bus1.subscribe();

        // Emit on cloned bus
        bus2.emit(TestEvent { value: 55 });

        // Should receive on original subscriber
        let envelope = receiver.recv().await.unwrap();
        assert_eq!(envelope.downcast_ref::<TestEvent>().unwrap().value, 55);
    }

    #[tokio::test]
    async fn test_late_subscriber_misses_events() {
        let bus = EventBus::new();

        // Emit before subscribing
        bus.emit(TestEvent { value: 1 });

        // Subscribe after emit
        let mut receiver = bus.subscribe();

        // Emit another event
        bus.emit(TestEvent { value: 2 });

        // Should only receive the second event
        let envelope = receiver.recv().await.unwrap();
        assert_eq!(envelope.downcast_ref::<TestEvent>().unwrap().value, 2);
    }

    #[test]
    fn test_debug_impl() {
        let bus = EventBus::new();
        let _r1 = bus.subscribe();
        let debug_str = format!("{:?}", bus);
        assert!(debug_str.contains("EventBus"));
        assert!(debug_str.contains("subscriber_count"));
    }

    #[tokio::test]
    async fn test_with_capacity() {
        let bus = EventBus::with_capacity(10);
        let mut receiver = bus.subscribe();

        // Should work with smaller capacity
        for i in 0..5 {
            bus.emit(TestEvent { value: i });
        }

        for i in 0..5 {
            let envelope = receiver.recv().await.unwrap();
            assert_eq!(envelope.downcast_ref::<TestEvent>().unwrap().value, i);
        }
    }
}
