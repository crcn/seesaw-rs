//! TDD tests for automatic serde serialization of commands.
//!
//! These tests demonstrate the desired behavior: commands should only need
//! #[derive(Serialize, Deserialize)] and use the auto_serialize!() macro.

use crate::{auto_serialize, Command, EventBus, ExecutionMode, JobSpec};
use crate::dispatch::{Dispatcher, JobQueue};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// Test dependencies
#[derive(Debug, Clone)]
struct TestDeps;

// ============================================================================
// Test Commands - Using ONLY serde derives, NO manual serialize_to_json()
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AutoSerializeBackgroundCommand {
    task: String,
    user_id: Uuid,
}

impl Command for AutoSerializeBackgroundCommand {
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Background
    }

    fn job_spec(&self) -> Option<JobSpec> {
        Some(JobSpec::new("auto:background"))
    }

    // Just one line instead of manual implementation!
    auto_serialize!();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AutoSerializeScheduledCommand {
    reminder: String,
    run_at: DateTime<Utc>,
}

impl Command for AutoSerializeScheduledCommand {
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Scheduled {
            run_at: self.run_at,
        }
    }

    fn job_spec(&self) -> Option<JobSpec> {
        Some(JobSpec::new("auto:scheduled"))
    }

    // Just one line instead of manual implementation!
    auto_serialize!();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AutoSerializeInlineCommand {
    action: String,
}

impl Command for AutoSerializeInlineCommand {
    // Inline commands don't need serialization, but it's fine if they have it
}

// ============================================================================
// Mock Job Queue for Testing
// ============================================================================

#[derive(Clone)]
struct TestJobQueue {
    enqueued: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    scheduled: Arc<Mutex<Vec<(String, serde_json::Value, DateTime<Utc>)>>>,
}

impl TestJobQueue {
    fn new() -> Self {
        Self {
            enqueued: Arc::new(Mutex::new(Vec::new())),
            scheduled: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn get_enqueued(&self) -> Vec<(String, serde_json::Value)> {
        self.enqueued.lock().unwrap().clone()
    }

    fn get_scheduled(&self) -> Vec<(String, serde_json::Value, DateTime<Utc>)> {
        self.scheduled.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl JobQueue for TestJobQueue {
    async fn enqueue(&self, payload: serde_json::Value, spec: JobSpec) -> Result<Uuid> {
        self.enqueued
            .lock()
            .unwrap()
            .push((spec.job_type.to_string(), payload));
        Ok(Uuid::new_v4())
    }

    async fn schedule(
        &self,
        payload: serde_json::Value,
        spec: JobSpec,
        run_at: DateTime<Utc>,
    ) -> Result<Uuid> {
        self.scheduled
            .lock()
            .unwrap()
            .push((spec.job_type.to_string(), payload, run_at));
        Ok(Uuid::new_v4())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn test_background_command_auto_serializes() {
    let queue = TestJobQueue::new();
    let bus = EventBus::new();
    let dispatcher = Dispatcher::with_job_queue(TestDeps, bus, Arc::new(queue.clone()));

    let user_id = Uuid::new_v4();
    let cmd: Box<dyn crate::core::AnyCommand> = Box::new(AutoSerializeBackgroundCommand {
        task: "send_email".to_string(),
        user_id,
    });

    // This should succeed without manual serialize_to_json()
    dispatcher.dispatch_one(cmd).await.unwrap();

    // Verify the command was serialized and enqueued
    let enqueued = queue.get_enqueued();
    assert_eq!(enqueued.len(), 1);
    assert_eq!(enqueued[0].0, "auto:background");

    // Verify the payload is valid JSON
    let payload = &enqueued[0].1;
    assert_eq!(payload["task"], "send_email");
    assert_eq!(payload["user_id"], user_id.to_string());
}

#[tokio::test]
async fn test_scheduled_command_auto_serializes() {
    let queue = TestJobQueue::new();
    let bus = EventBus::new();
    let dispatcher = Dispatcher::with_job_queue(TestDeps, bus, Arc::new(queue.clone()));

    let run_at = Utc::now() + chrono::Duration::hours(1);
    let cmd: Box<dyn crate::core::AnyCommand> = Box::new(AutoSerializeScheduledCommand {
        reminder: "Meeting in 1 hour".to_string(),
        run_at,
    });

    // This should succeed without manual serialize_to_json()
    dispatcher.dispatch_one(cmd).await.unwrap();

    // Verify the command was serialized and scheduled
    let scheduled = queue.get_scheduled();
    assert_eq!(scheduled.len(), 1);
    assert_eq!(scheduled[0].0, "auto:scheduled");
    assert_eq!(scheduled[0].2, run_at);

    // Verify the payload is valid JSON
    let payload = &scheduled[0].1;
    assert_eq!(payload["reminder"], "Meeting in 1 hour");
}

#[tokio::test]
async fn test_inline_command_does_not_require_serialization() {
    // Inline commands should work even without Serialize derive,
    // but this test shows they work WITH it too
    let cmd: Box<dyn crate::core::AnyCommand> = Box::new(AutoSerializeInlineCommand {
        action: "log".to_string(),
    });

    // Just verify we can create the command and get its execution mode
    assert_eq!(cmd.get_execution_mode(), ExecutionMode::Inline);
}

#[test]
fn test_command_can_be_deserialized() {
    // Verify round-trip serialization works
    let original = AutoSerializeBackgroundCommand {
        task: "process".to_string(),
        user_id: Uuid::new_v4(),
    };

    let json = serde_json::to_value(&original).unwrap();
    let deserialized: AutoSerializeBackgroundCommand = serde_json::from_value(json).unwrap();

    assert_eq!(original.task, deserialized.task);
    assert_eq!(original.user_id, deserialized.user_id);
}
