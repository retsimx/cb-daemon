//! CB protocol engine: sync session core + async link runner.
//!
//! Owns negotiation, register dump, steady poll, outbound write queue, and
//! [`EngineEvent`] emission over a [`aa_link::Link`].

#![allow(clippy::redundant_pub_crate)]

mod event;
mod runner;
mod session;
mod wire;

pub use event::{EngineCmd, EngineEvent, SessionState};
pub use runner::CbEngine;
