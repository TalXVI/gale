//! The worker's credentials, resolved from the environment and an
//! optional `KEY=value` file.
//!
//! Environment variables keep the documented manual-setup contract:
//!
//! - `GALE_WORKER_TOKEN`: bearer token the API requires.
//! - `GALE_WORKER_REMOTE_PASSWORD`: FTP/SFTP password or key passphrase.
//! - `GALE_WORKER_REFRESH_TOKEN`: initial Gale sync refresh token. The
//!   rotated token is kept in the journal afterwards, so this is a seed.
//! - `GALE_WORKER_DATHOST_PASSWORD`: DatHost API password, when the host
//!   provider is DatHost.
//!
//! A persistent service cannot receive per-user environment variables,
//! so the Windows service install writes the same keys to a `secrets.env`
//! file instead (ACL-protected, referenced by the config's `secretsFile`).
//! An environment variable always wins over the file, so operators can
//! override a single value without editing the file.

#[cfg(feature = "worker")]
use std::path::Path;

#[cfg(feature = "worker")]
use eyre::{Context, OptionExt, Result};

#[cfg(feature = "worker")]
use super::config::WorkerConfig;

pub const ENV_TOKEN: &str = "GALE_WORKER_TOKEN";
pub const ENV_REMOTE_PASSWORD: &str = "GALE_WORKER_REMOTE_PASSWORD";
pub const ENV_REFRESH_TOKEN: &str = "GALE_WORKER_REFRESH_TOKEN";
pub const ENV_DATHOST_PASSWORD: &str = "GALE_WORKER_DATHOST_PASSWORD";

#[derive(Debug, Default)]
pub struct Secrets {
    pub token: Option<String>,
    pub remote_password: Option<String>,
    pub refresh_token: Option<String>,
    pub dat_host_password: Option<String>,
}

impl Secrets {
    /// Loads secrets for `config`: values from `secrets_file` (when set),
    /// overlaid by any environment variables that are present.
    #[cfg(feature = "worker")]
    pub fn resolve(config: &WorkerConfig) -> Result<Self> {
        let mut secrets = match &config.secrets_file {
            Some(path) => Self::load_file(path)?,
            None => Self::default(),
        };
        secrets.overlay_env();
        Ok(secrets)
    }

    /// Parses a `KEY=value` secrets file. Blank lines and `#` comments are
    /// ignored; unknown keys are skipped so the file stays forward-
    /// compatible with newer workers.
    #[cfg(feature = "worker")]
    pub fn load_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read worker secrets file {}", path.display()))?;
        Ok(Self::parse(&content))
    }

    #[cfg(feature = "worker")]
    pub fn parse(content: &str) -> Self {
        let mut secrets = Self::default();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                ENV_TOKEN => secrets.token = Some(value.to_owned()),
                ENV_REMOTE_PASSWORD => secrets.remote_password = Some(value.to_owned()),
                ENV_REFRESH_TOKEN => secrets.refresh_token = Some(value.to_owned()),
                ENV_DATHOST_PASSWORD => secrets.dat_host_password = Some(value.to_owned()),
                _ => {}
            }
        }
        secrets
    }

    /// Renders the file provisioning writes. `None` fields are omitted
    /// rather than written as empty values, so a later overlay cannot
    /// mistake "stored empty" for "absent".
    #[cfg(windows)]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut put = |key: &str, value: &Option<String>| {
            if let Some(value) = value {
                out.push_str(key);
                out.push('=');
                out.push_str(value);
                out.push('\n');
            }
        };
        put(ENV_TOKEN, &self.token);
        put(ENV_REMOTE_PASSWORD, &self.remote_password);
        put(ENV_REFRESH_TOKEN, &self.refresh_token);
        put(ENV_DATHOST_PASSWORD, &self.dat_host_password);
        out
    }

    #[cfg(feature = "worker")]
    fn overlay_env(&mut self) {
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        if let Some(value) = env(ENV_TOKEN) {
            self.token = Some(value);
        }
        if let Some(value) = env(ENV_REMOTE_PASSWORD) {
            self.remote_password = Some(value);
        }
        if let Some(value) = env(ENV_REFRESH_TOKEN) {
            self.refresh_token = Some(value);
        }
        if let Some(value) = env(ENV_DATHOST_PASSWORD) {
            self.dat_host_password = Some(value);
        }
    }

    /// The API bearer token. Required — an unauthenticated worker would
    /// let any loopback caller drive deployments.
    #[cfg(feature = "worker")]
    pub fn token(&self) -> Result<String> {
        self.token.clone().ok_or_eyre(format!(
            "no worker API token; set {ENV_TOKEN} or provide a secretsFile"
        ))
    }

    /// The remote transport credential, empty when the protocol does not
    /// need one (e.g. SSH agent).
    #[cfg(feature = "worker")]
    pub fn remote_password(&self) -> String {
        self.remote_password.clone().unwrap_or_default()
    }

    /// The initial sync refresh token. Once the journal holds a rotated
    /// token the seed is ignored.
    #[cfg(feature = "worker")]
    pub fn seed_refresh_token(&self) -> Option<String> {
        self.refresh_token.clone()
    }

    #[cfg(feature = "worker")]
    pub fn dat_host_password(&self) -> Option<String> {
        self.dat_host_password.clone()
    }
}

#[cfg(all(test, windows, feature = "worker"))]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_known_keys_and_ignores_the_rest() {
        let secrets = Secrets::parse(
            "# comment\n\
             GALE_WORKER_TOKEN=abc\n\
             UNKNOWN_KEY=x\n\
             GALE_WORKER_REMOTE_PASSWORD=\n\
             GALE_WORKER_REFRESH_TOKEN= seed \n\
             not-a-key-value\n",
        );

        assert_eq!(secrets.token.as_deref(), Some("abc"));
        // Empty values parse as absent so they cannot shadow the file
        // format's "unset" state.
        assert_eq!(secrets.remote_password, None);
        assert_eq!(secrets.refresh_token.as_deref(), Some("seed"));
        assert_eq!(secrets.dat_host_password, None);
    }

    #[test]
    fn render_omits_absent_values_and_round_trips() {
        let secrets = Secrets {
            token: Some("t=with=equals".to_owned()),
            refresh_token: Some("r".to_owned()),
            ..Secrets::default()
        };

        let rendered = secrets.render();
        assert!(!rendered.contains("REMOTE_PASSWORD"));
        assert!(!rendered.contains("DATHOST"));

        let parsed = Secrets::parse(&rendered);
        assert_eq!(parsed.token.as_deref(), Some("t=with=equals"));
        assert_eq!(parsed.refresh_token.as_deref(), Some("r"));
    }

    #[test]
    fn a_configured_but_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = WorkerConfig {
            secrets_file: Some(dir.path().join("secrets.env")),
            ..WorkerConfig::default()
        };
        assert!(Secrets::resolve(&config).is_err());
    }

    #[test]
    fn no_secrets_file_means_environment_only() {
        // No file configured: resolution cannot fail regardless of what
        // the environment happens to contain.
        let config = WorkerConfig::default();
        Secrets::resolve(&config).unwrap();
    }
}
