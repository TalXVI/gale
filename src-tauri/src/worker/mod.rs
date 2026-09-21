//! The standalone sync worker (`gale-worker`).
//!
//! An always-on process that deploys canonical profile publications to a
//! dedicated server. It uses the exact same planner, staging, state, and
//! lease machinery as the desktop's Local mode. `api` defines the wire
//! contract the desktop's worker client also uses; `config` and `secrets`
//! are ungated because the desktop provisions managed-worker installs with
//! them. `server`, built only with the `worker` cargo feature, is the
//! runnable binary's HTTP API and automatic-sync poll loop.

pub(crate) mod api;
/// Config-file contract — the worker runtime loads it, and the Windows
/// desktop writes it when provisioning the managed service.
#[cfg(any(windows, feature = "worker"))]
pub(crate) mod config;
/// Shared layout constants for the managed Windows service.
#[cfg(windows)]
pub(crate) mod local;
/// Used by the worker at runtime and by the desktop's provisioning on
/// Windows; dead code elsewhere.
#[cfg(any(windows, feature = "worker"))]
pub(crate) mod secrets;

#[cfg(feature = "worker")]
pub(crate) mod journal;
#[cfg(feature = "worker")]
pub(crate) mod sync_client;

#[cfg(feature = "worker")]
pub(crate) mod server;

#[cfg(all(windows, feature = "worker"))]
pub mod service;

/// The worker binary's entry point for a foreground run. Loads
/// `config_path`, resolves secrets, then serves the API and runs the
/// publication poll loop until Ctrl-C or a fatal error.
///
/// It takes a filesystem path rather than the config type on purpose, so
/// `gale-worker` links no internal profile types.
#[cfg(feature = "worker")]
pub async fn run(config_path: &std::path::Path) -> eyre::Result<()> {
    let config = config::WorkerConfig::load(config_path)?;
    let secrets = secrets::Secrets::resolve(&config)?;

    let shutdown = tokio_util::sync::CancellationToken::new();
    let token = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            token.cancel();
        }
    });

    let result = server::run(config.clone(), secrets, shutdown).await;
    server::report_run_state(&config, api::WorkerRunPhase::Stopped);
    result
}
