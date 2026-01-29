//! Job system interfaces for seesaw.
//!
//! This module provides policy-light interfaces for job execution:
//! - [`JobStore`] - Trait for claiming and managing jobs from persistent storage
//! - [`ClaimedJob`] - A job claimed by a worker, ready for execution
//! - [`CommandRegistry`] - Registry for deserializing job payloads back to commands
//! - [`DeserializationError`] - Explicit failure modes for deserialization
//! - [`FailureKind`] - Classification of job failures for retry decisions
//!
//! # Design Philosophy
//!
//! seesaw-rs owns interfaces only. Policy decisions (polling cadence, retry backoff,
//! concurrency limits) belong in the application's job worker implementation.
//!
//! # Example
//!
//! ```ignore
//! use seesaw::job::{JobStore, ClaimedJob, CommandRegistry, FailureKind};
//!
//! // Implement JobStore for your database
//! struct PostgresJobStore { /* ... */ }
//!
//! #[async_trait]
//! impl JobStore for PostgresJobStore {
//!     async fn claim_ready(&self, worker_id: &str, limit: i64) -> Result<Vec<ClaimedJob>> {
//!         // Use FOR UPDATE SKIP LOCKED pattern
//!     }
//!     // ... other methods
//! }
//!
//! // Register command deserializers
//! let mut registry = CommandRegistry::new();
//! registry.register::<SendEmailCommand>("email:send", vec![1, 2]);
//!
//! // Worker loop (policy lives here, not in seesaw)
//! loop {
//!     let jobs = store.claim_ready("worker-1", 10).await?;
//!     for job in jobs {
//!         match registry.deserialize(&job) {
//!             Ok(cmd) => {
//!                 dispatcher.dispatch_one(cmd).await?;
//!                 store.mark_succeeded(job.id).await?;
//!             }
//!             Err(DeserializationError::UnknownCommandType(_)) => {
//!                 store.mark_failed(job.id, "unknown type", FailureKind::NonRetryable).await?;
//!             }
//!             // ... handle other cases
//!         }
//!     }
//!     tokio::time::sleep(poll_interval).await;
//! }
//! ```

use std::collections::HashMap;

use anyhow::Result;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::core::{AnyCommand, Command};

/// Trait for claiming jobs from a persistent store.
///
/// The store decides what "ready" means (scheduling, retries, visibility timeout).
/// Workers stay dumb and just poll for ready jobs.
///
/// # Implementer Notes
///
/// - Use `FOR UPDATE SKIP LOCKED` (PostgreSQL) or equivalent for atomic claiming
/// - Set lease expiration when claiming to handle worker crashes
/// - The store should handle retry delay calculation internally
#[async_trait::async_trait]
pub trait JobStore: Send + Sync {
    /// Claim ready jobs for execution.
    ///
    /// The store decides what "ready" means:
    /// - `next_run_at <= now` for scheduled jobs
    /// - `status = pending` and not claimed
    /// - Retry delay elapsed for failed jobs
    ///
    /// # Arguments
    ///
    /// * `worker_id` - Identifier for this worker (for lease tracking)
    /// * `limit` - Maximum number of jobs to claim
    ///
    /// # Returns
    ///
    /// A vector of claimed jobs, which may be empty if no jobs are ready.
    async fn claim_ready(&self, worker_id: &str, limit: i64) -> Result<Vec<ClaimedJob>>;

    /// Mark a job as succeeded.
    ///
    /// The store should update the job status and record completion time.
    async fn mark_succeeded(&self, job_id: Uuid) -> Result<()>;

    /// Mark a job as failed.
    ///
    /// # Arguments
    ///
    /// * `job_id` - The job that failed
    /// * `error` - Error message to store
    /// * `kind` - Whether this failure is retryable
    ///
    /// For retryable failures, the store should:
    /// - Increment retry count
    /// - Calculate next retry time (exponential backoff)
    /// - Mark as pending if retries remain, dead-letter otherwise
    ///
    /// For non-retryable failures, the store should:
    /// - Mark as dead-letter immediately
    async fn mark_failed(&self, job_id: Uuid, error: &str, kind: FailureKind) -> Result<()>;

    /// Send a heartbeat to extend the lease.
    ///
    /// Workers should call this periodically for long-running jobs to prevent
    /// the job from being reclaimed by another worker.
    async fn heartbeat(&self, job_id: Uuid) -> Result<()>;
}

/// Classification of job failures for retry decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Failure may be transient; the job should be retried.
    ///
    /// Examples: network timeout, temporary unavailability, rate limiting
    Retryable,

    /// Failure is permanent; the job should not be retried.
    ///
    /// Examples: deserialization failure, unsupported version, invalid input
    NonRetryable,
}

