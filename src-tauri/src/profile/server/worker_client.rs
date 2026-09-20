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
    profile::export::ConfigPath,
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
            // Plaintext http carries the bearer token unencrypted, so it is
            // only acceptable when the worker runs on this same machine.
            // Remote workers need https:// or a secure tunnel/reverse
            // proxy — a LAN is not a trusted transport.
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
            format!("{url}?refresh=1")
        } else {
            url
        };
        self.send(self.http.get(url)).await
    }

    pub async fn preview(
        &self,
        selection: &DeploySelection,
        restart_policy: Option<RestartPolicy>,
    ) -> Result<PreviewResponse> {
        self.send(
            self.http
                .post(format!("{}{}/preview", self.base, api::API_BASE))
                .json(&PreviewRequest {
                    selection: selection.clone(),
                    restart_policy,
                }),
        )
        .await
    }

    pub async fn deploy(
        &self,
        selection: &DeploySelection,
        plan_hash: &str,
        restart_policy: Option<RestartPolicy>,
        force: bool,
    ) -> Result<DeployResponse> {
        self.send(
            self.http
                .post(format!("{}{}/deploy", self.base, api::API_BASE))
                .json(&DeployRequest {
                    selection: selection.clone(),
                    plan_hash: plan_hash.to_owned(),
                    restart_policy,
                    force,
                }),
        )
        .await
    }

    /// Sets a persistent per-file config update policy on the remote
    /// deployment state, through the worker. The worker derives the
    /// publication pin itself — clients never supply it.
    pub async fn set_policy(&self, path: &ConfigPath, policy: ConfigUpdatePolicy) -> Result<()> {
        self.send::<serde_json::Value>(
            self.http
                .post(format!("{}{}/policy", self.base, api::API_BASE))
                .json(&PolicyRequest {
                    path: path.clone(),
                    policy,
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
    use super::is_loopback_host;
    use crate::profile::server::{settings::RemoteServerSettings, worker_client::WorkerClient};

    fn settings(address: &str) -> RemoteServerSettings {
        let mut settings = RemoteServerSettings::default();
        settings.worker.address = address.to_owned();
        settings
    }

    #[test]
    fn rejects_non_loopback_plaintext_http() {
        // A LAN or internet address over http would send the bearer token
        // in cleartext — not a trusted transport.
        for address in [
            "http://192.168.1.10:8472",
            "http://worker.local:8472",
            "http://10.0.0.5",
            "http://[fd00::1]:8472",
        ] {
            let err = match WorkerClient::new(&settings(address), "token".to_owned()) {
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
            "http://localhost:8472",
            "http://[::1]:8472",
            "https://worker.example.com",
            "https://192.168.1.10:8473",
        ] {
            WorkerClient::new(&settings(address), "token".to_owned())
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
            assert!(WorkerClient::new(&settings(address), token.to_owned()).is_err());
        }
    }

    #[test]
    fn loopback_host_detection() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.99"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("::ffff:127.0.0.1"));
        assert!(!is_loopback_host("192.168.0.1"));
        assert!(!is_loopback_host("worker.local"));
        assert!(!is_loopback_host(""));
    }
}
