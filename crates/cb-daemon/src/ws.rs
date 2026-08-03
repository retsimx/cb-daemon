//! Axum WebSocket bridge: single-session gate + JSON ↔ engine cmds/events.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aa_engine::{EngineCmd, EngineEvent};
use aa_mailbox::{
    AckStatus, ClientMessage, ServerMessage, records_from_update_with_bank,
    snapshot_from_bank_with_can_records, system_status_to_dto, zone_config_to_dto,
    zone_dto_from_state,
};
use aa_registers::{DecodedRegister, RegisterBank, UnitId, UnitType};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::mock_feeder::FEEDER_UNIT_ID;

/// Close code when a second client tries to open `/v1/mailbox-stream`.
pub(crate) const SINGLE_CLIENT_CLOSE_CODE: u16 = 4009;
/// Close reason for the single-session gate.
pub(crate) const SINGLE_CLIENT_CLOSE_REASON: &str = "Single client limit enforced";

/// Held engine snapshot for late WebSocket clients (bank + CB dump hex).
#[derive(Debug, Clone)]
pub(crate) struct HeldSnapshot {
    pub bank: RegisterBank,
    /// CB dump `can_records` for `MyAir5` `rawCan` (excludes synthesized regs).
    pub can_records: Option<Vec<String>>,
}

/// Shared state for the axum router.
#[derive(Clone)]
pub(crate) struct WsState {
    /// Engine command sender (bound 32 upstream).
    pub cmd_tx: mpsc::Sender<EngineCmd>,
    /// Latest dump/resync snapshot (`None` until first [`EngineEvent::Snapshot`]).
    pub snapshot: watch::Receiver<Option<HeldSnapshot>>,
    /// Fan-out of non-snapshot engine events to the active session.
    pub events: broadcast::Sender<EngineEvent>,
    /// Single-session gate (`true` while a client holds the slot).
    pub session_held: Arc<AtomicBool>,
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

async fn handle_socket(mut socket: WebSocket, state: WsState) {
    if state
        .session_held
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        info!("rejecting second WebSocket client with {SINGLE_CLIENT_CLOSE_CODE}");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: SINGLE_CLIENT_CLOSE_CODE,
                reason: SINGLE_CLIENT_CLOSE_REASON.into(),
            })))
            .await;
        let _ = socket.close().await;
        return;
    }

    let result = run_session(socket, state.clone()).await;
    state.session_held.store(false, Ordering::SeqCst);
    if let Err(err) = result {
        debug!(?err, "mailbox-stream session ended");
    }
}

async fn run_session(mut socket: WebSocket, state: WsState) -> anyhow::Result<()> {
    let held = wait_for_snapshot(&mut socket, state.snapshot.clone()).await?;
    let unit_id = resolve_unit_id(&held.bank, state.unit_id_hint);
    info!(%unit_id, "mailbox snapshot unit_id");
    let snap = snapshot_from_bank_with_can_records(
        &held.bank,
        UnitType::AIRCON,
        unit_id,
        held.can_records,
    );
    send_json(&mut socket, &snap).await?;
    bridge_until_disconnect(&mut socket, &state).await
}

/// Wait for the first engine Snapshot, aborting if the client disconnects.
///
/// Releases the session gate promptly when the holder drops before dump completes
/// (otherwise `session_held` would stick until a Snapshot arrives — or forever).
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

/// Per-session `mailbox_update` ack state: acks are deferred until the engine
/// confirms the write was transmitted ([`EngineEvent::WriteFlushed`]), so a
/// success ack never lies when the bus is dead. FIFO: aaservice serializes
/// outbound actions, so at most one write is in flight at a time.
struct PendingAcks {
    queue: std::collections::VecDeque<(String, tokio::time::Instant)>,
    timeout: std::time::Duration,
}

impl PendingAcks {
    const fn new() -> Self {
        Self {
            queue: std::collections::VecDeque::new(),
            timeout: std::time::Duration::from_secs(10),
        }
    }

    fn push(&mut self, msg_id: String) {
        self.queue
            .push_back((msg_id, tokio::time::Instant::now() + self.timeout));
    }

    /// Pop the oldest pending ack as a success (write was flushed).
    fn pop_front(&mut self) -> Option<String> {
        self.queue.pop_front().map(|(msg_id, _)| msg_id)
    }

