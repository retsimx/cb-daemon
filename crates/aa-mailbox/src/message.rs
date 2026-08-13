//! Client- and server-directed protocol message enums.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dto::{AckStatus, StatusState};

/// Messages sent from the daemon to the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)] // `serde_json::Value` is not `Eq`.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Full register snapshot of all known units (after connect / resync).
    Snapshot {
        /// `"{unit_type}:{unit_id}"` (`Display` 2-hex type / 5-hex id, e.g.
        /// `"07:11111"`) → register id → typed DTO (or nested zone map / raw
        /// hex).
        units: BTreeMap<String, BTreeMap<String, Value>>,
    },
    /// Incremental register change notification for one unit.
    Event {
        /// Unit type (2-hex lowercase).
        unit_type: String,
        /// Unit id (5-hex lowercase).
        unit_id: String,
        /// Register id (2-hex lowercase).
        register: String,
        /// Zone id for zone-bearing registers (`03`/`04`).
        #[serde(skip_serializing_if = "Option::is_none")]
        zone: Option<u8>,
        /// Typed register payload (or raw 14-char hex for unknown regs).
        payload: Value,
    },
    /// Reply to a client `Read` request.
    ReadResult {
        /// Client correlation id.
        msg_id: String,
        /// Unit type (2-hex lowercase).
        unit_type: String,
        /// Unit id (5-hex lowercase).
        unit_id: String,
        /// Register id (2-hex lowercase).
        register: String,
        /// Zone id for zone-bearing registers (`03`/`04`).
        #[serde(skip_serializing_if = "Option::is_none")]
        zone: Option<u8>,
        /// Typed register payload (or raw 14-char hex for unknown regs).
        payload: Value,
    },
    /// Response to a client `Write` / `Command`.
    Ack {
        /// Client correlation id.
        msg_id: String,
        /// `success` or `error`.
        status: AckStatus,
        /// Optional human-readable reason on error (or info).
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Protocol / broker link-state change.
    Status {
        /// Link state.
        state: StatusState,
        /// Optional detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Protocol / transport error not tied to a `msg_id`.
    Error {
        /// Error summary.
        message: String,
        /// Optional detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Messages sent from the client to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)] // `serde_json::Value` is not `Eq`.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Write a register payload to a unit.
    Write {
        /// Client correlation id.
        msg_id: String,
        /// Unit type (2-hex lowercase); defaults to the primary unit.
        #[serde(skip_serializing_if = "Option::is_none")]
        unit_type: Option<String>,
        /// Unit id (5-hex lowercase); defaults to the primary unit.
        #[serde(skip_serializing_if = "Option::is_none")]
        unit_id: Option<String>,
        /// Register id (2-hex lowercase).
        register: String,
        /// Zone id for zone-bearing registers (`03`/`04`).
        #[serde(skip_serializing_if = "Option::is_none")]
        zone: Option<u8>,
        /// Typed register payload (or raw 14-char hex for unknown regs).
        payload: Value,
    },
    /// Read a register from a unit.
    Read {
        /// Client correlation id.
        msg_id: String,
        /// Unit type (2-hex lowercase); defaults to the primary unit.
        #[serde(skip_serializing_if = "Option::is_none")]
        unit_type: Option<String>,
        /// Unit id (5-hex lowercase); defaults to the primary unit.
        #[serde(skip_serializing_if = "Option::is_none")]
        unit_id: Option<String>,
        /// Register id (2-hex lowercase).
        register: String,
        /// Zone id for zone-bearing registers (`03`/`04`).
        #[serde(skip_serializing_if = "Option::is_none")]
        zone: Option<u8>,
    },
    /// Non-register command (`resync`).
    Command {
        /// Client correlation id.
        msg_id: String,
        /// Action name.
        action: String,
    },
}
