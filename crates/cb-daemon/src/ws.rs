//! Axum WebSocket bridge: single-session gate + JSON ↔ engine cmds/events.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aa_engine::{EngineCmd, EngineEvent};
use aa_mailbox::{
    AckStatus, ClientMessage, ServerMessage, records_from_update, snapshot_from_bank,
    system_status_to_dto, zone_config_to_dto, zone_dto_from_state,
};
use aa_registers::{DecodedRegister, RegisterBank, UnitType};
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

/// Shared state for the axum router.
#[derive(Clone)]
pub(crate) struct WsState {
    /// Engine command sender (bound 32 upstream).
    pub cmd_tx: mpsc::Sender<EngineCmd>,
    /// Latest dump/resync bank (`None` until first [`EngineEvent::Snapshot`]).
    pub snapshot: watch::Receiver<Option<RegisterBank>>,
    /// Fan-out of non-snapshot engine events to the active session.
    pub events: broadcast::Sender<EngineEvent>,
    /// Single-session gate (`true` while a client holds the slot).
    pub session_held: Arc<AtomicBool>,
    /// Optional spy for tests (records cmds accepted from WS).
    pub cmd_spy: Option<Arc<tokio::sync::Mutex<Vec<EngineCmd>>>>,
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
    let bank = wait_for_snapshot(&mut socket, state.snapshot.clone()).await?;
    let snap = snapshot_from_bank(&bank, UnitType::AIRCON, FEEDER_UNIT_ID);
    send_json(&mut socket, &snap).await?;
    bridge_until_disconnect(&mut socket, &state).await
}

/// Wait for the first engine Snapshot, aborting if the client disconnects.
///
/// Releases the session gate promptly when the holder drops before dump completes
/// (otherwise `session_held` would stick until a Snapshot arrives — or forever).
async fn wait_for_snapshot(
    socket: &mut WebSocket,
    mut rx: watch::Receiver<Option<RegisterBank>>,
) -> anyhow::Result<RegisterBank> {
    loop {
        let current = rx.borrow_and_update().clone();
        if let Some(bank) = current {
            return Ok(bank);
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

async fn bridge_until_disconnect(socket: &mut WebSocket, state: &WsState) -> anyhow::Result<()> {
    let mut ev_rx = state.events.subscribe();
    loop {
        tokio::select! {
            msg = socket.next() => {
                if !handle_ws_message(socket, state, msg).await? {
                    break;
                }
            }
            ev = ev_rx.recv() => {
                if !forward_engine_event(socket, ev).await? {
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
) -> anyhow::Result<bool> {
    match msg {
        Some(Ok(Message::Text(text))) => {
            handle_client_text(socket, state, &text).await?;
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
    ev: Result<EngineEvent, broadcast::error::RecvError>,
) -> anyhow::Result<bool> {
    match ev {
        Ok(EngineEvent::RegistersChanged { records }) => {
            for msg in mailbox_events_from_records(&records) {
                send_json(socket, &msg).await?;
            }
            Ok(true)
        }
        Ok(EngineEvent::Snapshot(bank)) => {
            let snap = snapshot_from_bank(&bank, UnitType::AIRCON, FEEDER_UNIT_ID);
            send_json(socket, &snap).await?;
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
) -> anyhow::Result<()> {
    let parsed: Result<ClientMessage, _> = serde_json::from_str(text);
    match parsed {
        Ok(ClientMessage::MailboxUpdate {
            msg_id,
            register,
            payload,
        }) => match records_from_update(UnitType::AIRCON, FEEDER_UNIT_ID, &register, &payload) {
            Ok(records) => {
                let cmd = EngineCmd::WriteRegisters(records);
                record_spy(state, &cmd).await;
                if let Err(err) = state.cmd_tx.send(cmd).await {
                    send_ack(socket, &msg_id, AckStatus::Error, Some(err.to_string())).await?;
                } else {
                    send_ack(socket, &msg_id, AckStatus::Success, None).await?;
                }
            }
            Err(err) => {
                send_ack(socket, &msg_id, AckStatus::Error, Some(err.to_string())).await?;
            }
        },
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
