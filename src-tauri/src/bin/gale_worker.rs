//! `gale-worker`: the standalone dedicated-server sync worker.
//!
//! Usage:
//!
//! ```text
//! gale-worker --config ./gale-worker.json
//! ```
//!
//! Secrets are read from the environment or the config's `secretsFile`,
//! never from the config itself: `GALE_WORKER_TOKEN`,
//! `GALE_WORKER_REMOTE_PASSWORD`, `GALE_WORKER_REFRESH_TOKEN`,
//! `GALE_WORKER_DATHOST_PASSWORD`.
//!
//! On Windows the binary doubles as its own service manager:
//!
//! ```text
//! gale-worker service --config X --log-file Y   # entrypoint the SCM launches
//! gale-worker service install --from <dir>      # elevated provisioning
//! gale-worker service reinstall                 # elevated binary/config swap
//! gale-worker service uninstall                 # elevated removal
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

#[derive(Parser)]
#[command(name = "gale-worker", about = "Gale dedicated-server sync worker")]
struct Args {
    /// Path to the worker configuration file.
    #[arg(long, default_value = "gale-worker.json")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Windows service mode. `service` alone is the entrypoint the SCM
    /// launches; `install`/`uninstall` are the elevated management steps.
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: Option<ServiceAction>,
        /// Path to the worker configuration file the service runs with.
        #[arg(long, default_value = "gale-worker.json")]
        config: PathBuf,
        /// File the service writes its log to.
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
}

#[cfg(windows)]
#[derive(Subcommand)]
enum ServiceAction {
    /// Install the GaleWorker service from a prepared staging directory.
    Install {
        /// Directory holding the staged `gale-worker.json` and `secrets.env`.
        #[arg(long)]
        from: PathBuf,
        /// File progress and errors are written to.
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Stop and remove the GaleWorker service and its state directory.
    Uninstall {
        /// File progress and errors are written to.
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Re-register the service from the installed config and swap in
    /// this binary. Used to roll out an updated worker without
    /// reprovisioning credentials.
    Reinstall {
        /// File progress and errors are written to.
        #[arg(long)]
        log: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    // rustls' provider auto-detection is ambiguous in this build (ring
    // and aws-lc-rs are both compiled in), and nothing else in this
    // process installs a default. Pick one before any TLS use — FTPS
    // sessions, HTTPS publication fetches, and package downloads all
    // build ClientConfigs that would otherwise panic.
    gale::install_crypto_provider();

    let args = Args::parse();

    #[cfg(windows)]
    if let Some(Command::Service {
        action,
        config,
        log_file,
    }) = args.command
    {
        return match action {
            None => {
                let log = log_file.unwrap_or_else(|| PathBuf::from("worker.log"));
                gale::worker::service::run_as_service(config, log)
            }
            Some(ServiceAction::Install { from, log }) => gale::worker::service::install(
                &from,
                &log.unwrap_or_else(|| from.join("install.log")),
            ),
            Some(ServiceAction::Uninstall { log }) => gale::worker::service::uninstall(
                &log.unwrap_or_else(|| PathBuf::from("uninstall.log")),
            ),
            Some(ServiceAction::Reinstall { log }) => gale::worker::service::reinstall(
                &log.unwrap_or_else(|| PathBuf::from("reinstall.log")),
            ),
        };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(gale::worker::run(&args.config))
}