    /// Pop the oldest pending ack only when its deadline has passed.
    fn pop_expired(&mut self) -> Option<String> {
        let expired = self
            .queue
            .front()
            .is_some_and(|(_, deadline)| tokio::time::Instant::now() >= *deadline);
        if expired {
            return self.queue.pop_front().map(|(msg_id, _)| msg_id);
        }
        None
    }
}

async fn bridge_until_disconnect(socket: &mut WebSocket, state: &WsState) -> anyhow::Result<()> {
    let mut ev_rx = state.events.subscribe();
    let mut pending = PendingAcks::new();
    loop {
        // Expire stale write acks so a dead bus surfaces as an error ack.
        if let Some(msg_id) = pending.pop_expired() {
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
            // USB parity: forward non-empty steady getCANs as a rawCan frame
            // (MyAir5 secure rawCan), plus the typed DTO events for the mapper.
            if !records.is_empty() {
                let mut payload = String::from("getCAN 1");
                for record in &records {
                    payload.push(' ');
                    payload.push_str(&record.to_wire());
                }
                send_json(socket, &ServerMessage::RawCan { payload }).await?;
            }
            for msg in mailbox_events_from_records(&records) {
                send_json(socket, &msg).await?;
            }
            Ok(true)
        }
        Ok(EngineEvent::Snapshot { bank, can_records }) => {
            let unit_id = resolve_unit_id(&bank, state.unit_id_hint);
            let snap =
                snapshot_from_bank_with_can_records(&bank, UnitType::AIRCON, unit_id, can_records);
            send_json(socket, &snap).await?;
            Ok(true)
        }
        Ok(EngineEvent::WriteFlushed) => {
            if let Some(msg_id) = pending.pop_front() {
                send_ack(socket, &msg_id, AckStatus::Success, None).await?;
            }
            Ok(true)
        }
        Ok(EngineEvent::DirectReply { payload }) => {
            let text = String::from_utf8_lossy(&payload).into_owned();
            send_json(socket, &ServerMessage::DirectReply { payload: text }).await?;
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
        Ok(ClientMessage::MailboxUpdate {
            msg_id,
            register,
            payload,
        }) => {
            handle_mailbox_update(socket, state, pending, &msg_id, &register, &payload).await?;
        }
        Ok(ClientMessage::WriteCan { msg_id, tokens }) => {
            handle_write_can(socket, state, pending, &msg_id, &tokens).await?;
        }
        Ok(ClientMessage::Direct { msg_id, payload }) => {
            handle_direct(socket, state, pending, &msg_id, &payload).await?;
        }
        Ok(ClientMessage::Command { msg_id, action }) => {
            if action == "resync_mailbox" {
                let cmd = EngineCmd::ResyncMailbox;
                record_spy(state, &cmd).await;
                if let Err(err) = state.cmd_tx.send(cmd).await {
                    send_ack(socket, &msg_id, AckStatus::Error, Some(err.to_string())).await?;
                } else {
                    send_ack(socket, &msg_id, AckStatus::Success, None).await?;
                }
            } else {
                send_ack(
                    socket,
                    &msg_id,
                    AckStatus::Error,
                    Some(format!("unknown action: {action}")),
                )
                .await?;
            }
        }
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

/// `mailbox_update`: sparse register write merged over the held bank.
/// Ack is deferred until the engine confirms the frame was transmitted.
async fn handle_mailbox_update(
    socket: &mut WebSocket,
    state: &WsState,
    pending: &mut PendingAcks,
    msg_id: &str,
    register: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    let unit_id = state.snapshot.borrow().as_ref().map_or_else(
        || state.unit_id_hint.unwrap_or(FEEDER_UNIT_ID),
        |held| resolve_unit_id(&held.bank, state.unit_id_hint),
    );
    let held_bank = state
        .snapshot
        .borrow()
        .as_ref()
        .map(|held| held.bank.clone())
        .unwrap_or_default();
    match records_from_update_with_bank(&held_bank, UnitType::AIRCON, unit_id, register, payload) {
        Ok(records) => {
            let cmd = EngineCmd::WriteRegisters(records);
            record_spy(state, &cmd).await;
            if let Err(err) = state.cmd_tx.send(cmd).await {
                send_ack(socket, msg_id, AckStatus::Error, Some(err.to_string())).await?;
            } else {
                pending.push(msg_id.to_owned());
            }
        }
        Err(err) => {
            send_ack(socket, msg_id, AckStatus::Error, Some(err.to_string())).await?;
        }
    }
    Ok(())
}

/// `write_can`: raw 25-char CAN2 tokens forwarded as register writes
/// (`MyAir5` `CAN_TO_CB` / `BROADCAST_CAN_TO_CB` parity).
async fn handle_write_can(
    socket: &mut WebSocket,
    state: &WsState,
    pending: &mut PendingAcks,
    msg_id: &str,
    tokens: &[String],
) -> anyhow::Result<()> {
    let mut records = Vec::with_capacity(tokens.len());
    let mut first_err: Option<aa_registers::WireError> = None;
    for token in tokens {
        match aa_registers::CanRecord::parse_one(token) {
            Ok(record) => records.push(record),
            Err(err) => {
                first_err.get_or_insert(err);
            }
        }
    }
    if let Some(err) = first_err {
        send_ack(
            socket,
            msg_id,
            AckStatus::Error,
            Some(format!("invalid CAN token: {err:?}")),
        )
        .await?;
    } else if records.is_empty() {
        send_ack(
            socket,
            msg_id,
            AckStatus::Error,
            Some("no CAN tokens".into()),
        )
        .await?;
    } else {
        let cmd = EngineCmd::WriteRegisters(records);
        record_spy(state, &cmd).await;
        if let Err(err) = state.cmd_tx.send(cmd).await {
            send_ack(socket, msg_id, AckStatus::Error, Some(err.to_string())).await?;
        } else {
            pending.push(msg_id.to_owned());
        }
    }
    Ok(())
}

/// `direct`: one-shot raw request (poll tag / `setAllZoneSensorData?`); the CB
/// reply is delivered as [`ServerMessage::DirectReply`].
async fn handle_direct(
    socket: &mut WebSocket,
    state: &WsState,
    pending: &mut PendingAcks,
    msg_id: &str,
    payload: &str,
) -> anyhow::Result<()> {
    let cmd = EngineCmd::WriteDirect(payload.as_bytes().to_vec());
    record_spy(state, &cmd).await;
    if let Err(err) = state.cmd_tx.send(cmd).await {
        send_ack(socket, msg_id, AckStatus::Error, Some(err.to_string())).await?;
    } else {
        pending.push(msg_id.to_owned());
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

/// Daemon-local map of changed registers → `mailbox_event` messages.
fn mailbox_events_from_records(records: &[aa_registers::CanRecord]) -> Vec<ServerMessage> {
    let mut out = Vec::with_capacity(records.len());
    for record in records {
        match record.decode() {
            DecodedRegister::SystemStatus(status) => {
                if let Ok(payload) = serde_json::to_value(system_status_to_dto(&status)) {
                    out.push(ServerMessage::MailboxEvent {
                        register: "system_status".into(),
                        payload,
                    });
                }
            }
            DecodedRegister::ZoneState(state) => {
                if let Ok(mut payload) = serde_json::to_value(zone_dto_from_state(&state)) {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("zone_id".into(), json!(state.zone.to_string()));
                    }
                    out.push(ServerMessage::MailboxEvent {
                        register: "zone_state".into(),
                        payload,
                    });
                }
            }
            DecodedRegister::ZoneConfig(cfg) => {
                if let Ok(payload) = serde_json::to_value(zone_config_to_dto(&cfg)) {
                    out.push(ServerMessage::MailboxEvent {
                        register: "zone_config".into(),
                        payload,
                    });
                }
            }
            other => {
                out.push(ServerMessage::MailboxEvent {
                    register: format!("{:02x}", other.reg_id().get()),
                    payload: json!({ "opaque": true }),
                });
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_registers::{CanRecord, Dest, RegId, UnitType};

    #[test]
    fn resolve_unit_id_prefers_live_dump_over_feeder_default() {
        // Regression: mailbox snapshot used hardcoded abcde while AOA dump was 181f3.
        let mut bank = RegisterBank::new();
        let live = UnitId::try_new(0x0_181F3).unwrap();
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