/// A job claimed by a worker, ready for execution.
///
/// Contains all information needed to deserialize and execute the job command.
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    /// Unique identifier for this job.
    pub id: Uuid,

    /// The command type (e.g., "email:send", "agent_response").
    /// Used to look up the deserializer in the registry.
    pub job_type: String,

    /// The serialized command payload.
    pub payload: serde_json::Value,

    /// The payload schema version.
    /// Used for backward-compatible deserialization.
    pub version: i32,

    /// The attempt number (1-based).
    /// First attempt is 1, first retry is 2, etc.
    pub attempt: i32,
}

/// Deserialization errors with explicit failure modes.
///
/// Each variant maps to a specific handling strategy in the worker:
/// - `UnknownCommandType` → Dead-letter (non-retryable)
/// - `UnsupportedVersion` → Dead-letter (non-retryable)
/// - `InvalidPayload` → Dead-letter (non-retryable)
#[derive(Debug, thiserror::Error)]
pub enum DeserializationError {
    /// The command type is not registered in the registry.
    #[error("unknown command type: {0}")]
    UnknownCommandType(String),

    /// The job version is not supported by the registered deserializer.
    #[error("unsupported version {version} for command {job_type}")]
    UnsupportedVersion {
        /// The command type.
        job_type: String,
        /// The unsupported version.
        version: i32,
    },

