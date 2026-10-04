//! The desktop's client for an independently running `gale-worker`.
//!
//! A thin mirror of `worker::api`. The worker is bound to one profile and
//! one remote at startup, so requests carry selections and approvals, and
//! never need to say which profile or server they target.

use eyre::{Context, Result, bail, ensure};
use futures_util::future::BoxFuture;
use reqwest::StatusCode;

use super::{
    executor::{ExecutorStatus, SyncExecutor},
    progress::SyncProgress,
    settings::RestartPolicy,
};
use crate::worker::api::{
    self, ConfigureRequest, DeployRequest, DeployResponse, ErrorResponse, PolicyRequest,
    PreviewRequest, PreviewResponse, StatusResponse,
};

pub struct WorkerClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl WorkerClient {
    pub fn new(address: &str, token: String) -> Result<Self> {
        let base = address.trim().trim_end_matches('/');
        let url = reqwest::Url::parse(base)
            .context("worker address must be an http:// or https:// URL")?;
        ensure!(
            url.scheme() == "http" || url.scheme() == "https",
            "worker address must be an http:// or https:// URL"
        );
        // Credentials belong in the Authorization header, never the URL.
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "worker address must not embed credentials"
        );
        if url.scheme() == "http" {
            // Plaintext http sends the bearer token unencrypted, so it is
            // only acceptable when the worker runs on this same machine.
            // Remote workers need https:// or a secure tunnel/reverse
            // proxy. A LAN is not a trusted transport.
            ensure!(
                is_loopback_host(url.host_str().unwrap_or_default()),
                "plaintext http:// is only allowed for a worker on this machine; \
                 use https:// or a secure tunnel for remote workers"
            );
        }
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
            format!("{url}?refresh=true")
        } else {
            url
        };
        self.send(self.http.get(url)).await
    }

    /// Updates automatic mod deployment and restart policy. Manual Deploy
    /// Now requests are unaffected.
    pub async fn configure(
        &self,
        auto_deploy_mods: bool,
        restart_policy: RestartPolicy,
    ) -> Result<()> {
        self.send(
            self.http
                .post(format!("{}{}/config", self.base, api::API_BASE))
                .json(&ConfigureRequest {
                    auto_deploy_mods,
                    restart_policy,
                }),
        )
        .await
    }
}

impl SyncExecutor for WorkerClient {
    fn read_status(&self, refresh: bool) -> BoxFuture<'_, Result<Option<ExecutorStatus>>> {
        Box::pin(async move { Ok(Some(ExecutorStatus::Worker(self.status(refresh).await?))) })
    }

    fn progress(&self) -> BoxFuture<'_, Result<Option<SyncProgress>>> {
        Box::pin(
            self.send(
                self.http
                    .get(format!("{}{}/progress", self.base, api::API_BASE)),
            ),
        )
    }

    fn preview(&self, request: PreviewRequest) -> BoxFuture<'_, Result<PreviewResponse>> {
        Box::pin(
            self.send(
                self.http
                    .post(format!("{}{}/preview", self.base, api::API_BASE))
                    .json(&request),
            ),
        )
    }

    fn deploy(&self, request: DeployRequest) -> BoxFuture<'_, Result<DeployResponse>> {
        Box::pin(
            self.send(
                self.http
                    .post(format!("{}{}/deploy", self.base, api::API_BASE))
                    .json(&request),
            ),
        )
    }

    /// The worker derives the publication pin itself.
    fn set_policy(&self, request: PolicyRequest) -> BoxFuture<'_, Result<()>> {
        Box::pin(
            self.send(
                self.http
                    .post(format!("{}{}/policy", self.base, api::API_BASE))
                    .json(&request),
            ),
        )
    }

    fn acknowledge_external_restart(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(
            self.send(
                self.http
                    .post(format!("{}{}/restart-ack", self.base, api::API_BASE)),
            ),
        )
    }
}

/// Whether an http:// host string refers to this machine: `localhost` or a
/// loopback IP (127.0.0.0/8, ::1, including IPv4-mapped IPv6 forms).
fn is_loopback_host(host: &str) -> bool {
    // Url::host_str renders IPv6 hosts with their brackets.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            ip.is_loopback()
                || matches!(ip, std::net::IpAddr::V6(v6) if v6.to_canonical().is_loopback())
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::profile::server::worker_client::WorkerClient;

    #[test]
    fn rejects_non_loopback_plaintext_http() {
        // Over http, a LAN or internet address would send the bearer token
        // in cleartext. Neither network counts as a trusted transport.
        for address in [
            "http://192.168.1.10:8472",
            "http://worker.local:8472",
            "http://10.0.0.5",
            "http://[fd00::1]:8472",
        ] {
            let err = match WorkerClient::new(address, "token".to_owned()) {
                Err(err) => err,
                Ok(_) => panic!("{address} must be rejected"),
            };
            assert!(err.to_string().contains("loopback") || err.to_string().contains("machine"));
        }
    }

    #[test]
    fn accepts_loopback_http_and_any_https() {
        for address in [
            "http://127.0.0.1:8472",
            "http://127.0.0.99:8472",
            "http://localhost:8472",
            "http://LOCALHOST:8472",
            "http://[::1]:8472",
            "http://[::ffff:127.0.0.1]:8472",
            "https://worker.example.com",
            "https://192.168.1.10:8473",
        ] {
            WorkerClient::new(address, "token".to_owned())
                .unwrap_or_else(|err| panic!("{address} rejected: {err}"));
        }
    }

    #[test]
    fn rejects_url_credentials_bad_scheme_and_empty_token() {
        for (address, token) in [
            ("ftp://127.0.0.1", "token"),
            ("http://user:pass@127.0.0.1:8472", "token"),
            ("http://127.0.0.1:8472", ""),
        ] {
            assert!(WorkerClient::new(address, token.to_owned()).is_err());
        }
    }
}
