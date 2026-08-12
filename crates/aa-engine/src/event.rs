//! Engine command and event types for the CB session runner.

use aa_registers::{CanRecord, RegId, RegisterBank, UnitId, UnitType};

/// Commands sent into [`crate::CbEngine::run`] via an mpsc channel.
#[derive(Debug, Clone)]
pub enum EngineCmd {
    /// Enqueue register writes for the next steady-state `setCAN` window.
    WriteRegisters(Vec<CanRecord>),
    /// Queue a unit-scoped reg-06 flush and track a pending read; the flush's
    /// `getCAN` resolves it via [`EngineEvent::RegisterRead`].
    ReadRegister {
        /// Target unit type (also addresses the queued flush).
        unit_type: UnitType,
        /// Target unit id (also addresses the queued flush).
        unit_id: UnitId,
        /// Register to read.
        reg: RegId,
        /// Zone for zone-bearing registers (`0x03`/`0x04`); `None` otherwise.
        zone: Option<u8>,
    },
    /// Re-enter the dump path; emits a fresh [`EngineEvent::Snapshot`] when done.
    ResyncMailbox,
    /// Stop the runner and close the link.
    Shutdown,
}

/// Session state of the engine's mailbox lifecycle.
///
/// The runner emits [`EngineEvent::SessionState`] on every transition. The
/// daemon maps these 1:1 onto its wire `status` frames (`negotiating`,
/// `synced`, `resyncing`, `link_down`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Session is starting; negotiating with the unit (`getSystemData` polls
    /// until `CAN2 in use`).
    Negotiating,
    /// Mailbox in sync: emitted alongside every [`EngineEvent::Snapshot`]
    /// (initial dump or completed resync).
    Synced,
    /// [`EngineCmd::ResyncMailbox`] was applied; re-entering the dump path.
    Resyncing,
    /// Link I/O failure; the runner is tearing the session down (the daemon
    /// keeps serving stale state).
    LinkDown,
}

/// Events emitted by [`crate::CbEngine::run`] via an mpsc channel.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// Negotiation succeeded (`CAN2 in use`).
    Negotiated { detail: String },
    /// Full mailbox snapshot after dump / resync.
    ///
    /// `can_records` are the CB dump's opaque 25-char hex records for `MyAir5`
    /// `rawCan` (USB parity).
    Snapshot {
        bank: RegisterBank,
        can_records: Option<Vec<String>>,
    },
    /// Incremental register updates from a steady-state `getCAN`.
    RegistersChanged { records: Vec<CanRecord> },
    /// Queued register writes were transmitted in a `setCAN` frame.
    /// Lets the WS bridge defer mailbox acks until the bus actually sent them.
    WriteFlushed,
    /// Result of a previously queued [`EngineCmd::ReadRegister`]: the flush's
    /// `getCAN` resolved the register against the bank. `data` is `Some` when
    /// the flush response carried the register, `None` when it did not (reads
    /// are never answered with pre-flush bank state).
    RegisterRead {
        unit_type: UnitType,
        unit_id: UnitId,
        reg: RegId,
        zone: Option<u8>,
        data: Option<[u8; 7]>,
    },
    /// Session lifecycle state transition; the daemon maps it to a wire
    /// `status` frame (see [`SessionState`]).
    SessionState(SessionState),
    /// Link I/O failure; runner is exiting.
    LinkError(String),
    /// Non-fatal protocol anomaly (e.g. negotiate mismatch); session stays alive.
    ProtocolWarn(String),
}
