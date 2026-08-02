//! CLI configuration for the daemon.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Default bind address (local-only, architecture default).
pub(crate) const DEFAULT_BIND: &str = "127.0.0.1:2026";

/// Link backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    /// In-memory [`aa_link::MockLink`] + negotiate/dump feeder (never opens accessory).
    Mock,
    /// Raw `/dev/usb_accessory` ([`aa_link::AoaLink`]).
    Aoa,
    /// USB-serial / USB-RS485 TTY ([`aa_link::TtyLink`]).
    Tty,
}

/// Command-line options for `cb-daemon` (D8: CLI only, no TOML).
#[derive(Debug, Clone, Parser)]
#[command(name = "cb-daemon", about = "Control Box mailbox sync daemon")]
pub struct Config {
    /// Byte-link backend.
    #[arg(long, value_enum, default_value_t = Backend::Mock)]
    pub backend: Backend,

    /// Device path for `aoa` / `tty` (crate defaults when omitted).
    #[arg(long)]
    pub device: Option<PathBuf>,

    /// HTTP / WebSocket bind address.
    #[arg(long, default_value = DEFAULT_BIND)]
    pub bind: SocketAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Mock,
            device: None,
            // DEFAULT_BIND is a valid socket addr literal.
            bind: DEFAULT_BIND
                .parse()
                .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 2026))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_mock_and_local_bind() {
        let cfg = Config::default();
        assert_eq!(cfg.backend, Backend::Mock);
        assert_eq!(cfg.bind, "127.0.0.1:2026".parse().unwrap());
        assert!(cfg.device.is_none());
    }

    #[test]
    fn mock_is_not_aoa_backend() {
        assert_ne!(Backend::Mock, Backend::Aoa);
        assert_ne!(Backend::Mock, Backend::Tty);
    }
}
