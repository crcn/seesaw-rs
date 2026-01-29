//! Request/response helpers for edge code.
//!
//! This module provides ergonomic helpers for emitting events and awaiting
//! correlated responses. These are **syntactic sugar** over the event bus -
//! they do not change seesaw's semantics.
//!
//! # Important Semantics
//!
//! These helpers do NOT guarantee a response exists. They emit an event and
//! wait for a correlated response. If no response arrives, they timeout.
//!
//! This is honest event-driven behavior, not RPC.
//!
//! # Example
//!
//! ```ignore
//! use seesaw::{dispatch_request, EnvelopeMatch};
//!
//! let entry: Entry = dispatch_request(
//!     EntryRequestEvent::Create { ... },
//!     &bus,
//!     |m| {
//!         m.try_match(|e: &EntryEvent| match e {
//!             EntryEvent::Created { entry } => Some(Ok(entry.clone())),
//!             _ => None,
//!         })
//!         .or_try(|denied: &AuthorizationDenied| {
//!             Some(Err(anyhow!("Permission denied: {}", denied.reason)))
//!         })
//!         .result()
//!     }
//! ).await?;
//! ```

use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::time::timeout;

use crate::bus::EventBus;
use crate::core::{CorrelationId, EnvelopeMatch, Event};
use crate::error::CommandFailed;

