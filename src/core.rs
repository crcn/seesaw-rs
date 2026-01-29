//! Core traits for the seesaw event-driven architecture.
//!
//! # Overview
//!
//! Seesaw separates **facts** from **intent**:
//! - [`Event`] = Facts (what happened)
//! - [`Command`] = Intent (requests for IO with transaction authority)
//!
//! The key principle: **One Command = One Transaction**. If multiple writes
//! must be atomic, they belong in one command handled by one effect.
//!
//! # Correlation
//!
//! Events and commands can be tagged with a [`CorrelationId`] to track
//! related work across the system. This enables:
//! - Awaiting completion of all work triggered by an API request
//! - Distributed tracing
//! - Error propagation back to the caller

use std::any::{Any, TypeId};
use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Job specification for background and scheduled commands.
///
/// Commands that use `ExecutionMode::Background` or `ExecutionMode::Scheduled`
/// should return a `JobSpec` from their `job_spec()` method. This provides
/// all the metadata needed for durable job execution.
///
/// # Example
///
/// ```ignore
/// use seesaw::{Command, ExecutionMode, JobSpec};
///
/// #[derive(Debug, Clone, Serialize, Deserialize)]
/// struct SendEmailCommand {
///     user_id: Uuid,
///     template: String,
/// }
///
/// impl Command for SendEmailCommand {
///     fn execution_mode(&self) -> ExecutionMode {
///         ExecutionMode::Background
///     }
///
///     fn job_spec(&self) -> Option<JobSpec> {
///         Some(JobSpec::new("email:send")
///             .with_idempotency_key(format!("email:{}:{}", self.user_id, self.template)))
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct JobSpec {
    /// Stable identifier used for persistence, deserialization, and routing.
    /// Must not change once jobs exist in the queue.
    pub job_type: &'static str,

    /// Optional idempotency key for deduplication.
    /// If provided, only one pending/running job with this key can exist.
    pub idempotency_key: Option<String>,

    /// Maximum retry attempts on failure.
    pub max_retries: i32,

    /// Priority for job ordering (higher = sooner).
    pub priority: i32,

    /// Payload schema version for backward compatibility.
    /// Versioning/migration logic belongs in the job worker, not here.
    pub version: i32,

    /// Optional reference ID for tracking purposes.
    pub reference_id: Option<Uuid>,

    /// Optional container scope for multi-tenancy.
    pub container_id: Option<Uuid>,
}

impl JobSpec {
    /// Create a new JobSpec with default values.
    ///
    /// # Arguments
    ///
    /// * `job_type` - Stable identifier used for routing (e.g., "email:send")
    pub fn new(job_type: &'static str) -> Self {
        Self {
            job_type,
            idempotency_key: None,
            max_retries: 3,
            priority: 0,
            version: 1,
            reference_id: None,
            container_id: None,
        }
    }

    /// Set the idempotency key for deduplication.
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// Set the maximum number of retry attempts.
    pub fn with_max_retries(mut self, n: i32) -> Self {
        self.max_retries = n;
        self
    }

    /// Set the job priority (higher values run sooner).
    pub fn with_priority(mut self, p: i32) -> Self {
        self.priority = p;
        self
    }

    /// Set the payload schema version.
    pub fn with_version(mut self, v: i32) -> Self {
        self.version = v;
        self
    }

    /// Set an optional reference ID for tracking.
    pub fn with_reference_id(mut self, id: Uuid) -> Self {
        self.reference_id = Some(id);
        self
    }

    /// Set an optional container ID for multi-tenancy.
    pub fn with_container_id(mut self, id: Uuid) -> Self {
        self.container_id = Some(id);
        self
    }
}

/// Correlation ID for tracking related events and commands.
///
/// Each `emit_and_await` call generates a unique correlation ID that propagates
/// through the system, allowing the caller to wait for all related inline work.
///
/// Use `CorrelationId::NONE` for uncorrelated events, or `CorrelationId::new()`
/// to generate a fresh ID.
///
/// # Example
///
/// ```ignore
/// use seesaw::CorrelationId;
///
/// // Create a new random correlation ID
/// let cid = CorrelationId::new();
///
/// // Use NONE for uncorrelated events
/// let uncorrelated = CorrelationId::NONE;
/// assert!(uncorrelated.is_none());
///
/// // Convert from existing Uuid
/// let cid = CorrelationId::from(my_uuid);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CorrelationId(Uuid);

