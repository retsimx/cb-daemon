//! Axum WebSocket bridge: multi-consumer sessions + JSON ↔ engine cmds/events.

use std::collections::BTreeMap;
use std::sync::Arc;

use aa_engine::{EngineCmd, EngineEvent};
use aa_mailbox::{
    AckStatus, ClientMessage, PolicyMode, ServerMessage, StatusState, decode_payload,
    encode_payload, event_body, snapshot_units, validate_write, write_policy,
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

/// Held engine snapshot for late WebSocket clients (bank).
#[derive(Debug, Clone)]
pub(crate) struct HeldSnapshot {
    pub bank: RegisterBank,
}

/// Fan-out message broadcast to every connected session.
#[derive(Debug, Clone)]
pub(crate) enum WsEvent {
    /// Raw engine event (snapshot, register change, write flush, …).
    Engine(EngineEvent),
    /// Wire `status` frame: session-state transitions + link-down detail.
    Status {
        state: StatusState,
        detail: Option<String>,
    },
}

/// Shared state for the axum router.
#[derive(Clone)]
pub(crate) struct WsState {
    /// Engine command sender (bound 32 upstream).
    pub cmd_tx: mpsc::Sender<EngineCmd>,
    /// Latest dump/resync snapshot (`None` until first [`EngineEvent::Snapshot`]).
    pub snapshot: watch::Receiver<Option<HeldSnapshot>>,
    /// Fan-out of non-snapshot events to every connected session.
    pub events: broadcast::Sender<WsEvent>,
    /// Latest engine session state (`negotiating` before the first snapshot).
    pub status: watch::Receiver<StatusState>,
    /// Optional spy for tests (records cmds accepted from WS).
    pub cmd_spy: Option<Arc<tokio::sync::Mutex<Vec<EngineCmd>>>>,
    /// Config `unit_id_hint` (preferred when present in the bank).
    pub unit_id_hint: Option<UnitId>,
}

/// Map an engine session-state onto the wire `status` state (1:1, D-8).
pub(crate) const fn map_session_state(state: aa_engine::SessionState) -> StatusState {
    match state {
        aa_engine::SessionState::Negotiating => StatusState::Negotiating,
        aa_engine::SessionState::Synced => StatusState::Synced,
        aa_engine::SessionState::Resyncing => StatusState::Resyncing,
        aa_engine::SessionState::LinkDown => StatusState::LinkDown,
    }
}

/// Resolve the primary (`unit_type`, `unit_id`) from the bank.
///
/// A hint present in the bank wins (preferring AIRCON when the id exists under
/// multiple types); else the smallest unit id across all unit types (tie-break
/// by unit type byte); an empty bank with a hint set reports `(AIRCON, hint)`;
/// an empty bank with no hint returns `None` (no primary unit available).
fn primary_unit(bank: &RegisterBank, hint: Option<UnitId>) -> Option<(UnitType, UnitId)> {
    if let Some(hint) = hint {
        let found = if bank.unit_ids(UnitType::AIRCON).contains(&hint) {
            Some(UnitType::AIRCON)
        } else {
            bank.unit_types()
                .into_iter()
                .find(|t| bank.unit_ids(*t).contains(&hint))
        };
        if let Some(unit_type) = found {
            return Some((unit_type, hint));
        }
    }
    let mut best: Option<(UnitType, UnitId)> = None;
    for unit_type in bank.unit_types() {
        let ids = bank.unit_ids(unit_type);
        let Some(first) = ids.first() else {
            continue;
        };
        let candidate = (unit_type, *first);
        best = Some(match best {
            None => candidate,
            Some((best_type, best_id))
                if (candidate.1.get(), candidate.0.get()) < (best_id.get(), best_type.get()) =>
            {
                candidate
            }
            Some(cur) => cur,
        });
    }
    match best {
        Some((unit_type, unit_id)) => Some((unit_type, unit_id)),
        None => hint.map(|hint| (UnitType::AIRCON, hint)),
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

async fn run_session(mut socket: WebSocket, mut state: WsState) -> anyhow::Result<()> {
    // Late clients learn the current health instantly (D-8): send the latest
    // session state on connect, before the snapshot wait. The watch already
    // holds the most recent transition (negotiating until the first dump).
    let connect_state = *state.status.borrow_and_update();
    let status = ServerMessage::Status {
        state: connect_state,
        detail: None,
    };
    send_json(&mut socket, &status).await?;
    let held = wait_for_snapshot(&mut socket, state.snapshot.clone()).await?;

    // Subscribe to the event fan-out BEFORE re-reading the status watch: the
    // broadcast has no history, so a transition that raced the snapshot wait
    // must either arrive on this subscription or be echoed from the watch
    // here. The fanout writes the watch before broadcasting, so exactly one
    // of the two paths is guaranteed to deliver a changed state.
    let ev_rx = state.events.subscribe();
    let current_state = *state.status.borrow_and_update();
    if current_state != connect_state {
        let status = ServerMessage::Status {
            state: current_state,
            detail: None,
        };
        send_json(&mut socket, &status).await?;
    }
    if let Some((_, unit_id)) = primary_unit(&held.bank, state.unit_id_hint) {
        info!(%unit_id, "mailbox snapshot unit_id");
    }
    let snap = ServerMessage::Snapshot {
        units: snapshot_units(&held.bank),
    };
    send_json(&mut socket, &snap).await?;
    bridge_until_disconnect(&mut socket, &state, ev_rx).await
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

/// Read correlation key: one engine [`EngineEvent::RegisterRead`] resolves
/// every pending read that targeted the same unit/register/zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ReadKey {
    unit_type: UnitType,
    unit_id: UnitId,
    reg: RegId,
    zone: Option<u8>,
}

// `aa_registers` ids do not implement `Ord`, so the BTreeMap ordering is
// defined here on the raw wire bytes.
impl Ord for ReadKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.unit_type.get(),
            self.unit_id.get(),
            self.reg.get(),
            self.zone,
        )
            .cmp(&(
                other.unit_type.get(),
                other.unit_id.get(),
                other.reg.get(),
                other.zone,
            ))
    }
}

