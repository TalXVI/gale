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
/// is optional. An absent value means "unknown", never "empty"/"stopped".
#[derive(Debug, Clone, Default)]
pub struct HostStatus {
    pub running: Option<bool>,
    /// Startup in progress, when the provider reports it.
    pub booting: Option<bool>,
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
/// - `GET  /api/0.1/game-servers/{id}` refreshes and returns `booting`.
/// - `GET  /api/0.1/game-servers/{id}/metrics` returns the player count
///   where the game supports it.
/// - `POST /api/0.1/game-servers/{id}/start` is documented to *restart* an
///   already-running server, so Gale uses it for both start and restart.
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
    booting: Option<bool>,
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
                booting: server.booting,
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
/// Credentials come from the caller. The desktop passes keyring secrets,
/// the worker passes config secrets. A `None` password gives
/// [`NoHostControl`], which is a supported configuration, not an error.
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

#[cfg(all(test, feature = "worker"))]
mod tests {
    use super::*;
    use crate::profile::server::{
        engine::apply_restart_policy, settings::RestartPolicy, state::RestartOutcome,
    };
    use axum::{
        Router,
        extract::{Request, State},
        http::StatusCode,
        response::{IntoResponse, Response},
    };
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct Api {
        players: Option<u32>,
        fail_status: bool,
        fail_restart: bool,
        post_restart_statuses: Arc<Mutex<VecDeque<bool>>>,
        calls: Arc<Mutex<Vec<String>>>,
        violations: Arc<Mutex<Vec<String>>>,
    }

    async fn handle(State(api): State<Api>, request: Request) -> Response {
        let route = format!("{} {}", request.method(), request.uri());
        api.calls.lock().unwrap().push(route.clone());
        if request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            != Some("Basic dXNlcjpwYXNz")
        {
            api.violations
                .lock()
                .unwrap()
                .push("missing or wrong credentials".into());
            return StatusCode::UNAUTHORIZED.into_response();
        }
        match route.as_str() {
            "GET /game-servers/server-1/" if api.fail_status => {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
            "GET /game-servers/server-1/" => {
                let booting = api
                    .post_restart_statuses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(false);
                axum::Json(serde_json::json!({"on": true, "booting": booting})).into_response()
            }
            "GET /game-servers/server-1/metrics" => {
                axum::Json(serde_json::json!({"player_count": api.players})).into_response()
            }
            "POST /game-servers/server-1/start" if api.fail_restart => {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
            "POST /game-servers/server-1/start" => {
                api.post_restart_statuses
                    .lock()
                    .unwrap()
                    .extend([true, false]);
                StatusCode::NO_CONTENT.into_response()
            }
            _ => {
                api.violations.lock().unwrap().push(route);
                StatusCode::BAD_REQUEST.into_response()
            }
        }
    }

    #[tokio::test]
    async fn restart_policy_uses_authenticated_host_status_and_restart_requests() {
        use RestartOutcome::*;
        use RestartPolicy::*;
        let status = [
            "GET /game-servers/server-1/",
            "GET /game-servers/server-1/metrics",
        ];
        let restart = "POST /game-servers/server-1/start";
        for (policy, required, players, fail_status, fail_restart, expected, calls) in [
            (Immediate, false, Some(0), false, false, NotRequired, vec![]),
            (Manual, true, Some(0), false, false, AwaitingManual, vec![]),
            (
                WhenEmpty,
                true,
                Some(2),
                false,
                false,
                AwaitingEmpty,
                status.to_vec(),
            ),
            (
                WhenEmpty,
                true,
                None,
                false,
                false,
                AwaitingEmpty,
                status.to_vec(),
            ),
            (
                WhenEmpty,
                true,
                Some(0),
                true,
                false,
                AwaitingEmpty,
                vec![status[0]],
            ),
            (
                WhenEmpty,
                true,
                Some(0),
                false,
                false,
                Restarted,
                vec![
                    status[0], status[1], restart, status[0], status[1], status[0], status[1],
                ],
            ),
            (
                Immediate,
                true,
                Some(2),
                false,
                false,
                Restarted,
                vec![
                    status[0], status[1], restart, status[0], status[1], status[0], status[1],
                ],
            ),
            (
                Immediate,
                true,
                Some(0),
                false,
                true,
                Failed,
                vec![status[0], status[1], restart],
            ),
            (
                Immediate,
                true,
                Some(0),
                true,
                false,
                StartupUnverified,
                vec![status[0], restart, status[0]],
            ),
        ] {
            let api = Api {
                players,
                fail_status,
                fail_restart,
                post_restart_statuses: Default::default(),
                calls: Default::default(),
                violations: Default::default(),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new().fallback(handle).with_state(api.clone());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let host = DatHostControl {
                client: reqwest::Client::new(),
                base,
                server_id: "server-1".into(),
                username: "user".into(),
                password: "pass".into(),
            };
            let outcome = apply_restart_policy(&host, policy, required).await;
            task.abort();
            assert_eq!(outcome, expected, "policy {policy:?}, players {players:?}");
            assert_eq!(*api.calls.lock().unwrap(), calls);
            assert!(api.violations.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn missing_dat_host_booting_state_stays_unknown() {
        let server: DatHostServer =
            serde_json::from_value(serde_json::json!({"on": true})).unwrap();
        assert_eq!(server.booting, None);
    }
}