impl CorrelationId {
    /// Sentinel value for uncorrelated events.
    ///
    /// Uses nil UUID (`00000000-0000-0000-0000-000000000000`).
    pub const NONE: Self = Self(Uuid::nil());

    /// Create a new random correlation ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Check if this is the NONE sentinel value.
    pub fn is_none(&self) -> bool {
        self.0.is_nil()
    }

    /// Check if this is a real correlation ID (not NONE).
    pub fn is_some(&self) -> bool {
        !self.is_none()
    }

    /// Get the inner UUID value.
    pub fn into_inner(self) -> Uuid {
        self.0
    }

    /// Get a reference to the inner UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Uuid> for CorrelationId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl From<CorrelationId> for Uuid {
    fn from(cid: CorrelationId) -> Uuid {
        cid.0
    }
}

impl From<Option<Uuid>> for CorrelationId {
    fn from(opt: Option<Uuid>) -> Self {
        match opt {
            Some(uuid) => Self(uuid),
            None => Self::NONE,
        }
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() {
            write!(f, "NONE")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// Envelope wrapping an event with correlation metadata.
///
/// `EventEnvelope` is the internal transport format for events. It carries:
/// - The correlation ID for tracking related work
/// - The type ID for filtering by machines
/// - The event payload
///
/// Domain event enums remain clean - correlation is transport-level metadata.
#[derive(Clone)]
pub struct EventEnvelope {
    /// Correlation ID for tracking related work
    pub cid: CorrelationId,
    /// Type ID of the payload event
    pub type_id: TypeId,
    /// The actual event payload
    pub payload: Arc<dyn Any + Send + Sync>,
}

impl EventEnvelope {
    /// Create a new event envelope.
    pub fn new<E: Any + Send + Sync + 'static>(cid: CorrelationId, event: E) -> Self {
        Self {
            cid,
            type_id: TypeId::of::<E>(),
            payload: Arc::new(event),
        }
    }

    /// Create a new event envelope from a raw UUID (for internal use).
    pub(crate) fn new_with_uuid<E: Any + Send + Sync + 'static>(cid: Uuid, event: E) -> Self {
        Self {
            cid: CorrelationId::from(cid),
            type_id: TypeId::of::<E>(),
            payload: Arc::new(event),
        }
    }

    /// Create an envelope with a new random correlation ID.
    pub fn new_random<E: Any + Send + Sync + 'static>(event: E) -> Self {
        Self::new(CorrelationId::new(), event)
    }

    /// Downcast the payload to a concrete event type.
    pub fn downcast_ref<E: Any>(&self) -> Option<&E> {
        self.payload.downcast_ref()
    }
}

impl std::fmt::Debug for EventEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventEnvelope")
            .field("cid", &self.cid)
            .field("type_id", &self.type_id)
            .finish_non_exhaustive()
    }
}

/// A fact - something that happened.
///
/// The role of an event in the system.
///
/// Events flow through a single bus but serve different purposes:
/// - **Input**: Edge-originated events (user requests, API calls)
/// - **Fact**: Effect-produced events (ground truth, what actually happened)
/// - **Signal**: Ephemeral UI notifications (typing indicators, progress)
///
/// # Design Principle
///
/// One bus, one thing that flows, but events have roles.
/// This avoids the ceremony of separate Message/Event types while
/// maintaining clear semantics.
///
/// # Example
///
/// ```ignore
/// enum DeckEvent {
///     // Input - "a request was made" (edge-originated)
///     CreateRequested { container_id: Uuid, actor_id: MemberId },
///
///     // Fact - "this happened" (effect-produced)
///     Created { deck: Deck },
///
///     // Signal would use ctx.signal() instead
/// }
///
/// impl DeckEvent {
///     pub fn role(&self) -> EventRole {
///         match self {
///             Self::CreateRequested { .. } => EventRole::Input,
///             Self::Created { .. } => EventRole::Fact,
///             // ...
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventRole {
    /// Edge-originated events (user requests, API calls).
    /// Machines decide on these to produce commands.
    Input,
    /// Effect-produced events (ground truth).
    /// These are the facts that machines can react to.
    Fact,
    /// Ephemeral UI notifications (typing indicators, progress).
    /// Machines should ignore these.
    Signal,
}

impl EventRole {
    /// Returns true if this is an input event.
    pub fn is_input(&self) -> bool {
        matches!(self, EventRole::Input)
    }

    /// Returns true if this is a fact event.
    pub fn is_fact(&self) -> bool {
        matches!(self, EventRole::Fact)
    }

