//! Binary entry for `cb-daemon`.

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cb_daemon::{Config, run};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let config = Config::parse();
    run(config).await.context("cb-daemon failed")
}
