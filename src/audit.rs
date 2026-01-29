//! Debug-only event auditing for development visibility.
//!
//! This module provides tools to track which machines observe and emit commands
//! in response to events. It's only active in debug builds and has zero production cost.
//!
//! # Purpose
//!
//! Auditing catches:
//! - Wiring mistakes (event has no handlers)
//! - Dead domains (machines that never emit)
//! - Forgotten machines after refactors
//!
//! # Usage
//!
//! ```ignore
//! #[cfg(debug_assertions)]
//! {
//!     let audit = runtime.audit_log();
//!     for entry in audit.recent_entries() {
//!         if entry.emitters.is_empty() {
//!             tracing::warn!(
//!                 event_type = ?entry.event_type,
//!                 observers = ?entry.observers,
//!                 "event had no command emitters"
//!             );
//!         }
//!     }
//! }
//! ```

use std::any::TypeId;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Maximum number of audit entries to retain.
const MAX_AUDIT_ENTRIES: usize = 1000;

/// A single audit entry for one event processing cycle.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// The TypeId of the event that was processed.
    pub event_type: TypeId,
    /// Human-readable event type name (from std::any::type_name).
    pub event_type_name: &'static str,
    /// Names of machines that observed this event (matched the event type).
    pub observers: Vec<&'static str>,
    /// Names of machines that emitted a command in response.
    pub emitters: Vec<&'static str>,
    /// Whether any machine emitted a command.
    pub had_effect: bool,
}

impl AuditEntry {
    /// Returns true if no machine emitted a command for this event.
    pub fn was_silent(&self) -> bool {
        self.emitters.is_empty()
    }

    /// Returns true if machines observed but none emitted.
    pub fn observed_but_silent(&self) -> bool {
        !self.observers.is_empty() && self.emitters.is_empty()
    }
}

/// Audit log for tracking event processing.
///
/// Thread-safe collection of recent audit entries. Only retains the most
/// recent `MAX_AUDIT_ENTRIES` to bound memory usage.
#[derive(Debug, Default)]
pub struct AuditLog {
    entries: Mutex<VecDeque<AuditEntry>>,
}

impl AuditLog {
    /// Create a new empty audit log.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(MAX_AUDIT_ENTRIES)),
        }
    }

    /// Acquire the entries lock, recovering from poison if necessary.
    fn lock_entries(&self) -> std::sync::MutexGuard<'_, VecDeque<AuditEntry>> {
        self.entries.lock().unwrap_or_else(|poisoned| {
            // Recover from poisoned mutex - audit log is debug-only,
            // so we prefer availability over strict consistency
            poisoned.into_inner()
        })
    }

    /// Record an audit entry.
    pub fn record(&self, entry: AuditEntry) {
        let mut entries = self.lock_entries();
        if entries.len() >= MAX_AUDIT_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Get all recent entries (clone).
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.lock_entries().iter().cloned().collect()
    }

    /// Get the most recent N entries.
    pub fn recent(&self, n: usize) -> Vec<AuditEntry> {
        let entries = self.lock_entries();
        entries.iter().rev().take(n).cloned().collect()
    }

    /// Get entries where no machine emitted a command.
    pub fn silent_events(&self) -> Vec<AuditEntry> {
        self.lock_entries()
            .iter()
            .filter(|e| e.was_silent())
            .cloned()
            .collect()
    }

    /// Get entries where machines observed but none emitted.
    pub fn observed_but_silent(&self) -> Vec<AuditEntry> {
        self.lock_entries()
            .iter()
            .filter(|e| e.observed_but_silent())
            .cloned()
            .collect()
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.lock_entries().clear();
    }

    /// Get the number of entries.
    pub fn len(&self) -> usize {
        self.lock_entries().len()
    }

    /// Check if the log is empty.
    pub fn is_empty(&self) -> bool {
        self.lock_entries().is_empty()
    }

    /// Get summary statistics.
    pub fn stats(&self) -> AuditStats {
        let entries = self.lock_entries();
        let total = entries.len();
        let silent = entries.iter().filter(|e| e.was_silent()).count();
        let with_effect = entries.iter().filter(|e| e.had_effect).count();

        AuditStats {
            total_events: total,
            silent_events: silent,
            events_with_effect: with_effect,
        }
    }
}

/// Summary statistics from the audit log.
#[derive(Debug, Clone, Copy)]
pub struct AuditStats {
    /// Total number of events processed.
    pub total_events: usize,
    /// Events where no machine emitted a command.
    pub silent_events: usize,
    /// Events where at least one machine emitted a command.
    pub events_with_effect: usize,
}

/// Builder for constructing audit entries during event processing.
#[derive(Debug)]
pub struct AuditEntryBuilder {
    event_type: TypeId,
    event_type_name: &'static str,
    observers: Vec<&'static str>,
    emitters: Vec<&'static str>,
}

