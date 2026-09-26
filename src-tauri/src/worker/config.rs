//! Worker configuration file (`gale-worker.json`).
//!
//! Secrets are kept out of this file. They come from the environment
//! (`GALE_WORKER_TOKEN` and friends) or from a `KEY=value` file referenced
//! by `secretsFile`, which is how the Gale-managed Windows service passes
//! them in — see `worker::secrets`.
//!
//! Example:
//!
//! ```json
//! {
//!   "workerId": "my-vps",
//!   "listen": "127.0.0.1:8472",
//!   "profileId": "sync-profile-uuid",
//!   "game": "valheim",
//!   "remote": {
//!     "protocol": "sftp",
//!     "host": "example.dathost.net",
//!     "port": 8822,
//!     "username": "user",
//!     "serverDirectory": "/",
//!     "authentication": "password",
//!     "trustedHostKey": "SHA256:..."
//!   },
//!   "hostControl": {
//!     "provider": "datHost",
//!     "datHostServerId": "...",
//!     "datHostUsername": "email@example.com"
//!   },
//!   "autoSync": true,
//!   "autoMods": true,
//!   "restartPolicy": "whenEmpty",
//!   "pollIntervalSecs": 300,
//!   "stateDir": "."
//! }
//! ```

#[cfg(any(windows, feature = "worker"))]
use std::path::Path;
use std::path::PathBuf;

#[cfg(any(windows, feature = "worker"))]
use eyre::Context;
use eyre::Result;
#[cfg(feature = "worker")]
use eyre::ensure;
use serde::{Deserialize, Serialize};

use crate::profile::server::settings::{HostSettings, RemoteServerSettings, RestartPolicy};

const DEFAULT_LISTEN: &str = "127.0.0.1:8472";
const DEFAULT_POLL_SECS: u64 = 300;
#[cfg(feature = "worker")]
const MIN_POLL_SECS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkerConfig {
    /// Stable identity shown in lease records and status responses.
    pub worker_id: String,
    /// Address the HTTP API binds to.
    pub listen: String,
    /// The sync profile this worker is bound to. Requests cannot redirect
    /// it to another profile; this binding is the security boundary.
    pub profile_id: String,
    /// Expected game slug from the publication manifest. The worker
    /// rejects mismatched publications instead of deploying the wrong
    /// modpack.
    pub game: String,
    /// Sync API base URL; defaults to Gale's production service.
    pub sync_url: Option<String>,
    /// Remote server transport settings (host/port/auth/pins).
    pub remote: RemoteServerSettings,
    /// Hosting provider for restart/presence operations.
    pub host_control: HostSettings,
    /// Whether the worker may deploy new publications on its own.
    pub auto_sync: bool,
    /// When `auto_sync` is on, whether mod revisions may deploy
    /// automatically. Configs always follow per-file policies.
    pub auto_mods: bool,
    /// What may happen to the server process after deployment.
    pub restart_policy: RestartPolicy,
    /// How often the worker checks for new publications.
    pub poll_interval_secs: u64,
    /// Where the journal and staged packages live.
    pub state_dir: PathBuf,
    /// Optional `KEY=value` file supplying the worker's secrets. Values
    /// set in the environment still win, so this is a fallback store for
    /// contexts that cannot provide env vars (e.g. a Windows service).
    pub secrets_file: Option<PathBuf>,
    /// Optional path the worker writes its run state to (`running`,
    /// `stopped`, `shutdown`). The desktop reads it to report service
    /// status; it stays absent in manual operation.
    pub status_file: Option<PathBuf>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: String::new(),
            listen: DEFAULT_LISTEN.to_owned(),
            profile_id: String::new(),
            game: String::new(),
            sync_url: None,
            remote: RemoteServerSettings::default(),
            host_control: HostSettings::default(),
            auto_sync: false,
            auto_mods: false,
            restart_policy: RestartPolicy::default(),
            poll_interval_secs: DEFAULT_POLL_SECS,
            state_dir: PathBuf::from("."),
            secrets_file: None,
            status_file: None,
        }
    }
}

impl WorkerConfig {
    #[cfg(feature = "worker")]
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read worker config {}", path.display()))?;
        let mut config: Self =
            serde_json::from_slice(&bytes).context("worker config is not valid JSON")?;
        config.validate()?;
        if config.state_dir.as_os_str().is_empty() {
            config.state_dir = PathBuf::from(".");
        }
        Ok(config)
    }

    #[cfg(feature = "worker")]
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.worker_id.trim().is_empty(),
            "workerId cannot be empty"
        );
        ensure!(
            !self.profile_id.trim().is_empty(),
            "profileId cannot be empty"
        );
        ensure!(!self.game.trim().is_empty(), "game cannot be empty");
        ensure!(!self.listen.trim().is_empty(), "listen cannot be empty");
        ensure!(
            self.poll_interval_secs >= MIN_POLL_SECS,
            "pollIntervalSecs must be at least {MIN_POLL_SECS}"
        );
        self.remote.validate()?;
        Ok(())
    }

    /// Serializes back to a config file. Used by the desktop when it
    /// provisions the managed local worker.
    #[cfg(windows)]
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("failed to serialize worker config")?;
        std::fs::write(path, bytes)
            .with_context(|| format!("failed to write worker config {}", path.display()))
    }
}

#[cfg(all(test, feature = "worker"))]
mod tests {
    use super::*;

    fn valid_json() -> serde_json::Value {
        serde_json::json!({
            "workerId": "vps",
            "profileId": "p1",
            "game": "valheim",
            "remote": {
                "protocol": "ftp",
                "host": "example.com",
                "port": 21,
                "username": "u",
                "serverDirectory": "/srv",
                "authentication": "password",
                "privateKeyPath": ""
            },
            "pollIntervalSecs": 60
        })
    }

    fn load(value: serde_json::Value) -> Result<WorkerConfig> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gale-worker.json");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        WorkerConfig::load(&path)
    }

    #[test]
    fn binds_to_one_profile_and_rejects_empty_identity() {
        for field in ["workerId", "profileId", "game"] {
            let mut value = valid_json();
            value[field] = serde_json::json!("");
            assert!(load(value).is_err(), "{field} should be required");
        }
    }

    #[test]
    fn enforces_a_minimum_poll_interval() {
        let mut value = valid_json();
        value["pollIntervalSecs"] = serde_json::json!(MIN_POLL_SECS - 1);
        assert!(load(value).is_err());
    }
}
