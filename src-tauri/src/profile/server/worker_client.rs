//! The desktop's client for an independently running `gale-worker`.
//!
//! Thin and symmetric with `worker::api`: the worker is bound to one
//! profile and one remote at startup, so requests carry selections and
//! approvals — never target identity.

use eyre::{Context, Result, bail, ensure};
use reqwest::StatusCode;

use super::{
    plan::DeploySelection,
    settings::{RemoteServerSettings, RestartPolicy},
};
use crate::{
    profile::export::{ConfigPath, ContentHash},
    profile::sync::ConfigUpdatePolicy,
    worker::api::{
        self, ConfigureRequest, DeployRequest, DeployResponse, ErrorResponse, PolicyRequest,
        PreviewRequest, PreviewResponse, StatusResponse,
    },
};

pub struct WorkerClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl WorkerClient {
    pub fn new(settings: &RemoteServerSettings, token: String) -> Result<Self> {
        let base = settings.worker.address.trim().trim_end_matches('/');
        ensure!(
            base.starts_with("http://") || base.starts_with("https://"),
            "worker address must be an http:// or https:// URL"
        );
        ensure!(!token.is_empty(), "worker token is not configured");

        Ok(Self {
            base: base.to_owned(),
            token,
            http: reqwest::Client::new(),
        })
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to reach the worker")?;

        let status = response.status();
        if status.is_success() {
            if status == StatusCode::NO_CONTENT {
                // `T` must deserialize from `null`.
                return serde_json::from_value(serde_json::Value::Null)
                    .context("unexpected empty worker response");
            }
            return response
                .json()
                .await
                .context("worker response was malformed");
        }

        let message = match response.json::<ErrorResponse>().await {
            Ok(body) => body.error,
            Err(_) => format!("worker request failed: {status}"),
        };
        bail!(message);
    }

    pub async fn status(&self, refresh: bool) -> Result<StatusResponse> {
        let url = format!("{}{}/status", self.base, api::API_BASE);
        let url = if refresh {
            format!("{url}?refresh=1")
        } else {
            url
        };
        self.send(self.http.get(url)).await
    }

    pub async fn preview(&self, selection: &DeploySelection) -> Result<PreviewResponse> {
        self.send(
            self.http
                .post(format!("{}{}/preview", self.base, api::API_BASE))
                .json(&PreviewRequest {
                    selection: selection.clone(),
                }),
        )
        .await
    }

    pub async fn deploy(
        &self,
        selection: &DeploySelection,
        plan_hash: &str,
        restart_policy: Option<RestartPolicy>,
    ) -> Result<DeployResponse> {
        self.send(
            self.http
                .post(format!("{}{}/deploy", self.base, api::API_BASE))
                .json(&DeployRequest {
                    selection: selection.clone(),
                    plan_hash: plan_hash.to_owned(),
                    restart_policy,
                }),
        )
        .await
    }

    /// Sets a persistent per-file config update policy on the remote
    /// deployment state, through the worker.
    pub async fn set_policy(
        &self,
        path: &ConfigPath,
        policy: ConfigUpdatePolicy,
        pinned_at: Option<ContentHash>,
    ) -> Result<()> {
        self.send::<serde_json::Value>(
            self.http
                .post(format!("{}{}/policy", self.base, api::API_BASE))
                .json(&PolicyRequest {
                    path: path.clone(),
                    policy,
                    pinned_at,
                }),
        )
        .await?;
        Ok(())
    }

    /// Updates the worker's automation toggles. Manual Deploy Now requests
    /// are unaffected by `auto_sync`.
    pub async fn configure(
        &self,
        auto_sync: bool,
        auto_mods: bool,
        restart_policy: RestartPolicy,
    ) -> Result<()> {
        self.send::<serde_json::Value>(
            self.http
                .post(format!("{}{}/config", self.base, api::API_BASE))
                .json(&ConfigureRequest {
                    auto_sync,
                    auto_mods,
                    restart_policy,
                }),
        )
        .await?;
        Ok(())
    }
}
