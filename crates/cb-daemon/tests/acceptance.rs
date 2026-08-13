//! Acceptance tests for issue #9 (D8): mock backend WebSocket bridge.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::time::Duration;

use aa_engine::EngineCmd;
use aa_link::AOA_DEFAULT_PATH;
use aa_registers::{UnitId, UnitType};
use cb_daemon::{App, Backend, FeederSpec, SessionTimeouts, mock_backend_avoids_accessory};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_daemon() -> cb_daemon::AppHandle {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    App::spawn_mock(bind).await.expect("spawn mock daemon")
}

async fn connect_ws(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/v1/mailbox-stream");
    timeout(Duration::from_secs(5), async {
        loop {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((ws, _)) => return ws,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("connect timeout")
}

async fn recv_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    timeout(Duration::from_secs(5), async {
        loop {
            let msg = ws.next().await.expect("ws stream").expect("ws msg");
            match msg {
                Message::Text(text) => {
                    return serde_json::from_str(&text).expect("json");
                }
                Message::Ping(p) => {
                    ws.send(Message::Pong(p)).await.ok();
                }
                Message::Close(frame) => {
                    panic!("unexpected close: {frame:?}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("recv timeout")
}

/// Read one JSON text message; `None` on close / EOF / error (pings ponged).
///
/// For watcher loops that must distinguish "closed" from "no message yet" —
/// callers wrap with their own `timeout`.
async fn recv_json_or_close(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<Value> {
    loop {
        let Some(Ok(msg)) = ws.next().await else {
            return None;
        };
        match msg {
            Message::Text(text) => return serde_json::from_str(&text).ok(),
            Message::Ping(p) => {
                ws.send(Message::Pong(p)).await.ok();
            }
            _ => {}
        }
    }
}

/// D-5: a `read` request; `msg_id` and `register` are the only varying fields
/// across the read tests.
fn reg_read(msg_id: &str, register: &str) -> Value {
    json!({
        "type": "read",
        "msg_id": msg_id,
        "register": register
    })
}

/// Reg-05 (aircon) write request; the `msg_id` is the only varying field
/// across the write tests. The payload is raw hex (byte-exact passthrough):
/// D-6 rejects typed reg-05 writes carrying the read-only `rf_sys_id` field,
/// so these plumbing tests write the wire bytes directly.
fn reg05_write(msg_id: &str) -> Value {
    json!({
        "type": "write",
        "msg_id": msg_id,
        "register": "05",
        "payload": "0101032e000100"
    })
}

/// Wait until a JSON message of type `ty` arrives; panics on close/timeout.
///
/// Tolerates interleaved `status` frames (D-8): sessions send the
/// connect-time `status` before the snapshot, and transitions may be
/// broadcast between other messages.
async fn wait_for_type(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ty: &str,
) -> Value {
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(ws)
                .await
                .expect("closed before message arrived");
            if msg["type"] == ty {
                return msg;
            }
        }
    })
    .await
    .expect("message timeout")
}

/// Wait until the ack for `msg_id` arrives; panics on close/timeout.
async fn wait_for_ack(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    msg_id: &str,
) -> Value {
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(ws)
                .await
                .expect("closed before ack arrived");
            if msg["type"] == "ack" && msg["msg_id"] == msg_id {
                return msg;
            }
        }
    })
    .await
    .expect("ack timeout")
}

/// Wait until the reg-`register` event from `unit_id` arrives; panics on
/// close/timeout.
async fn wait_for_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    register: &str,
    unit_id: &str,
) -> Value {
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(ws)
                .await
                .expect("closed before event arrived");
            if msg["type"] == "event" && msg["register"] == register {
                assert_eq!(msg["unit_id"], unit_id);
                return msg;
            }
        }
    })
    .await
    .expect("event timeout")
}

/// Wait until the reg-`register` event from `unit_id` arrives, skipping
/// same-register events broadcast by other units; panics on close/timeout.
///
/// For events whose unit id must distinguish the sender (e.g. the scripted
/// JZ18 reply, which shares register 07 with the dump-phase announcement
/// echoes).
async fn wait_for_event_from(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    register: &str,
    unit_id: &str,
) -> Value {
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(ws)
                .await
                .expect("closed before event arrived");
            if msg["type"] == "event" && msg["register"] == register && msg["unit_id"] == unit_id {
                return msg;
            }
        }
    })
    .await
    .expect("event timeout")
}

/// Wait until the `read_result` for `msg_id` arrives; panics on close/timeout.
///
/// Tolerates interleaved `status`, `event`, and other `read_result` frames
/// (D-5): the flush's `getCAN` may also fan out `RegistersChanged` events
/// before the correlated `read_result`.
async fn wait_for_read_result(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    msg_id: &str,
) -> Value {
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(ws)
                .await
                .expect("closed before read_result arrived");
            if msg["type"] == "read_result" && msg["msg_id"] == msg_id {
                return msg;
            }
        }
    })
    .await
    .expect("read_result timeout")
}

#[tokio::test]
async fn mock_backend_ws_receives_mailbox_snapshot() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;

    // D-8: the connect-time `status` frame precedes the snapshot.
    let msg = wait_for_type(&mut ws, "snapshot").await;
    // Multi-unit map keyed "{unit_type}:{unit_id}" (D-3): the mock feeder
    // round-trips the 07 sample dump and the 08-type flush dump.
    // Mock feeder dump delivers reg 05 (system status) for `abcde`:
    // [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00] → on/cool/high/24.0/off.
    let reg05 = &msg["units"]["07:abcde"]["05"];
    assert!(reg05.is_object(), "reg 05 missing from snapshot: {msg}");
    assert_eq!(reg05["power"], "on");
    assert_eq!(reg05["mode"], "cool");
    assert_eq!(reg05["fan"], "high");
    assert_eq!(reg05["target_temp_c"], 24.0);
    assert_eq!(reg05["fresh_air"], "off");
    assert_eq!(reg05["rf_sys_id"], 0);
    // 08-type record from the flush-dump reply must surface as its own unit.
    let reg06 = &msg["units"]["08:abcde"]["06"];
    assert!(reg06.is_object(), "reg 06 missing from 08 unit: {msg}");
    assert_eq!(reg06["fw_major"], 0);

    handle.shutdown().await.expect("shutdown");
}

