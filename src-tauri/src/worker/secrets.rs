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
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            // Values are verbatim — passwords and passphrases may carry
            // meaningful whitespace or '=' characters, so nothing is
            // trimmed. `lines()` already guarantees no \n or \r survives.
            let value = Some(value.to_owned());
            match key.trim() {
                ENV_TOKEN => secrets.token = value,
                ENV_REMOTE_PASSWORD => secrets.remote_password = value,
                ENV_REFRESH_TOKEN => secrets.refresh_token = value,
                ENV_DATHOST_PASSWORD => secrets.dat_host_password = value,
                _ => {}
            }
        }
        secrets
    }

    /// Renders the file provisioning writes. `None` fields are omitted
    /// rather than written as empty values, so a later overlay cannot
    /// mistake "stored empty" for "absent". Values are written verbatim;
    /// a newline or carriage return cannot be expressed in the
    /// `KEY=value` line format and is rejected instead of corrupting
    /// the file's structure.
    #[cfg(windows)]
    pub fn render(&self) -> eyre::Result<String> {
        let mut out = String::new();
        for (key, value) in [
            (ENV_TOKEN, &self.token),
            (ENV_REMOTE_PASSWORD, &self.remote_password),
            (ENV_REFRESH_TOKEN, &self.refresh_token),
            (ENV_DATHOST_PASSWORD, &self.dat_host_password),
        ] {
            let Some(value) = value else { continue };
            eyre::ensure!(
                !value.contains(['\n', '\r']),
                "{key} cannot contain a newline"
            );
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push('\n');
        }
        Ok(out)
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
    /// let any loopback caller drive deployments. An empty stored value
    /// counts as unset: a zero-length token must never authenticate.
    #[cfg(feature = "worker")]
    pub fn token(&self) -> Result<String> {
        self.token
            .clone()
            .filter(|token| !token.is_empty())
            .ok_or_eyre(format!(
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
             GALE_WORKER_REFRESH_TOKEN=seed\n\
             not-a-key-value\n",
        );

        assert_eq!(secrets.token.as_deref(), Some("abc"));
        assert_eq!(secrets.refresh_token.as_deref(), Some("seed"));
        assert_eq!(secrets.remote_password, None);
        assert_eq!(secrets.dat_host_password, None);
    }

    #[test]
    fn values_round_trip_verbatim() {
        // Passwords and passphrases may legitimately contain whitespace
        // or '=' — the format must carry them unchanged.
        let secrets = Secrets {
            token: Some("  padded  ".to_owned()),
            remote_password: Some("trailing \t".to_owned()),
            refresh_token: Some("a=b=c".to_owned()),
            dat_host_password: Some(String::new()),
        };

        let parsed = Secrets::parse(&secrets.render().unwrap());
        assert_eq!(parsed.token.as_deref(), Some("  padded  "));
        assert_eq!(parsed.remote_password.as_deref(), Some("trailing \t"));
        assert_eq!(parsed.refresh_token.as_deref(), Some("a=b=c"));
        // An empty stored value stays empty — distinct from absent.
        assert_eq!(parsed.dat_host_password.as_deref(), Some(""));
    }

    #[test]
    fn render_rejects_newlines_that_would_corrupt_the_file() {
        for bad in ["line\nbreak", "carriage\rreturn", "both\r\n"] {
            let secrets = Secrets {
                remote_password: Some(bad.to_owned()),
                ..Secrets::default()
            };
            assert!(secrets.render().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn an_empty_token_is_rejected_as_unset() {
        let secrets = Secrets {
            token: Some(String::new()),
            ..Secrets::default()
        };
        assert!(secrets.token().is_err());
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
}
