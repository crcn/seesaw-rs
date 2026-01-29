//! Machine trait and type-erased runner.
//!
//! Machines are pure state machines that interpret events and decide on commands.
//! State lives inside the machine, and the `decide` method is synchronous (no IO).
//!
//! # Key Properties
//!
//! - **State is internal**: Each machine owns its state via `&mut self`
//! - **Pure decisions**: No IO, no async, just state transitions and command emission
//! - **One event → one command**: Returns `Option<Command>`, not `Vec<Command>`
//! - **Fan-out via multiple machines**: Same event can be observed by many machines

use std::any::{Any, TypeId};
use std::panic::{catch_unwind, AssertUnwindSafe};

use tracing::error;

use crate::core::{AnyCommand, Command, Event};

/// A state machine that interprets events and decides on commands.
///
/// Machines are the decision-making layer of seesaw. They:
/// 1. Receive events (facts about what happened)
/// 2. Update internal state
/// 3. Optionally emit a command (intent for IO)
///
/// # State Ownership
///
/// State lives inside the machine. The `decide` method takes `&mut self`,
/// allowing the machine to update its state in response to events.
///
/// # Example
///
/// ```ignore
/// use std::collections::HashSet;
/// use uuid::Uuid;
///
/// struct BakeMachine {
///     pending: HashSet<Uuid>,
///     active: HashSet<Uuid>,
/// }
///
/// impl Machine for BakeMachine {
///     type Event = BakeEvent;
///     type Command = BakeCommand;
///
///     fn decide(&mut self, event: &BakeEvent) -> Option<BakeCommand> {
///         match event {
///             BakeEvent::Requested { deck_id, recipe_id } => {
///                 self.pending.insert(*deck_id);
///                 Some(BakeCommand::SetupLoaf {
///                     deck_id: *deck_id,
///                     recipe_id: *recipe_id,
///                 })
///             }
///             BakeEvent::LoafReady { loaf_id } => {
///                 Some(BakeCommand::GenerateCards { loaf_id: *loaf_id })
///             }
///             BakeEvent::GenerationComplete { loaf_id } => {
///                 self.pending.remove(loaf_id);
///                 self.active.insert(*loaf_id);
///                 Some(BakeCommand::CompleteLoaf { loaf_id: *loaf_id })
///             }
///             _ => None,
///         }
///     }
/// }
/// ```
pub trait Machine: Send + Sync + 'static {
    /// The event type this machine handles.
    type Event: Event;

    /// The command type this machine can emit.
    type Command: Command;

    /// Process an event and optionally return a command.
    ///
    /// This method is called for each event that matches `Self::Event`.
    /// The machine can:
    /// - Update its internal state
    /// - Return `Some(command)` to request IO
    /// - Return `None` to take no action
    ///
    /// # Guarantees
    ///
    /// - Called synchronously (no async)
    /// - Called serially (no concurrent calls)
    /// - At most one command per event
    fn decide(&mut self, event: &Self::Event) -> Option<Self::Command>;
}

/// Type-erased machine trait for internal use.
pub(crate) trait AnyMachine: Send + Sync {
    /// Process a type-erased event and optionally return a type-erased command.
    fn decide_any(&mut self, event: &dyn Any) -> Option<Box<dyn AnyCommand>>;
}

impl<M: Machine> AnyMachine for M {
    fn decide_any(&mut self, event: &dyn Any) -> Option<Box<dyn AnyCommand>> {
        let event = event.downcast_ref::<M::Event>()?;
        let cmd = self.decide(event)?;
        Some(Box::new(cmd))
    }
}

/// Type-erased wrapper for machines.
///
/// `MachineRunner` enables a runtime to hold multiple machines with different
/// event and command types in a single collection.
pub struct MachineRunner {
    inner: Box<dyn AnyMachine>,
    event_type: TypeId,
    /// Human-readable name for debugging/auditing.
    name: &'static str,
}

impl MachineRunner {
    /// Create a new machine runner wrapping the given machine.
    ///
    /// The name is derived from the machine's type name.
    pub fn new<M: Machine>(machine: M) -> Self {
        Self {
            event_type: TypeId::of::<M::Event>(),
            inner: Box::new(machine),
            name: std::any::type_name::<M>(),
        }
    }

    /// Create a new machine runner with a custom name.
    ///
    /// Useful when you have multiple instances of the same machine type.
    pub fn with_name<M: Machine>(machine: M, name: &'static str) -> Self {
        Self {
            event_type: TypeId::of::<M::Event>(),
            inner: Box::new(machine),
            name,
        }
    }