/// Holder disconnects before Snapshot → no registry/gate: a second client
/// must connect and stay connected (no close frame at all).
#[tokio::test]
async fn disconnect_before_snapshot_second_client_stays_connected() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let handle = App::spawn_mock_without_feeder(bind)
        .await
        .expect("spawn without feeder");
    let addr = handle.local_addr();

    let mut first = connect_ws(addr).await;
    // No Snapshot will arrive — give the handler time to enter wait_for_snapshot.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = first.close(None).await;
    // Drain until the server observes close / stream ends.
    let _ = timeout(Duration::from_secs(2), async {
        while first.next().await.is_some() {}
    })
    .await;

    // No gate: a second client connects and stays connected. There is no
    // feeder, so no Snapshot ever arrives — the assertion is that the session
    // is simply never rejected with a close.
    let url = format!("ws://{addr}/v1/mailbox-stream");
    let (mut second, _) = timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(&url),
    )
    .await
    .expect("connect timeout")
    .expect("second connect");

    let closed = timeout(Duration::from_secs(1), async {
        loop {
            match second.next().await {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return true,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;

    assert!(
        !matches!(closed, Ok(true)),
        "second client was closed — session must not gate/reject without a Snapshot"
    );

    let _ = second.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// Two clients connect concurrently: each gets its own full snapshot, and
/// neither is closed (a close would panic inside `recv_json`).
#[tokio::test]
async fn two_clients_both_receive_snapshot() {
    let handle = spawn_daemon().await;
    let addr = handle.local_addr();
    let mut a = connect_ws(addr).await;
    let mut b = connect_ws(addr).await;

    let snap_a = wait_for_type(&mut a, "snapshot").await;
    assert!(snap_a["units"]["07:abcde"].is_object());

    let snap_b = wait_for_type(&mut b, "snapshot").await;
    assert!(snap_b["units"]["07:abcde"].is_object());

    let _ = a.close(None).await;
    let _ = b.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A write from client A must be seen by both sessions: A gets its success
/// ack and the reg-05 event, B gets the reg-05 event (CB echo via mock
/// feeder → `RegistersChanged` fan-out). Ack and event may arrive in any order.
#[tokio::test]
async fn event_fanout_to_all_clients() {
    let handle = spawn_daemon().await;
    let addr = handle.local_addr();
    let mut a = connect_ws(addr).await;
    let mut b = connect_ws(addr).await;
    let _ = recv_json(&mut a).await;
    let _ = recv_json(&mut b).await;

    let update = reg05_write("req-fanout");
    a.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();

    let ack = wait_for_ack(&mut a, "req-fanout").await;
    assert_eq!(ack["status"], "success", "A write ack: {ack}");
    // Both sessions must see the reg-05 event (CB echo → RegistersChanged
    // fan-out). Each session has its own broadcast receiver, so A's event is
    // buffered while B's is awaited below.
    let ev_a = wait_for_event(&mut a, "05", "abcde").await;
    let ev_b = wait_for_event(&mut b, "05", "abcde").await;
    assert_eq!(ev_a["unit_id"], ev_b["unit_id"]);

    let _ = a.close(None).await;
    let _ = b.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// Two back-to-back writes (distinct `msg_ids`) must BOTH be acked success —
/// whether the mock ping cadence batches them into one setCAN TX or two —
/// with no timeout/error ack for either within a generous window.
#[tokio::test]
async fn batch_ack_two_writes() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(reg05_write("req-b1").to_string().into()))
        .await
        .unwrap();
    ws.send(Message::Text(reg05_write("req-b2").to_string().into()))
        .await
        .unwrap();

    timeout(Duration::from_secs(6), async {
        let mut b1_ok = false;
        let mut b2_ok = false;
        loop {
            let msg = recv_json_or_close(&mut ws)
                .await
                .expect("closed before batch acks");
            match (msg["type"].as_str(), msg["msg_id"].as_str()) {
                (Some("ack"), Some("req-b1")) if msg["status"] == "success" => b1_ok = true,
                (Some("ack"), Some("req-b2")) if msg["status"] == "success" => b2_ok = true,
                (Some("ack"), Some(id @ ("req-b1" | "req-b2"))) => {
                    panic!("batch write {id} acked non-success: {msg}");
                }
                _ => {}
            }
            if b1_ok && b2_ok {
                return;
            }
        }
    })
    .await
    .expect("batch ack timeout (both writes must be acked success)");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// Closing client A must end only A's session: B keeps receiving acks and
/// events and is never closed or errored.
#[tokio::test]
async fn disconnect_isolation() {
    let handle = spawn_daemon().await;
    let addr = handle.local_addr();
    let mut a = connect_ws(addr).await;
    let mut b = connect_ws(addr).await;
    let _ = wait_for_type(&mut a, "snapshot").await;
    let _ = wait_for_type(&mut b, "snapshot").await;

    let _ = a.close(None).await;
    // Drain until the server observes A's close / stream ends.
    let _ = timeout(Duration::from_secs(2), async {
        while a.next().await.is_some() {}
    })
    .await;

    // B must still work end-to-end: write → success ack + reg-05 event.
    let update = reg05_write("req-iso");
    b.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();

    let ack = wait_for_ack(&mut b, "req-iso").await;
    assert_eq!(ack["status"], "success", "B write ack: {ack}");
    let _ = wait_for_event(&mut b, "05", "abcde").await;

    let _ = b.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn mailbox_write_and_resync_reach_engine() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = reg05_write("req-101");
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = recv_json(&mut ws).await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["msg_id"], "req-101");
    assert_eq!(ack["status"], "success");

    let resync = json!({
        "type": "command",
        "msg_id": "req-201",
        "action": "resync"
    });
    ws.send(Message::Text(resync.to_string().into()))
        .await
        .unwrap();
    let ack2 = recv_json(&mut ws).await;
    assert_eq!(ack2["type"], "ack");
    assert_eq!(ack2["msg_id"], "req-201");
    assert_eq!(ack2["status"], "success");

    timeout(Duration::from_secs(3), async {
        loop {
            let spy = handle.cmd_spy.lock().await;
            let has_write = spy
                .iter()
                .any(|c| matches!(c, EngineCmd::WriteRegisters(_)));
            let has_resync = spy.iter().any(|c| matches!(c, EngineCmd::ResyncMailbox));
            if has_write && has_resync {
                return;
            }
            drop(spy);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cmds not observed on spy");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

#[test]
fn mock_backend_does_not_open_usb_accessory() {
    assert_eq!(AOA_DEFAULT_PATH, "/dev/usb_accessory");
    assert!(mock_backend_avoids_accessory(Backend::Mock));
    // Default config uses mock — structural guarantee that mock path never
    // selects AoaLink / `/dev/usb_accessory`.
    let cfg = cb_daemon::Config::default();
    assert_eq!(cfg.backend, Backend::Mock);
}

/// M1 regression: a zone-bearing write (regs 03/04) must stamp the client's
/// zone into the wire zone byte (`data[0]`) so the frame addresses the zone.
#[tokio::test]
async fn write_zone_bearing_register_stamps_wire_zone() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = json!({
        "type": "write",
        "msg_id": "req-301",
        "register": "03",
        "zone": 2,
        "payload": "00e40230170100"
    });
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = recv_json(&mut ws).await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["msg_id"], "req-301");
    assert_eq!(ack["status"], "success");

    timeout(Duration::from_secs(3), async {
        loop {
            let spy = handle.cmd_spy.lock().await;
            if let Some(cmd) = spy
                .iter()
                .find(|c| matches!(c, EngineCmd::WriteRegisters(_)))
            {
                match cmd {
                    EngineCmd::WriteRegisters(records) => {
                        assert_eq!(records.len(), 1);
                        let rec = &records[0];
                        assert_eq!(rec.reg.get(), 0x03);
                        assert_eq!(rec.data[0], 2, "zone must be stamped into wire byte 0");
                        assert_eq!(rec.data[1], 0xE4, "payload bytes must be untouched");
                        return;
                    }
                    _ => unreachable!("spy only records WriteRegisters here"),
                }
            }
            drop(spy);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("zone write not observed on spy");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// M1 guard: a `zone` on a non-zone-bearing write must be ignored — the zone
/// byte belongs to the register's payload, not the CAN address.
#[tokio::test]
async fn write_non_zone_register_ignores_zone() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = json!({
        "type": "write",
        "msg_id": "req-302",
        "register": "05",
        "zone": 9,
        "payload": "01010330000100"
    });
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = recv_json(&mut ws).await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["status"], "success");

    timeout(Duration::from_secs(3), async {
        loop {
            let spy = handle.cmd_spy.lock().await;
            if let Some(cmd) = spy
                .iter()
                .find(|c| matches!(c, EngineCmd::WriteRegisters(_)))
            {
                match cmd {
                    EngineCmd::WriteRegisters(records) => {
                        let rec = &records[0];
                        assert_eq!(rec.reg.get(), 0x05);
                        assert_eq!(rec.data[0], 0x01, "power byte must not be overwritten");
                        return;
                    }
                    _ => unreachable!("spy only records WriteRegisters here"),
                }
            }
            drop(spy);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("reg-05 write not observed on spy");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-8: the first frame every client receives is a `status` frame with the
/// current session state (connect-time health), before any snapshot. The
/// state is whatever the watch currently holds — `negotiating` when the
/// client joins during negotiation, `synced` if the initial dump already
/// landed (both are valid wire values).
#[tokio::test]
async fn status_first_message_on_connect() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;

    let msg = recv_json(&mut ws).await;
    assert_eq!(msg["type"], "status");
    assert!(
        matches!(
            msg["state"].as_str(),
            Some("negotiating" | "synced" | "resyncing" | "link_down")
        ),
        "unexpected status state: {msg}"
    );

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-8: a client connected during negotiation observes `negotiating`, then
/// `synced` once the initial dump snapshot lands. `synced` is mandatory; a
/// `negotiating` frame is expected unless the dump already completed before
/// the connection (then the connect-time status is legitimately `synced`
/// and no `negotiating` transition is observable on that session).
#[tokio::test]
async fn status_negotiating_then_synced() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;

    let mut saw_negotiating = false;
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(&mut ws)
                .await
                .expect("closed before synced status");
            if msg["type"] != "status" {
                continue;
            }
            match msg["state"].as_str() {
                Some("negotiating") => saw_negotiating = true,
                Some("synced") => return,
                Some(other) => panic!("unexpected status state: {other}: {msg}"),
                None => panic!("status without state: {msg}"),
            }
        }
    })
    .await
    .expect("synced status timeout");

    if !saw_negotiating {
        // Join-after-sync: the connect-time status was already `synced` —
        // the snapshot must still be delivered to reach bridge mode.
        let _ = wait_for_type(&mut ws, "snapshot").await;
    }

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-8: a `resync` command re-enters the dump path: `resyncing` on command
/// apply, then `synced` once the fresh snapshot lands.
#[tokio::test]
async fn status_resync_emits_resyncing_then_synced() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    // Bridge mode (status frames only reach sessions past the snapshot wait).
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let resync = json!({
        "type": "command",
        "msg_id": "req-resync",
        "action": "resync"
    });
    ws.send(Message::Text(resync.to_string().into()))
        .await
        .unwrap();

    let mut saw_resyncing = false;
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(&mut ws)
                .await
                .expect("closed before resync statuses");
            if msg["type"] != "status" {
                continue;
            }
            match msg["state"].as_str() {
                Some("resyncing") => saw_resyncing = true,
                Some("synced") if saw_resyncing => return,
                // Pre-resync `synced` / any other frame: keep waiting.
                Some(_) => {}
                None => panic!("status without state: {msg}"),
            }
        }
    })
    .await
    .expect("resyncing → synced statuses timeout");
    assert!(saw_resyncing);

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-8: forcing the mock link closed drives the engine's
/// `SessionState(LinkDown)` → `LinkError` path; clients receive a
/// `link_down` status whose `detail` carries the link error string
/// (the `SessionState` broadcast carries `detail: None` first).
#[tokio::test]
async fn status_link_down_on_forced_link_close() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (handle, ctrl) = App::spawn_mock_ctrl(bind, true)
        .await
        .expect("spawn mock with ctrl");
    let mut ws = connect_ws(handle.local_addr()).await;
    // Bridge mode: the fan-out only reaches sessions past the snapshot wait.
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ctrl.close().await;

    let status = timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(&mut ws)
                .await
                .expect("closed before link_down status");
            if msg["type"] == "status" && msg["state"] == "link_down" && msg["detail"].is_string() {
                return msg;
            }
        }
    })
    .await
    .expect("link_down status with detail timeout");
    assert_eq!(status["detail"], "link closed");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-8: a client connecting after the mailbox is in sync immediately receives
/// a `synced` status on connect — no waiting for the next transition.
#[tokio::test]
async fn status_late_client_receives_current_synced() {
    let handle = spawn_daemon().await;
    let addr = handle.local_addr();
    let mut a = connect_ws(addr).await;
    // Wait until A has observed the `synced` transition (the status watch
    // now holds Synced, so B's connect-time read is deterministic).
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(&mut a)
                .await
                .expect("closed before synced status");
            if msg["type"] == "status" && msg["state"] == "synced" {
                return;
            }
        }
    })
    .await
    .expect("synced status timeout");

    let mut b = connect_ws(addr).await;
    let first = recv_json(&mut b).await;
    assert_eq!(first["type"], "status");
    assert_eq!(
        first["state"], "synced",
        "late client connect-time status: {first}"
    );

    let _ = a.close(None).await;
    let _ = b.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-5: a read of a known register (05) resolves on the flush's `getCAN` to
/// a typed `read_result` correlated by the client's `msg_id`. The mock feeder
/// answers the reg-06 flush with the full sample set, so the bank serves reg
/// 05's current value ([0x01,0x01,0x03,0x30,0x00,0x01,0x00] → on/cool/high/
/// 24.0/off). Interleaved status/event frames are tolerated by the wait loop.
#[tokio::test]
async fn read_known_register_returns_typed_read_result() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg_read("req-read-01", "05").to_string().into(),
    ))
    .await
    .unwrap();
    let result = wait_for_read_result(&mut ws, "req-read-01").await;
    assert_eq!(result["unit_type"], "07");
    assert_eq!(result["unit_id"], "abcde");
    assert_eq!(result["register"], "05");
    let payload = &result["payload"];
    assert!(
        payload.is_object(),
        "known register payload must be typed: {result}"
    );
    assert_eq!(payload["power"], "on");
    assert_eq!(payload["mode"], "cool");
    assert_eq!(payload["fan"], "high");
    assert_eq!(payload["target_temp_c"], 24.0);
    assert_eq!(payload["fresh_air"], "off");
    assert_eq!(payload["rf_sys_id"], 0);

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-5: a read of an unknown register (16) returns a raw 14-char lowercase hex
/// payload — the flush's `getCAN` carries the mock's reg-16 record, and the
/// unknown-register codec falls back to the raw hex string (no DTO exists).
#[tokio::test]
async fn read_unknown_register_returns_raw_hex() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg_read("req-read-02", "16").to_string().into(),
    ))
    .await
    .unwrap();
    let result = wait_for_read_result(&mut ws, "req-read-02").await;
    assert_eq!(result["register"], "16");
    assert_eq!(result["unit_type"], "07");
    assert_eq!(result["unit_id"], "abcde");
    assert_eq!(
        result["payload"], "deadbeef010203",
        "unknown register must decode to raw hex: {result}"
    );

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-5: a read of the write-only register (09) is rejected up front by the
/// read policy — no flush, no `getCAN` round-trip.
#[tokio::test]
async fn read_write_only_register_rejected() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg_read("req-read-03", "09").to_string().into(),
    ))
    .await
    .unwrap();
    let ack = wait_for_ack(&mut ws, "req-read-03").await;
    assert_eq!(ack["status"], "error", "reg-09 read ack: {ack}");
    assert_eq!(ack["reason"], "register 09 is write-only");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-5: a read of the internal register (07) is rejected up front by the read