    /// Returns true if this is a signal event.
    pub fn is_signal(&self) -> bool {
        matches!(self, EventRole::Signal)
    }

    /// Returns true if this event should be processed by machines.
    /// Signals are typically filtered out.
    pub fn is_actionable(&self) -> bool {
        !self.is_signal()
    }
}

/// Events are immutable descriptions of what occurred. They contain no IO,
/// no side effects, and are processed synchronously by machines.
///
/// **Note**: This trait is automatically implemented for any type that is
/// `Clone + Send + Sync + 'static`. You don't need to implement it manually.
///
/// # Event Roles
///
/// Events have roles (see [`EventRole`]):
/// - **Input**: Requests from edges (e.g., `CreateRequested`)
/// - **Fact**: Results from effects (e.g., `Created`)
/// - **Signal**: Ephemeral UI updates (via `ctx.signal()`)
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone)]
/// enum UserEvent {
///     // Input
///     CreateRequested { email: String },
///     // Fact
///     Created { user_id: Uuid, email: String },
///     Deleted { user_id: Uuid },
/// }
/// // Event is automatically implemented!
/// ```
pub trait Event: Any + Send + Sync + 'static {}

// Blanket implementation for any type that meets the requirements
impl<T: Clone + Send + Sync + 'static> Event for T {}

/// A request for IO with transaction authority.
///
/// Commands represent intent to perform side effects. Each command maps to
/// exactly one effect execution and one database transaction.
///
/// # Execution Modes
///
/// Commands specify their execution mode via [`Command::execution_mode`]:
/// - [`ExecutionMode::Inline`] - Execute immediately in the current context
/// - [`ExecutionMode::Background`] - Queue for durable job execution
/// - [`ExecutionMode::Scheduled`] - Execute at a specific time in the future
///
/// # Job Specification
///
/// Commands using `Background` or `Scheduled` execution modes should provide
/// a [`JobSpec`] via the [`Command::job_spec`] method. This enables the
/// dispatcher to route the command to the job queue with the appropriate
/// metadata.
///
/// # Example
///
/// ```ignore
/// #[derive(Debug, Clone, Serialize, Deserialize)]
/// struct SendEmailCommand {
///     user_id: Uuid,
///     template: String,
/// }
///
/// impl Command for SendEmailCommand {
///     fn execution_mode(&self) -> ExecutionMode {
///         ExecutionMode::Background
///     }
///
///     fn job_spec(&self) -> Option<JobSpec> {
///         Some(JobSpec::new("email:send")
///             .with_idempotency_key(format!("email:{}:{}", self.user_id, self.template)))
///     }
/// }
/// ```
pub trait Command: Any + Send + Sync + 'static {
    /// Returns the execution mode for this command.
    ///
    /// Defaults to [`ExecutionMode::Inline`].
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Inline
    }

    /// Returns the job specification for background/scheduled commands.
    ///
    /// Commands using [`ExecutionMode::Background`] or [`ExecutionMode::Scheduled`]
    /// should return `Some(JobSpec)`. Inline commands can return `None`.
    ///
    /// Returns `None` by default, which is appropriate for inline-only commands.
    fn job_spec(&self) -> Option<JobSpec> {
        None
    }

    /// Serialize the command to JSON for job queue persistence.
    ///
    /// For commands that derive `Serialize`, you can use the `auto_serialize!` helper:
    ///
    /// ```ignore
    /// use seesaw::{Command, auto_serialize};
    /// use serde::{Serialize, Deserialize};
    ///
    /// #[derive(Debug, Clone, Serialize, Deserialize)]
    /// struct SendEmailCommand {
    ///     user_id: Uuid,
    ///     template: String,
    /// }
    ///
    /// impl Command for SendEmailCommand {
    ///     fn execution_mode(&self) -> ExecutionMode {
    ///         ExecutionMode::Background
    ///     }
    ///
    ///     fn job_spec(&self) -> Option<JobSpec> {
    ///         Some(JobSpec::new("email:send"))
    ///     }
    ///
    ///     auto_serialize!();  // One-liner instead of manual implementation!
    /// }
    /// ```
    fn serialize_to_json(&self) -> Option<serde_json::Value> {
        None
    }
}

