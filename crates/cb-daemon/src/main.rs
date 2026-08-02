//! Binary entry for `cb-daemon`.

use anyhow::Context;

use cb_daemon::{init_tracing, load_config, run};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = load_config().context("load configuration")?;
    init_tracing(&config.log_level).context("init tracing")?;
    run(config).await.context("cb-daemon failed")
}