/// policy — no flush, no `getCAN` round-trip.
#[tokio::test]
async fn read_internal_register_rejected() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg_read("req-read-04", "07").to_string().into(),
    ))
    .await
    .unwrap();
    let ack = wait_for_ack(&mut ws, "req-read-04").await;
    assert_eq!(ack["status"], "error", "reg-07 read ack: {ack}");
    assert_eq!(ack["reason"], "register 07 is handled internally");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-5: a zone-bearing read (03) with an explicit `zone` resolves that zone's
/// bank slot. The mock flush reply carries no reg 03, so a reg-03 write for
/// zone 2 first populates the bank's zone-2 slot (CB echo); the wait for the
/// write's `RegistersChanged` event guarantees the echo has landed before the
/// read's flush resolves against the bank.
#[tokio::test]
async fn read_zone_bearing_register_with_zone_returns_zone_value() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = json!({
        "type": "write",
        "msg_id": "req-read-zone-write",
        "register": "03",
        "zone": 2,
        "payload": "00e40230170100"
    });
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-read-zone-write").await;
    assert_eq!(ack["status"], "success", "zone write ack: {ack}");
    let _ = wait_for_event(&mut ws, "03", "abcde").await;

    let read = json!({
        "type": "read",
        "msg_id": "req-read-05",
        "register": "03",
        "zone": 2
    });
    ws.send(Message::Text(read.to_string().into()))
        .await
        .unwrap();
    let result = wait_for_read_result(&mut ws, "req-read-05").await;
    assert_eq!(result["unit_type"], "07");
    assert_eq!(result["unit_id"], "abcde");
    assert_eq!(result["register"], "03");
    assert_eq!(result["zone"], 2, "zone read result: {result}");
    let payload = &result["payload"];
    assert!(payload.is_object(), "zone payload must be typed: {result}");
    assert_eq!(payload["open"], true);
    assert_eq!(payload["damper_pct"], 100);

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-5: a zone-bearing read (03) without a `zone` is rejected up front — the
/// bank keys zone-bearing registers by zone byte, so no zone means no lookup.
#[tokio::test]
async fn read_zone_bearing_register_without_zone_rejected() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg_read("req-read-06", "03").to_string().into(),
    ))
    .await
    .unwrap();
    let ack = wait_for_ack(&mut ws, "req-read-06").await;
    assert_eq!(ack["status"], "error", "zone-less reg-03 read ack: {ack}");
    assert_eq!(ack["reason"], "zone required for register 03");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-5: a read of a register the mock CB never reports (02) resolves on the
