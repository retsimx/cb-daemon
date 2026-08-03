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
        /// Opaque 25-char hex CAN records for this unit (full bank dump).
        #[serde(skip_serializing_if = "Option::is_none")]
        can_records: Option<Vec<String>>,
    },
    /// Incremental register change notification.
    MailboxEvent {
        /// Register JSON key (`system_status`, `zone_state`, …).
        register: String,
        /// Event payload (typed or sparse).
        payload: Value,
    },
    /// Raw steady-state `getCAN` frame forwarded verbatim (USB `rawCan` parity):
    /// `getCAN 1 <25-char records…>`. `MyAir5` receives these as secure rawCan.
    RawCan {
        /// Full getCAN payload.
        payload: String,
    },
    /// Reply to a client `direct` request (one-shot poll / raw command).
    /// The CB's reply payload (typically XML with a `<request>` tag).
    DirectReply {
        /// Raw CB reply payload.
        payload: String,
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
            can_records: body.can_records,
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
    /// Forward raw CAN2 tokens (25-char hex records) from `MyAir5` `CAN_TO_CB` /
    /// `BROADCAST_CAN_TO_CB` intents (sensor pairing, unit flushes, …).
    WriteCan {
        /// Client correlation id.
        msg_id: String,
        /// Wire token strings (25-char hex records).
        tokens: Vec<String>,
    },
    /// One-shot raw request to the CB (direct-message queue, USB parity):
    /// payload is the raw command string (e.g. `setAllZoneSensorData?` or a
    /// poll tag such as `getZoneTimer`). The CB reply is delivered as
    /// [`ServerMessage::DirectReply`].
    Direct {
        /// Client correlation id.
        msg_id: String,
        /// Raw command string to write on the next ping.
        payload: String,
    },
}