impl PartialOrd for ReadKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Per-session read state: reads are correlated by [`ReadKey`] and resolved
/// when the flush's `getCAN` emits [`EngineEvent::RegisterRead`]. Multiple
/// `msg_id`s may share one key (each gets its own correlated `read_result`),
/// and each entry carries its own 5s deadline so a register the bus never
/// reports times out with an error ack.
struct PendingReads {
    deadlines: BTreeMap<ReadKey, Vec<(String, tokio::time::Instant)>>,
    timeout: std::time::Duration,
}

impl PendingReads {
    const fn new() -> Self {
        Self {
            deadlines: BTreeMap::new(),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    /// Test hook: override the read timeout for fast expiry tests.
    #[cfg(test)]
    const fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            deadlines: BTreeMap::new(),
            timeout,
        }
    }

    /// Track `msg_id` for `key` with a 5s deadline; resolved on the next
    /// [`EngineEvent::RegisterRead`] for `key`.
    fn push(&mut self, key: ReadKey, msg_id: String) {
        self.deadlines
            .entry(key)
            .or_default()
            .push((msg_id, tokio::time::Instant::now() + self.timeout));
    }

    /// Remove and return every pending `msg_id` for `key`.
    fn resolve(&mut self, key: ReadKey) -> Vec<String> {
        self.deadlines
            .remove(&key)
            .map(|entries| entries.into_iter().map(|(msg_id, _)| msg_id).collect())
            .unwrap_or_default()
    }

    /// Remove and return every `(key, msg_id)` whose deadline has passed.
    fn drain_expired(&mut self) -> Vec<(ReadKey, String)> {
        let now = tokio::time::Instant::now();
        let mut expired = Vec::new();
        self.deadlines.retain(|key, entries| {
            let (kept, done): (Vec<_>, Vec<_>) =
                entries.drain(..).partition(|(_, deadline)| now < *deadline);
            for (msg_id, _) in done {
                expired.push((*key, msg_id));
            }
            *entries = kept;
            !entries.is_empty()
        });
        expired
    }
}