/// flush's `getCAN` with `data: None` — the engine acks "register 02 has no
/// value" rather than the 5s deadline ("read timeout"). The read's own flush
/// never carries reg 02, so this is deterministic (~sub-second). The 5s
/// timeout path itself is unit-tested (`pending_reads_drain_expired_returns_
/// only_dead_entries`); the mock feeder always answers a flush, so it cannot
/// be exercised end-to-end.
#[tokio::test]
async fn read_unreported_register_acks_has_no_value() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg_read("req-read-07", "02").to_string().into(),
    ))
    .await
    .unwrap();
    // Own timeout loop: the read deadline (5s) is only the upper bound — the
    // mock resolves the flush well within it, so the ack should land fast.
    let ack = timeout(Duration::from_secs(8), async {
        loop {
            let msg = recv_json_or_close(&mut ws)
                .await
                .expect("closed before read ack");
            if msg["type"] == "ack" && msg["msg_id"] == "req-read-07" {
                return msg;
            }
        }
    })
    .await
    .expect("read ack timeout");
    assert_eq!(ack["status"], "error", "reg-02 read ack: {ack}");
    assert_eq!(ack["reason"], "register 02 has no value");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// Wait until a `link_down` status carrying a string `detail` arrives; panics
/// on close/timeout.
///
/// The engine emits `SessionState(LinkDown)` (detail-less) before `LinkError`,
/// so the detail-carrying re-broadcast must be waited for (D-8).
async fn wait_for_link_down(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    timeout(Duration::from_secs(6), async {
        loop {
            let msg = recv_json_or_close(ws)
                .await
                .expect("closed before link_down status");
            if msg["type"] == "status" && msg["state"] == "link_down" && msg["detail"].is_string() {
                return msg;
            }
        }
    })
    .await
    .expect("link_down status with detail timeout")
}

