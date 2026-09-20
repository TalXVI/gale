//! Hosting-provider lifecycle control.
//!
//! File transfer and process management are separate capabilities: SFTP
//! does not imply the ability to restart a game server. Restart and
//! player-presence queries go through this abstraction so the engine can
//! apply [`RestartPolicy`] without assuming the transport can do either.
//!
//! Presence is deliberately nullable. When the provider cannot supply a
//! reliable player count, `WhenEmpty` execution must fail closed rather
//! than guessing the server is empty.

use std::future::Future;
use std::pin::Pin;

use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::settings::{HostProvider, HostSettings};

const DATHOST_API: &str = "https://dathost.net/api/0.1";

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The observable state needed for restart policy decisions. Every field
/// is optional — an absent value means "unknown", never "empty"/"stopped".
#[derive(Debug, Clone, Default)]
pub struct HostStatus {
    pub running: Option<bool>,
    /// Connected player count, when the provider reports one.
    pub players: Option<u32>,
}

/// What the provider supports, so the UI and policy engine can block
/// policies that cannot be honored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostCapabilities {
    pub can_restart: bool,
    pub reports_players: bool,
}

pub trait HostControl: Send + Sync {
    fn capabilities(&self) -> HostCapabilities;
    fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>>;
    fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>>;
    fn name(&self) -> &'static str;
}

/// A provider that offers no lifecycle operations at all. Restart
/// policies degrade to "awaiting manual action" rather than pretending to
/// work.
struct NoHostControl;

impl HostControl for NoHostControl {
    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            can_restart: false,
            reports_players: false,
        }
    }

    fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { bail!("no hosting provider is configured") })
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
        Box::pin(async { Ok(HostStatus::default()) })
    }

    fn name(&self) -> &'static str {
        "none"
    }
}

/// DatHost game-server API adapter.
///
/// - `GET  /api/0.1/game-servers/{id}` — status including `on`.
/// - `GET  /api/0.1/game-servers/{id}/metrics` — player count where the
///   game supports it.
/// - `POST /api/0.1/game-servers/{id}/start` — documented to *restart* an
///   already-running server; used for both start and restart.
pub struct DatHostControl {
    client: reqwest::Client,
    base: String,
    server_id: String,
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct DatHostServer {
    on: bool,
}

#[derive(Debug, Deserialize)]
struct DatHostMetrics {
    /// Present only for games with a metrics probe.
    player_count: Option<u32>,
}

impl DatHostControl {
    fn new(server_id: String, username: String, password: String) -> Self {
        let base = std::env::var("GALE_DATHOST_API")
            .unwrap_or_else(|_| DATHOST_API.to_owned())
            .trim_end_matches('/')
            .to_owned();

        Self {
            client: reqwest::Client::new(),
            base,
            server_id,
            username,
            password,
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(format!(
                "{}/game-servers/{}/{}",
                self.base, self.server_id, path
            ))
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .context("dathost request failed")?
            .error_for_status()
            .context("dathost rejected the request")?;

        Ok(response.json().await?)
    }
}

impl HostControl for DatHostControl {
    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            can_restart: true,
            // DatHost only reports player counts for games with a metrics
            // probe; a missing field maps to `None` and fails closed.
            reports_players: true,
        }
    }

    fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.client
                .post(format!(
                    "{}/game-servers/{}/start",
                    self.base, self.server_id
                ))
                .basic_auth(&self.username, Some(&self.password))
                .send()
                .await
                .context("dathost restart request failed")?
                .error_for_status()
                .context("dathost rejected the restart request")?;
            Ok(())
        })
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
        Box::pin(async move {
            let server: DatHostServer = self.get("").await?;
            let players = match self.get::<DatHostMetrics>("metrics").await {
                Ok(metrics) => metrics.player_count,
                Err(_) => None,
            };
            Ok(HostStatus {
                running: Some(server.on),
                players,
            })
        })
    }

    fn name(&self) -> &'static str {
        "dathost"
    }
}

/// Builds a host controller from settings and credentials.
///
/// Credentials come from the caller — the desktop passes keyring secrets,
/// the worker passes config secrets. `None` yields [`NoHostControl`],
/// which is a supported configuration, not an error.
pub fn from_settings(settings: &HostSettings, password: Option<&str>) -> Box<dyn HostControl> {
    match settings.provider {
        HostProvider::None => Box::new(NoHostControl),
        HostProvider::DatHost => {
            let (server_id, username, password) = (
                settings.dat_host_server_id.trim(),
                settings.dat_host_username.trim(),
                password,
            );
            if server_id.is_empty() || username.is_empty() || password.is_none() {
                warn_missing();
                return Box::new(NoHostControl);
            }
            Box::new(DatHostControl::new(
                server_id.to_owned(),
                username.to_owned(),
                password.unwrap_or_default().to_owned(),
            ))
        }
    }
}

fn warn_missing() {
    tracing::warn!(
        "host provider configured but credentials are incomplete; host control disabled"
    );
}
