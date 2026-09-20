//! `gale-worker`: the standalone dedicated-server sync worker.
//!
//! Usage:
//!
//! ```text
//! gale-worker --config ./gale-worker.json
//! ```
//!
//! Secrets are read from the environment, never from the config file:
//! `GALE_WORKER_TOKEN`, `GALE_WORKER_REMOTE_PASSWORD`,
//! `GALE_WORKER_REFRESH_TOKEN`, `GALE_WORKER_DATHOST_PASSWORD`.

use std::path::PathBuf;

use clap::Parser;
use eyre::Result;

#[derive(Parser)]
#[command(name = "gale-worker", about = "Gale dedicated-server sync worker")]
struct Args {
    /// Path to the worker configuration file.
    #[arg(long, default_value = "gale-worker.json")]
    config: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(gale::worker::run(&args.config))
}