/// A1 (T4): three clients connect concurrently to one daemon; none is closed
/// (a close would panic inside `recv_json`) and each receives a full
/// `snapshot` covering both the 07 (reg 05) and 08 (reg 06) units of the mock
/// feeder's `abcde` unit id.
#[tokio::test]
async fn three_clients_all_receive_snapshot_and_none_rejected() {
    let handle = spawn_daemon().await;
    let addr = handle.local_addr();
    let mut a = connect_ws(addr).await;
    let mut b = connect_ws(addr).await;
    let mut c = connect_ws(addr).await;

    for ws in [&mut a, &mut b, &mut c] {
        let snap = wait_for_type(ws, "snapshot").await;
        let reg05 = &snap["units"]["07:abcde"]["05"];
        assert!(reg05.is_object(), "reg 05 missing from snapshot: {snap}");
        assert_eq!(reg05["power"], "on");
        let reg06 = &snap["units"]["08:abcde"]["06"];
        assert!(reg06.is_object(), "reg 06 missing from snapshot: {snap}");
        assert_eq!(reg06["fw_major"], 0);
    }

    let _ = a.close(None).await;
    let _ = b.close(None).await;
    let _ = c.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A2 (T2): every write-policy violation path returns the exact error-ack
/// reason over the wire: read-only register (08), read-only field
/// (`rf_sys_id` on 05), internal register (07), unverified register (0b),
/// and an out-of-range typed value (reg 01 `total_zones` > 10). Each ack
/// carries type "ack", status "error", the matching `msg_id`, and the exact
/// reason; the connection stays healthy — a follow-up write is acked success.
#[tokio::test]
async fn write_policy_error_acks_exact_reasons() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let cases: [(&str, &str, Value, &str); 5] = [
        (
            "req-a2-1",
            "08",
            json!("00000000000000"),
            "register 08 is read-only",
        ),
        (
            "req-a2-2",
            "05",
            json!({"rf_sys_id": 1}),
            "field 'rf_sys_id' is read-only on register 05",
        ),
        (
            "req-a2-3",
            "07",
            json!("00000000000000"),
            "register 07 is handled internally",
        ),
        (
            "req-a2-4",
            "0b",
            json!("00000000000000"),
            "register 0b is unverified; writes not permitted",
        ),
        (
            "req-a2-5",
            "05",
            json!({"myzone_id": 11}),
            "field 'myzone' 11 out of range (max 10)",
        ),
    ];
    for (msg_id, register, payload, reason) in cases {
        let req = json!({
            "type": "write",
            "msg_id": msg_id,
            "register": register,
            "payload": payload,
        });
        ws.send(Message::Text(req.to_string().into()))
            .await
            .unwrap();
        let ack = wait_for_ack(&mut ws, msg_id).await;
        assert_eq!(ack["type"], "ack");
        assert_eq!(ack["status"], "error", "{msg_id} ack: {ack}");
        assert_eq!(ack["reason"], reason, "{msg_id} ack: {ack}");
    }

    // Connection stays healthy after every error: a follow-up write acks success.
    ws.send(Message::Text(
        reg05_write("req-a2-health").to_string().into(),
    ))
    .await
    .unwrap();
    let ack = wait_for_ack(&mut ws, "req-a2-health").await;
    assert_eq!(ack["status"], "success", "post-error write ack: {ack}");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A3 (T5): a scripted unsolicited reg-06 announcement from a distinctive
/// unit (type `0a`, id `fedcb` — never emitted by the default dump, which
/// only announces 08:abcde) triggers the engine's JZ18 handshake: the
/// all-zero reg-07 setCAN reply echoes the announcement's unit type + id,
/// is echoed back by the mock feeder as a getCAN record, and surfaces as an
/// `event` frame for register 07 addressed to `0a:fedcb` with the raw hex
/// payload "00000000000000". The dump-phase 08:abcde reg-07 echoes arrive
/// first and are skipped by `wait_for_event_from`, so the assertion can only
/// match the scripted injection's reply. Interleaved status frames are
/// tolerated.
#[tokio::test]
async fn jz18_scripted_reg06_announcement_replies_reg07() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let spec = FeederSpec::default().scripted_reg06(
        UnitType::new(0x0A),
        UnitId::try_new(0x0_FEDCB).unwrap(),
        [0; 7],
    );
    let handle = App::spawn_mock_with_spec(bind, spec)
        .await
        .expect("spawn mock with spec");
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let event = wait_for_event_from(&mut ws, "07", "fedcb").await;
    assert_eq!(event["unit_type"], "0a", "JZ18 reply echo: {event}");
    assert_eq!(
        event["payload"], "00000000000000",
        "JZ18 reply echo: {event}"
    );

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A4 (T5): a write addressed with explicit `unit_type: "08"` and
/// `unit_id: "abcde"` is acked success and its broadcast `event` echo carries
/// the same 08 type and unit id (multi-unit write echo).
#[tokio::test]
async fn multi_unit_write_to_08_unit_echoes_type_08() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = json!({
        "type": "write",
        "msg_id": "req-a4-08",
        "unit_type": "08",
        "unit_id": "abcde",
        "register": "05",
        "payload": "0101032e000100"
    });
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-a4-08").await;
    assert_eq!(ack["status"], "success", "08-unit write ack: {ack}");
    let event = wait_for_event(&mut ws, "05", "abcde").await;
    assert_eq!(event["unit_type"], "08", "08-unit write echo: {event}");
    assert_eq!(event["unit_id"], "abcde");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A5 (T5): with a `without_reg05()` feeder spec the dump never carries
/// reg 05, so the snapshot contains the 08 unit but no reg-05 key under any
/// unit — and no `system_status` DTO (`rf_sys_id` is unique to it) anywhere.
#[tokio::test]
async fn no_synthesis_snapshot_without_reg05() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let handle = App::spawn_mock_with_spec(bind, FeederSpec::default().without_reg05())
        .await
        .expect("spawn mock without reg05");
    let mut ws = connect_ws(handle.local_addr()).await;

    let snap = wait_for_type(&mut ws, "snapshot").await;
    let reg06 = &snap["units"]["08:abcde"]["06"];
    assert!(
        reg06.is_object(),
        "08 unit reg 06 missing from snapshot: {snap}"
    );
    for registers in snap["units"].as_object().unwrap().values() {
        assert!(
            !registers.as_object().unwrap().contains_key("05"),
            "reg 05 must not be synthesized: {snap}"
        );
    }
    assert!(
        !snap.to_string().contains("rf_sys_id"),
        "no system_status DTO may appear: {snap}"
    );

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A6 (T4): a forced link close broadcasts the `link_down` status to every
/// connected client — all three synced sessions observe the frame.
#[tokio::test]
async fn status_transition_broadcast_to_all_clients() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (handle, ctrl) = App::spawn_mock_ctrl(bind, true)
        .await
        .expect("spawn mock with ctrl");
    let addr = handle.local_addr();
    let mut a = connect_ws(addr).await;
    let mut b = connect_ws(addr).await;
    let mut c = connect_ws(addr).await;
    let _ = wait_for_type(&mut a, "snapshot").await;
    let _ = wait_for_type(&mut b, "snapshot").await;
    let _ = wait_for_type(&mut c, "snapshot").await;

    ctrl.close().await;

    let sa = wait_for_link_down(&mut a).await;
    assert_eq!(sa["detail"], "link closed");
    let sb = wait_for_link_down(&mut b).await;
    assert_eq!(sb["detail"], "link closed");
    let sc = wait_for_link_down(&mut c).await;
    assert_eq!(sc["detail"], "link closed");

    let _ = a.close(None).await;
    let _ = b.close(None).await;
    let _ = c.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A7 (T3): a binary WebSocket frame is answered with an `error` frame
/// "binary frames not supported" and the connection stays usable — a
/// follow-up write is acked success.
#[tokio::test]
async fn binary_frame_rejected_with_error() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Binary(vec![0x00, 0x01, 0x02].into()))
        .await
        .unwrap();
    let err = wait_for_type(&mut ws, "error").await;
    assert_eq!(err["message"], "binary frames not supported");

    ws.send(Message::Text(reg05_write("req-a7").to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-a7").await;
    assert_eq!(ack["status"], "success", "post-binary write ack: {ack}");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A8 (T3): malformed text is answered with an `error` frame
/// "invalid client message" carrying a serde `reason`, and the connection
/// stays usable — a follow-up write is acked success.
#[tokio::test]
async fn invalid_json_rejected_with_error() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text("{not json".into())).await.unwrap();
    let err = wait_for_type(&mut ws, "error").await;
    assert_eq!(err["message"], "invalid client message");
    assert!(err["reason"].is_string(), "serde reason missing: {err}");

    ws.send(Message::Text(reg05_write("req-a8").to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-a8").await;
    assert_eq!(ack["status"], "success", "post-error write ack: {ack}");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// A9 (T3): an unknown command action returns the exact "unknown action: …"
/// error ack, and the connection stays usable afterwards — a follow-up write
/// is acked success.
#[tokio::test]
async fn unknown_command_returns_error_ack_and_connection_stays_usable() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let bogus = json!({
        "type": "command",
        "msg_id": "req-a9-bogus",
        "action": "defragment"
    });
    ws.send(Message::Text(bogus.to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-a9-bogus").await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["status"], "error", "unknown action ack: {ack}");
    assert_eq!(ack["reason"], "unknown action: defragment");

    ws.send(Message::Text(
        reg05_write("req-a9-health").to_string().into(),
    ))
    .await
    .unwrap();
    let ack = wait_for_ack(&mut ws, "req-a9-health").await;
    assert_eq!(ack["status"], "success", "post-command write ack: {ack}");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-11: a sparse typed write on reg 05 merges over the bank's current value
/// (the mock dump seeds reg 05 for `07:abcde` with
/// [0x01,0x01,0x03,0x30,0x00,0x01,0x00] → on/cool/high/24.0/off). The client
/// sends only `{"fan":"low"}`; the ack is success and the write's `event`
/// echo carries the merged DTO: `fan` from the client, every other field
/// preserved from the bank. A follow-up read still resolves a typed reg-05
/// payload with the preserved fields intact.
///
/// Note on the read's `fan`: the mock feeder answers a reg-06 flush (the
/// read's unit-scoped flush) with its canned sample set, which includes the
/// original reg-05 bytes; that getCAN overwrites the bank's reg-05 slot
/// before the read resolves, so the read's `fan` reflects the canned sample
/// rather than the merged write. The merged state is therefore asserted on
/// the `event` echo (the authoritative end-to-end evidence of the merge).
#[tokio::test]
async fn sparse_typed_write_on_reg05_merges_over_bank() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = json!({
        "type": "write",
        "msg_id": "req-d11-1",
        "register": "05",
        "payload": {"fan": "low"}
    });
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-d11-1").await;
    assert_eq!(ack["status"], "success", "sparse merge write ack: {ack}");

    // The mock feeder echoes the written record back as a getCAN; the `event`
    // fan-out carries the merged DTO — the authoritative end-to-end evidence
    // of the sparse merge.
    let ev = wait_for_event(&mut ws, "05", "abcde").await;
    let payload = &ev["payload"];
    assert!(payload.is_object(), "event payload must be typed: {ev}");
    assert_eq!(payload["fan"], "low", "fan from client: {ev}");
    assert_eq!(payload["power"], "on", "power preserved from bank: {ev}");
    assert_eq!(payload["mode"], "cool", "mode preserved from bank: {ev}");
    assert_eq!(
        payload["target_temp_c"], 24.0,
        "target_temp_c preserved from bank: {ev}"
    );
    assert_eq!(
        payload["fresh_air"], "off",
        "fresh_air preserved from bank: {ev}"
    );
    assert_eq!(
        payload["rf_sys_id"], 0,
        "rf_sys_id preserved from bank: {ev}"
    );

    // The read path still resolves a typed reg-05 payload with the preserved
    // fields intact (see the note above on the mock's canned flush reply).
    ws.send(Message::Text(
        reg_read("req-d11-read", "05").to_string().into(),
    ))
    .await
    .unwrap();
    let result = wait_for_read_result(&mut ws, "req-d11-read").await;
    assert_eq!(result["unit_type"], "07");
    assert_eq!(result["unit_id"], "abcde");
    assert_eq!(result["register"], "05");
    let payload = &result["payload"];
    assert!(
        payload.is_object(),
        "known register payload must be typed: {result}"
    );
    assert_eq!(payload["power"], "on", "power preserved: {result}");
    assert_eq!(payload["mode"], "cool", "mode preserved: {result}");
    assert_eq!(
        payload["target_temp_c"], 24.0,
        "target_temp_c preserved: {result}"
    );
    assert_eq!(payload["fresh_air"], "off", "fresh_air preserved: {result}");
    assert_eq!(payload["rf_sys_id"], 0, "rf_sys_id preserved: {result}");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-11: a sparse typed write on a register/zone the bank has no state for
/// (reg 03 is zone-bearing and absent from the default dump) is rejected with
/// the documented no-bank reason — never silently written as zeros.
#[tokio::test]
async fn sparse_typed_write_with_no_bank_state_errors() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = json!({
        "type": "write",
        "msg_id": "req-d11-2",
        "register": "03",
        "zone": 2,
        "payload": {"open": true}
    });
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-d11-2").await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["status"], "error", "no-bank sparse write ack: {ack}");
    assert_eq!(
        ack["reason"],
        "no bank state for register 03 [zone 2]; send a full payload or issue a read first",
        "no-bank sparse write ack: {ack}"
    );

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-11: a sparse typed write carrying a read-only field is rejected — the
/// read-only check applies to the client-sent subset, so `rf_sys_id` on reg
/// 05 errors even though the bank's decoded DTO legitimately carries it.
#[tokio::test]
async fn sparse_typed_write_client_sent_read_only_field_errors() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    let update = json!({
        "type": "write",
        "msg_id": "req-d11-3",
        "register": "05",
        "payload": {"rf_sys_id": 1}
    });
    ws.send(Message::Text(update.to_string().into()))
        .await
        .unwrap();
    let ack = wait_for_ack(&mut ws, "req-d11-3").await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["status"], "error", "read-only sparse write ack: {ack}");
    assert_eq!(
        ack["reason"], "field 'rf_sys_id' is read-only on register 05",
        "read-only sparse write ack: {ack}"
    );

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}
/// A10 (T6): idle-failsafe watchdog end-to-end. A real WS client connects,
/// receives the snapshot, disconnects; with the (short) idle timeout armed
/// by the >0 → 0 transition, the daemon queues a power-off write for the
/// AIRCON unit — observed on the `cmd_spy` with reg-05 bytes preserved and
/// power = Off. The client never writes, so the first `WriteRegisters` on the
/// spy can only be the failsafe's.
#[tokio::test]
async fn idle_failsafe_powers_off_after_client_disconnect() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let handle =
        App::spawn_mock_with_timeouts(bind, Duration::from_millis(150), Duration::from_millis(100))
            .await
            .expect("spawn mock with short failsafe timeouts");
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;
    let _ = ws.close(None).await;
    // Drain until the server observes the close / stream ends (the session
    // count decrement arms the watchdog from this moment).
    let _ = timeout(Duration::from_secs(2), async {
        while ws.next().await.is_some() {}
    })
    .await;

    timeout(Duration::from_secs(6), async {
        loop {
            let spy = handle.cmd_spy.lock().await;
            if let Some(cmd) = spy
                .iter()
                .find(|cmd| matches!(cmd, EngineCmd::WriteRegisters(_)))
            {
                match cmd {
                    EngineCmd::WriteRegisters(records) => {
                        assert_eq!(records.len(), 1, "one AIRCON unit in the bank");
                        let rec = &records[0];
                        assert_eq!(rec.unit_type, UnitType::AIRCON);
                        assert_eq!(rec.unit_id, UnitId::try_new(0x0_ABCDE).unwrap());
                        assert_eq!(rec.reg.get(), 0x05);
                        assert_eq!(
                            rec.data,
                            [0x00, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
                            "power byte flipped to Off, every other byte preserved"
                        );
                        return;
                    }
                    _ => unreachable!("spy only records WriteRegisters here"),
                }
            }
            drop(spy);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("idle-failsafe power-off write not observed on spy");

    handle.shutdown().await.expect("shutdown");
}

/// D-12: on a silent bus (feeder stops after the dump; the engine never
/// flushes), a write's ack must time out into an error ack — and the timeout
/// ack is the *next* frame after the write: no status, event, or
/// `read_result` traffic interleaves on the dead bus.
#[tokio::test]
async fn silent_bus_write_times_out() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (handle, _ctrl) = App::spawn_mock_ctrl_with_session_timeouts(
        bind,
        Some(FeederSpec::default().stop_after_dump()),
        Duration::from_mins(1),
        Duration::from_mins(1),
        SessionTimeouts {
            write_ack: Duration::from_millis(400),
            read: Duration::from_millis(300),
        },
    )
    .await
    .expect("spawn mock with short session timeouts");
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg05_write("req-silent-w").to_string().into(),
    ))
    .await
    .unwrap();

    // The very next frame must be the timeout ack ("without any other
    // traffic"); wait_for_ack guards against a close, `timeout` bounds the
    // window, and the final asserts pin status/reason.
    let ack = timeout(Duration::from_secs(2), async {
        let msg = recv_json_or_close(&mut ws)
            .await
            .expect("closed before write timeout ack");
        assert_eq!(
            msg["type"], "ack",
            "silent bus must not interleave {msg:?} before the timeout ack"
        );
        msg
    })
    .await
    .expect("write timeout ack missing");
    assert_eq!(ack["msg_id"], "req-silent-w", "write timeout ack: {ack}");
    assert_eq!(ack["status"], "error", "write timeout ack: {ack}");
    assert_eq!(ack["reason"], "write timeout", "write timeout ack: {ack}");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-12: on a silent bus, a read's deadline expires into an error ack too —
/// the read never resolves (no flush `getCAN` reply) and must surface as
/// "read timeout" carrying the read's `msg_id`.
#[tokio::test]
async fn silent_bus_read_times_out() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (handle, _ctrl) = App::spawn_mock_ctrl_with_session_timeouts(
        bind,
        Some(FeederSpec::default().stop_after_dump()),
        Duration::from_mins(1),
        Duration::from_mins(1),
        SessionTimeouts {
            write_ack: Duration::from_millis(400),
            read: Duration::from_millis(300),
        },
    )
    .await
    .expect("spawn mock with short session timeouts");
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(
        reg_read("req-silent-r", "05").to_string().into(),
    ))
    .await
    .unwrap();

    let ack = timeout(Duration::from_secs(2), async {
        let msg = recv_json_or_close(&mut ws)
            .await
            .expect("closed before read timeout ack");
        assert_eq!(
            msg["type"], "ack",
            "silent bus must not interleave {msg:?} before the read timeout ack"
        );
        msg
    })
    .await
    .expect("read timeout ack missing");
    assert_eq!(ack["msg_id"], "req-silent-r", "read timeout ack: {ack}");
    assert_eq!(ack["status"], "error", "read timeout ack: {ack}");
    assert_eq!(ack["reason"], "read timeout", "read timeout ack: {ack}");

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// D-12 positive control: short session timeouts must NOT fire on a live bus.
/// A write acks success and a read resolves to a `read_result` well inside
/// the (`400 ms` / `300 ms`) deadlines, and neither `msg_id` is ever
/// double-acked with a timeout error afterwards.
#[tokio::test]
async fn short_timeouts_normal_traffic_not_double_acked() {
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (handle, _ctrl) = App::spawn_mock_ctrl_with_session_timeouts(
        bind,
        Some(FeederSpec::default()),
        Duration::from_mins(1),
        Duration::from_mins(1),
        SessionTimeouts {
            write_ack: Duration::from_millis(400),
            read: Duration::from_millis(300),
        },
    )
    .await
    .expect("spawn mock with short session timeouts");
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = wait_for_type(&mut ws, "snapshot").await;

    ws.send(Message::Text(reg05_write("req-fast-w").to_string().into()))
        .await
        .unwrap();
    ws.send(Message::Text(
        reg_read("req-fast-r", "05").to_string().into(),
    ))
    .await
    .unwrap();

    let mut write_ok = false;
    let mut read_ok = false;
    timeout(Duration::from_secs(2), async {
        loop {
            let msg = recv_json_or_close(&mut ws)
                .await
                .expect("closed before live-bus results");
            match (msg["type"].as_str(), msg["msg_id"].as_str()) {
                (Some("ack"), Some("req-fast-w")) => {
                    assert_eq!(msg["status"], "success", "fast write ack: {msg}");
                    write_ok = true;
                }
                (Some("ack"), Some("req-fast-r")) => {
                    panic!("read acked instead of resolving to read_result: {msg}");
                }
                (Some("read_result"), Some("req-fast-r")) => read_ok = true,
                _ => {}
            }
            if write_ok && read_ok {
                return;
            }
        }
    })
    .await
    .expect("fast write+read not both resolved within 2s");
    assert!(write_ok, "write must ack success on live bus");
    assert!(read_ok, "read must resolve to read_result on live bus");

    // Negative check: neither msg_id may be acked again (timeout or otherwise)
    // within a quiet window beyond its deadline.
    let late = timeout(Duration::from_millis(500), async {
        loop {
            let msg = recv_json_or_close(&mut ws)
                .await
                .expect("closed during negative window");
            if msg["type"] == "ack"
                && let Some(id @ ("req-fast-w" | "req-fast-r")) = msg["msg_id"].as_str()
            {
                return Some(id.to_owned());
            }
        }
    })
    .await;
    assert!(
        late.is_err(),
        "double-ack for an already-resolved msg_id on live bus: {late:?}"
    );

    let _ = ws.close(None).await;
    handle.shutdown().await.expect("shutdown");
}
