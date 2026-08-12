//! Axum WebSocket bridge: multi-consumer sessions + JSON ↔ engine cmds/events.

use std::collections::BTreeMap;
use std::sync::Arc;

use aa_engine::{EngineCmd, EngineEvent};
use aa_mailbox::{
    AckStatus, ClientMessage, ServerMessage, UnitSnapshot, decode_payload, encode_payload,
};
use aa_registers::{CanRecord, Dest, RegId, RegisterBank, UnitId, UnitType, is_zone_bearing};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::mock_feeder::FEEDER_UNIT_ID;

/// Held engine snapshot for late WebSocket clients (bank).
#[derive(Debug, Clone)]
pub(crate) struct HeldSnapshot {
    pub bank: RegisterBank,
}

/// Shared state for the axum router.
#[derive(Clone)]
pub(crate) struct WsState {
    /// Engine command sender (bound 32 upstream).
    pub cmd_tx: mpsc::Sender<EngineCmd>,
    /// Latest dump/resync snapshot (`None` until first [`EngineEvent::Snapshot`]).
    pub snapshot: watch::Receiver<Option<HeldSnapshot>>,
    /// Fan-out of non-snapshot engine events to every connected session.
    pub events: broadcast::Sender<EngineEvent>,
    /// Optional spy for tests (records cmds accepted from WS).
    pub cmd_spy: Option<Arc<tokio::sync::Mutex<Vec<EngineCmd>>>>,
    /// Config `unit_id_hint` (preferred when present in the bank).
    pub unit_id_hint: Option<UnitId>,
}

fn resolve_unit_id(bank: &RegisterBank, hint: Option<UnitId>) -> UnitId {
    let id = bank.preferred_unit_id(UnitType::AIRCON, hint);
    if id == UnitId::ZERO {
        // Mock feeder / empty bank fallback.
        hint.unwrap_or(FEEDER_UNIT_ID)
    } else {
        id
    }
}

/// Build the mailbox `Snapshot` for one (primary) unit from the held bank.
///
/// Non-zone registers decode to typed DTOs (or raw 14-char hex for unknown
/// registers); zone-bearing registers (`03`/`04`) are nested zone → DTO maps.
/// Registers with no bank slot are skipped.
fn snapshot_message(bank: &RegisterBank, unit_type: UnitType, unit_id: UnitId) -> ServerMessage {
    let mut registers = BTreeMap::new();
    for record in bank.records_for_unit(unit_type, unit_id) {
        if is_zone_bearing(record.reg) {
            continue;
        }
        match decode_payload(record.reg, record.data) {
            Ok(payload) => {
                registers.insert(format!("{:02x}", record.reg.get()), payload);
            }
            Err(err) => debug!(%err, "snapshot: skipping undecodable register"),
        }
    }
    for reg_id in [0x03, 0x04] {
        let reg = RegId::new(reg_id);
        let mut zones: BTreeMap<String, Value> = BTreeMap::new();
        for zone in 1..=10u8 {
            if let Some(data) = bank.get_zone(unit_type, unit_id, reg, zone)
                && let Ok(payload) = decode_payload(reg, data)
            {
                zones.insert(zone.to_string(), payload);
            }
        }
        if !zones.is_empty() {
            registers.insert(
                format!("{reg_id:02x}"),
                Value::Object(zones.into_iter().collect()),
            );
        }
    }
    ServerMessage::Snapshot {
        units: vec![UnitSnapshot {
            unit_type: unit_type.to_string(),
            unit_id: unit_id.to_string(),
            registers,
        }],
    }
}

/// Build the axum router with `GET /v1/mailbox-stream`.
pub(crate) fn router(state: WsState) -> Router {
    Router::new()
        .route("/v1/mailbox-stream", get(mailbox_stream_upgrade))
        .with_state(state)
}

async fn mailbox_stream_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Run one independent session per connection (no single-session gate).
async fn handle_socket(socket: WebSocket, state: WsState) {
    let result = run_session(socket, state).await;
    if let Err(err) = result {
        debug!(?err, "mailbox-stream session ended");
    }
}

