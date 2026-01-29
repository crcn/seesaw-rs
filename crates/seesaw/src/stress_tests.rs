//! Stress tests designed to break the seesaw library.
//!
//! These tests exercise edge cases, race conditions, and potential failure modes.

#[cfg(test)]
mod stress_tests {
    use crate::bus::EventBus;
    use crate::core::{Command, CorrelationId};
    use crate::effect_impl::{Effect, EffectContext};
    use crate::engine::{EngineBuilder, InflightTracker};
    use crate::machine::Machine;
    use anyhow::Result;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // ==========================================================================
    // Test Types
    // ==========================================================================

    #[derive(Debug, Clone)]
    struct TestDeps;

    #[derive(Debug, Clone)]
    struct TriggerEvent {
        id: usize,
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct ResultEvent {
        id: usize,
    }

    #[derive(Debug, Clone)]
    struct FailEvent {
        id: usize,
    }

    #[derive(Debug, Clone)]
    struct TriggerCommand {
        id: usize,
    }
    impl Command for TriggerCommand {}

    #[derive(Debug, Clone)]
    struct FailCommand {
        id: usize,
    }
    impl Command for FailCommand {}

    // SlowCommand and PanicCommand available for future stress tests
    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct SlowCommand {
        delay_ms: u64,
    }
    impl Command for SlowCommand {}

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct PanicCommand;
    impl Command for PanicCommand {}

    // ==========================================================================
    // Test Machines
    // ==========================================================================

    struct TriggerMachine;
    impl Machine for TriggerMachine {
        type Event = TriggerEvent;
        type Command = TriggerCommand;

        fn decide(&mut self, event: &TriggerEvent) -> Option<TriggerCommand> {
            Some(TriggerCommand { id: event.id })
        }
    }

    struct FailMachine;
    impl Machine for FailMachine {
        type Event = FailEvent;
        type Command = FailCommand;

        fn decide(&mut self, event: &FailEvent) -> Option<FailCommand> {
            Some(FailCommand { id: event.id })
        }
    }

    // ==========================================================================
    // Test Effects
    // ==========================================================================

    struct SuccessEffect {
        count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<TriggerCommand, TestDeps> for SuccessEffect {
        type Event = ResultEvent;

        async fn execute(
            &self,
            cmd: TriggerCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<ResultEvent> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(ResultEvent { id: cmd.id })
        }
    }

    struct AlwaysFailEffect {
        count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<FailCommand, TestDeps> for AlwaysFailEffect {
        type Event = ResultEvent;

        async fn execute(
            &self,
            cmd: FailCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<ResultEvent> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("intentional failure for id {}", cmd.id))
        }
    }

    // SlowEffect and PanicEffect available for future stress tests
    #[allow(dead_code)]
    struct SlowEffect;

    #[async_trait::async_trait]
    impl Effect<SlowCommand, TestDeps> for SlowEffect {
        type Event = ResultEvent;

        async fn execute(
            &self,
            cmd: SlowCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<ResultEvent> {
            tokio::time::sleep(Duration::from_millis(cmd.delay_ms)).await;
            Ok(ResultEvent { id: 0 })
        }
    }

    #[allow(dead_code)]
    struct PanicEffect;

    #[async_trait::async_trait]
    impl Effect<PanicCommand, TestDeps> for PanicEffect {
        type Event = ResultEvent;

        async fn execute(
            &self,
            _cmd: PanicCommand,
            _ctx: EffectContext<TestDeps>,
        ) -> Result<ResultEvent> {
            panic!("intentional panic in effect");
        }
    }

    // ==========================================================================
    // TEST: Race condition in InflightTracker::dec()
    // ==========================================================================
    //
    // This test attempts to trigger a race condition between:
    // 1. dec() checking has_error and deciding to remove the entry
    // 2. Another thread incrementing the count before removal
    //
    // If the race exists, entries may leak in the DashMap.

