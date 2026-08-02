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
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

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

#[tokio::test]
async fn mock_backend_ws_receives_mailbox_snapshot() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;

    let msg = recv_json(&mut ws).await;
    assert_eq!(msg["type"], "mailbox_snapshot");
    assert_eq!(msg["unit_id"], "abcde");
    assert!(msg.get("system_status").is_some());

    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn second_client_gets_close_4009() {
    let handle = spawn_daemon().await;
    let addr = handle.local_addr();
    let mut first = connect_ws(addr).await;
    let _ = recv_json(&mut first).await; // snapshot — session held

    let url = format!("ws://{addr}/v1/mailbox-stream");
    let (mut second, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("second connect");

    let close = timeout(Duration::from_secs(3), async {
        loop {
            match second.next().await {
                Some(Ok(Message::Close(Some(frame)))) => return frame,
                Some(Ok(Message::Close(None))) => panic!("close without frame"),
                Some(Ok(_)) => {}
                Some(Err(err)) => panic!("ws error: {err}"),
                None => panic!("stream ended without close"),
            }
        }
    })
    .await
    .expect("close timeout");

    assert_eq!(close.code, CloseCode::Library(4009));
    assert_eq!(close.reason, "Single client limit enforced");

    let _ = first.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

/// Holder disconnects before Snapshot → session gate must release (QA MEDIUM).
#[tokio::test]
async fn disconnect_before_snapshot_releases_session_gate() {
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

    // Gate must be free: a second client must not receive 4009.
    let url = format!("ws://{addr}/v1/mailbox-stream");
    let (mut second, _) = timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(&url),
    )
    .await
    .expect("connect timeout")
    .expect("second connect");

    let rejected = timeout(Duration::from_millis(500), async {
        loop {
            match second.next().await {
                Some(Ok(Message::Close(Some(frame)))) if frame.code == CloseCode::Library(4009) => {
                    return true;
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return false,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;

    assert!(
        !matches!(rejected, Ok(true)),
        "second client got 4009 — session gate stuck after disconnect before Snapshot"
    );

    let _ = second.close(None).await;
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn mailbox_update_and_resync_reach_engine() {
    let handle = spawn_daemon().await;
    let mut ws = connect_ws(handle.local_addr()).await;
    let _ = recv_json(&mut ws).await;

    let update = json!({
        "type": "mailbox_update",
        "msg_id": "req-101",
        "register": "system_status",
        "payload": {
            "power": "on",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 23.0,
            "myzone_id": 0,
            "fresh_air": false
        }
    });
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
        "action": "resync_mailbox"
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