async fn run_session(mut socket: WebSocket, state: WsState) -> anyhow::Result<()> {
    let held = wait_for_snapshot(&mut socket, state.snapshot.clone()).await?;
    let unit_id = resolve_unit_id(&held.bank, state.unit_id_hint);
    info!(%unit_id, "mailbox snapshot unit_id");
    let snap = snapshot_message(&held.bank, UnitType::AIRCON, unit_id);
    send_json(&mut socket, &snap).await?;
    bridge_until_disconnect(&mut socket, &state).await
}

/// Wait for the first engine Snapshot, aborting if the client disconnects.
///
/// Aborts promptly when the holder drops before dump completes: no bank is ever
/// published, so a waiting session would hang forever otherwise.
async fn wait_for_snapshot(
    socket: &mut WebSocket,
    mut rx: watch::Receiver<Option<HeldSnapshot>>,
) -> anyhow::Result<HeldSnapshot> {
    loop {
        let current = rx.borrow_and_update().clone();
        if let Some(held) = current {
            return Ok(held);
        }
        tokio::select! {
            changed = rx.changed() => {
                changed?;
            }
            msg = socket.next() => {
                match msg {
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = socket.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        anyhow::bail!("client disconnected while waiting for snapshot");
                    }
                    Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                        debug!("ignoring client frame while waiting for snapshot");
                    }
                    Some(Err(err)) => {
                        return Err(err.into());
                    }
                }
            }
        }
    }
}

/// Per-session write ack state: acks are deferred until the engine
/// confirms the write was transmitted ([`EngineEvent::WriteFlushed`]), so a
/// success ack never lies when the bus is dead. The engine batches the entire
/// write queue into one CAN TX and emits exactly one `WriteFlushed` per TX, so
/// every pending `msg_id` since the last flush belongs to that TX and is acked
/// together.
struct PendingAcks {
    deadlines: BTreeMap<String, tokio::time::Instant>,
    timeout: std::time::Duration,
}

impl PendingAcks {
    const fn new() -> Self {
        Self {
            deadlines: BTreeMap::new(),
            timeout: std::time::Duration::from_secs(10),
        }
    }

    /// Track `msg_id` with a 10s deadline; acked on [`EngineEvent::WriteFlushed`].
    fn push(&mut self, msg_id: String) {
        self.deadlines
            .insert(msg_id, tokio::time::Instant::now() + self.timeout);
    }

    /// Drain every pending `msg_id` as a success (writes were flushed).
    fn drain_all(&mut self) -> Vec<String> {
        std::mem::take(&mut self.deadlines).into_keys().collect()
    }

    /// Remove and return every `msg_id` whose deadline has passed.
    fn drain_expired(&mut self) -> Vec<String> {
        let now = tokio::time::Instant::now();
        let expired: Vec<String> = self
            .deadlines
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|(msg_id, _)| msg_id.clone())
            .collect();
        for msg_id in &expired {
            self.deadlines.remove(msg_id);
        }
        expired
    }
}

