//! Macros for reducing command boilerplate.

/// Auto-implement `serialize_to_json()` for commands that derive `Serialize`.
///
/// Use this inside your `Command` impl block as a one-liner replacement for
/// manual serialization code.
///
/// # Example
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
///     auto_serialize!();  // One line instead of a whole method!
/// }
/// ```
#[macro_export]
macro_rules! auto_serialize {
    () => {
        fn serialize_to_json(&self) -> Option<serde_json::Value> {
            serde_json::to_value(self).ok()
        }
    };
}