impl AuditEntryBuilder {
    /// Create a new builder for the given event type.
    pub fn new<E: 'static>() -> Self {
        Self {
            event_type: TypeId::of::<E>(),
            event_type_name: std::any::type_name::<E>(),
            observers: Vec::new(),
            emitters: Vec::new(),
        }
    }

    /// Create a new builder with explicit type info.
    pub fn with_type_id(event_type: TypeId, event_type_name: &'static str) -> Self {
        Self {
            event_type,
            event_type_name,
            observers: Vec::new(),
            emitters: Vec::new(),
        }
    }

    /// Record that a machine observed this event.
    pub fn observed(&mut self, machine_name: &'static str) {
        self.observers.push(machine_name);
    }

    /// Record that a machine emitted a command.
    pub fn emitted(&mut self, machine_name: &'static str) {
        self.emitters.push(machine_name);
    }

    /// Build the final audit entry.
    pub fn build(self) -> AuditEntry {
        AuditEntry {
            event_type: self.event_type,
            event_type_name: self.event_type_name,
            observers: self.observers,
            emitters: self.emitters.clone(),
            had_effect: !self.emitters.is_empty(),
        }
    }
}

/// Shared handle to an audit log.
pub type SharedAuditLog = Arc<AuditLog>;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestEvent;

    #[derive(Debug)]
    struct OtherEvent;

    #[test]
    fn test_audit_entry_silent() {
        let entry = AuditEntry {
            event_type: TypeId::of::<TestEvent>(),
            event_type_name: "TestEvent",
            observers: vec!["MachineA", "MachineB"],
            emitters: vec![],
            had_effect: false,
        };

        assert!(entry.was_silent());
        assert!(entry.observed_but_silent());
    }

    #[test]
    fn test_audit_entry_with_effect() {
        let entry = AuditEntry {
            event_type: TypeId::of::<TestEvent>(),
            event_type_name: "TestEvent",
            observers: vec!["MachineA"],
            emitters: vec!["MachineA"],
            had_effect: true,
        };

        assert!(!entry.was_silent());
        assert!(!entry.observed_but_silent());
    }

    #[test]
    fn test_audit_log_record() {
        let log = AuditLog::new();

        let entry = AuditEntryBuilder::new::<TestEvent>().build();

        log.record(entry);
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn test_audit_log_max_entries() {
        let log = AuditLog::new();

        for _ in 0..MAX_AUDIT_ENTRIES + 100 {
            log.record(AuditEntryBuilder::new::<TestEvent>().build());
        }

        assert_eq!(log.len(), MAX_AUDIT_ENTRIES);
    }

    #[test]
    fn test_audit_log_silent_events() {
        let log = AuditLog::new();

        // Silent event
        let mut builder = AuditEntryBuilder::new::<TestEvent>();
        builder.observed("MachineA");
        log.record(builder.build());

        // Event with effect
        let mut builder = AuditEntryBuilder::new::<OtherEvent>();
        builder.observed("MachineB");
        builder.emitted("MachineB");
        log.record(builder.build());

        let silent = log.silent_events();
        assert_eq!(silent.len(), 1);
        assert_eq!(silent[0].event_type_name, "seesaw::audit::tests::TestEvent");
    }

    #[test]
    fn test_audit_log_stats() {
        let log = AuditLog::new();

        // 2 silent events
        log.record(AuditEntryBuilder::new::<TestEvent>().build());
        log.record(AuditEntryBuilder::new::<TestEvent>().build());

        // 1 event with effect
        let mut builder = AuditEntryBuilder::new::<OtherEvent>();
        builder.emitted("MachineA");
        log.record(builder.build());

        let stats = log.stats();
        assert_eq!(stats.total_events, 3);
        assert_eq!(stats.silent_events, 2);
        assert_eq!(stats.events_with_effect, 1);
    }

    #[test]
    fn test_audit_entry_builder() {
        let mut builder = AuditEntryBuilder::new::<TestEvent>();
        builder.observed("MachineA");
        builder.observed("MachineB");
        builder.emitted("MachineA");

        let entry = builder.build();

        assert_eq!(entry.observers.len(), 2);
        assert_eq!(entry.emitters.len(), 1);
        assert!(entry.had_effect);
    }

    #[test]
    fn test_audit_log_recent() {
        let log = AuditLog::new();

        for i in 0..10 {
            let mut builder = AuditEntryBuilder::with_type_id(
                TypeId::of::<TestEvent>(),
                if i % 2 == 0 { "Even" } else { "Odd" },
            );
            if i >= 5 {
                builder.emitted("Machine");
            }
            log.record(builder.build());
        }

        let recent = log.recent(3);
        assert_eq!(recent.len(), 3);
        // Most recent first
        assert!(recent[0].had_effect);
    }
}