/// Execution mode for commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Execute immediately in the current context.
    ///
    /// The effect runs synchronously (from the runtime's perspective) and
    /// blocks until completion. Use for fast operations that don't need
    /// durability guarantees.
    Inline,

    /// Queue for durable background execution.
    ///
    /// The command is persisted to a job queue and executed by a worker.
    /// Use for:
    /// - Long-running operations (AI, external APIs)
    /// - Operations that need retry guarantees
    /// - Operations that can be delayed
    ///
    /// **Note**: Commands using this mode must implement [`DurableCommand`].
    Background,

    /// Schedule for execution at a specific time.
    ///
    /// The command is persisted to a job queue with a `run_at` timestamp.
    /// The job queue is responsible for executing the command at or after
    /// the specified time.
    ///
    /// Use for:
    /// - Delayed notifications
    /// - Scheduled reminders
    /// - Time-based state transitions
    /// - Retry with exponential backoff
    ///
    /// **Note**: Commands using this mode must provide a [`JobSpec`] via
    /// [`Command::job_spec`] and must be serializable.
    ///
    /// # Example
    ///
    /// ```ignore
    /// impl Command for ReminderCommand {
    ///     fn execution_mode(&self) -> ExecutionMode {
    ///         ExecutionMode::Scheduled { run_at: self.remind_at }
    ///     }
    ///
    ///     fn job_spec(&self) -> Option<JobSpec> {
    ///         Some(JobSpec::new("reminder:send"))
    ///     }
    /// }
    /// ```
    Scheduled {
        /// The time at which to execute the command.
        run_at: DateTime<Utc>,
    },
}

/// Type-erased command trait for internal use.
///
/// This trait is automatically implemented for all types that implement [`Command`].
/// For commands that need to be serialized (background/scheduled execution),
/// see also [`SerializableCommand`].
pub trait AnyCommand: Send + Sync {
    /// Returns the execution mode for this command.
    fn get_execution_mode(&self) -> ExecutionMode;

    /// Returns the job specification for background/scheduled commands.
    fn get_job_spec(&self) -> Option<JobSpec>;

    /// Serialize the command to JSON for job queue persistence.
    fn get_serialize_to_json(&self) -> Option<serde_json::Value>;

    /// Returns the TypeId of this command.
    fn command_type_id(&self) -> std::any::TypeId;

    /// Downcast to concrete type.
    fn as_any(&self) -> &dyn Any;

    /// Downcast to concrete type (boxed).
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync>;

    /// Try to get this command as a serializable trait object.
    ///
    /// Returns Some if the command implements Serialize, None otherwise.
    /// This enables automatic serialization for commands that derive Serialize.
    fn as_serializable(&self) -> Option<&dyn erased_serde::Serialize>;
}

impl<C: Command> AnyCommand for C {
    fn get_execution_mode(&self) -> ExecutionMode {
        Command::execution_mode(self)
    }

    fn get_job_spec(&self) -> Option<JobSpec> {
        Command::job_spec(self)
    }

    fn get_serialize_to_json(&self) -> Option<serde_json::Value> {
        Command::serialize_to_json(self)
    }

    fn command_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<C>()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync> {
        self
    }

    fn as_serializable(&self) -> Option<&dyn erased_serde::Serialize> {
        None
    }
}

/// Type-erased serializable command trait for job queue integration.
///
/// This trait extends [`AnyCommand`] with serialization capability using
/// `erased_serde`. Commands that need to be enqueued to a job queue must
/// implement both [`Command`] and [`serde::Serialize`].
///
/// # Example
///
/// ```ignore
/// use seesaw::{Command, ExecutionMode, JobSpec, SerializableCommand};
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Debug, Clone, Serialize, Deserialize)]
/// struct SendEmailCommand {
///     user_id: Uuid,
///     template: String,
/// }
///
/// impl Command for SendEmailCommand {
///     fn execution_mode(&self) -> ExecutionMode {
///         ExecutionMode::Background
///     }
///
///     fn job_spec(&self) -> Option<JobSpec> {
///         Some(JobSpec::new("email:send"))
///     }
/// }
///
/// // SendEmailCommand automatically implements SerializableCommand
/// // because it implements both Command and Serialize
/// ```
pub trait SerializableCommand: AnyCommand + erased_serde::Serialize {}

impl<C: Command + serde::Serialize> SerializableCommand for C {}

// Enable serialization of boxed SerializableCommand
impl serde::Serialize for dyn SerializableCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        erased_serde::serialize(self, serializer)
    }
}

impl serde::Serialize for dyn SerializableCommand + Send {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        erased_serde::serialize(self, serializer)
    }
}