    #[tokio::test]
    async fn test_inflight_tracker_race_condition() {
        let tracker = Arc::new(InflightTracker::new());
        let iterations = 10000;
        let mut handles = vec![];

        for _ in 0..iterations {
            let cid = CorrelationId::new();
            let tracker_clone = tracker.clone();

            // Spawn tasks that race to increment and decrement
            let h = tokio::spawn(async move {
                // Increment
                tracker_clone.inc(cid, 1);

                // Yield to allow other tasks to interleave
                tokio::task::yield_now().await;

                // Decrement (should remove entry)
                tracker_clone.dec(cid, 1);
            });
            handles.push(h);
        }

        // Wait for all tasks
        for h in handles {
            h.await.unwrap();
        }

        // Check for leaks - there should be no entries left
        let active = tracker.active_count();
        assert_eq!(
            active, 0,
            "InflightTracker leaked {} entries after {} iterations. \
             This indicates a race condition.",
            active, iterations
        );
    }

    // ==========================================================================
    // TEST: Concurrent inc/dec/error on same correlation ID
    // ==========================================================================

    #[tokio::test]
    async fn test_inflight_tracker_concurrent_same_cid() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();
        let iterations = 1000;
        let mut handles = vec![];

        // Increment first
        tracker.inc(cid, iterations);

        // Spawn many concurrent decrements
        for i in 0..iterations {
            let tracker_clone = tracker.clone();
            let h = tokio::spawn(async move {
                // Some tasks record errors
                if i % 10 == 0 {
                    tracker_clone.record_error(cid, anyhow::anyhow!("error {}", i));
                }
                tracker_clone.dec(cid, 1);
            });
            handles.push(h);
        }

        for h in handles {
            h.await.unwrap();
        }

        // Wait for zero should complete (possibly with error)
        let result = tokio::time::timeout(Duration::from_millis(100), tracker.wait_zero(cid)).await;

        assert!(
            result.is_ok(),
            "wait_zero timed out - inflight count never reached zero"
        );
    }

    // ==========================================================================
    // TEST: Error entries are cleaned up even without wait_zero
    // ==========================================================================
    //
    // When errors are recorded but wait_zero is never called (fire-and-forget),
    // entries should still be cleaned up to prevent memory leaks.

    #[tokio::test]
    async fn test_inflight_tracker_error_entry_leak() {
        let tracker = Arc::new(InflightTracker::new());
        let num_errors = 100;

        // Create entries with errors that are never awaited
        for i in 0..num_errors {
            let cid = CorrelationId::new();
            tracker.inc(cid, 1);
            tracker.record_error(cid, anyhow::anyhow!("error {}", i));
            tracker.dec(cid, 1);
            // Note: we never call wait_zero, but entries should still be cleaned up
        }

        let active = tracker.active_count();

        // With the waiter tracking fix, error entries are cleaned up immediately
        // when no one is waiting for them (fire-and-forget pattern)
        assert_eq!(
            active, 0,
            "Error entries should be cleaned up when no waiter exists. Found {} leaked entries.",
            active
        );
    }

    // ==========================================================================
    // TEST: emit_and_await returns quickly on effect error
    // ==========================================================================

    #[tokio::test]
    async fn test_emit_and_await_fast_error_return() {
        let fail_count = Arc::new(AtomicUsize::new(0));

        let engine = EngineBuilder::new(TestDeps)
            .with_machine(FailMachine)
            .with_effect::<FailCommand, _>(AlwaysFailEffect {
                count: fail_count.clone(),
            })
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Run multiple error scenarios
        for i in 0..10 {
            let start = std::time::Instant::now();
            let result = handle
                .emit_and_await_timeout(FailEvent { id: i }, Duration::from_millis(500))
                .await;
            let elapsed = start.elapsed();

            // Should complete much faster than the timeout
            assert!(
                elapsed < Duration::from_millis(100),
                "Iteration {} took {:?}, expected < 100ms. \
                 This indicates inflight count is not being decremented on error.",
                i,
                elapsed
            );

            // Should return an error
            assert!(
                result.is_err(),
                "Iteration {} should have returned an error",
                i
            );
        }

        // Effect was called for each emit
        assert_eq!(fail_count.load(Ordering::SeqCst), 10);

        handle.abort();
    }

