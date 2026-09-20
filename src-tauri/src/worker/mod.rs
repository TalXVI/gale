//! The standalone sync worker (`gale-worker`).
//!
//! An always-on process that deploys canonical profile publications to a
//! dedicated server, sharing the exact same planner, staging, state, and
//! lease machinery as the desktop's Local mode. `api` defines the wire
//! contract the desktop's worker client also uses; `server` — gated behind
//! the `worker` cargo feature — is the runnable binary's HTTP API and
//! automatic-sync poll loop.

pub(crate) mod api;

#[cfg(feature = "worker")]
pub(crate) mod config;
#[cfg(feature = "worker")]
pub(crate) mod journal;
#[cfg(feature = "worker")]
pub(crate) mod sync_client;

#[cfg(feature = "worker")]
pub(crate) mod server;

/// The worker binary's entry point: loads `config_path`, then serves the
/// API and runs the publication poll loop until the process exits.
///
/// This is the crate's only public worker surface — deliberately a
/// filesystem path rather than the config type, so `gale-worker` links no
/// internal profile types.
#[cfg(feature = "worker")]
pub async fn run(config_path: &std::path::Path) -> eyre::Result<()> {
    let config = config::WorkerConfig::load(config_path)?;
    server::run(config).await
}