/// Default timeout for request/response operations.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Emits a request event and waits for a correlated response.
///
/// This does NOT guarantee a response exists. It emits an event and waits
/// until some correlated event matches the extractor, or times out.
///
/// # Arguments
///
/// * `request` - The event to emit
/// * `bus` - The event bus to emit on and subscribe to
/// * `extractor` - A function that examines each correlated event and returns
///   `Some(result)` when it finds the response, or `None` to keep waiting
///
/// # Returns
///
/// The extracted response, or an error on timeout/bus closure.
///
/// # Example
///
/// ```ignore
/// let entry = dispatch_request(
///     CreateEntryRequest { ... },
///     &bus,
///     |m| m.try_match(|e: &EntryCreated| Some(Ok(e.entry.clone()))).result()
/// ).await?;
/// ```
pub async fn dispatch_request<Req, Res, F>(
    request: Req,
    bus: &EventBus,
    extractor: F,
) -> Result<Res>
where
    Req: Event + Clone,
    F: Fn(EnvelopeMatch<'_>) -> Option<Result<Res>>,
{
    dispatch_request_timeout(request, bus, DEFAULT_REQUEST_TIMEOUT, extractor).await
}

/// Emits a request event and waits for a correlated response with custom timeout.
///
/// See [`dispatch_request`] for details.
pub async fn dispatch_request_timeout<Req, Res, F>(
    request: Req,
    bus: &EventBus,
    request_timeout: Duration,
    extractor: F,
) -> Result<Res>
where
    Req: Event + Clone,
    F: Fn(EnvelopeMatch<'_>) -> Option<Result<Res>>,
{
    // Generate correlation ID
    let cid = CorrelationId::new();

    // Subscribe before emitting to avoid race
    let mut receiver = bus.subscribe();

    // Emit with correlation
    bus.emit_with_correlation(request, cid);

    // Wait for matching response
    let result = timeout(request_timeout, async {
        loop {
            match receiver.recv().await {
                Ok(envelope) => {
                    // Only consider events with matching correlation ID
                    if envelope.cid != cid {
                        continue;
                    }

                    // Try to extract result via user's extractor
                    if let Some(result) = extractor(EnvelopeMatch::new(&envelope)) {
                        return result;
                    }

                    // Auto-handle CommandFailed events so edges don't have to
                    if let Some(failed) = envelope.downcast_ref::<CommandFailed>() {
                        return Err(anyhow!("{}", failed.safe_message));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(anyhow!("event bus closed"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(lagged = n, "request receiver lagged, events may be missed");
                    continue;
                }
            }
        }
    })
    .await;

    match result {
        Ok(res) => res,
        Err(_) => Err(anyhow!("request timed out after {:?}", request_timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SafeErrorCategory;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug, Clone)]
    struct TestRequest {
        value: i32,
    }

    #[derive(Debug, Clone)]
    struct TestResponse {
        result: i32,
    }

    #[derive(Debug, Clone)]
    struct TestDenied {
        reason: String,
    }

    #[tokio::test]
    async fn test_dispatch_request_success() {
        let bus = EventBus::new();

        // Spawn a "handler" that responds to requests
        let handler_bus = bus.clone();
        tokio::spawn(async move {
            let mut rx = handler_bus.subscribe();
            while let Ok(envelope) = rx.recv().await {
                if let Some(req) = envelope.downcast_ref::<TestRequest>() {
                    // Respond with same correlation ID
                    handler_bus.emit_with_correlation(
                        TestResponse {
                            result: req.value * 2,
                        },
                        envelope.cid,
                    );
                    break;
                }
            }
        });

        // Small delay to let handler subscribe
        tokio::time::sleep(Duration::from_millis(10)).await;

        let result: i32 = dispatch_request(TestRequest { value: 21 }, &bus, |m| {
            m.try_match(|r: &TestResponse| Some(Ok(r.result))).result()
        })
        .await
        .unwrap();

        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_dispatch_request_denied() {
        let bus = EventBus::new();

        // Spawn a "handler" that denies requests
        let handler_bus = bus.clone();
        tokio::spawn(async move {
            let mut rx = handler_bus.subscribe();
            while let Ok(envelope) = rx.recv().await {
                if envelope.downcast_ref::<TestRequest>().is_some() {
                    handler_bus.emit_with_correlation(
                        TestDenied {
                            reason: "not allowed".into(),
                        },
                        envelope.cid,
                    );
                    break;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let result: Result<i32> = dispatch_request(TestRequest { value: 21 }, &bus, |m| {
            m.try_match(|r: &TestResponse| Some(Ok(r.result)))
                .or_try(|d: &TestDenied| Some(Err(anyhow!("denied: {}", d.reason))))
                .result()
        })
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("denied"));
    }

    #[tokio::test]
    async fn test_dispatch_request_timeout() {
        let bus = EventBus::new();

        // No handler - will timeout
        let result: Result<i32> = dispatch_request_timeout(
            TestRequest { value: 1 },
            &bus,
            Duration::from_millis(50),
            |m| m.try_match(|r: &TestResponse| Some(Ok(r.result))).result(),
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn test_dispatch_request_ignores_uncorrelated() {
        let bus = EventBus::new();
        let call_count = Arc::new(AtomicUsize::new(0));

        // Spawn handler that sends both correlated and uncorrelated responses
        let handler_bus = bus.clone();
        tokio::spawn(async move {
            let mut rx = handler_bus.subscribe();
            while let Ok(envelope) = rx.recv().await {
                if let Some(req) = envelope.downcast_ref::<TestRequest>() {
                    // Send uncorrelated response first (should be ignored)
                    handler_bus.emit(TestResponse { result: 999 });

                    // Then send correlated response
                    handler_bus.emit_with_correlation(
                        TestResponse {
                            result: req.value * 2,
                        },
                        envelope.cid,
                    );
                    break;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let cc = call_count.clone();
        let result: i32 = dispatch_request(TestRequest { value: 21 }, &bus, move |m| {
            cc.fetch_add(1, Ordering::Relaxed);
            m.try_match(|r: &TestResponse| Some(Ok(r.result))).result()
        })
        .await
        .unwrap();

        // Should get 42 (correlated), not 999 (uncorrelated)
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_dispatch_request_auto_handles_command_failed() {
        let bus = EventBus::new();

        // Spawn a "handler" that emits CommandFailed
        let handler_bus = bus.clone();
        tokio::spawn(async move {
            let mut rx = handler_bus.subscribe();
            while let Ok(envelope) = rx.recv().await {
                if envelope.downcast_ref::<TestRequest>().is_some() {
                    // Emit CommandFailed with the same correlation ID
                    handler_bus.emit_with_correlation(
                        CommandFailed {
                            command_type: "TestCommand",
                            category: SafeErrorCategory::Unauthorized,
                            safe_message: "Not authorized to perform this action".into(),
                            cid: envelope.cid,
                        },
                        envelope.cid,
                    );
                    break;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        // The extractor only matches TestResponse, but CommandFailed is auto-handled
        let result: Result<i32> = dispatch_request_timeout(
            TestRequest { value: 21 },
            &bus,
            Duration::from_millis(500),
            |m| m.try_match(|r: &TestResponse| Some(Ok(r.result))).result(),
        )
        .await;

        assert!(result.is_err(), "Expected error from CommandFailed");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Not authorized"),
            "Expected auth error, got: {}",
            err_msg
        );
    }
}