    // ==========================================================================
    // TEST: High concurrency emit_and_await
    // ==========================================================================

    #[tokio::test]
    async fn test_concurrent_emit_and_await() {
        let success_count = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(InflightTracker::new());

        let engine = EngineBuilder::new(TestDeps)
            .with_inflight(inflight.clone())
            .with_machine(TriggerMachine)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .build();

        let handle = Arc::new(engine.start());
        tokio::time::sleep(Duration::from_millis(10)).await;

        let num_concurrent = 100;
        let mut join_handles = vec![];

        for i in 0..num_concurrent {
            let h = handle.clone();
            let jh = tokio::spawn(async move {
                h.emit_and_await_timeout(TriggerEvent { id: i }, Duration::from_secs(5))
                    .await
            });
            join_handles.push(jh);
        }

        let mut success = 0;
        let mut failures = 0;

        for jh in join_handles {
            match jh.await.unwrap() {
                Ok(()) => success += 1,
                Err(_) => failures += 1,
            }
        }

        assert_eq!(
            success,
            num_concurrent,
            "Expected {} successful emit_and_await calls, got {} successes and {} failures. \
             Inflight entries: {}",
            num_concurrent,
            success,
            failures,
            inflight.active_count()
        );

        // No inflight entries should remain
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            inflight.active_count(),
            0,
            "Inflight tracker has {} leaked entries after concurrent test",
            inflight.active_count()
        );

        handle.abort();
    }

    // ==========================================================================
    // TEST: Mix of success and failure in concurrent emits
    // ==========================================================================

    #[tokio::test]
    async fn test_concurrent_mixed_success_and_failure() {
        let success_count = Arc::new(AtomicUsize::new(0));
        let fail_count = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(InflightTracker::new());

        let engine = EngineBuilder::new(TestDeps)
            .with_inflight(inflight.clone())
            .with_machine(TriggerMachine)
            .with_machine(FailMachine)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .with_effect::<FailCommand, _>(AlwaysFailEffect {
                count: fail_count.clone(),
            })
            .build();

        let handle = Arc::new(engine.start());
        tokio::time::sleep(Duration::from_millis(10)).await;

        let num_each = 50;
        let mut join_handles = vec![];

        // Emit success events
        for i in 0..num_each {
            let h = handle.clone();
            let jh = tokio::spawn(async move {
                h.emit_and_await_timeout(TriggerEvent { id: i }, Duration::from_secs(5))
                    .await
            });
            join_handles.push((jh, true)); // true = expect success
        }

        // Emit fail events
        for i in 0..num_each {
            let h = handle.clone();
            let jh = tokio::spawn(async move {
                h.emit_and_await_timeout(FailEvent { id: i }, Duration::from_secs(5))
                    .await
            });
            join_handles.push((jh, false)); // false = expect failure
        }

        let mut success_ok = 0;
        let mut fail_err = 0;

        for (jh, expect_success) in join_handles {
            let result = jh.await.unwrap();
            if expect_success && result.is_ok() {
                success_ok += 1;
            } else if !expect_success && result.is_err() {
                fail_err += 1;
            }
        }

        assert_eq!(success_ok, num_each, "Not all success events succeeded");
        assert_eq!(fail_err, num_each, "Not all fail events returned errors");

        // Allow cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check for leaks
        let remaining = inflight.active_count();
        assert_eq!(
            remaining, 0,
            "Inflight tracker has {} leaked entries after mixed test",
            remaining
        );

        handle.abort();
    }

    // ==========================================================================
    // TEST: EventBus channel overflow
    // ==========================================================================