    /// Try to decide on a command for the given event.
    ///
    /// Returns `Ok(None)` if:
    /// - The event type doesn't match this machine's event type
    /// - The machine decides not to emit a command
    ///
    /// Returns `Ok(Some(cmd))` if the machine emits a command.
    ///
    /// Returns `Err(message)` if the machine panics.
    ///
    /// # Panic Safety
    ///
    /// If the machine's `decide` method panics, this method catches the panic
    /// and returns `Err` with the panic message. This prevents a single machine
    /// from crashing the entire runtime. The machine's state may be inconsistent
    /// after a panic.
    pub fn decide(&mut self, event: &dyn Any) -> Result<Option<Box<dyn AnyCommand>>, String> {
        // Wrap in catch_unwind to prevent machine panics from crashing the runtime.
        // AssertUnwindSafe is needed because &mut self is not UnwindSafe by default.
        // This is safe because we don't access the machine after a panic.
        let result = catch_unwind(AssertUnwindSafe(|| self.inner.decide_any(event)));

        match result {
            Ok(cmd) => Ok(cmd),
            Err(panic_info) => {
                // Extract panic message if available
                let panic_msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };

                error!(
                    machine = self.name,
                    panic = %panic_msg,
                    "machine panicked in decide()"
                );
                Err(format!("machine '{}' panicked: {}", self.name, panic_msg))
            }
        }
    }

    /// Check if this machine handles the given event type.
    ///
    /// Returns true if the event's TypeId matches this machine's event type.
    pub fn handles_event(&self, event: &dyn Any) -> bool {
        (*event).type_id() == self.event_type
    }

    /// Returns the TypeId of events this machine handles.
    pub fn event_type(&self) -> TypeId {
        self.event_type
    }

    /// Returns the machine's name for debugging/auditing.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // Test event types
    #[derive(Debug, Clone)]
    enum CounterEvent {
        Increment,
        Decrement,
        Reset,
    }
    // Event auto-impl by blanket

    // Test command types
    #[derive(Debug, Clone, PartialEq)]
    enum CounterCommand {
        UpdateDisplay { value: i32 },
        PlaySound { sound: String },
    }
    impl Command for CounterCommand {}

    // Test machine
    struct CounterMachine {
        count: i32,
    }

    impl CounterMachine {
        fn new() -> Self {
            Self { count: 0 }
        }
    }

    impl Machine for CounterMachine {
        type Event = CounterEvent;
        type Command = CounterCommand;

        fn decide(&mut self, event: &CounterEvent) -> Option<CounterCommand> {
            match event {
                CounterEvent::Increment => {
                    self.count += 1;
                    Some(CounterCommand::UpdateDisplay { value: self.count })
                }
                CounterEvent::Decrement => {
                    self.count -= 1;
                    Some(CounterCommand::UpdateDisplay { value: self.count })
                }
                CounterEvent::Reset => {
                    self.count = 0;
                    Some(CounterCommand::PlaySound {
                        sound: "reset".to_string(),
                    })
                }
            }
        }
    }

    #[test]
    fn test_machine_state_updates() {
        let mut machine = CounterMachine::new();

        machine.decide(&CounterEvent::Increment);
        assert_eq!(machine.count, 1);

        machine.decide(&CounterEvent::Increment);
        assert_eq!(machine.count, 2);

        machine.decide(&CounterEvent::Decrement);
        assert_eq!(machine.count, 1);

        machine.decide(&CounterEvent::Reset);
        assert_eq!(machine.count, 0);
    }

    #[test]
    fn test_machine_returns_commands() {
        let mut machine = CounterMachine::new();

        let cmd = machine.decide(&CounterEvent::Increment);
        assert_eq!(cmd, Some(CounterCommand::UpdateDisplay { value: 1 }));

        let cmd = machine.decide(&CounterEvent::Reset);
        assert_eq!(
            cmd,
            Some(CounterCommand::PlaySound {
                sound: "reset".to_string()
            })
        );
    }

    #[test]
    fn test_machine_runner_decide() {
        let machine = CounterMachine::new();
        let mut runner = MachineRunner::new(machine);

        let event = CounterEvent::Increment;
        let result = runner.decide(&event);

        assert!(result.is_ok());
        let cmd = result.unwrap();
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        let downcasted = cmd.as_any().downcast_ref::<CounterCommand>();
        assert!(downcasted.is_some());
        assert_eq!(
            *downcasted.unwrap(),
            CounterCommand::UpdateDisplay { value: 1 }
        );
    }

    #[test]
    fn test_machine_runner_wrong_event_type() {
        #[derive(Debug, Clone)]
        struct OtherEvent;
        // Event auto-impl by blanket

        let machine = CounterMachine::new();
        let mut runner = MachineRunner::new(machine);

        // This event type doesn't match CounterEvent
        let event = OtherEvent;
        let result = runner.decide(&event);

        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "Should return None for wrong event type"
        );
    }

    #[test]
    fn test_machine_runner_panic_returns_error() {
        struct PanicMachine;

        impl Machine for PanicMachine {
            type Event = CounterEvent;
            type Command = CounterCommand;

            fn decide(&mut self, _event: &CounterEvent) -> Option<CounterCommand> {
                panic!("intentional panic");
            }
        }

        let machine = PanicMachine;
        let mut runner = MachineRunner::new(machine);

        let event = CounterEvent::Increment;
        let result = runner.decide(&event);

        assert!(result.is_err());
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("Expected error"),
        };
        assert!(err.contains("panicked"), "Error should mention panic: {}", err);
        assert!(
            err.contains("intentional panic"),
            "Error should contain panic message: {}",
            err
        );
    }

    #[test]
    fn test_machine_runner_event_type() {
        let machine = CounterMachine::new();
        let runner = MachineRunner::new(machine);

        assert_eq!(runner.event_type(), TypeId::of::<CounterEvent>());
    }

    // Test machine that sometimes returns None
    struct SelectiveMachine {
        threshold: i32,
        count: i32,
    }

    #[derive(Debug, Clone)]
    struct ValueEvent {
        value: i32,
    }
    // Event auto-impl by blanket

    #[derive(Debug, Clone, PartialEq)]
    struct AlertCommand {
        message: String,
    }
    impl Command for AlertCommand {}

    impl Machine for SelectiveMachine {
        type Event = ValueEvent;
        type Command = AlertCommand;

        fn decide(&mut self, event: &ValueEvent) -> Option<AlertCommand> {
            self.count += 1;
            if event.value > self.threshold {
                Some(AlertCommand {
                    message: format!("Value {} exceeds threshold", event.value),
                })
            } else {
                None // No command for values below threshold
            }
        }
    }

    #[test]
    fn test_machine_can_return_none() {
        let mut machine = SelectiveMachine {
            threshold: 10,
            count: 0,
        };

        // Below threshold - no command
        let cmd = machine.decide(&ValueEvent { value: 5 });
        assert!(cmd.is_none());
        assert_eq!(machine.count, 1); // But state still updated

        // Above threshold - command emitted
        let cmd = machine.decide(&ValueEvent { value: 15 });
        assert!(cmd.is_some());
        assert_eq!(machine.count, 2);
    }

    // Test multiple machines observing same event
    #[derive(Debug, Clone)]
    struct SharedEvent {
        id: u64,
    }
    // Event auto-impl by blanket

    #[derive(Debug, Clone, PartialEq)]
    struct LogCommand {
        message: String,
    }
    impl Command for LogCommand {}

    #[derive(Debug, Clone, PartialEq)]
    struct MetricCommand {
        name: String,
        value: f64,
    }
    impl Command for MetricCommand {}

    struct LogMachine {
        seen: HashSet<u64>,
    }

    impl Machine for LogMachine {
        type Event = SharedEvent;
        type Command = LogCommand;

        fn decide(&mut self, event: &SharedEvent) -> Option<LogCommand> {
            self.seen.insert(event.id);
            Some(LogCommand {
                message: format!("Saw event {}", event.id),
            })
        }
    }

    struct MetricMachine {
        total: f64,
    }

    impl Machine for MetricMachine {
        type Event = SharedEvent;
        type Command = MetricCommand;

        fn decide(&mut self, event: &SharedEvent) -> Option<MetricCommand> {
            self.total += 1.0;
            Some(MetricCommand {
                name: "event_count".to_string(),
                value: self.total,
            })
        }
    }

    #[test]
    fn test_multiple_machines_same_event() {
        let event = SharedEvent { id: 42 };

        let mut log_runner = MachineRunner::new(LogMachine {
            seen: HashSet::new(),
        });

        let mut metric_runner = MachineRunner::new(MetricMachine { total: 0.0 });

        // Both machines handle the same event
        let log_result = log_runner.decide(&event);
        let metric_result = metric_runner.decide(&event);

        assert!(log_result.is_ok());
        assert!(metric_result.is_ok());
        let log_cmd = log_result.unwrap();
        let metric_cmd = metric_result.unwrap();

        assert!(log_cmd.is_some());
        assert!(metric_cmd.is_some());

        // Verify each returns its own command type
        let log_cmd = log_cmd
            .unwrap()
            .as_any()
            .downcast_ref::<LogCommand>()
            .cloned();
        let metric_cmd = metric_cmd
            .unwrap()
            .as_any()
            .downcast_ref::<MetricCommand>()
            .cloned();

        assert_eq!(
            log_cmd,
            Some(LogCommand {
                message: "Saw event 42".to_string()
            })
        );
        assert_eq!(
            metric_cmd,
            Some(MetricCommand {
                name: "event_count".to_string(),
                value: 1.0
            })
        );
    }
}