async fn bridge_until_disconnect(
    socket: &mut WebSocket,
    state: &WsState,
    mut ev_rx: broadcast::Receiver<WsEvent>,
) -> anyhow::Result<()> {
    let mut pending = PendingAcks::new();
    let mut pending_reads = PendingReads::new();
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
        // Expire stale read deadlines so a register the bus never reports
        // surfaces as an error ack.
        for (_, msg_id) in pending_reads.drain_expired() {
            send_ack(
                socket,
                &msg_id,
                AckStatus::Error,
                Some("read timeout".into()),
            )
            .await?;
        }
        tokio::select! {
            msg = socket.next() => {
                if !handle_ws_message(socket, state, msg, &mut pending, &mut pending_reads).await? {
                    break;
                }
            }
            ev = ev_rx.recv() => {
                if !forward_engine_event(socket, state, ev, &mut pending, &mut pending_reads).await? {
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
    pending_reads: &mut PendingReads,
) -> anyhow::Result<bool> {
    match msg {
        Some(Ok(Message::Text(text))) => {
            handle_client_text(socket, state, &text, pending, pending_reads).await?;
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

/// Shape an engine register-change record into its WS `event` body.
///
/// Type-agnostic: an 08-type record produces `unit_type: "08"` with the record's
/// own `unit_id` (multi-unit D-3). Undecodable payloads fall back to the raw
/// 14-char hex string (see [`event_body`]).
fn event_message(record: &CanRecord) -> ServerMessage {
    let (register, zone, payload) = event_body(record);
    ServerMessage::Event {
        unit_type: record.unit_type.to_string(),
        unit_id: record.unit_id.to_string(),
        register,
        zone,
        payload,
    }
}

/// Shape a resolved [`EngineEvent::RegisterRead`] into its wire reply for one
/// `msg_id`: a `read_result` with the typed (or raw hex) payload, or an error
/// ack when the register has no value.
fn read_result_message(
    msg_id: String,
    unit_type: UnitType,
    unit_id: UnitId,
    reg: RegId,
    zone: Option<u8>,
    data: Option<[u8; 7]>,
) -> ServerMessage {
    match data.and_then(|bytes| decode_payload(reg, bytes).ok()) {
        Some(payload) => ServerMessage::ReadResult {
            msg_id,
            unit_type: unit_type.to_string(),
            unit_id: unit_id.to_string(),
            register: format!("{:02x}", reg.get()),
            zone,
            payload,
        },
        None => ServerMessage::Ack {
            msg_id,
            status: AckStatus::Error,
            reason: Some(format!("register {:02x} has no value", reg.get())),
        },
    }
}

/// Returns `false` when the session should end.
async fn forward_engine_event(
    socket: &mut WebSocket,
    _state: &WsState,
    ev: Result<WsEvent, broadcast::error::RecvError>,
    pending: &mut PendingAcks,
    pending_reads: &mut PendingReads,
) -> anyhow::Result<bool> {
    match ev {
        Ok(WsEvent::Engine(EngineEvent::RegistersChanged { records })) => {
            for record in records {
                send_json(socket, &event_message(&record)).await?;
            }
            Ok(true)
        }
        Ok(WsEvent::Engine(EngineEvent::Snapshot { bank, .. })) => {
            let snap = ServerMessage::Snapshot {
                units: snapshot_units(&bank),
            };
            send_json(socket, &snap).await?;
            Ok(true)
        }
        Ok(WsEvent::Engine(EngineEvent::WriteFlushed)) => {
            for msg_id in pending.drain_all() {
                send_ack(socket, &msg_id, AckStatus::Success, None).await?;
            }
            Ok(true)
        }
        Ok(WsEvent::Engine(EngineEvent::RegisterRead {
            unit_type,
            unit_id,
            reg,
            zone,
            data,
        })) => {
            let key = ReadKey {
                unit_type,
                unit_id,
                reg,
                zone,
            };
            for msg_id in pending_reads.resolve(key) {
                send_json(
                    socket,
                    &read_result_message(msg_id, unit_type, unit_id, reg, zone, data),
                )
                .await?;
            }
            Ok(true)
        }
        Ok(WsEvent::Engine(EngineEvent::SessionState(state))) => {
            let status = ServerMessage::Status {
                state: map_session_state(state),
                detail: None,
            };
            send_json(socket, &status).await?;
            Ok(true)
        }
        Ok(WsEvent::Status { state, detail }) => {
            let status = ServerMessage::Status { state, detail };
            send_json(socket, &status).await?;
            Ok(true)
        }
        Ok(WsEvent::Engine(other)) => {
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
    pending_reads: &mut PendingReads,
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
        Ok(ClientMessage::Read {
            msg_id,
            unit_type,
            unit_id,
            register,
            zone,
        }) => {
            let req = ReadRequest {
                msg_id,
                unit_type,
                unit_id,
                register,
                zone,
            };
            handle_read(socket, state, pending_reads, req).await?;
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

/// Resolve the addressed unit from optional client fields, the bank, and the
/// hint.
///
/// Shared by the write and read paths. Omitted `unit_type` / `unit_id` default
/// to the bank's primary unit when a snapshot exists; a `unit_id` present under
/// exactly one unit type resolves that type when `unit_type` is omitted.
/// Without a snapshot the hint supplies the id (type AIRCON) and fails when no
/// hint is configured.
///
/// # Errors
///
/// Returns a human-readable reason for invalid identifiers or no primary unit
/// when neither a bank nor a hint is available.
fn resolve_unit(
    bank: Option<&RegisterBank>,
    hint: Option<UnitId>,
    unit_type: Option<String>,
    unit_id: Option<String>,
) -> Result<(UnitType, UnitId), String> {
    let (primary_type, primary_id) = bank
        .map_or_else(
            || hint.map(|id| (UnitType::AIRCON, id)),
            |bank| primary_unit(bank, hint),
        )
        .ok_or_else(|| "no primary unit available".to_owned())?;
    let parsed_type = unit_type
        .map(|s| UnitType::from_hex(&s))
        .transpose()
        .map_err(|err| format!("invalid unit_type: {err:?}"))?;
    let parsed_id = unit_id
        .map(|s| UnitId::from_hex(&s))
        .transpose()
        .map_err(|err| format!("invalid unit_id: {err:?}"))?;
    let unit_type = match (parsed_type, parsed_id) {
        (Some(unit_type), _) => unit_type,
        (None, Some(unit_id)) => bank
            .and_then(|bank| {
                bank.unit_types()
                    .into_iter()
                    .find(|t| bank.unit_ids(*t).contains(&unit_id))
            })
            .unwrap_or(primary_type),
        (None, None) => primary_type,
    };
    Ok((unit_type, parsed_id.unwrap_or(primary_id)))
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

/// Decoded `read` fields for [`handle_read`].
struct ReadRequest {
    msg_id: String,
    unit_type: Option<String>,
    unit_id: Option<String>,
    register: String,
    /// Zone id for zone-bearing registers (`03`/`04`); required for those.
    zone: Option<u8>,
}

/// Validate the register/zone of a `read` before it touches the bus.
///
/// Mirrors the write path's parse-error style, then rejects registers the read
/// path cannot serve: write-only (`09`), internal (`07`), and zone-bearing
/// registers (`03`/`04`) addressed without a zone.
fn validate_read_register(register: &str, zone: Option<u8>) -> Result<RegId, String> {
    let reg = RegId::from_hex(register).map_err(|err| format!("invalid register: {err:?}"))?;
    match write_policy(reg).mode {
        PolicyMode::WriteOnly => {
            return Err(format!("register {:02x} is write-only", reg.get()));
        }
        PolicyMode::Internal => {
            return Err(format!("register {:02x} is handled internally", reg.get()));
        }
        PolicyMode::ReadWrite | PolicyMode::ReadOnly | PolicyMode::Unverified => {}
    }
    if is_zone_bearing(reg) && zone.is_none() {
        return Err(format!("zone required for register {:02x}", reg.get()));
    }
    Ok(reg)
}

/// `read`: validate the register, resolve the unit, and queue an
/// [`EngineCmd::ReadRegister`]. The reply arrives later as a correlated
/// `read_result` from the flush's [`EngineEvent::RegisterRead`] (or a `read
/// timeout` error ack after 5s when the bus never reports the register).
async fn handle_read(
    socket: &mut WebSocket,
    state: &WsState,
    pending: &mut PendingReads,
    msg: ReadRequest,
) -> anyhow::Result<()> {
    let reg = match validate_read_register(&msg.register, msg.zone) {
        Ok(reg) => reg,
        Err(reason) => {
            send_ack(socket, &msg.msg_id, AckStatus::Error, Some(reason)).await?;
            return Ok(());
        }
    };
    // Clone the held snapshot: `watch::Ref` is not `Send`, so the borrow guard
    // must not span the awaits below.
    let bank = state.snapshot.borrow().as_ref().cloned();
    let (unit_type, unit_id) = match resolve_unit(
        bank.as_ref().map(|held| &held.bank),
        state.unit_id_hint,
        msg.unit_type,
        msg.unit_id,
    ) {
        Ok(unit) => unit,
        Err(reason) => {
            send_ack(socket, &msg.msg_id, AckStatus::Error, Some(reason)).await?;
            return Ok(());
        }
    };
    // Zone is part of the CAN address for zone-bearing registers only; on a
    // non-zone register the client's zone is ignored (never echoed on the
    // read_result, matching the write path's ignore behavior).
    let zone = if is_zone_bearing(reg) { msg.zone } else { None };
    let cmd = EngineCmd::ReadRegister {
        unit_type,
        unit_id,
        reg,
        zone,
    };
    record_spy(state, &cmd).await;
    if let Err(err) = state.cmd_tx.send(cmd).await {
        send_ack(socket, &msg.msg_id, AckStatus::Error, Some(err.to_string())).await?;
    } else {
        let key = ReadKey {
            unit_type,
            unit_id,
            reg,
            zone,
        };
        pending.push(key, msg.msg_id);
    }
    Ok(())
}

/// Resolve and encode a `write` into a single-register [`CanRecord`].
///
/// Unit addressing defers to [`resolve_unit`]; zone-bearing registers
/// (`03`/`04`) stamp the client's zone into wire byte 0.
///
/// # Errors
///
/// Returns a human-readable reason for invalid identifiers/register, an
/// unencodable payload (mirrors the ack `reason` field), or no primary unit
/// when neither a bank nor a hint is available.
fn build_write_record(
    bank: Option<&RegisterBank>,
    hint: Option<UnitId>,
    unit_type: Option<String>,
    unit_id: Option<String>,
    register: &str,
    zone: Option<u8>,
    payload: &Value,
) -> Result<CanRecord, String> {
    let (unit_type, unit_id) = resolve_unit(bank, hint, unit_type, unit_id)?;
    let reg = RegId::from_hex(register).map_err(|err| format!("invalid register: {err:?}"))?;
    validate_write(reg, payload).map_err(|e| e.to_string())?;
    let mut data = encode_payload(reg, payload).map_err(|err| err.to_string())?;
    // The zone id is part of the CAN address, not the payload: the codec stamps
    // wire byte 0 as 0x00, so a zone-bearing write (regs 03/04) addressed by
    // the client must be stamped here to reach the addressed zone.
    if is_zone_bearing(reg)
        && let Some(zone) = zone
    {
        data[0] = zone;
    }
    Ok(CanRecord {
        unit_type,
        dest: Dest::ControlBox,
        unit_id,
        reg,
        data,
    })
}

/// `write`: encode the register payload and queue it as a single register
/// write. Ack is deferred until the engine confirms the frame was transmitted.
async fn handle_write(
    socket: &mut WebSocket,
    state: &WsState,
    pending: &mut PendingAcks,
    msg: WriteRequest,
) -> anyhow::Result<()> {
    // Clone the held snapshot: `watch::Ref` is not `Send`, so the borrow guard
    // must not span the awaits below.
    let bank = state.snapshot.borrow().as_ref().cloned();
    let record = match build_write_record(
        bank.as_ref().map(|held| &held.bank),
        state.unit_id_hint,
        msg.unit_type,
        msg.unit_id,
        &msg.register,
        msg.zone,
        &msg.payload,
    ) {
        Ok(record) => record,
        Err(reason) => {
            send_ack(socket, &msg.msg_id, AckStatus::Error, Some(reason)).await?;
            return Ok(());
        }
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
    fn primary_unit_prefers_live_dump_over_hint_default() {
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
        assert_eq!(
            primary_unit(&bank, Some(live)),
            Some((UnitType::AIRCON, live))
        );
        assert_eq!(primary_unit(&bank, None), Some((UnitType::AIRCON, live)));
        // Empty bank: no primary; hint alone supplies (AIRCON, hint).
        let empty = RegisterBank::new();
        assert_eq!(primary_unit(&empty, None), None);
        assert_eq!(
            primary_unit(&empty, Some(live)),
            Some((UnitType::AIRCON, live))
        );
    }

    #[test]
    fn primary_unit_picks_08_unit_when_bank_has_only_08_records() {
        let mut bank = RegisterBank::new();
        let small = UnitId::try_new(0x0_00001).unwrap();
        let large = UnitId::try_new(0x0_00002).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: large,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: small,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        assert_eq!(
            primary_unit(&bank, None),
            Some((UnitType::new(0x08), small))
        );
    }

    #[test]
    fn primary_unit_with_hint_at_08_unit_returns_unit_type_08() {
        let mut bank = RegisterBank::new();
        let split = UnitId::try_new(0x0_00042).unwrap();
        let aircon = UnitId::try_new(0x0_00001).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: split,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: aircon,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        assert_eq!(
            primary_unit(&bank, Some(split)),
            Some((UnitType::new(0x08), split))
        );
    }

    /// Raw-hex reg-05 payload (byte-exact passthrough) for write tests.
    fn write_payload() -> Value {
        Value::String("01010330000100".to_owned())
    }

    #[test]
    fn event_message_shapes_08_unit_record() {
        // Acceptance (b): an event for an 08-type unit is emitted with
        // `unit_type: "08"`, the correct `unit_id`, and `zone` for reg 03.
        let rec = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_0ABCD).unwrap(),
            reg: RegId::new(0x03),
            data: [0x02, 0xe4, 0x00, 0x03, 0x00, 0x00, 0x00],
        };
        let event = event_message(&rec);
        let ServerMessage::Event {
            unit_type,
            unit_id,
            register,
            zone,
            payload,
        } = event
        else {
            panic!("expected Event message");
        };
        assert_eq!(unit_type, "08");
        assert_eq!(unit_id, "0abcd");
        assert_eq!(register, "03");
        assert_eq!(zone, Some(0x02));
        assert_eq!(payload["open"], true);
        assert_eq!(payload["damper_pct"], 100);

        // Non-zone 08 record → zone None.
        let rec = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_0ABCD).unwrap(),
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        };
        let ServerMessage::Event { zone, .. } = event_message(&rec) else {
            panic!("expected Event message");
        };
        assert_eq!(zone, None);
    }

    #[test]
    fn build_write_record_addresses_08_unit() {
        // Acceptance (c): a write to an 08-type unit produces a setCAN record
        // with unit type 08.
        let rec = build_write_record(
            None,
            Some(UnitId::try_new(0x0_ABCDE).unwrap()),
            Some("08".to_owned()),
            Some("abcde".to_owned()),
            "05",
            None,
            &write_payload(),
        )
        .expect("record");
        assert_eq!(rec.unit_type, UnitType::new(0x08));
        assert_eq!(rec.unit_id, UnitId::try_new(0x0_ABCDE).unwrap());
        assert_eq!(rec.reg, RegId::new(0x05));

        // Omitted unit_type + unit_id present in the bank under type 08 →
        // resolves type 08.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        let rec = build_write_record(
            Some(&bank),
            None,
            None,
            Some("abcde".to_owned()),
            "05",
            None,
            &write_payload(),
        )
        .expect("record");
        assert_eq!(rec.unit_type, UnitType::new(0x08));
        assert_eq!(rec.unit_id, id);
    }

    #[test]
    fn build_write_record_defaults_omitted_fields_to_primary() {
        // Acceptance (d): omitted unit_type/unit_id defaults to the primary
        // unit on the emitted record.
        let mut bank = RegisterBank::new();
        let primary_id = UnitId::try_new(0x0_181F3).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: primary_id,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });

        // Both omitted → primary (type, id).
        let rec = build_write_record(Some(&bank), None, None, None, "05", None, &write_payload())
            .expect("record");
        assert_eq!(rec.unit_type, UnitType::AIRCON);
        assert_eq!(rec.unit_id, primary_id);

        // Only unit_id omitted → primary id but explicit type kept.
        let rec = build_write_record(
            Some(&bank),
            None,
            Some("08".to_owned()),
            None,
            "05",
            None,
            &write_payload(),
        )
        .expect("record");
        assert_eq!(rec.unit_type, UnitType::new(0x08));
        assert_eq!(rec.unit_id, primary_id);
    }

    #[test]
    fn build_write_record_errors_without_primary_when_bank_empty_and_no_hint() {
        // D-9: no FEEDER_UNIT_ID fallback — omitted addressing with no bank
        // and no hint must error instead of silently targeting the mock id.
        let err = build_write_record(None, None, None, None, "05", None, &write_payload())
            .expect_err("no primary unit available");
        assert_eq!(err, "no primary unit available");
        let empty = RegisterBank::new();
        let err = build_write_record(Some(&empty), None, None, None, "05", None, &write_payload())
            .expect_err("no primary unit available");
        assert_eq!(err, "no primary unit available");
    }

    #[test]
    fn build_write_record_stamps_zone_for_zone_bearing_register() {
        let rec = build_write_record(
            None,
            Some(UnitId::try_new(0x0_ABCDE).unwrap()),
            None,
            None,
            "03",
            Some(2),
            &write_payload(),
        )
        .expect("record");
        assert_eq!(rec.reg, RegId::new(0x03));
        assert_eq!(rec.data[0], 2, "zone must be stamped into wire byte 0");
    }

    #[test]
    fn validate_read_register_rejects_write_only() {
        // Acceptance (3): a read of reg 09 (write-only) is rejected up front
        // with the exact reason string.
        let err = validate_read_register("09", None).expect_err("write-only read");
        assert_eq!(err, "register 09 is write-only");
    }

    #[test]
    fn validate_read_register_rejects_internal() {
        // Acceptance (4): a read of reg 07 (internal) is rejected up front.
        let err = validate_read_register("07", None).expect_err("internal read");
        assert_eq!(err, "register 07 is handled internally");
    }

    #[test]
    fn validate_read_register_requires_zone_for_zone_bearing_register() {
        // Acceptance (6): a zone-less read of reg 03 is rejected with the
        // exact reason string; a zone fixes it. Zone on a non-zone register
        // is ignored (consistent with the write path).
        let err = validate_read_register("03", None).expect_err("zone-less read");
        assert_eq!(err, "zone required for register 03");
        assert_eq!(validate_read_register("03", Some(2)), Ok(RegId::new(0x03)));
        assert_eq!(validate_read_register("05", Some(2)), Ok(RegId::new(0x05)));
    }

    #[test]
    fn validate_read_register_parses_register_like_write_path() {
        let err = validate_read_register("xyz", None).expect_err("bad register");
        assert!(err.starts_with("invalid register: "), "got {err:?}");
    }

    #[test]
    fn pending_reads_resolve_returns_all_msg_ids_for_key() {
        let mut pending = PendingReads::new();
        let key = ReadKey {
            unit_type: UnitType::AIRCON,
            unit_id: UnitId::try_new(0x0_181F3).unwrap(),
            reg: RegId::new(0x05),
            zone: None,
        };
        assert_eq!(pending.resolve(key), Vec::<String>::new(), "absent key");

        pending.push(key, "r1".to_owned());
        pending.push(key, "r2".to_owned());
        assert_eq!(pending.resolve(key), vec!["r1", "r2"]);
        assert_eq!(
            pending.resolve(key),
            Vec::<String>::new(),
            "resolved key is removed"
        );

        // Keys differ by any of the four fields (zone here).
        let zoned = ReadKey {
            reg: RegId::new(0x03),
            zone: Some(2),
            ..key
        };
        pending.push(zoned, "r3".to_owned());
        assert_eq!(pending.resolve(zoned), vec!["r3"]);
    }

    #[tokio::test]
    async fn pending_reads_drain_expired_returns_only_dead_entries() {
        // Real-time (no paused clock available): 200ms timeout with generous
        // sleep margins so the first entry's deadline passes while the
        // second's has not (sleeps only overshoot, never undershoot).
        let mut pending = PendingReads::with_timeout(std::time::Duration::from_millis(200));
        let key = ReadKey {
            unit_type: UnitType::AIRCON,
            unit_id: UnitId::try_new(0x0_181F3).unwrap(),
            reg: RegId::new(0x02),
            zone: None,
        };
        pending.push(key, "early".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        pending.push(key, "late".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(pending.drain_expired(), vec![(key, "early".to_owned())]);
        assert_eq!(pending.resolve(key), vec!["late".to_owned()]);
    }

    #[test]
    fn read_result_shapes_zone_read_as_read_result() {
        // Acceptance (5): a resolved zone read of reg 03 carries the zone id
        // on the read_result with the typed payload.
        let msg = read_result_message(
            "m1".to_owned(),
            UnitType::new(0x08),
            UnitId::try_new(0x0_ABCDE).unwrap(),
            RegId::new(0x03),
            Some(2),
            Some([0x02, 0xe4, 0x00, 0x03, 0x00, 0x00, 0x00]),
        );
        let ServerMessage::ReadResult {
            msg_id,
            unit_type,
            unit_id,
            register,
            zone,
            payload,
        } = msg
        else {
            panic!("expected ReadResult message");
        };
        assert_eq!(msg_id, "m1");
        assert_eq!(unit_type, "08");
        assert_eq!(unit_id, "abcde");
        assert_eq!(register, "03");
        assert_eq!(zone, Some(2));
        assert_eq!(payload["open"], true);
        assert_eq!(payload["damper_pct"], 100);
    }

    #[test]
    fn read_result_unknown_register_falls_back_to_raw_hex() {
        // Acceptance (2): an unknown register (16) yields the raw 14-char hex
        // payload on the read_result.
        let msg = read_result_message(
            "m2".to_owned(),
            UnitType::AIRCON,
            UnitId::try_new(0x0_181F3).unwrap(),
            RegId::new(0x16),
            None,
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x11]),
        );
        let ServerMessage::ReadResult { payload, .. } = msg else {
            panic!("expected ReadResult message");
        };
        assert_eq!(payload, Value::String("aabbccddeeff11".to_owned()));
    }

    #[test]
    fn read_result_missing_value_acks_error() {
        // Acceptance (7): a resolved read whose register has no value acks an
        // error instead of a read_result.
        let msg = read_result_message(
            "m3".to_owned(),
            UnitType::AIRCON,
            UnitId::try_new(0x0_181F3).unwrap(),
            RegId::new(0x02),
            None,
            None,
        );
        let ServerMessage::Ack { status, reason, .. } = msg else {
            panic!("expected Ack message");
        };
        assert_eq!(status, AckStatus::Error);
        assert_eq!(reason.as_deref(), Some("register 02 has no value"));
    }

    #[test]
    fn build_write_record_rejects_read_only_register() {
        // D-6: a write to a read-only register is rejected with the exact
        // WriteError reason before any encoding / bus traffic.
        let err = build_write_record(
            None,
            Some(UnitId::try_new(0x0_ABCDE).unwrap()),
            None,
            None,
            "08",
            None,
            &serde_json::json!({}),
        )
        .expect_err("read-only register write must be rejected");
        assert_eq!(err, "register 08 is read-only");
    }
}
