//! Worker configuration file (`gale-worker.json`).
//!
//! Secrets are kept out of this file and taken from the environment:
//!
//! - `GALE_WORKER_TOKEN` — bearer token the API requires.
//! - `GALE_WORKER_REMOTE_PASSWORD` — FTP/SFTP password or key passphrase.
//! - `GALE_WORKER_REFRESH_TOKEN` — initial Gale sync refresh token; the
//!   rotated token is then kept in the journal, so this is only a seed.
//! - `GALE_WORKER_DATHOST_PASSWORD` — DatHost API password, when the host
//!   provider is DatHost.
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

use std::path::{Path, PathBuf};

use eyre::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::profile::server::settings::{HostSettings, RemoteServerSettings, RestartPolicy};

const DEFAULT_LISTEN: &str = "127.0.0.1:8472";
const DEFAULT_POLL_SECS: u64 = 300;
const MIN_POLL_SECS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkerConfig {
    /// Stable identity shown in lease records and status responses.
    pub worker_id: String,
    /// Address the HTTP API binds to.
    pub listen: String,
    /// The sync profile this worker is bound to. Requests cannot redirect
    /// it to another profile — this is the worker's authorization scope.
    pub profile_id: String,
    /// Expected game slug from the publication manifest. Mismatched
    /// publications are rejected instead of deploying the wrong modpack.
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
        }
    }
}

impl WorkerConfig {
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

    /// The API bearer token from `GALE_WORKER_TOKEN`.
    pub fn token() -> Result<String> {
        std::env::var("GALE_WORKER_TOKEN")
            .context("GALE_WORKER_TOKEN is not set; the worker API requires a bearer token")
    }

    /// The remote transport credential from `GALE_WORKER_REMOTE_PASSWORD`.
    pub fn remote_password() -> String {
        std::env::var("GALE_WORKER_REMOTE_PASSWORD").unwrap_or_default()
    }

    /// The initial sync refresh token from `GALE_WORKER_REFRESH_TOKEN`.
    pub fn seed_refresh_token() -> Option<String> {
        std::env::var("GALE_WORKER_REFRESH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
    }

    /// The DatHost API password from `GALE_WORKER_DATHOST_PASSWORD`.
    pub fn dat_host_password() -> Option<String> {
        std::env::var("GALE_WORKER_DATHOST_PASSWORD")
            .ok()
            .filter(|token| !token.is_empty())
    }
}

#[cfg(test)]
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
    fn loads_a_valid_config() {
        let config = load(valid_json()).unwrap();
        assert_eq!(config.worker_id, "vps");
        assert_eq!(config.profile_id, "p1");
        assert_eq!(config.poll_interval_secs, 60);
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

    #[test]
    fn malformed_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gale-worker.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(WorkerConfig::load(&path).is_err());
    }
}
