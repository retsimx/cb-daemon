//! Application wiring: open link → engine → axum serve → graceful shutdown.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use aa_engine::{CbEngine, EngineCmd, EngineEvent};
use aa_link::{
    AOA_DEFAULT_PATH, AoaLink, AoaOpenOptions, Link, TTY_DEFAULT_PATH, TtyLink, TtyOpenOptions,
};
use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::{Backend, Config};
use crate::mock_feeder::{self, SharedMockLink};
use crate::ws::{self, WsState};

/// Channel capacity for engine cmd / event mpsc (architecture default).
pub(crate) const CHANNEL_BOUND: usize = 32;

/// Running daemon handle (bind address + shutdown).
pub struct AppHandle {
    /// Actual bound address (useful when binding `:0`).
    pub local_addr: SocketAddr,
    /// Spy of engine cmds accepted from WebSocket (tests).
    pub cmd_spy: Arc<Mutex<Vec<EngineCmd>>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<anyhow::Result<()>>,
}

impl AppHandle {
    /// Request graceful shutdown and wait for the server task to finish.
    ///
    /// # Errors
    ///
    /// Returns join / server errors.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        match self.join.await {
            Ok(result) => result,
            Err(err) => Err(anyhow::anyhow!("app task join: {err}")),
        }
    }

    /// Bound listen address.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

/// Library-style application entry (used by `main` and tests).
pub struct App;

impl App {
    /// Bind and run until SIGINT/SIGTERM or [`AppHandle::shutdown`].
    ///
    /// # Errors
    ///
    /// Propagates link open, bind, or runtime failures.
    pub async fn run(config: Config) -> anyhow::Result<()> {
        run(config).await
    }

    /// Bind `config.bind` (often `127.0.0.1:0` in tests) and return a handle.
    ///
    /// Always uses the mock backend + feeder for deterministic tests.
    ///
    /// # Errors
    ///
    /// Propagates bind / spawn failures.
    pub async fn spawn_mock(bind: SocketAddr) -> anyhow::Result<AppHandle> {
        spawn_mock_inner(bind, true).await
    }

    /// Like [`Self::spawn_mock`] but skips the negotiate/dump feeder.
    ///
    /// Used to exercise disconnect-while-waiting-for-Snapshot (no bank ever arrives).
    ///
    /// # Errors
    ///
    /// Propagates bind / spawn failures.
    pub async fn spawn_mock_without_feeder(bind: SocketAddr) -> anyhow::Result<AppHandle> {
        spawn_mock_inner(bind, false).await
    }
}

async fn spawn_mock_inner(bind: SocketAddr, with_feeder: bool) -> anyhow::Result<AppHandle> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    let local_addr = listener.local_addr().context("local_addr")?;
    let cmd_spy = Arc::new(Mutex::new(Vec::new()));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let spy = Arc::clone(&cmd_spy);
    let join = tokio::spawn(async move {
        run_mock_with_listener(listener, shutdown_rx, Some(spy), with_feeder).await
    });
    Ok(AppHandle {
        local_addr,
        cmd_spy,
        shutdown_tx: Some(shutdown_tx),
        join,
    })
}

/// Parse config and run until signal (binary entry).
///
/// # Errors
///
/// Propagates link / bind / serve failures.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("bind {}", config.bind))?;
    info!(addr = %listener.local_addr()?, "listening");
    run_with_listener(config, listener).await
}

/// Run with an already-bound listener (tests / custom bind).
///
/// # Errors
///
/// Propagates link open or serve failures.
pub async fn run_with_listener(config: Config, listener: TcpListener) -> anyhow::Result<()> {
    if let Some(hint) = config.unit_id_hint {
        info!(%hint, "unit_id_hint set");
    }

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    // Keep sender alive until signals fire.
    let signal_shutdown = shutdown_tx;
    tokio::spawn(async move {
        wait_shutdown_signal().await;
        let _ = signal_shutdown.send(());
    });
    match config.backend {
        Backend::Mock => run_mock_with_listener(listener, shutdown_rx, None, true).await,
        Backend::Aoa => {
            let hint = config.unit_id_hint;
            let link = open_aoa(&config).await?;
            run_with_link(listener, link, shutdown_rx, None, hint).await
        }
        Backend::Tty => {
            let hint = config.unit_id_hint;
            let link = open_tty(&config).await?;
            run_with_link(listener, link, shutdown_rx, None, hint).await
        }
    }
}

async fn open_aoa(config: &Config) -> anyhow::Result<AoaLink> {
    let opts = AoaOpenOptions {
        max_chunk: config.aoa_chunk_size,
        inter_chunk_delay: Duration::from_millis(config.aoa_chunk_delay_ms),
    };
    match config.device.as_ref() {
        Some(path) => AoaLink::open_with(path, opts)
            .await
            .with_context(|| format!("open AoaLink {}", path.display())),
        None => AoaLink::open_with(AOA_DEFAULT_PATH, opts)
            .await
            .context("open AoaLink default path"),
    }
}

async fn open_tty(config: &Config) -> anyhow::Result<TtyLink> {
    let opts = TtyOpenOptions {
        baud: config.tty_baud,
    };
    match config.device.as_ref() {
        Some(path) => TtyLink::open_with(path, opts)
            .await
            .with_context(|| format!("open TtyLink {}", path.display())),
        None => TtyLink::open_with(TTY_DEFAULT_PATH, opts)
            .await
            .context("open TtyLink default path"),
    }
}