impl serde::Serialize for dyn SerializableCommand + Send + Sync {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        erased_serde::serialize(self, serializer)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EnvelopeMatch - Ergonomic event matching
// ─────────────────────────────────────────────────────────────────────────────

/// Ergonomic wrapper for matching events in an envelope.
///
/// Provides a cleaner API for downcasting event envelopes without
/// verbose `downcast_ref` calls scattered throughout match logic.
///
/// # Example
///
/// ```ignore
/// use seesaw::EnvelopeMatch;
///
/// let result = EnvelopeMatch::new(&envelope)
///     .try_match(|e: &UserEvent| match e {
///         UserEvent::Created { user } => Some(Ok(user.clone())),
///         _ => None,
///     })
///     .or_try(|denied: &AuthorizationDenied| {
///         Some(Err(anyhow!("Permission denied: {}", denied.reason)))
///     })
///     .result();
/// ```
pub struct EnvelopeMatch<'a> {
    env: &'a EventEnvelope,
}

impl<'a> EnvelopeMatch<'a> {
    /// Create a new envelope matcher.
    pub fn new(env: &'a EventEnvelope) -> Self {
        Self { env }
    }

    /// Try to downcast to a specific event type.
    pub fn event<E: 'static>(&self) -> Option<&E> {
        self.env.downcast_ref::<E>()
    }

    /// Check if envelope contains this event type.
    pub fn is<E: 'static>(&self) -> bool {
        self.env.type_id == TypeId::of::<E>()
    }

    /// Try to extract and map an event type.
    pub fn map<E: 'static, T>(&self, f: impl FnOnce(&E) -> T) -> Option<T> {
        self.event::<E>().map(f)
    }

    /// Try to extract and flat_map an event type.
    pub fn and_then<E: 'static, T>(&self, f: impl FnOnce(&E) -> Option<T>) -> Option<T> {
        self.event::<E>().and_then(f)
    }

    /// Start a match chain with the first event type to try.
    pub fn try_match<E: 'static, T>(&self, f: impl FnOnce(&E) -> Option<T>) -> MatchChain<'a, T> {
        MatchChain {
            env: self.env,
            result: self.event::<E>().and_then(f),
        }
    }
}

/// A chain of event type matches.
///
/// Created by [`EnvelopeMatch::try_match`] and extended with [`MatchChain::or_try`].
pub struct MatchChain<'a, T> {
    env: &'a EventEnvelope,
    result: Option<T>,
}