    /// The payload could not be deserialized.
    #[error("invalid payload: {0}")]
    InvalidPayload(#[from] anyhow::Error),
}

impl DeserializationError {
    /// Returns the appropriate failure kind for this error.
    ///
    /// All deserialization errors are non-retryable because they indicate
    /// a permanent problem with the job data.
    pub fn failure_kind(&self) -> FailureKind {
        FailureKind::NonRetryable
    }
}

/// Type-erased deserializer function.
type DeserializeFn = Box<dyn Fn(&serde_json::Value) -> Result<Box<dyn AnyCommand>> + Send + Sync>;

/// Internal representation of a registered command deserializer.
struct CommandDeserializer {
    /// Versions this deserializer supports.
    supported_versions: Vec<i32>,
    /// The deserializer function.
    deserialize: DeserializeFn,
}

/// Registry for deserializing job payloads back to commands.
///
/// The registry maps job types to deserializers with version support.
/// This enables backward-compatible deserialization of jobs that were
/// enqueued with older payload formats.
///
/// # Example
///
/// ```ignore
/// let mut registry = CommandRegistry::new();
///
/// // Register a command with supported versions
/// registry.register::<SendEmailCommand>("email:send", vec![1, 2]);
///
/// // Later, in the worker:
/// match registry.deserialize(&claimed_job) {
///     Ok(cmd) => dispatcher.dispatch_one(cmd).await?,
///     Err(e) => store.mark_failed(job.id, &e.to_string(), e.failure_kind()).await?,
/// }
/// ```
#[derive(Default)]
pub struct CommandRegistry {
    deserializers: HashMap<&'static str, CommandDeserializer>,
}

impl CommandRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a command type with supported versions.
    ///
    /// # Type Parameters
    ///
    /// * `C` - The command type. Must implement `Command` and `DeserializeOwned`.
    ///
    /// # Arguments
    ///
    /// * `job_type` - The job type string (must match `JobSpec::job_type`)
    /// * `supported_versions` - List of payload versions this deserializer handles
    ///
    /// # Panics
    ///
    /// Panics if a deserializer is already registered for this job type.
    pub fn register<C>(&mut self, job_type: &'static str, supported_versions: Vec<i32>)
    where
        C: Command + DeserializeOwned + 'static,
    {
        if self.deserializers.contains_key(job_type) {
            panic!("deserializer already registered for job type: {}", job_type);
        }

        let deserialize: DeserializeFn = Box::new(|payload: &serde_json::Value| {
            let command: C = serde_json::from_value(payload.clone())
                .map_err(|e| anyhow::anyhow!("JSON deserialization failed: {}", e))?;
            Ok(Box::new(command) as Box<dyn AnyCommand>)
        });

        self.deserializers.insert(
            job_type,
            CommandDeserializer {
                supported_versions,
                deserialize,
            },
        );
    }

    /// Deserialize a claimed job back to a command.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The job type is not registered (`UnknownCommandType`)
    /// - The job version is not supported (`UnsupportedVersion`)
    /// - The payload cannot be deserialized (`InvalidPayload`)
    pub fn deserialize(
        &self,
        job: &ClaimedJob,
    ) -> Result<Box<dyn AnyCommand>, DeserializationError> {
        let entry = self
            .deserializers
            .get(job.job_type.as_str())
            .ok_or_else(|| DeserializationError::UnknownCommandType(job.job_type.clone()))?;

        if !entry.supported_versions.contains(&job.version) {
            return Err(DeserializationError::UnsupportedVersion {
                job_type: job.job_type.clone(),
                version: job.version,
            });
        }

        (entry.deserialize)(&job.payload).map_err(DeserializationError::InvalidPayload)
    }

    /// Check if a job type is registered.
    pub fn has(&self, job_type: &str) -> bool {
        self.deserializers.contains_key(job_type)
    }

    /// Get the number of registered deserializers.
    pub fn len(&self) -> usize {
        self.deserializers.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.deserializers.is_empty()
    }
}

impl std::fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandRegistry")
            .field(
                "registered_types",
                &self.deserializers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestCommand {
        message: String,
    }

    impl Command for TestCommand {}

    #[test]
    fn test_registry_register_and_deserialize() {
        let mut registry = CommandRegistry::new();
        registry.register::<TestCommand>("test:command", vec![1]);

        let job = ClaimedJob {
            id: Uuid::new_v4(),
            job_type: "test:command".to_string(),
            payload: serde_json::json!({ "message": "hello" }),
            version: 1,
            attempt: 1,
        };

        let result = registry.deserialize(&job);
        assert!(result.is_ok());

        let cmd = result.unwrap();
        let test_cmd = cmd.as_any().downcast_ref::<TestCommand>().unwrap();
        assert_eq!(test_cmd.message, "hello");
    }

    #[test]
    fn test_registry_unknown_command_type() {
        let registry = CommandRegistry::new();

        let job = ClaimedJob {
            id: Uuid::new_v4(),
            job_type: "unknown:type".to_string(),
            payload: serde_json::json!({}),
            version: 1,
            attempt: 1,
        };

        let result = registry.deserialize(&job);
        assert!(matches!(
            result,
            Err(DeserializationError::UnknownCommandType(_))
        ));
    }

    #[test]
    fn test_registry_unsupported_version() {
        let mut registry = CommandRegistry::new();
        registry.register::<TestCommand>("test:command", vec![1, 2]);

        let job = ClaimedJob {
            id: Uuid::new_v4(),
            job_type: "test:command".to_string(),
            payload: serde_json::json!({ "message": "hello" }),
            version: 99, // Not supported
            attempt: 1,
        };

        let result = registry.deserialize(&job);
        assert!(matches!(
            result,
            Err(DeserializationError::UnsupportedVersion { version: 99, .. })
        ));
    }

    #[test]
    fn test_registry_invalid_payload() {
        let mut registry = CommandRegistry::new();
        registry.register::<TestCommand>("test:command", vec![1]);

        let job = ClaimedJob {
            id: Uuid::new_v4(),
            job_type: "test:command".to_string(),
            payload: serde_json::json!({ "wrong_field": "value" }), // Missing 'message'
            version: 1,
            attempt: 1,
        };

        let result = registry.deserialize(&job);
        assert!(matches!(
            result,
            Err(DeserializationError::InvalidPayload(_))
        ));
    }

    #[test]
    fn test_deserialization_error_failure_kind() {
        let err = DeserializationError::UnknownCommandType("test".to_string());
        assert_eq!(err.failure_kind(), FailureKind::NonRetryable);

        let err = DeserializationError::UnsupportedVersion {
            job_type: "test".to_string(),
            version: 1,
        };
        assert_eq!(err.failure_kind(), FailureKind::NonRetryable);
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn test_registry_duplicate_registration_panics() {
        let mut registry = CommandRegistry::new();
        registry.register::<TestCommand>("test:command", vec![1]);
        registry.register::<TestCommand>("test:command", vec![2]); // Should panic
    }

    #[test]
    fn test_registry_has() {
        let mut registry = CommandRegistry::new();
        registry.register::<TestCommand>("test:command", vec![1]);

        assert!(registry.has("test:command"));
        assert!(!registry.has("other:command"));
    }

    #[test]
    fn test_registry_len() {
        let mut registry = CommandRegistry::new();
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());

        registry.register::<TestCommand>("test:command", vec![1]);
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }

    #[test]
    fn test_claimed_job_debug() {
        let job = ClaimedJob {
            id: Uuid::nil(),
            job_type: "test".to_string(),
            payload: serde_json::json!({}),
            version: 1,
            attempt: 1,
        };
        let debug = format!("{:?}", job);
        assert!(debug.contains("ClaimedJob"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn test_failure_kind_eq() {
        assert_eq!(FailureKind::Retryable, FailureKind::Retryable);
        assert_eq!(FailureKind::NonRetryable, FailureKind::NonRetryable);
        assert_ne!(FailureKind::Retryable, FailureKind::NonRetryable);
    }
}