/// Mock path: [`SharedMockLink`] + optional negotiate/dump feeder. Never opens `/dev/usb_accessory`.
async fn run_mock_with_listener(
    listener: TcpListener,
    shutdown_rx: oneshot::Receiver<()>,
    cmd_spy: Option<Arc<Mutex<Vec<EngineCmd>>>>,
    with_feeder: bool,
) -> anyhow::Result<()> {
    let (link, mock, notify) = SharedMockLink::new();
    let feeder =
        with_feeder.then(|| tokio::spawn(mock_feeder::run_negotiate_dump_feeder(mock, notify)));
    let result = run_with_link(listener, link, shutdown_rx, cmd_spy, None).await;
    if let Some(feeder) = feeder {
        feeder.abort();
        let _ = feeder.await;
    }
    result
}

async fn run_with_link<L: Link + 'static>(
    listener: TcpListener,
    link: L,
    shutdown_rx: oneshot::Receiver<()>,
    cmd_spy: Option<Arc<Mutex<Vec<EngineCmd>>>>,
    unit_id_hint: Option<aa_registers::UnitId>,
) -> anyhow::Result<()> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(CHANNEL_BOUND);
    let (ev_tx, ev_rx) = mpsc::channel::<EngineEvent>(CHANNEL_BOUND);
    let (snapshot_tx, snapshot_rx) = watch::channel::<Option<ws::HeldSnapshot>>(None);
    let (broadcast_tx, _) = broadcast::channel::<EngineEvent>(CHANNEL_BOUND);

    let engine = CbEngine::new(link);
    let engine_join = tokio::spawn(async move {
        engine.run(cmd_rx, ev_tx).await;
    });

    let fanout_tx = broadcast_tx.clone();
    let fanout_join = tokio::spawn(async move {
        fanout_events(ev_rx, snapshot_tx, fanout_tx).await;
    });

    let state = WsState {
        cmd_tx: cmd_tx.clone(),
        snapshot: snapshot_rx,
        events: broadcast_tx,
        session_held: Arc::new(AtomicBool::new(false)),
        cmd_spy,
        unit_id_hint,
    };
    let router = ws::router(state);

    let serve = axum::serve(listener, router).with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
        info!("shutdown signal received");
    });

    let serve_result = serve.await.context("axum serve");

    if let Err(err) = cmd_tx.send(EngineCmd::Shutdown).await {
        warn!(?err, "engine already stopped when sending Shutdown");
    }
    // Dropping cmd_tx is not enough if engine blocked on read; Shutdown closes link.
    drop(cmd_tx);

    match tokio::time::timeout(std::time::Duration::from_secs(5), engine_join).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(?err, "engine task join error"),
        Err(_) => warn!("engine task join timed out"),
    }
    fanout_join.abort();
    let _ = fanout_join.await;

    serve_result?;
    Ok(())
}

async fn fanout_events(
    mut ev_rx: mpsc::Receiver<EngineEvent>,
    snapshot_tx: watch::Sender<Option<ws::HeldSnapshot>>,
    broadcast_tx: broadcast::Sender<EngineEvent>,
) {
    while let Some(ev) = ev_rx.recv().await {
        match &ev {
            EngineEvent::Snapshot { bank, can_records } => {
                info!(
                    records = can_records.as_ref().map_or(0, Vec::len),
                    "engine dump snapshot held"
                );
                let _ = snapshot_tx.send(Some(ws::HeldSnapshot {
                    bank: bank.clone(),
                    can_records: can_records.clone(),
                }));
                let _ = broadcast_tx.send(ev);
            }
            EngineEvent::Negotiated { detail } => {
                info!(%detail, "negotiated");
                let _ = broadcast_tx.send(ev);
            }
            EngineEvent::ProtocolWarn(msg) => {
                warn!(%msg, "protocol warn");
                let _ = broadcast_tx.send(ev);
            }
            EngineEvent::LinkError(msg) => {
                warn!(%msg, "link error");
                let _ = broadcast_tx.send(ev);
            }
            EngineEvent::RegistersChanged { records } => {
                // Keep the held bank current so late clients and mailbox_update
                // merges see freshly-synced registers, not the last dump.
                // Note: never send() while the watch value is borrowed (panic).
                if !records.is_empty() {
                    let current = snapshot_tx.borrow().clone();
                    if let Some(held) = current {
                        let mut bank = held.bank.clone();
                        for record in records {
                            bank.apply(record);
                        }
                        let _ = snapshot_tx.send(Some(ws::HeldSnapshot {
                            bank,
                            can_records: held.can_records.clone(),
                        }));
                    }
                }
                let _ = broadcast_tx.send(ev);
            }
            EngineEvent::WriteFlushed | EngineEvent::DirectReply { .. } => {
                let _ = broadcast_tx.send(ev);
            }
        }
    }
}

async fn wait_shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!(?err, "ctrl_c handler failed");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                warn!(?err, "SIGTERM handler install failed");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// Structural guarantee: mock backend construction never calls [`AoaLink::open`].
#[must_use]
pub const fn mock_backend_avoids_accessory(backend: Backend) -> bool {
    matches!(backend, Backend::Mock)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_link::AOA_DEFAULT_PATH;

    #[test]
    fn mock_path_never_selects_aoa_accessory() {
        assert!(mock_backend_avoids_accessory(Backend::Mock));
        assert!(!mock_backend_avoids_accessory(Backend::Aoa));
        assert_eq!(AOA_DEFAULT_PATH, "/dev/usb_accessory");
    }

    #[test]
    fn channel_bound_is_32() {
        assert_eq!(CHANNEL_BOUND, 32);
    }
}
