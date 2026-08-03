//! Engine command and event types for the CB session runner.

use aa_registers::{CanRecord, RegisterBank};

/// Commands sent into [`crate::CbEngine::run`] via an mpsc channel.
#[derive(Debug, Clone)]
pub enum EngineCmd {
    /// Enqueue register writes for the next steady-state `setCAN` window.
    WriteRegisters(Vec<CanRecord>),
    /// Re-enter the dump path; emits a fresh [`EngineEvent::Snapshot`] when done.
    ResyncMailbox,
    /// Stop the runner and close the link.
    Shutdown,
}

/// Events emitted by [`crate::CbEngine::run`] via an mpsc channel.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// Negotiation succeeded (`CAN2 in use`).
    Negotiated { detail: String },
    /// Full mailbox snapshot after dump / resync.
    ///
    /// `can_records` are the CB dump's opaque 25-char hex records for `MyAir5`
    /// `rawCan` (USB parity). They intentionally exclude daemon-synthesized
    /// registers such as reg `05` seeded into [`bank`] for typed `system_status`.
    Snapshot {
        bank: RegisterBank,
        can_records: Option<Vec<String>>,
    },
    /// Incremental register updates from a steady-state `getCAN`.
    RegistersChanged { records: Vec<CanRecord> },
    /// Link I/O failure; runner is exiting.
    LinkError(String),
    /// Non-fatal protocol anomaly (e.g. negotiate mismatch); session stays alive.
    ProtocolWarn(String),
}
