//! Control Box mailbox sync daemon library entry points.
//!
//! Prefer [`run`] / [`Config`] from integration tests rather than shelling out
//! to the binary.

#![allow(clippy::redundant_pub_crate)]

mod app;
mod config;
mod mock_feeder;
mod ws;

pub use app::{App, AppHandle, mock_backend_avoids_accessory, run, run_with_listener};
pub use config::{Backend, Config};