impl<'a, T> MatchChain<'a, T> {
    /// Try another event type if previous didn't match.
    pub fn or_try<E: 'static>(self, f: impl FnOnce(&E) -> Option<T>) -> Self {
        if self.result.is_some() {
            return self;
        }
        Self {
            env: self.env,
            result: self.env.downcast_ref::<E>().and_then(f),
        }
    }

    /// Get the match result.
    pub fn result(self) -> Option<T> {
        self.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct TestEvent {
        value: i32,
    }
    // Event auto-impl by blanket

    #[derive(Debug, Clone)]
    struct TestCommand {
        action: String,
    }
    impl Command for TestCommand {}

    #[derive(Debug, Clone)]
    struct BackgroundCommand;
    impl Command for BackgroundCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Background
        }
    }

    #[derive(Debug, Clone)]
    struct ScheduledCommand {
        run_at: DateTime<Utc>,
    }
    impl Command for ScheduledCommand {
        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Scheduled {
                run_at: self.run_at,
            }
        }
    }

    #[test]
    fn test_event_is_any() {
        let event = TestEvent { value: 42 };
        let any: &dyn Any = &event;
        assert!(any.downcast_ref::<TestEvent>().is_some());
    }

    #[test]
    fn test_command_default_execution_mode() {
        let cmd = TestCommand {
            action: "test".to_string(),
        };
        assert_eq!(cmd.execution_mode(), ExecutionMode::Inline);
    }

    #[test]
    fn test_command_background_execution_mode() {
        let cmd = BackgroundCommand;
        assert_eq!(cmd.execution_mode(), ExecutionMode::Background);
    }

    #[test]
    fn test_command_scheduled_execution_mode() {
        let run_at = Utc::now() + chrono::Duration::hours(1);
        let cmd = ScheduledCommand { run_at };
        assert_eq!(cmd.execution_mode(), ExecutionMode::Scheduled { run_at });
    }

    #[test]
    fn test_any_command_type_id() {
        let cmd = TestCommand {
            action: "test".to_string(),
        };
        let any_cmd: &dyn AnyCommand = &cmd;
        assert_eq!(
            any_cmd.command_type_id(),
            std::any::TypeId::of::<TestCommand>()
        );
    }

    #[test]
    fn test_any_command_downcast() {
        let cmd = TestCommand {
            action: "hello".to_string(),
        };
        let any_cmd: &dyn AnyCommand = &cmd;
        let downcasted = any_cmd.as_any().downcast_ref::<TestCommand>();
        assert!(downcasted.is_some());
        assert_eq!(downcasted.unwrap().action, "hello");
    }

    // =========================================================================
    // JobSpec Tests
    // =========================================================================

    #[test]
    fn test_job_spec_new_defaults() {
        let spec = JobSpec::new("test:job");

        assert_eq!(spec.job_type, "test:job");
        assert_eq!(spec.idempotency_key, None);
        assert_eq!(spec.max_retries, 3);
        assert_eq!(spec.priority, 0);
        assert_eq!(spec.version, 1);
        assert_eq!(spec.reference_id, None);
        assert_eq!(spec.container_id, None);
    }

    #[test]
    fn test_job_spec_with_idempotency_key() {
        let spec = JobSpec::new("email:send").with_idempotency_key("email:user:123");

        assert_eq!(spec.idempotency_key, Some("email:user:123".to_string()));
    }

    #[test]
    fn test_job_spec_with_idempotency_key_string() {
        let key = String::from("dynamic:key:456");
        let spec = JobSpec::new("email:send").with_idempotency_key(key);

        assert_eq!(spec.idempotency_key, Some("dynamic:key:456".to_string()));
    }

    #[test]
    fn test_job_spec_with_max_retries() {
        let spec = JobSpec::new("flaky:job").with_max_retries(10);

        assert_eq!(spec.max_retries, 10);
    }

    #[test]
    fn test_job_spec_with_zero_retries() {
        let spec = JobSpec::new("critical:job").with_max_retries(0);

        assert_eq!(spec.max_retries, 0);
    }

    #[test]
    fn test_job_spec_with_priority() {
        let spec = JobSpec::new("urgent:job").with_priority(100);

        assert_eq!(spec.priority, 100);
    }

    #[test]
    fn test_job_spec_with_negative_priority() {
        let spec = JobSpec::new("low:priority").with_priority(-50);

        assert_eq!(spec.priority, -50);
    }

    #[test]
    fn test_job_spec_with_version() {
        let spec = JobSpec::new("versioned:job").with_version(3);

        assert_eq!(spec.version, 3);
    }

    #[test]
    fn test_job_spec_with_reference_id() {
        let id = Uuid::new_v4();
        let spec = JobSpec::new("entity:process").with_reference_id(id);

        assert_eq!(spec.reference_id, Some(id));
    }

    #[test]
    fn test_job_spec_with_container_id() {
        let tenant_id = Uuid::new_v4();
        let spec = JobSpec::new("tenant:job").with_container_id(tenant_id);

        assert_eq!(spec.container_id, Some(tenant_id));
    }

    #[test]
    fn test_job_spec_builder_chaining() {
        let ref_id = Uuid::new_v4();
        let container_id = Uuid::new_v4();

        let spec = JobSpec::new("complex:job")
            .with_idempotency_key("key:123")
            .with_max_retries(5)
            .with_priority(10)
            .with_version(2)
            .with_reference_id(ref_id)
            .with_container_id(container_id);

        assert_eq!(spec.job_type, "complex:job");
        assert_eq!(spec.idempotency_key, Some("key:123".to_string()));
        assert_eq!(spec.max_retries, 5);
        assert_eq!(spec.priority, 10);
        assert_eq!(spec.version, 2);
        assert_eq!(spec.reference_id, Some(ref_id));
        assert_eq!(spec.container_id, Some(container_id));
    }

    #[test]
    fn test_job_spec_clone() {
        let spec = JobSpec::new("clone:test")
            .with_idempotency_key("original")
            .with_priority(5);

        let cloned = spec.clone();

        assert_eq!(cloned.job_type, spec.job_type);
        assert_eq!(cloned.idempotency_key, spec.idempotency_key);
        assert_eq!(cloned.priority, spec.priority);
    }

    #[test]
    fn test_job_spec_debug() {
        let spec = JobSpec::new("debug:test").with_idempotency_key("test-key");

        let debug = format!("{:?}", spec);
        assert!(debug.contains("JobSpec"));
        assert!(debug.contains("debug:test"));
        assert!(debug.contains("test-key"));
    }

    // =========================================================================
    // CorrelationId Tests
    // =========================================================================

    #[test]
    fn test_correlation_id_new() {
        let cid1 = CorrelationId::new();
        let cid2 = CorrelationId::new();

        assert!(cid1.is_some());
        assert!(cid2.is_some());
        assert_ne!(cid1, cid2);
    }

    #[test]
    fn test_correlation_id_none() {
        let cid = CorrelationId::NONE;

        assert!(cid.is_none());
        assert!(!cid.is_some());
        assert_eq!(cid.into_inner(), Uuid::nil());
    }

    #[test]
    fn test_correlation_id_from_uuid() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);

        assert_eq!(cid.into_inner(), uuid);
        assert!(cid.is_some());
    }

    #[test]
    fn test_correlation_id_from_option_some() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(Some(uuid));

        assert_eq!(cid.into_inner(), uuid);
    }

    #[test]
    fn test_correlation_id_from_option_none() {
        let cid = CorrelationId::from(None);

        assert!(cid.is_none());
        assert_eq!(cid, CorrelationId::NONE);
    }

    #[test]
    fn test_correlation_id_display_none() {
        let cid = CorrelationId::NONE;
        assert_eq!(format!("{}", cid), "NONE");
    }

    #[test]
    fn test_correlation_id_display_some() {
        let uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let cid = CorrelationId::from(uuid);
        assert_eq!(format!("{}", cid), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn test_correlation_id_as_uuid() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);

        assert_eq!(cid.as_uuid(), &uuid);
    }

    #[test]
    fn test_correlation_id_into_uuid() {
        let uuid = Uuid::new_v4();
        let cid = CorrelationId::from(uuid);
        let back: Uuid = cid.into();

        assert_eq!(back, uuid);
    }

    #[test]
    fn test_correlation_id_ordering() {
        let uuid1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let uuid2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let cid1 = CorrelationId::from(uuid1);
        let cid2 = CorrelationId::from(uuid2);

        assert!(cid1 < cid2);
    }

    #[test]
    fn test_correlation_id_hash() {
        use std::collections::HashSet;

        let cid1 = CorrelationId::new();
        let cid2 = CorrelationId::new();

        let mut set = HashSet::new();
        set.insert(cid1);
        set.insert(cid2);
        set.insert(cid1); // Duplicate

        assert_eq!(set.len(), 2);
    }

    // =========================================================================
    // EventRole Tests
    // =========================================================================

    #[test]
    fn test_event_role_is_input() {
        assert!(EventRole::Input.is_input());
        assert!(!EventRole::Fact.is_input());
        assert!(!EventRole::Signal.is_input());
    }

    #[test]
    fn test_event_role_is_fact() {
        assert!(!EventRole::Input.is_fact());
        assert!(EventRole::Fact.is_fact());
        assert!(!EventRole::Signal.is_fact());
    }

    #[test]
    fn test_event_role_is_signal() {
        assert!(!EventRole::Input.is_signal());
        assert!(!EventRole::Fact.is_signal());
        assert!(EventRole::Signal.is_signal());
    }

    #[test]
    fn test_event_role_is_actionable() {
        assert!(EventRole::Input.is_actionable());
        assert!(EventRole::Fact.is_actionable());
        assert!(!EventRole::Signal.is_actionable());
    }

    // =========================================================================
    // ExecutionMode Tests
    // =========================================================================

    #[test]
    fn test_execution_mode_inline_eq() {
        assert_eq!(ExecutionMode::Inline, ExecutionMode::Inline);
    }

    #[test]
    fn test_execution_mode_background_eq() {
        assert_eq!(ExecutionMode::Background, ExecutionMode::Background);
    }

    #[test]
    fn test_execution_mode_scheduled_eq() {
        let time = Utc::now();
        assert_eq!(
            ExecutionMode::Scheduled { run_at: time },
            ExecutionMode::Scheduled { run_at: time }
        );
    }

    #[test]
    fn test_execution_mode_scheduled_ne_different_times() {
        let time1 = Utc::now();
        let time2 = time1 + chrono::Duration::hours(1);
        assert_ne!(
            ExecutionMode::Scheduled { run_at: time1 },
            ExecutionMode::Scheduled { run_at: time2 }
        );
    }

    #[test]
    fn test_execution_mode_ne_different_variants() {
        assert_ne!(ExecutionMode::Inline, ExecutionMode::Background);
        assert_ne!(
            ExecutionMode::Background,
            ExecutionMode::Scheduled { run_at: Utc::now() }
        );
    }

    #[test]
    fn test_execution_mode_debug() {
        let debug = format!("{:?}", ExecutionMode::Background);
        assert_eq!(debug, "Background");
    }

    // =========================================================================
    // EnvelopeMatch Tests
    // =========================================================================

    #[derive(Debug, Clone, PartialEq)]
    struct UserCreated {
        user_id: Uuid,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct UserDeleted {
        user_id: Uuid,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct AuthDenied {
        reason: String,
    }

    #[test]
    fn test_envelope_match_event() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let matcher = EnvelopeMatch::new(&envelope);
        let event = matcher.event::<UserCreated>();

        assert!(event.is_some());
        assert_eq!(event.unwrap().user_id, user_id);
    }

    #[test]
    fn test_envelope_match_event_wrong_type() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        let matcher = EnvelopeMatch::new(&envelope);
        let event = matcher.event::<UserDeleted>();

        assert!(event.is_none());
    }

    #[test]
    fn test_envelope_match_is() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        let matcher = EnvelopeMatch::new(&envelope);

        assert!(matcher.is::<UserCreated>());
        assert!(!matcher.is::<UserDeleted>());
    }

    #[test]
    fn test_envelope_match_map() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let matcher = EnvelopeMatch::new(&envelope);
        let result = matcher.map(|e: &UserCreated| e.user_id);

        assert_eq!(result, Some(user_id));
    }

    #[test]
    fn test_envelope_match_and_then() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let matcher = EnvelopeMatch::new(&envelope);
        let result = matcher.and_then(|e: &UserCreated| {
            if e.user_id == user_id {
                Some("found")
            } else {
                None
            }
        });

        assert_eq!(result, Some("found"));
    }

    #[test]
    fn test_envelope_match_try_match_success() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        let result = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(e.user_id))
            .result();

        assert_eq!(result, Some(user_id));
    }

    #[test]
    fn test_envelope_match_try_match_or_try() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserDeleted { user_id });

        let result: Option<Result<Uuid, &str>> = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(Ok(e.user_id)))
            .or_try(|e: &UserDeleted| Some(Err("deleted")))
            .result();

        assert!(matches!(result, Some(Err("deleted"))));
    }

    #[test]
    fn test_envelope_match_chain_first_matches() {
        let user_id = Uuid::new_v4();
        let envelope = EventEnvelope::new(CorrelationId::new(), UserCreated { user_id });

        // First matcher should succeed, second should not be evaluated
        let result = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(e.user_id))
            .or_try(|_: &UserDeleted| Some(Uuid::nil()))
            .result();

        assert_eq!(result, Some(user_id));
    }

    #[test]
    fn test_envelope_match_chain_no_match() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            AuthDenied {
                reason: "test".into(),
            },
        );

        let result: Option<Uuid> = EnvelopeMatch::new(&envelope)
            .try_match(|e: &UserCreated| Some(e.user_id))
            .or_try(|e: &UserDeleted| Some(e.user_id))
            .result();

        assert!(result.is_none());
    }

    // =========================================================================
    // EventEnvelope Tests
    // =========================================================================

    #[test]
    fn test_event_envelope_new() {
        let cid = CorrelationId::new();
        let event = UserCreated {
            user_id: Uuid::new_v4(),
        };
        let envelope = EventEnvelope::new(cid, event.clone());

        assert_eq!(envelope.cid, cid);
        assert_eq!(envelope.type_id, TypeId::of::<UserCreated>());
        assert_eq!(envelope.downcast_ref::<UserCreated>(), Some(&event));
    }

    #[test]
    fn test_event_envelope_new_random() {
        let event = UserCreated {
            user_id: Uuid::new_v4(),
        };
        let envelope = EventEnvelope::new_random(event);

        assert!(envelope.cid.is_some());
    }

    #[test]
    fn test_event_envelope_downcast_wrong_type() {
        let envelope = EventEnvelope::new(
            CorrelationId::new(),
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        assert!(envelope.downcast_ref::<UserDeleted>().is_none());
    }

    #[test]
    fn test_event_envelope_debug() {
        let cid = CorrelationId::new();
        let envelope = EventEnvelope::new(
            cid,
            UserCreated {
                user_id: Uuid::new_v4(),
            },
        );

        let debug = format!("{:?}", envelope);
        assert!(debug.contains("EventEnvelope"));
        assert!(debug.contains("cid"));
    }
}