async fn bridge_until_disconnect(socket: &mut WebSocket, state: &WsState) -> anyhow::Result<()> {
    let mut ev_rx = state.events.subscribe();
    let mut pending = PendingAcks::new();
    loop {
        // Expire stale write acks so a dead bus surfaces as an error ack.
        for msg_id in pending.drain_expired() {
            send_ack(
                socket,
                &msg_id,
                AckStatus::Error,
                Some("write timeout".into()),
            )
            .await?;
        }
        tokio::select! {
            msg = socket.next() => {
                if !handle_ws_message(socket, state, msg, &mut pending).await? {
                    break;
                }
            }
            ev = ev_rx.recv() => {
                if !forward_engine_event(socket, state, ev, &mut pending).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Returns `false` when the session should end.
async fn handle_ws_message(
    socket: &mut WebSocket,
    state: &WsState,
    msg: Option<Result<Message, axum::Error>>,
    pending: &mut PendingAcks,
) -> anyhow::Result<bool> {
    match msg {
        Some(Ok(Message::Text(text))) => {
            handle_client_text(socket, state, &text, pending).await?;
            Ok(true)
        }
        Some(Ok(Message::Ping(payload))) => {
            let _ = socket.send(Message::Pong(payload)).await;
            Ok(true)
        }
        Some(Ok(Message::Close(_))) | None => {
            info!("mailbox-stream client disconnected");
            Ok(false)
        }
        Some(Ok(Message::Binary(_))) => {
            let err = ServerMessage::Error {
                message: "binary frames not supported".into(),
                reason: None,
            };
            send_json(socket, &err).await?;
            Ok(true)
        }
        Some(Ok(Message::Pong(_))) => Ok(true),
        Some(Err(err)) => {
            warn!(?err, "websocket receive error");
            Ok(false)
        }
    }
}

/// Returns `false` when the session should end.
async fn forward_engine_event(
    socket: &mut WebSocket,
    state: &WsState,
    ev: Result<EngineEvent, broadcast::error::RecvError>,
    pending: &mut PendingAcks,
) -> anyhow::Result<bool> {
    match ev {
        Ok(EngineEvent::RegistersChanged { records }) => {
            for record in records {
                match decode_payload(record.reg, record.data) {
                    Ok(payload) => {
                        let event = ServerMessage::Event {
                            unit_type: record.unit_type.to_string(),
                            unit_id: record.unit_id.to_string(),
                            register: format!("{:02x}", record.reg.get()),
                            zone: is_zone_bearing(record.reg).then_some(record.data[0]),
                            payload,
                        };
                        send_json(socket, &event).await?;
                    }
                    Err(err) => debug!(%err, "event: skipping undecodable register"),
                }
            }
            Ok(true)
        }
        Ok(EngineEvent::Snapshot { bank, .. }) => {
            let unit_id = resolve_unit_id(&bank, state.unit_id_hint);
            let snap = snapshot_message(&bank, UnitType::AIRCON, unit_id);
            send_json(socket, &snap).await?;
            Ok(true)
        }
        Ok(EngineEvent::WriteFlushed) => {
            for msg_id in pending.drain_all() {
                send_ack(socket, &msg_id, AckStatus::Success, None).await?;
            }
            Ok(true)
        }
        Ok(other) => {
            debug!(?other, "engine event (not forwarded to WS)");
            Ok(true)
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            warn!(n, "event broadcast lagged");
            Ok(true)
        }
        Err(broadcast::error::RecvError::Closed) => Ok(false),
    }
}

async fn handle_client_text(
    socket: &mut WebSocket,
    state: &WsState,
    text: &str,
    pending: &mut PendingAcks,
) -> anyhow::Result<()> {
    let parsed: Result<ClientMessage, _> = serde_json::from_str(text);
    match parsed {
        Ok(ClientMessage::Write {
            msg_id,
            unit_type,
            unit_id,
            register,
            zone,
            payload,
        }) => {
            let req = WriteRequest {
                msg_id,
                unit_type,
                unit_id,
                register,
                zone,
                payload,
            };
            handle_write(socket, state, pending, req).await?;
        }
        Ok(ClientMessage::Read { msg_id, .. }) => {
            send_ack(
                socket,
                &msg_id,
                AckStatus::Error,
                Some("read not implemented yet".into()),
            )
            .await?;
        }
        Ok(ClientMessage::Command { msg_id, action }) => match action.as_str() {
            "resync" => {
                let cmd = EngineCmd::ResyncMailbox;
                record_spy(state, &cmd).await;
                if let Err(err) = state.cmd_tx.send(cmd).await {
                    send_ack(socket, &msg_id, AckStatus::Error, Some(err.to_string())).await?;
                } else {
                    send_ack(socket, &msg_id, AckStatus::Success, None).await?;
                }
            }
            "flush_unit" => {
                send_ack(
                    socket,
                    &msg_id,
                    AckStatus::Error,
                    Some("flush_unit not implemented yet".into()),
                )
                .await?;
            }
            other => {
                send_ack(
                    socket,
                    &msg_id,
                    AckStatus::Error,
                    Some(format!("unknown action: {other}")),
                )
                .await?;
            }
        },
        Err(err) => {
            let msg = ServerMessage::Error {
                message: "invalid client message".into(),
                reason: Some(err.to_string()),
            };
            send_json(socket, &msg).await?;
        }
    }
    Ok(())
}

/// Decoded `write` fields for [`handle_write`].
struct WriteRequest {
    msg_id: String,
    unit_type: Option<String>,
    unit_id: Option<String>,
    register: String,
    /// Zone id for zone-bearing registers (`03`/`04`); part of the CAN address.
    zone: Option<u8>,
    payload: Value,
}

/// `write`: encode the register payload and queue it as a single register
/// write. Ack is deferred until the engine confirms the frame was transmitted.
async fn handle_write(
    socket: &mut WebSocket,
    state: &WsState,
    pending: &mut PendingAcks,
    msg: WriteRequest,
) -> anyhow::Result<()> {
    let resolved_id = state.snapshot.borrow().as_ref().map_or_else(
        || state.unit_id_hint.unwrap_or(FEEDER_UNIT_ID),
        |held| resolve_unit_id(&held.bank, state.unit_id_hint),
    );
    let unit_type = match msg.unit_type.map(|s| UnitType::from_hex(&s)).transpose() {
        Ok(Some(unit_type)) => unit_type,
        Ok(None) => UnitType::AIRCON,
        Err(err) => {
            send_ack(
                socket,
                &msg.msg_id,
                AckStatus::Error,
                Some(format!("invalid unit_type: {err:?}")),
            )
            .await?;
            return Ok(());
        }
    };
    let unit_id = match msg.unit_id.map(|s| UnitId::from_hex(&s)).transpose() {
        Ok(Some(unit_id)) => unit_id,
        Ok(None) => resolved_id,
        Err(err) => {
            send_ack(
                socket,
                &msg.msg_id,
                AckStatus::Error,
                Some(format!("invalid unit_id: {err:?}")),
            )
            .await?;
            return Ok(());
        }
    };
    let reg = match RegId::from_hex(&msg.register) {
        Ok(reg) => reg,
        Err(err) => {
            send_ack(
                socket,
                &msg.msg_id,
                AckStatus::Error,
                Some(format!("invalid register: {err:?}")),
            )
            .await?;
            return Ok(());
        }
    };
    let mut data = match encode_payload(reg, &msg.payload) {
        Ok(data) => data,
        Err(err) => {
            send_ack(socket, &msg.msg_id, AckStatus::Error, Some(err.to_string())).await?;
            return Ok(());
        }
    };
    // The zone id is part of the CAN address, not the payload: the codec stamps
    // wire byte 0 as 0x00, so a zone-bearing write (regs 03/04) addressed by
    // the client must be stamped here to reach the addressed zone.
    if is_zone_bearing(reg)
        && let Some(zone) = msg.zone
    {
        data[0] = zone;
    }
    let record = CanRecord {
        unit_type,
        dest: Dest::ControlBox,
        unit_id,
        reg,
        data,
    };
    let cmd = EngineCmd::WriteRegisters(vec![record]);
    record_spy(state, &cmd).await;
    if let Err(err) = state.cmd_tx.send(cmd).await {
        send_ack(socket, &msg.msg_id, AckStatus::Error, Some(err.to_string())).await?;
    } else {
        pending.push(msg.msg_id.clone());
    }
    Ok(())
}

async fn record_spy(state: &WsState, cmd: &EngineCmd) {
    if let Some(spy) = &state.cmd_spy {
        let mut g = spy.lock().await;
        g.push(cmd.clone());
    }
}

async fn send_ack(
    socket: &mut WebSocket,
    msg_id: &str,
    status: AckStatus,
    reason: Option<String>,
) -> anyhow::Result<()> {
    send_json(
        socket,
        &ServerMessage::Ack {
            msg_id: msg_id.to_owned(),
            status,
            reason,
        },
    )
    .await
}

async fn send_json(socket: &mut WebSocket, msg: &ServerMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(msg)?;
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_registers::{CanRecord, Dest, RegId, UnitType};

    #[test]
    fn resolve_unit_id_prefers_live_dump_over_feeder_default() {
        // Regression: mailbox snapshot used hardcoded abcde while AOA dump was 11111.
        let mut bank = RegisterBank::new();
        let live = UnitId::try_new(0x0_11111).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: live,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        assert_eq!(resolve_unit_id(&bank, Some(live)), live);
        assert_eq!(resolve_unit_id(&bank, None), live);
        // Empty bank still falls back to feeder id (or hint).
        let empty = RegisterBank::new();
        assert_eq!(resolve_unit_id(&empty, None), FEEDER_UNIT_ID);
        assert_eq!(resolve_unit_id(&empty, Some(live)), live);
    }
}
