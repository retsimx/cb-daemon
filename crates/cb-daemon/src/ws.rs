//! Axum WebSocket bridge: multi-consumer sessions + JSON ↔ engine cmds/events.

use std::collections::BTreeMap;
use std::sync::Arc;

use aa_engine::{EngineCmd, EngineEvent};
use aa_mailbox::{
    AckStatus, ClientMessage, PolicyMode, ServerMessage, StatusState, decode_payload,
    encode_payload, event_body, merge_payload, snapshot_units, validate_write,
    validate_write_merged, write_policy,
};
use aa_registers::{
    CanRecord, Dest, Power, RegId, RegisterBank, SystemStatus, UnitId, UnitType, is_zone_bearing,
};
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

/// Per-session write/read timeout durations (production defaults 10 s / 5 s).
#[derive(Debug, Clone, Copy)]
pub struct SessionTimeouts {
    pub write_ack: std::time::Duration,
    pub read: std::time::Duration,
}

impl Default for SessionTimeouts {
    fn default() -> Self {
        Self {
            write_ack: std::time::Duration::from_secs(10),
            read: std::time::Duration::from_secs(5),
        }
    }
}

/// Shared state for the axum router.
#[derive(Clone)]
pub(crate) struct WsState {
    /// Per-session write/read timeout durations.
    pub timeouts: SessionTimeouts,
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
    /// Connected-session counter (idle-failsafe arming signal).
    pub clients: watch::Sender<usize>,
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

/// Session-counter guard: increments [`WsState::clients`] on acquire and
/// decrements on drop.
///
/// Bound at session entry so every exit path (disconnect, snapshot-wait
/// abort, transport error) decrements exactly once — including panics, which
/// unwind through `Drop`.
struct ClientCountGuard {
    clients: watch::Sender<usize>,
}

impl ClientCountGuard {
    fn acquire(clients: watch::Sender<usize>) -> Self {
        clients.send_modify(|count| *count += 1);
        Self { clients }
    }
}

impl Drop for ClientCountGuard {
    fn drop(&mut self) {
        self.clients.send_modify(|count| *count -= 1);
    }
}

/// Run one independent session per connection (no single-session gate).
async fn handle_socket(socket: WebSocket, state: WsState) {
    let _guard = ClientCountGuard::acquire(state.clients.clone());
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
    bridge_until_disconnect(&mut socket, &state, ev_rx, state.timeouts).await
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
    /// Override the write-ack timeout (session timeouts / fast tests).
    const fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            deadlines: BTreeMap::new(),
            timeout,
        }
    }

    /// Track `msg_id` with the configured deadline; acked on [`EngineEvent::WriteFlushed`].
    fn push(&mut self, msg_id: String) {
        self.deadlines
            .insert(msg_id, tokio::time::Instant::now() + self.timeout);
    }

    /// Earliest pending write-ack deadline across all entries (`None` when
    /// nothing is pending). The map is keyed by `msg_id`, so the earliest
    /// deadline is the minimum over values, not `first_key_value()`.
    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.deadlines.values().copied().min()
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
    #[allow(dead_code)]
    const fn new() -> Self {
        Self {
            deadlines: BTreeMap::new(),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    /// Override the read timeout (session timeouts / fast tests).
    const fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            deadlines: BTreeMap::new(),
            timeout,
        }
    }

    /// Track `msg_id` for `key` with the configured deadline; resolved on the next
    /// [`EngineEvent::RegisterRead`] for `key`.
    fn push(&mut self, key: ReadKey, msg_id: String) {
        self.deadlines
            .entry(key)
            .or_default()
            .push((msg_id, tokio::time::Instant::now() + self.timeout));
    }

    /// Earliest pending read deadline across all entries (`None` when empty).
    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.deadlines
            .values()
            .flat_map(|entries| entries.iter().map(|(_, deadline)| *deadline))
            .min()
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

