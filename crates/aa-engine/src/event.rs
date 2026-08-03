//! Engine command and event types for the CB session runner.

use aa_registers::{CanRecord, RegisterBank};

/// Commands sent into [`crate::CbEngine::run`] via an mpsc channel.
#[derive(Debug, Clone)]
pub enum EngineCmd {
    /// Enqueue register writes for the next steady-state `setCAN` window.
    WriteRegisters(Vec<CanRecord>),
    /// Enqueue a raw direct-message write (one-shot poll / command string) for
    /// the next steady-state ping (aaservice direct-queue parity).
    WriteDirect(Vec<u8>),
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
    /// Queued register writes were transmitted in a `setCAN` frame.
    /// Lets the WS bridge defer mailbox acks until the bus actually sent them.
    WriteFlushed,
    /// Reply to a [`EngineCmd::WriteDirect`] request (non-getCAN CB frame).
    DirectReply { payload: Vec<u8> },
    /// Link I/O failure; runner is exiting.
    LinkError(String),
    /// Non-fatal protocol anomaly (e.g. negotiate mismatch); session stays alive.
    ProtocolWarn(String),
}