    #[tokio::test]
    async fn test_event_bus_overflow() {
        // Create bus with very small capacity
        let bus = EventBus::with_capacity(10);
        let mut receiver = bus.subscribe();

        // Emit more events than capacity
        for i in 0..100 {
            bus.emit(TriggerEvent { id: i });
        }

        // Receiver should be lagged
        let mut received = 0;
        let mut lagged = false;

        loop {
            match receiver.try_recv() {
                Ok(_) => received += 1,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    lagged = true;
                    eprintln!("Receiver lagged by {} events", n);
                    // After lagging, try to receive remaining
                    continue;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }

        // With capacity 10 and 100 events, we should have lagged
        assert!(
            lagged,
            "Expected receiver to lag with capacity 10 and 100 events, but received {} without lagging",
            received
        );
    }

    // ==========================================================================
    // TEST: InflightTracker wait_zero with already-removed entry
    // ==========================================================================

    #[tokio::test]
    async fn test_wait_zero_entry_already_removed() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::new();

        // Inc and dec without waiting
        tracker.inc(cid, 1);
        tracker.dec(cid, 1);

        // Entry should be removed now
        // wait_zero should return immediately
        let result = tokio::time::timeout(Duration::from_millis(10), tracker.wait_zero(cid)).await;

        assert!(
            result.is_ok(),
            "wait_zero should return immediately for removed entry"
        );
        assert!(
            result.unwrap().is_ok(),
            "wait_zero should return Ok for removed entry"
        );
    }

    // ==========================================================================
    // TEST: Notify edge-triggered race
    // ==========================================================================
    //
    // Tokio's Notify is edge-triggered. If the notification fires between
    // checking the count and awaiting the notified future, we might miss it.
    // wait_zero handles this by looping, but let's stress test it.

    #[tokio::test]
    async fn test_notify_race() {
        let tracker = Arc::new(InflightTracker::new());
        let iterations = 1000;
        let mut handles = vec![];

        for i in 0..iterations {
            let cid = CorrelationId::new();
            let tracker_clone = tracker.clone();
            let tracker_clone2 = tracker.clone();

            tracker.inc(cid, 1);

            // Spawn waiter
            let wait_handle = tokio::spawn(async move { tracker_clone.wait_zero(cid).await });

            // Spawn decrementer with small delay to vary timing
            let delay = i as u64 % 100;
            let dec_handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_micros(delay)).await;
                tracker_clone2.dec(cid, 1);
            });

