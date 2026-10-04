use eyre::{Result, ensure};
use tauri::AppHandle;

use crate::state::ManagerExt;

pub mod commands;

pub(crate) mod args;
pub(crate) mod dedicated;
pub(crate) mod engine;
pub(crate) mod executor;
pub(crate) mod host;
pub(crate) mod lease;
pub(crate) mod local;
pub(crate) mod local_worker;
pub(crate) mod paths;
pub(crate) mod plan;
pub(crate) mod progress;
pub(crate) mod remote;
pub(crate) mod runtime;
pub(crate) mod secrets;
pub(crate) mod service;
pub(crate) mod settings;
pub(crate) mod settings_store;
pub(crate) mod spec;
pub(crate) mod stage;
pub(crate) mod state;
pub(crate) mod worker_client;

/// Refuses to change a profile that a running dedicated server loads its
/// mods from.
///
/// Call this while holding the manager lock, and keep holding it through the
/// change. `launch_dedicated_server` registers the server under that same
/// lock, so a launch cannot slip in between the check and the write.
pub(crate) fn ensure_profile_unlocked(app: &AppHandle, profile_id: i64) -> Result<()> {
    let runtime = app.lock_server_runtime();

    ensure!(
        !runtime.is_profile_locked(profile_id),
        "this profile is currently in use by a dedicated server"
    );

    Ok(())
}
