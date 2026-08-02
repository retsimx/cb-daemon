//! Server- and client-directed mailbox WebSocket message enums.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dto::{AckStatus, SnapshotBody, SystemStatusDto, ZoneConfigDto, ZoneDto};

/// Messages sent from the daemon to the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Full unit mailbox dump after connect / resync.
    MailboxSnapshot {
        /// Lowercase hex unit id.
        unit_id: String,
        /// System status when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        system_status: Option<SystemStatusDto>,
        /// Zone config when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        zone_config: Option<ZoneConfigDto>,
        /// Zones keyed by decimal zone id string.
        #[serde(skip_serializing_if = "Option::is_none")]
        zones: Option<std::collections::BTreeMap<String, ZoneDto>>,
    },
    /// Incremental register change notification.
    MailboxEvent {
        /// Register JSON key (`system_status`, `zone_state`, …).
        register: String,
        /// Event payload (typed or sparse).
        payload: Value,
    },
    /// Response to a client `mailbox_update` / `command`.
    Ack {
        /// Client correlation id.
        msg_id: String,
        /// `success` or `error`.
        status: AckStatus,
        /// Optional human-readable reason on error (or info).
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
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

impl ServerMessage {
    /// Build a snapshot message from a [`SnapshotBody`].
    #[must_use]
    pub fn from_snapshot_body(body: SnapshotBody) -> Self {
        Self::MailboxSnapshot {
            unit_id: body.unit_id,
            system_status: body.system_status,
            zone_config: body.zone_config,
            zones: body.zones,
        }
    }
}

/// Messages sent from the client to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)] // `serde_json::Value` is not `Eq`.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Request to write a register from JSON payload.
    MailboxUpdate {
        /// Client correlation id.
        msg_id: String,
        /// Register JSON key.
        register: String,
        /// Register payload.
        payload: Value,
    },
    /// Non-register command (e.g. `resync_mailbox`).
    Command {
        /// Client correlation id.
        msg_id: String,
        /// Action name.
        action: String,
    },
}