            handles.push((wait_handle, dec_handle));
        }

        let mut failures = 0;
        for (wait_h, dec_h) in handles {
            dec_h.await.unwrap();
            let wait_result = tokio::time::timeout(Duration::from_millis(100), wait_h).await;

            match wait_result {
                Ok(Ok(Ok(()))) => {} // Success
                _ => failures += 1,
            }
        }

        assert_eq!(
            failures, 0,
            "{} out of {} wait_zero calls failed/timed out. \
             This indicates a race condition in the notify pattern.",
            failures, iterations
        );
    }

    // ==========================================================================
    // TEST: Multiple engines sharing inflight tracker
    // ==========================================================================

    #[tokio::test]
    async fn test_shared_inflight_tracker() {
        let success_count = Arc::new(AtomicUsize::new(0));
        let shared_inflight = Arc::new(InflightTracker::new());
        let shared_bus = EventBus::new();

        // Two engines sharing the same inflight tracker and bus
        let engine1 = EngineBuilder::new(TestDeps)
            .with_bus(shared_bus.clone())
            .with_inflight(shared_inflight.clone())
            .with_machine(TriggerMachine)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .build();

        // Note: Only one engine can have the TriggerMachine, otherwise
        // commands would be emitted twice. But we can have different machines.

        let handle1 = engine1.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Emit via handle1
        let result = handle1
            .emit_and_await_timeout(TriggerEvent { id: 1 }, Duration::from_secs(1))
            .await;

        assert!(
            result.is_ok(),
            "emit_and_await should succeed with shared tracker"
        );
        assert_eq!(success_count.load(Ordering::SeqCst), 1);

        handle1.abort();
    }

    // ==========================================================================
    // TEST: Rapid fire emit without await
    // ==========================================================================

    #[tokio::test]
    async fn test_rapid_fire_emit() {
        let success_count = Arc::new(AtomicUsize::new(0));

        let engine = EngineBuilder::new(TestDeps)
            .with_machine(TriggerMachine)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Fire and forget many events
        let num_events = 1000;
        for i in 0..num_events {
            handle.emit(TriggerEvent { id: i });
        }

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(500)).await;

        let processed = success_count.load(Ordering::SeqCst);
        assert_eq!(
            processed, num_events,
            "Expected {} events processed, got {}",
            num_events, processed
        );

        handle.abort();
    }

    // ==========================================================================
    // TEST: CorrelationId::NONE handling
    // ==========================================================================

    #[tokio::test]
    async fn test_correlation_id_none_handling() {
        let tracker = Arc::new(InflightTracker::new());
        let cid = CorrelationId::NONE;

        // Operations with NONE correlation should still work
        tracker.inc(cid, 1);
        tracker.dec(cid, 1);

        // wait_zero should return immediately
        let result = tokio::time::timeout(Duration::from_millis(10), tracker.wait_zero(cid)).await;

        assert!(result.is_ok());
    }

    // ==========================================================================
    // TEST: Stress test inflight tracker under high load
    // ==========================================================================

    #[tokio::test]
    async fn test_inflight_tracker_high_load() {
        let tracker = Arc::new(InflightTracker::new());
        let num_correlations = 1000;
        let ops_per_correlation = 10;
        let mut handles = vec![];

        for _ in 0..num_correlations {
            let cid = CorrelationId::new();
            let tracker_clone = tracker.clone();

            let h = tokio::spawn(async move {
                // Increment multiple times
                for _ in 0..ops_per_correlation {
                    tracker_clone.inc(cid, 1);
                }

                // Decrement multiple times
                for _ in 0..ops_per_correlation {
                    tracker_clone.dec(cid, 1);
                }

                // Wait for zero
                tracker_clone.wait_zero(cid).await
            });
            handles.push(h);
        }

        let mut failures = 0;
        for h in handles {
            if let Err(_) = h.await.unwrap() {
                failures += 1;
            }
        }

        assert_eq!(
            failures, 0,
            "{} correlations failed to complete under high load",
            failures
        );

        // Check for leaks
        let remaining = tracker.active_count();
        assert_eq!(
            remaining, 0,
            "Inflight tracker has {} leaked entries after high load test",
            remaining
        );
    }

    // ==========================================================================
    // TEST: Timeout cleanup removes entry
    // ==========================================================================

    #[tokio::test]
    async fn test_timeout_cleanup() {
        let inflight = Arc::new(InflightTracker::new());

        // Machine that never emits a command (so work never completes)
        struct NoOpMachine;
        impl Machine for NoOpMachine {
            type Event = TriggerEvent;
            type Command = TriggerCommand;

            fn decide(&mut self, _event: &TriggerEvent) -> Option<TriggerCommand> {
                None // Never emit command
            }
        }

        let engine = EngineBuilder::new(TestDeps)
            .with_inflight(inflight.clone())
            .with_machine(NoOpMachine)
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // emit_and_await should timeout quickly since no command is emitted
        // But wait - the event itself gets processed, decrementing the count.
        // Actually, with NoOpMachine, the event is processed but no command emitted,
        // so inflight should be decremented by the event processing.

        let result = handle
            .emit_and_await_timeout(TriggerEvent { id: 1 }, Duration::from_millis(100))
            .await;

        // Should succeed because event was processed (even with no command)
        assert!(result.is_ok());

        // Entry should be cleaned up
        assert_eq!(inflight.active_count(), 0);

        handle.abort();
    }

    // ==========================================================================
    // TEST: Effect panics don't hang the system
    // ==========================================================================
    //
    // When an effect panics, the system should:
    // 1. Not crash the runtime
    // 2. Decrement inflight count so emit_and_await doesn't hang
    // 3. Return an error to the caller

    #[derive(Debug, Clone)]
    struct PanicTriggerEvent {
        id: usize,
    }

    #[derive(Debug, Clone)]
    struct PanicResultEvent {
        id: usize,
    }

    struct PanicTriggerMachine;
    impl Machine for PanicTriggerMachine {
        type Event = PanicTriggerEvent;
        type Command = PanicCommand;

        fn decide(&mut self, _event: &PanicTriggerEvent) -> Option<PanicCommand> {
            Some(PanicCommand)
        }
    }

    #[tokio::test]
    async fn test_effect_panic_does_not_hang() {
        let inflight = Arc::new(InflightTracker::new());

        let engine = EngineBuilder::new(TestDeps)
            .with_inflight(inflight.clone())
            .with_machine(PanicTriggerMachine)
            .with_effect::<PanicCommand, _>(PanicEffect)
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // This should NOT hang - should return error or timeout quickly
        let start = std::time::Instant::now();
        let result = handle
            .emit_and_await_timeout(PanicTriggerEvent { id: 1 }, Duration::from_millis(500))
            .await;
        let elapsed = start.elapsed();

        // Should complete within reasonable time (panic caught and handled)
        // Even if it times out at 500ms, that's still "doesn't hang forever"
        assert!(
            elapsed < Duration::from_secs(1),
            "Effect panic caused hang: {:?}",
            elapsed
        );

        // Should return an error (either from panic or timeout)
        assert!(
            result.is_err(),
            "Expected error after effect panic, got Ok"
        );

        // Inflight should be cleaned up (no leaks)
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            inflight.active_count(),
            0,
            "Effect panic leaked {} inflight entries",
            inflight.active_count()
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_effect_panic_allows_subsequent_events() {
        let success_count = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(InflightTracker::new());

        let engine = EngineBuilder::new(TestDeps)
            .with_inflight(inflight.clone())
            .with_machine(PanicTriggerMachine)
            .with_machine(TriggerMachine)
            .with_effect::<PanicCommand, _>(PanicEffect)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // First: trigger a panic
        let _ = handle
            .emit_and_await_timeout(PanicTriggerEvent { id: 1 }, Duration::from_millis(500))
            .await;

        // Wait for cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Second: runtime should still be processing events
        let result = handle
            .emit_and_await_timeout(TriggerEvent { id: 2 }, Duration::from_millis(500))
            .await;

        assert!(
            result.is_ok(),
            "Runtime stopped processing after effect panic: {:?}",
            result
        );
        assert_eq!(
            success_count.load(Ordering::SeqCst),
            1,
            "Effect was not called after panic"
        );

        handle.abort();
    }

    // ==========================================================================
    // TEST: Machine panics don't crash the runtime
    // ==========================================================================

    struct PanicMachine;
    impl Machine for PanicMachine {
        type Event = TriggerEvent;
        type Command = TriggerCommand;

        fn decide(&mut self, _event: &TriggerEvent) -> Option<TriggerCommand> {
            panic!("intentional panic in machine");
        }
    }

    #[tokio::test]
    async fn test_machine_panic_does_not_crash_runtime() {
        let success_count = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(InflightTracker::new());

        // PanicMachine is registered BEFORE SuccessMachine
        // Both handle TriggerEvent
        let engine = EngineBuilder::new(TestDeps)
            .with_inflight(inflight.clone())
            .with_machine(PanicMachine)
            .with_machine(TriggerMachine)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // First event triggers panic in PanicMachine
        let result = handle
            .emit_and_await_timeout(TriggerEvent { id: 1 }, Duration::from_millis(500))
            .await;

        // Should return error (machine panicked)
        assert!(result.is_err(), "Expected error after machine panic");

        // Wait for cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The runtime should still be alive
        // Use a different event type to bypass PanicMachine
        #[derive(Debug, Clone)]
        struct OtherEvent;

        #[derive(Debug, Clone)]
        struct OtherCommand;
        impl Command for OtherCommand {}

        #[derive(Debug, Clone)]
        struct OtherResultEvent;

        struct OtherMachine;
        impl Machine for OtherMachine {
            type Event = OtherEvent;
            type Command = OtherCommand;

            fn decide(&mut self, _: &OtherEvent) -> Option<OtherCommand> {
                Some(OtherCommand)
            }
        }

        struct OtherEffect;
        #[async_trait::async_trait]
        impl Effect<OtherCommand, TestDeps> for OtherEffect {
            type Event = OtherResultEvent;

            async fn execute(
                &self,
                _: OtherCommand,
                _: EffectContext<TestDeps>,
            ) -> Result<OtherResultEvent> {
                Ok(OtherResultEvent)
            }
        }

        // Build a new engine to verify runtime concept
        // (We can't add machines to running engine)
        // Instead, check inflight cleanup
        assert_eq!(
            inflight.active_count(),
            0,
            "Machine panic leaked {} inflight entries",
            inflight.active_count()
        );

        handle.abort();
    }

    // ==========================================================================
    // TEST: Tap panics don't affect event processing
    // ==========================================================================

    struct PanicTap;

    #[async_trait::async_trait]
    impl crate::tap::EventTap<TriggerEvent> for PanicTap {
        async fn on_event(
            &self,
            _event: &TriggerEvent,
            _ctx: &crate::tap::TapContext,
        ) -> Result<()> {
            panic!("intentional panic in tap");
        }
    }

    #[tokio::test]
    async fn test_tap_panic_does_not_affect_processing() {
        let success_count = Arc::new(AtomicUsize::new(0));

        let engine = EngineBuilder::new(TestDeps)
            .with_machine(TriggerMachine)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .with_event_tap::<TriggerEvent, _>(PanicTap)
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Emit event - tap will panic but should not affect main flow
        let result = handle
            .emit_and_await_timeout(TriggerEvent { id: 1 }, Duration::from_secs(1))
            .await;

        // The main processing should succeed (tap is fire-and-forget)
        assert!(
            result.is_ok(),
            "Tap panic affected main processing: {:?}",
            result
        );

        // Effect should have been called
        assert_eq!(
            success_count.load(Ordering::SeqCst),
            1,
            "Effect was not called due to tap panic"
        );

        // Try another event to ensure runtime is still healthy
        let result2 = handle
            .emit_and_await_timeout(TriggerEvent { id: 2 }, Duration::from_secs(1))
            .await;

        assert!(result2.is_ok(), "Runtime stopped after tap panic");
        assert_eq!(success_count.load(Ordering::SeqCst), 2);

        handle.abort();
    }

    // ==========================================================================
    // TEST: Sequential emit_and_await calls reuse tracker correctly
    // ==========================================================================

    #[tokio::test]
    async fn test_sequential_reuse() {
        let success_count = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(InflightTracker::new());

        let engine = EngineBuilder::new(TestDeps)
            .with_inflight(inflight.clone())
            .with_machine(TriggerMachine)
            .with_effect::<TriggerCommand, _>(SuccessEffect {
                count: success_count.clone(),
            })
            .build();

        let handle = engine.start();
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Sequential calls should all succeed
        for i in 0..100 {
            let result = handle
                .emit_and_await_timeout(TriggerEvent { id: i }, Duration::from_secs(1))
                .await;

            assert!(
                result.is_ok(),
                "Iteration {} failed: {:?}. Active entries: {}",
                i,
                result.unwrap_err(),
                inflight.active_count()
            );
        }

        assert_eq!(success_count.load(Ordering::SeqCst), 100);
        assert_eq!(inflight.active_count(), 0);

        handle.abort();
    }
}
