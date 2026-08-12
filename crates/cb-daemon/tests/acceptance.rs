//! Acceptance tests for issue #9 (D8): mock backend WebSocket bridge.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::time::Duration;

use aa_engine::EngineCmd;
use aa_link::AOA_DEFAULT_PATH;
use cb_daemon::{App, Backend, mock_backend_avoids_accessory};
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
    assert_eq!(reg05["fresh_air"], false);
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
    assert_eq!(payload["fresh_air"], false);
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