/// Sleep until the earliest pending deadline; when nothing is pending, never
/// completes so the `select!` degenerates to the socket/event branches.
async fn wait_until(earliest: Option<tokio::time::Instant>) {
    match earliest {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

async fn bridge_until_disconnect(
    socket: &mut WebSocket,
    state: &WsState,
    mut ev_rx: broadcast::Receiver<WsEvent>,
    timeouts: SessionTimeouts,
) -> anyhow::Result<()> {
    let mut pending = PendingAcks::with_timeout(timeouts.write_ack);
    let mut pending_reads = PendingReads::with_timeout(timeouts.read);
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
        // Wake at the earliest pending deadline so expiries fire even when
        // the bus is silent; the loop-top drains above do the removal (and
        // no entry survives past its deadline, so no double-ack).
        let earliest = pending
            .next_deadline()
            .into_iter()
            .chain(pending_reads.next_deadline())
            .min();
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
            () = wait_until(earliest) => {}
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

/// Sparse-merge a typed client payload over the bank's decoded DTO for the
/// addressed `(unit_type, unit_id, reg, zone?)`.
///
/// The client payload must be a JSON object; any other non-string value
/// (number, array, bool, null) is rejected exactly as the pre-merge path did.
/// The bank must hold a value for the address and it must decode to a typed
/// object — otherwise the write errors rather than silently writing zeros or
/// raw bytes. The merged payload is validated with [`validate_write_merged`]
/// (mode check on the merged payload, read-only field check on the client-sent
/// keys only, range check on the merged bytes).
///
/// # Errors
///
/// Returns a human-readable reason for a non-object payload, missing bank
/// state, or an undecodable (raw-hex) bank value.
fn sparse_merge_payload(
    bank: Option<&RegisterBank>,
    unit_type: UnitType,
    unit_id: UnitId,
    reg: RegId,
    zone: Option<u8>,
    payload: &Value,
) -> Result<Value, String> {
    if !payload.is_object() {
        let reason = match encode_payload(reg, payload) {
            Err(err) => err.to_string(),
            Ok(_) => "bad register payload: expected an object".to_owned(),
        };
        return Err(reason);
    }
    let zone_suffix = if is_zone_bearing(reg) {
        format!(" [zone {}]", zone.unwrap_or(0))
    } else {
        String::new()
    };
    let bank_data = if is_zone_bearing(reg) {
        bank.and_then(|bank| bank.get_zone(unit_type, unit_id, reg, zone.unwrap_or(0)))
    } else {
        bank.and_then(|bank| bank.get(unit_type, unit_id, reg))
    };
    let Some(bank_data) = bank_data else {
        return Err(format!(
            "no bank state for register {:02x}{}; send a full payload or issue a read first",
            reg.get(),
            zone_suffix
        ));
    };
    let bank_decoded = decode_payload(reg, bank_data).map_err(|err| err.to_string())?;
    if bank_decoded.as_str().is_some() {
        return Err(format!(
            "cannot merge: bank state for register {:02x} is not typed; send a full payload or issue a read first",
            reg.get()
        ));
    }
    let mut merged = merge_payload(&bank_decoded, payload);
    drop_reg12_read_only_fields(&mut merged, reg);
    validate_write_merged(reg, payload, &merged).map_err(|e| e.to_string())?;
    Ok(merged)
}

/// Drop the read-only fields from a merged reg-`12` payload so it encodes as
/// the write shape.
///
/// Reg `12` (sensor pairing) is special: its write DTO (`sensor_uid`, `zone`)
/// is a strict subset of its read DTO (`sensor_uid`, `pairing`,
/// `sensor_rev`). The bank holds the read shape, so a merged payload that
/// keeps `pairing`/`sensor_rev` makes the codec prefer the read shape and the
/// addressed zone is lost. No-op for every other register.
fn drop_reg12_read_only_fields(merged: &mut Value, reg: RegId) {
    if reg.get() == 0x12
        && let Some(obj) = merged.as_object_mut()
    {
        obj.remove("pairing");
        obj.remove("sensor_rev");
    }
}

/// Resolve and encode a `write` into a single-register [`CanRecord`].
///
/// Unit addressing defers to [`resolve_unit`]; zone-bearing registers
/// (`03`/`04`) stamp the client's zone into wire byte 0. Raw-hex payloads are
/// byte-exact passthrough (no bank lookup, no merge); typed payloads sparse
/// merge over the bank's decoded DTO via [`sparse_merge_payload`].
///
/// # Errors
///
/// Returns a human-readable reason for invalid identifiers/register, an
/// unencodable payload (mirrors the ack `reason` field), no primary unit when
/// neither a bank nor a hint is available, or a sparse-merge failure (missing
/// or undecodable bank state for the addressed register/zone).
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
    // Raw-hex payloads are byte-exact passthrough: verbatim, no bank lookup,
    // no merge (unchanged behavior).
    if payload.as_str().is_some() {
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
        return Ok(CanRecord {
            unit_type,
            dest: Dest::ControlBox,
            unit_id,
            reg,
            data,
        });
    }
    let merged = sparse_merge_payload(bank, unit_type, unit_id, reg, zone, payload)?;
    let mut data = encode_payload(reg, &merged).map_err(|err| err.to_string())?;
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

/// Build power-off [`CanRecord`]s for every AIRCON unit in `bank`.
///
/// Each unit's reg-05 [`SystemStatus`] is re-encoded with `power = Off`,
/// preserving every other byte (mode, fan, set temp, myzone, fresh air, RF
/// sys id). Units without a reg-05 slot are skipped — their mode/fan bytes
/// are unknown, so a blind off is refused. No AIRCON units → empty vec (the
/// caller sends no write).
fn power_off_records(bank: &RegisterBank) -> Vec<CanRecord> {
    let mut records = Vec::new();
    for unit_id in bank.unit_ids(UnitType::AIRCON) {
        let Some(data) = bank.get(UnitType::AIRCON, unit_id, RegId::new(0x05)) else {
            continue;
        };
        let mut status = SystemStatus::from(data);
        status.power = Power::Off;
        records.push(CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id,
            reg: RegId::new(0x05),
            data: status.into(),
        });
    }
    records
}

/// Fire the idle failsafe: queue a power-off write for every AIRCON unit
/// with a reg-05 status in the held snapshot.
///
/// `warn!` audit log carries the target unit ids on both success and failure.
async fn fire_idle_power_off(state: &WsState) {
    // Clone the held snapshot: `watch::Ref` is not `Send`, so the borrow
    // guard must not span the awaits below.
    let Some(held) = state.snapshot.borrow().as_ref().cloned() else {
        return;
    };
    let records = power_off_records(&held.bank);
    if records.is_empty() {
        return;
    }
    let units = records
        .iter()
        .map(|record| record.unit_id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let cmd = EngineCmd::WriteRegisters(records);
    record_spy(state, &cmd).await;
    if let Err(err) = state.cmd_tx.send(cmd).await {
        warn!(?err, %units, "idle failsafe: failed to queue power-off write");
    } else {
        warn!(%units, "idle failsafe: zero WebSocket clients; powering off aircon unit(s)");
    }
}

/// Spawn the idle-failsafe watchdog task.
///
/// With `timeout == 0` the task exits immediately (failsafe disabled). The
/// handle is intentionally detached: the task runs until the process ends.
pub(crate) fn spawn_idle_watchdog(
    state: WsState,
    timeout: std::time::Duration,
    retry_interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if timeout.is_zero() {
            return;
        }
        idle_watchdog_loop(state, timeout, retry_interval).await;
    })
}

/// Saturating cap for watchdog deadline spans: 100 years, far beyond any
/// configured timeout yet safely inside `Instant` arithmetic bounds.
const MAX_DEADLINE_SPAN: std::time::Duration = std::time::Duration::from_hours(876_000);

/// `now + duration` as a watchdog deadline, saturating instead of panicking.
///
/// `Instant` addition is a checked add: an overflowing duration (possible
/// via the test hooks, which bypass config validation) would panic the
/// detached watchdog task and silently disable the failsafe. An overflowing
/// span clamps to the latest representable deadline instead.
fn idle_deadline(duration: std::time::Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(duration).unwrap_or(now + MAX_DEADLINE_SPAN)
}

/// Idle-failsafe state machine: arms immediately when the daemon boots with
/// zero clients (startup arming — a restart while the network is down must
/// still fail safe), else on the first >0 → 0 client transition; fires at
/// `disconnected_at + timeout`, re-fires every `retry_interval` while the
/// count stays 0, disarms on any reconnect (count > 0).
async fn idle_watchdog_loop(
    state: WsState,
    timeout: std::time::Duration,
    retry_interval: std::time::Duration,
) {
    let mut clients = state.clients.subscribe();
    // `None` = disarmed (a client is connected). Armed deadlines are
    // absolute, so drift-free re-arming is just `now + interval`.
    let mut deadline: Option<tokio::time::Instant> = if *clients.borrow_and_update() == 0 {
        // Startup arming: no client ever connected — count is 0 at boot, so
        // arm from task start. A later >0 → 0 transition re-arms from that
        // disconnect.
        Some(idle_deadline(timeout))
    } else {
        None
    };
    loop {
        let sleep = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            changed = clients.changed() => {
                if changed.is_err() {
                    warn!("idle failsafe: client-count watch closed; watchdog exiting");
                    return;
                }
                if *clients.borrow_and_update() > 0 {
                    // Any connected client disarms the failsafe.
                    deadline = None;
                } else if deadline.is_none() {
                    // First >0 → 0 transition: arm at disconnect + timeout.
                    deadline = Some(idle_deadline(timeout));
                }
            }
            () = sleep => {
                // Re-read before firing: a reconnect racing the deadline
                // (both select branches ready) must disarm, not fire.
                if *clients.borrow_and_update() == 0 {
                    fire_idle_power_off(&state).await;
                    deadline = Some(idle_deadline(retry_interval));
                } else {
                    deadline = None;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_engine::EngineCmd;
    use aa_registers::{CanRecord, Dest, RegId, UnitId, UnitType};
    use futures_util::SinkExt;
    use std::net::SocketAddr;

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
    fn pending_acks_next_deadline_returns_earliest() {
        let mut pending = PendingAcks::with_timeout(std::time::Duration::from_mins(1));
        assert_eq!(pending.next_deadline(), None, "empty");

        pending.push("a".to_owned());
        let earliest = pending.next_deadline().expect("single deadline");
        assert!(
            earliest > tokio::time::Instant::now(),
            "deadline lies in the future"
        );

        // A later push cannot move the earliest deadline (pushes are ordered
        // in real time, so later entries always carry later deadlines).
        pending.push("b".to_owned());
        pending.push("c".to_owned());
        assert_eq!(pending.next_deadline(), Some(earliest));
    }

    #[test]
    fn pending_reads_next_deadline_returns_earliest() {
        let mut pending = PendingReads::with_timeout(std::time::Duration::from_mins(1));
        assert_eq!(pending.next_deadline(), None, "empty");

        let key = ReadKey {
            unit_type: UnitType::AIRCON,
            unit_id: UnitId::try_new(0x0_181F3).unwrap(),
            reg: RegId::new(0x05),
            zone: None,
        };
        pending.push(key, "r1".to_owned());
        let earliest = pending.next_deadline().expect("single deadline");
        assert!(
            earliest > tokio::time::Instant::now(),
            "deadline lies in the future"
        );

        // A later push on the same key — or on a second key — cannot move the
        // earliest deadline (pushes are ordered in real time).
        let zoned = ReadKey {
            reg: RegId::new(0x03),
            zone: Some(2),
            ..key
        };
        pending.push(key, "r2".to_owned());
        pending.push(zoned, "r3".to_owned());
        assert_eq!(pending.next_deadline(), Some(earliest));
    }

    #[tokio::test]
    async fn pending_acks_next_deadline_ignores_msg_id_order() {
        let mut pending = PendingAcks::with_timeout(std::time::Duration::from_mins(1));
        assert_eq!(pending.next_deadline(), None, "empty");

        // Push in NON-lex order: the first-pushed id sorts last, so the
        // lexicographically-first key ("req-a") carries the LATER deadline.
        pending.push("req-z".to_owned());
        let earliest = pending.next_deadline().expect("single deadline");
        assert!(
            earliest > tokio::time::Instant::now(),
            "deadline lies in the future"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        pending.push("req-a".to_owned());

        // "req-a" sorts before "req-z" in the BTreeMap, but its deadline is
        // later (pushes are ordered in real time), so the earliest deadline
        // is still the first-pushed entry's.
        assert_eq!(pending.next_deadline(), Some(earliest));
        assert!(
            earliest < pending.deadlines["req-a"],
            "req-a deadline is later, req-z must remain the earliest"
        );
    }

    #[tokio::test]
    async fn pending_acks_drain_expired_fires_earliest_deadline_across_ids() {
        // Real-time (no paused clock available): 10s timeout, generous sleep
        // margins. "req-z" is pushed first and "req-a" 250ms later, so at
        // drain time the first entry's deadline has passed while the second's
        // has not, even though "req-a" sorts first in the BTreeMap.
        let mut pending = PendingAcks::with_timeout(std::time::Duration::from_millis(200));
        pending.push("req-z".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        pending.push("req-a".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(pending.drain_expired(), vec!["req-z".to_owned()]);
        assert_eq!(pending.drain_all(), vec!["req-a".to_owned()]);
    }

    #[tokio::test]
    async fn pending_acks_drain_expired_returns_only_dead_entries() {
        // Real-time (no paused clock available): 200ms timeout with generous
        // sleep margins so the first entry's deadline passes while the
        // second's has not (sleeps only overshoot, never undershoot).
        let mut pending = PendingAcks::with_timeout(std::time::Duration::from_millis(200));
        pending.push("early".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        pending.push("late".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(pending.drain_expired(), vec!["early".to_owned()]);
        assert_eq!(pending.drain_all(), vec!["late".to_owned()]);
    }

    #[tokio::test]
    async fn interleaved_write_read_deadlines_expire_independently() {
        // Real-time (no paused clock available): one write and one read die
        // in the first window while their late siblings survive it, so each
        // drain only returns its own dead entries.
        let mut pending = PendingAcks::with_timeout(std::time::Duration::from_millis(200));
        let mut pending_reads = PendingReads::with_timeout(std::time::Duration::from_millis(200));
        let key = ReadKey {
            unit_type: UnitType::AIRCON,
            unit_id: UnitId::try_new(0x0_181F3).unwrap(),
            reg: RegId::new(0x02),
            zone: None,
        };
        pending.push("w1".to_owned());
        pending_reads.push(key, "r1".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        pending.push("w2".to_owned());
        pending_reads.push(key, "r2".to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(pending.drain_expired(), vec!["w1".to_owned()]);
        assert_eq!(pending_reads.drain_expired(), vec![(key, "r1".to_owned())]);
        assert_eq!(pending.drain_all(), vec!["w2".to_owned()]);
        assert_eq!(pending_reads.resolve(key), vec!["r2".to_owned()]);
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
        // WriteError reason before any encoding / bus traffic. The typed path
        // needs bank state to reach the mode check, so seed a reg-08 slot.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x08),
            data: [0; 7],
        });
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "08",
            None,
            &serde_json::json!({}),
        )
        .expect_err("read-only register write must be rejected");
        assert_eq!(err, "register 08 is read-only");
    }

    #[test]
    fn build_write_record_rejects_read_only_field() {
        // D-6: a typed write carrying a read-only field is rejected with the
        // exact WriteError reason before any encoding / bus traffic. The typed
        // path needs bank state to reach the field check, so seed a reg-05
        // slot.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "05",
            None,
            &serde_json::json!({ "rf_sys_id": 1 }),
        )
        .expect_err("read-only field write must be rejected");
        assert_eq!(err, "field 'rf_sys_id' is read-only on register 05");
    }

    #[test]
    fn build_write_record_rejects_unverified_register() {
        // D-6: a write to an unverified register is rejected with the exact
        // WriteError reason before any encoding / bus traffic. The typed path
        // needs bank state that decodes to a typed DTO to reach the mode
        // check, so seed a reg-13 (info byte) slot — reg 0b has no codec and
        // its bank decode falls back to raw hex ("cannot merge").
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x13),
            data: [0; 7],
        });
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "13",
            None,
            &serde_json::json!({}),
        )
        .expect_err("unverified register write must be rejected");
        assert_eq!(err, "register 13 is unverified; writes not permitted");
    }

    #[test]
    fn build_write_record_rejects_out_of_range_field_value() {
        // D-6: a wire value above its bound is rejected with the exact
        // WriteError reason before any bus traffic (numZones max 10). The
        // typed path needs bank state to reach the range check, so seed a
        // reg-01 slot.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x01),
            data: [0x20, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00],
        });
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "01",
            None,
            &serde_json::json!({
                "header": 0x20,
                "total_zones": 11,
                "constant_zones": 1,
                "constant_zone_ids": [1, 0, 0],
                "filter_clean_required": false,
            }),
        )
        .expect_err("out-of-range field write must be rejected");
        assert_eq!(err, "field 'numZones' 11 out of range (max 10)");
    }

    fn seed_reg05(bank: &mut RegisterBank, id: UnitId) {
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });
    }

    #[test]
    fn build_write_record_sparse_typed_merge_preserves_bank_fields() {
        // D-11: a sparse typed reg-05 write merges over the bank's decoded
        // DTO — the client's field wins, the bank's other fields are
        // preserved in the encoded bytes.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        seed_reg05(&mut bank, id);
        let rec = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "05",
            None,
            &serde_json::json!({"fan": "low"}),
        )
        .expect("sparse merge must succeed");
        assert_eq!(rec.data, [0x01, 0x01, 0x01, 0x30, 0x00, 0x01, 0x00]);
        assert_eq!(rec.data[1], 0x01, "mode (cool) preserved from the bank");
        assert_eq!(rec.data[2], 0x01, "fan (low) from the client");
        assert_eq!(rec.data[3], 0x30, "target_temp_c preserved from the bank");
        assert_eq!(rec.data[5], 0x01, "fresh_air preserved from the bank");
    }

    #[test]
    fn build_write_record_sparse_typed_merge_zone_bearing() {
        // D-11: a sparse typed reg-03 write addressed to a zone merges over
        // that zone's bank value and stamps the zone into wire byte 0.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x03),
            data: [0x02, 0x64, 0x00, 0x30, 0x00, 0x00, 0x00],
        });
        let rec = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "03",
            Some(2),
            &serde_json::json!({"open": true}),
        )
        .expect("zone merge must succeed");
        assert_eq!(rec.reg, RegId::new(0x03));
        assert_eq!(rec.data[0], 2, "zone must be stamped into wire byte 0");
        assert_eq!(rec.data[1], 0xE4, "open set by client, damper preserved");
        assert_eq!(rec.data[3], 0x30, "target_temp_c preserved from the bank");
    }

    #[test]
    fn build_write_record_sparse_typed_merge_no_bank_state() {
        // D-11: a sparse typed write with no bank value for the addressed
        // register errors ack with the exact documented reason.
        let bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "05",
            None,
            &serde_json::json!({"power": "on"}),
        )
        .expect_err("no bank state must be rejected");
        assert_eq!(
            err,
            "no bank state for register 05; send a full payload or issue a read first"
        );

        // Zone-bearing register: the reason carries the addressed zone.
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "03",
            Some(2),
            &serde_json::json!({"open": true}),
        )
        .expect_err("no zone bank state must be rejected");
        assert_eq!(
            err,
            "no bank state for register 03 [zone 2]; send a full payload or issue a read first"
        );
    }

    #[test]
    fn build_write_record_sparse_typed_merge_undecodable_bank() {
        // D-11: a bank value that cannot decode to a typed DTO (unknown enum
        // byte → raw hex fallback) errors ack with the exact documented
        // reason instead of silently writing.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x05),
            data: [0x7f, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "05",
            None,
            &serde_json::json!({"power": "on"}),
        )
        .expect_err("undecodable bank must be rejected");
        assert_eq!(
            err,
            "cannot merge: bank state for register 05 is not typed; send a full payload or issue a read first"
        );
    }

    #[test]
    fn build_write_record_rejects_non_object_typed_payload() {
        // D-11: a non-string, non-object payload (number/array/bool/null)
        // must be rejected with the pre-merge BadPayload reason — never
        // silently merged over the bank and written.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        seed_reg05(&mut bank, id);
        let err = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "05",
            None,
            &serde_json::json!(123),
        )
        .expect_err("non-object typed payload must be rejected");
        assert!(
            err.starts_with("bad register payload: "),
            "expected BadPayload reason, got {err:?}"
        );
    }

    #[test]
    fn build_write_record_sparse_typed_merge_reg12_write_shape() {
        // D-11: a sparse reg-12 write `{"zone":1}` merges `sensor_uid` from
        // the bank and encodes as the write shape (the bank's read-only
        // `pairing`/`sensor_rev` fields are dropped so the codec does not
        // prefer the read shape and lose the zone).
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x12),
            data: [0x01, 0x61, 0x3d, 0x00, 0x05, 0x00, 0x00],
        });
        let rec = build_write_record(
            Some(&bank),
            Some(id),
            None,
            None,
            "12",
            None,
            &serde_json::json!({"zone": 1}),
        )
        .expect("reg-12 merge must succeed");
        assert_eq!(
            rec.data,
            [0x01, 0x61, 0x3d, 0x01, 0x00, 0x00, 0x00],
            "uid from bank, zone from client, write shape"
        );
    }
    fn aircon_reg05(unit_id: UnitId, data: [u8; 7]) -> CanRecord {
        CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id,
            reg: RegId::new(0x05),
            data,
        }
    }

    #[test]
    fn power_off_records_preserves_bytes_with_power_off() {
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        // mode 02 (heat), fan 03 (high), set 0x30, myzone 00, fresh 01, rf 00.
        bank.apply(&aircon_reg05(
            id,
            [0x01, 0x02, 0x03, 0x30, 0x00, 0x01, 0x00],
        ));

        let records = power_off_records(&bank);
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.unit_type, UnitType::AIRCON);
        assert_eq!(rec.dest, Dest::ControlBox);
        assert_eq!(rec.unit_id, id);
        assert_eq!(rec.reg, RegId::new(0x05));
        assert_eq!(
            rec.data,
            [0x00, 0x02, 0x03, 0x30, 0x00, 0x01, 0x00],
            "power byte flipped to Off, every other byte preserved"
        );
    }

    #[test]
    fn power_off_records_skips_units_without_reg05() {
        let mut bank = RegisterBank::new();
        let with_status = UnitId::try_new(0x0_00001).unwrap();
        let without_status = UnitId::try_new(0x0_00002).unwrap();
        bank.apply(&aircon_reg05(
            with_status,
            [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        ));
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: without_status,
            reg: RegId::new(0x06),
            data: [0; 7],
        });

        let records = power_off_records(&bank);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].unit_id, with_status);

        // Only reg-06 present → nothing to power off.
        let mut bank = RegisterBank::new();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: without_status,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        assert!(power_off_records(&bank).is_empty());
    }

    #[test]
    fn power_off_records_ignores_non_aircon_units() {
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });
        assert!(
            power_off_records(&bank).is_empty(),
            "a non-AIRCON unit's reg-05 status must never be powered off"
        );
    }

    #[test]
    fn power_off_records_empty_bank_yields_empty_vec() {
        let bank = RegisterBank::new();
        assert!(power_off_records(&bank).is_empty());
    }

    /// One AIRCON unit with the default feeder's reg-05 status
    /// (`[0x01,0x01,0x03,0x30,0x00,0x01,0x00]` → on/cool/high/24.0/off).
    fn single_aircon_bank() -> RegisterBank {
        let mut bank = RegisterBank::new();
        bank.apply(&aircon_reg05(
            UnitId::try_new(0x0_ABCDE).unwrap(),
            [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        ));
        bank
    }

    /// Assert `rec` is the idle-failsafe power-off record for `expected_id`:
    /// AIRCON, `ControlBox`, reg 05, power byte flipped to Off with every other
    /// byte preserved.
    fn assert_power_off(rec: &CanRecord, expected_id: UnitId, expected_data: [u8; 7]) {
        assert_eq!(rec.unit_type, UnitType::AIRCON);
        assert_eq!(rec.dest, Dest::ControlBox);
        assert_eq!(rec.unit_id, expected_id);
        assert_eq!(rec.reg, RegId::new(0x05));
        assert_eq!(
            rec.data, expected_data,
            "power byte flipped to Off, every other byte preserved"
        );
    }

    /// Test harness: a `WsState` wired to the watchdog with a held snapshot
    /// bank, `cmd_spy`, and a live client-count watch the test drives directly
    /// (no network / feeder / timing races on the snapshot).
    struct WatchdogHarness {
        clients: watch::Sender<usize>,
        spy: Arc<tokio::sync::Mutex<Vec<EngineCmd>>>,
        watchdog: tokio::task::JoinHandle<()>,
        drain: tokio::task::JoinHandle<()>,
    }

    impl WatchdogHarness {
        /// Spawn the watchdog over `bank`, returning the harness.
        fn spawn(
            bank: RegisterBank,
            timeout: std::time::Duration,
            retry_interval: std::time::Duration,
        ) -> Self {
            let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCmd>(crate::app::CHANNEL_BOUND);
            let (snapshot_tx, snapshot_rx) = watch::channel::<Option<HeldSnapshot>>(None);
            let (_status_tx, status_rx) = watch::channel::<StatusState>(StatusState::Negotiating);
            let (events_tx, _events_rx) = broadcast::channel::<WsEvent>(crate::app::CHANNEL_BOUND);
            let (clients_tx, _clients_rx) = watch::channel::<usize>(0);
            let spy = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            snapshot_tx.send_modify(|held| *held = Some(HeldSnapshot { bank }));
            // Drain the engine cmd channel so the watchdog's sends never fail.
            let drain = tokio::spawn(async move { while cmd_rx.recv().await.is_some() {} });
            let state = WsState {
                cmd_tx,
                snapshot: snapshot_rx,
                events: events_tx,
                status: status_rx,
                cmd_spy: Some(spy.clone()),
                unit_id_hint: None,
                clients: clients_tx.clone(),
                timeouts: SessionTimeouts::default(),
            };
            let watchdog = spawn_idle_watchdog(state, timeout, retry_interval);
            Self {
                clients: clients_tx,
                spy,
                watchdog,
                drain,
            }
        }

        /// Simulate one WebSocket client connecting (count > 0 disarms).
        fn connect(&self) {
            self.clients.send_modify(|count| *count += 1);
        }

        /// Simulate one WebSocket client disconnecting (count → 0 arms).
        fn disconnect(&self) {
            self.clients.send_modify(|count| *count -= 1);
        }
    }

    impl Drop for WatchdogHarness {
        fn drop(&mut self) {
            self.watchdog.abort();
            self.drain.abort();
        }
    }

    /// Every `WriteRegisters` recorded on the spy so far.
    async fn spy_writes(spy: &Arc<tokio::sync::Mutex<Vec<EngineCmd>>>) -> Vec<EngineCmd> {
        spy.lock()
            .await
            .iter()
            .filter(|cmd| matches!(cmd, EngineCmd::WriteRegisters(_)))
            .cloned()
            .collect()
    }

    /// Poll the spy until at least `min` power-off writes are recorded.
    async fn wait_for_power_offs(harness: &WatchdogHarness, min: usize) -> Vec<EngineCmd> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let writes = spy_writes(&harness.spy).await;
                if writes.len() >= min {
                    return writes;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timeout waiting for idle-failsafe power-off write(s)")
    }

    #[tokio::test]
    async fn watchdog_fires_after_disconnect_timeout() {
        // Watchdog: fires a power-off write after the timeout once the last
        // client disconnects (the >0 → 0 transition arms the failsafe).
        let harness = WatchdogHarness::spawn(
            single_aircon_bank(),
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(100),
        );
        harness.connect();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        harness.disconnect();

        let writes = wait_for_power_offs(&harness, 1).await;
        let EngineCmd::WriteRegisters(records) = &writes[0] else {
            panic!("expected WriteRegisters");
        };
        assert_eq!(records.len(), 1);
        assert_power_off(
            &records[0],
            UnitId::try_new(0x0_ABCDE).unwrap(),
            [0x00, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        );
    }

    #[tokio::test]
    async fn watchdog_fires_on_all_aircon_units() {
        // Watchdog: the failsafe fires on ALL AIRCON units present in the
        // bank — two units with reg-05 status → one power-off record each,
        // all in a single write (startup arming supplies the fire).
        let mut bank = RegisterBank::new();
        bank.apply(&aircon_reg05(
            UnitId::try_new(0x0_ABCDE).unwrap(),
            [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        ));
        bank.apply(&aircon_reg05(
            UnitId::try_new(0x0_00002).unwrap(),
            [0x01, 0x02, 0x03, 0x2e, 0x00, 0x01, 0x00],
        ));
        let harness = WatchdogHarness::spawn(
            bank,
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        );

        let writes = wait_for_power_offs(&harness, 1).await;
        let EngineCmd::WriteRegisters(records) = &writes[0] else {
            panic!("expected WriteRegisters");
        };
        assert_eq!(records.len(), 2, "both AIRCON units must be powered off");
        let mut ids: Vec<u32> = records.iter().map(|rec| rec.unit_id.get()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![0x00_0002, 0x0_ABCDE]);
        for rec in records {
            // Known fixture expectations: each unit's reg-05 status with the
            // power byte flipped On → Off and every other byte preserved.
            let (expected_id, expected_data) = match rec.unit_id.get() {
                0x0_ABCDE => (
                    UnitId::try_new(0x0_ABCDE).unwrap(),
                    [0x00, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
                ),
                0x0_00002 => (
                    UnitId::try_new(0x0_00002).unwrap(),
                    [0x00, 0x02, 0x03, 0x2e, 0x00, 0x01, 0x00],
                ),
                id => panic!("unexpected unit id {id:05x}"),
            };
            assert_power_off(rec, expected_id, expected_data);
        }
    }

    #[tokio::test]
    async fn watchdog_skips_aircon_units_without_reg05() {
        // Watchdog: AIRCON units without a reg-05 status in the bank are
        // skipped — their mode/fan bytes are unknown, so no blind off. With
        // no reg-05 in the bank at all, the fire sends nothing.
        let mut bank = RegisterBank::new();
        let with_status = UnitId::try_new(0x0_ABCDE).unwrap();
        let without_status = UnitId::try_new(0x0_00002).unwrap();
        bank.apply(&aircon_reg05(
            with_status,
            [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        ));
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: without_status,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        let harness = WatchdogHarness::spawn(
            bank,
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        );

        let writes = wait_for_power_offs(&harness, 1).await;
        let EngineCmd::WriteRegisters(records) = &writes[0] else {
            panic!("expected WriteRegisters");
        };
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].unit_id, with_status,
            "reg-05-less unit must be skipped"
        );

        // No reg-05 anywhere → the fire is a no-op: no write at all.
        let mut bank = RegisterBank::new();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: without_status,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        let harness = WatchdogHarness::spawn(
            bank,
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let writes = spy_writes(&harness.spy).await;
        assert!(
            writes.is_empty(),
            "no power-off write may fire without reg-05: {writes:?}"
        );
    }

    #[tokio::test]
    async fn watchdog_never_powers_off_non_aircon_units() {
        // Watchdog: non-AIRCON (0x08) units are never touched — their reg-05
        // status stays in the bank even when the same unit id also exists as
        // an AIRCON that IS powered off.
        let mut bank = RegisterBank::new();
        let id = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&aircon_reg05(
            id,
            [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        ));
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });
        let harness = WatchdogHarness::spawn(
            bank.clone(),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        );

        let writes = wait_for_power_offs(&harness, 1).await;
        let EngineCmd::WriteRegisters(records) = &writes[0] else {
            panic!("expected WriteRegisters");
        };
        assert_eq!(records.len(), 1, "only the AIRCON unit may be powered off");
        assert_power_off(&records[0], id, [0x00, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00]);
        assert_eq!(
            bank.get(UnitType::new(0x08), id, RegId::new(0x05)),
            Some([0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00]),
            "08-type reg-05 status must remain untouched in the bank"
        );
    }

    #[tokio::test]
    async fn watchdog_reconnect_before_deadline_disarms() {
        // Watchdog: a reconnect before the deadline disarms the failsafe —
        // no power-off write fires even after the original deadline passes.
        let harness = WatchdogHarness::spawn(
            single_aircon_bank(),
            std::time::Duration::from_millis(300),
            std::time::Duration::from_millis(100),
        );
        harness.connect();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        harness.disconnect(); // t0: 300ms deadline starts here
        tokio::time::sleep(std::time::Duration::from_millis(150)).await; // well before the deadline
        harness.connect(); // reconnect
        tokio::time::sleep(std::time::Duration::from_millis(250)).await; // t0+400 > t0+300 deadline

        let writes = spy_writes(&harness.spy).await;
        assert!(
            writes.is_empty(),
            "reconnect must disarm the failsafe: {writes:?}"
        );
    }

    #[tokio::test]
    async fn watchdog_refires_periodically_while_disconnected() {
        // Watchdog: while the count stays 0, the failsafe re-fires every
        // retry interval — at least two power-off writes are observed across
        // two intervals, each with the same preserved reg-05 payload.
        let harness = WatchdogHarness::spawn(
            single_aircon_bank(),
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(200),
        );
        harness.connect();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        harness.disconnect();

        let writes = wait_for_power_offs(&harness, 2).await;
        assert_eq!(writes.len(), 2, "expected at least two re-fires");
        for write in &writes {
            let EngineCmd::WriteRegisters(records) = write else {
                panic!("expected WriteRegisters");
            };
            assert_eq!(records.len(), 1);
            assert_power_off(
                &records[0],
                UnitId::try_new(0x0_ABCDE).unwrap(),
                [0x00, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
            );
        }
    }

    #[tokio::test]
    async fn watchdog_connected_client_never_fires() {
        // Watchdog: any connected client (even idle) keeps the failsafe
        // disarmed — no power-off write while the count stays > 0, well past
        // the timeout.
        let harness = WatchdogHarness::spawn(
            single_aircon_bank(),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(50),
        );
        harness.connect();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let writes = spy_writes(&harness.spy).await;
        assert!(
            writes.is_empty(),
            "a connected client must keep the failsafe disarmed: {writes:?}"
        );
    }

    #[tokio::test]
    async fn watchdog_arms_at_startup_when_no_client_connected() {
        // Watchdog startup arming: booting with zero clients arms immediately
        // — the failsafe fires on its own after the timeout, with no
        // >0 → 0 transition ever observed.
        let harness = WatchdogHarness::spawn(
            single_aircon_bank(),
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(100),
        );

        let writes = wait_for_power_offs(&harness, 1).await;
        let EngineCmd::WriteRegisters(records) = &writes[0] else {
            panic!("expected WriteRegisters");
        };
        assert_eq!(records.len(), 1);
        assert_power_off(
            &records[0],
            UnitId::try_new(0x0_ABCDE).unwrap(),
            [0x00, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        );
    }

    #[tokio::test]
    async fn watchdog_zero_timeout_is_disabled() {
        // Watchdog: a zero timeout disables the failsafe entirely — the task
        // exits immediately and never fires, even with zero clients at
        // startup.
        let harness = WatchdogHarness::spawn(
            single_aircon_bank(),
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(50),
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let writes = spy_writes(&harness.spy).await;
        assert!(
            writes.is_empty(),
            "timeout 0 must disable the failsafe: {writes:?}"
        );
    }

    #[test]
    fn idle_deadline_saturates_on_overflowing_duration() {
        // Regression (F2): an extreme timeout must not panic the watchdog
        // task. `idle_deadline` clamps to a representable deadline instead of
        // panicking on `Instant` checked-add overflow.
        let saturated = idle_deadline(std::time::Duration::MAX);
        let fine = idle_deadline(std::time::Duration::from_millis(100));
        assert!(
            saturated >= tokio::time::Instant::now(),
            "saturated deadline must still be in the future"
        );
        let span = fine.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            span >= std::time::Duration::from_millis(80),
            "non-overflowing deadline must land ~now + duration, got {span:?}"
        );
    }

    /// Connect a real WebSocket client to the daemon at `addr` (retry loop).
    async fn app_connect(
        addr: SocketAddr,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!("ws://{addr}/v1/mailbox-stream");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match tokio_tungstenite::connect_async(&url).await {
                    Ok((ws, _)) => return ws,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("connect timeout")
    }

    /// Wait for the snapshot frame on a real WebSocket session.
    async fn app_wait_snapshot(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Value {
        use tokio_tungstenite::tungstenite::Message as WsMsg;
        tokio::time::timeout(std::time::Duration::from_secs(6), async {
            loop {
                let msg = ws.next().await.expect("ws stream").expect("ws msg");
                match msg {
                    WsMsg::Text(text) => {
                        let msg: Value = serde_json::from_str(&text).expect("json");
                        if msg["type"] == "snapshot" {
                            return msg;
                        }
                    }
                    WsMsg::Ping(payload) => {
                        let _ = ws.send(WsMsg::Pong(payload)).await;
                    }
                    WsMsg::Close(frame) => panic!("unexpected close: {frame:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("snapshot timeout")
    }

    #[tokio::test]
    async fn watchdog_without_reg05_feeder_never_writes() {
        // Watchdog (app wiring): with a `without_reg05` feeder spec the bank
        // never sees an AIRCON reg-05 status, so even a fully armed failsafe
        // sends no power-off write — while the daemon stays alive and serving
        // (verified via a client snapshot of the 08 unit after the window).
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = crate::app::App::spawn_mock_ctrl_with_timeouts(
            bind,
            Some(crate::mock_feeder::FeederSpec::default().without_reg05()),
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(100),
        )
        .await
        .expect("spawn mock with without_reg05 spec")
        .0;
        // Window covers the startup-armed fire deadline (200ms) with margin.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let spy = handle.cmd_spy.lock().await;
        assert!(
            spy.iter()
                .all(|cmd| !matches!(cmd, EngineCmd::WriteRegisters(_))),
            "no power-off write may fire without reg-05 in the bank: {spy:?}"
        );
        drop(spy);

        // The daemon is alive and its bank holds the 08 unit only.
        let mut ws = app_connect(handle.local_addr()).await;
        let snap = app_wait_snapshot(&mut ws).await;
        assert!(
            snap["units"]["08:abcde"].is_object(),
            "08 unit missing from snapshot: {snap}"
        );
        for registers in snap["units"].as_object().unwrap().values() {
            assert!(
                !registers.as_object().unwrap().contains_key("05"),
                "reg 05 must not be synthesized: {snap}"
            );
        }

        let _ = ws.close(None).await;
        handle.shutdown().await.expect("shutdown");
    }
}
