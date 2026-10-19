//! The desktop's client for an independently running `gale-worker`.
//!
//! A thin mirror of `worker::api`. The worker is bound to one profile and
//! one remote at startup, so requests carry selections and approvals, and
//! never need to say which profile or server they target.

use crate::worker::auth::{CHALLENGE_HEADER, MAX_RESPONSE_BYTES, Proof, RESPONSE_HEADER};
use eyre::{Context, OptionExt, Result, bail, ensure};
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
            // HTTP authenticates messages, but does not encrypt their contents.
            // Remote access still requires TLS.
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
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        let mut request = request.build()?;
        let proof = if request.url().scheme() == "http" {
            let target = match request.url().query() {
                Some(query) => format!("{}?{query}", request.url().path()),
                None => request.url().path().to_owned(),
            };
            let body = request
                .body()
                .map(|body| {
                    body.as_bytes()
                        .ok_or_eyre("Worker requests must have a buffered body")
                })
                .transpose()?
                .unwrap_or_default();
            let challenge = self
                .http
                .get(format!("{}{}/health", self.base, api::API_BASE))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .context("failed to reach the Worker")?
                .error_for_status()?;
            let epoch = challenge
                .headers()
                .get(CHALLENGE_HEADER)
                .ok_or_eyre(
                    "Worker authentication is unavailable; update or reprovision the Worker",
                )?
                .to_str()?
                .parse::<uuid::Uuid>()?;
            Some(Proof::sign(
                &self.token,
                epoch,
                request.method().as_str(),
                &target,
                body,
            ))
        } else {
            None
        };
        let authorization = match &proof {
            Some(proof) => proof.authorization(),
            None => format!("Bearer {}", self.token),
        };
        let mut authorization: reqwest::header::HeaderValue = authorization.parse()?;
        authorization.set_sensitive(true);
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, authorization);
        let mut response = self
            .http
            .execute(request)
            .await
            .context("failed to reach the worker")?;
        let status = response.status();
        let response_tag = response
            .headers()
            .get(RESPONSE_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= MAX_RESPONSE_BYTES - body.len(),
                "Worker response exceeds the size limit"
            );
            body.extend_from_slice(&chunk);
        }
        if let Some(proof) = proof {
            ensure!(
                proof.verify_response(&self.token, status.as_u16(), &body, &response_tag),
                "Worker identity could not be verified; check the bearer token, then update or reprovision the Worker"
            );
        }

        if status.is_success() {
            if status == StatusCode::NO_CONTENT {
                // `T` must deserialize from `null`.
                return serde_json::from_value(serde_json::Value::Null)
                    .context("unexpected empty worker response");
            }
            return serde_json::from_slice(&body).context("worker response was malformed");
        }

        let message = match serde_json::from_slice::<ErrorResponse>(&body) {
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

    const HEALTH: &str = "HTTP/1.1 204 No Content\r\nx-gale-challenge: 00000000-0000-4000-8000-000000000001\r\nConnection: close\r\n\r\n";
    const DENIED: &str =
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    const TOKEN: &str = "fixture-private-worker-token";

    fn assert_no_bearer(requests: &[String]) {
        for request in requests {
            assert!(!request.contains(TOKEN));
            assert!(
                !request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer ")
            );
        }
    }

    #[tokio::test]
    async fn security_an_impostor_with_a_health_challenge_still_receives_no_bearer() {
        let (result, requests) = crate::test_support::http_fixture(
            &["GET /v1/health HTTP/1.1", "GET /v1/status HTTP/1.1"],
            |index, _| [HEALTH, DENIED][index].into(),
            |base| async move {
                WorkerClient::new(&base, TOKEN.into())
                    .unwrap()
                    .status(false)
                    .await
            },
        )
        .await;
        assert!(result.is_err());
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: gale-v1 ")
        );
        assert_no_bearer(&requests);
    }

    #[tokio::test]
    async fn security_worker_rejects_a_response_without_server_authentication() {
        let (result, requests) = crate::test_support::http_fixture(
            &["GET /v1/health HTTP/1.1", "GET /v1/progress HTTP/1.1"],
            |index, _| {
                [
                    HEALTH,
                    "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnull",
                ][index]
                    .into()
            },
            |base| async move {
                let client = WorkerClient::new(&base, TOKEN.into()).unwrap();
                client
                    .send::<Option<String>>(client.http.get(format!("{base}/v1/progress")))
                    .await
            },
        )
        .await;
        assert!(result.is_err(), "accepted an unsigned successful response");
        assert_no_bearer(&requests);
    }

    #[tokio::test]
    async fn security_loopback_impostor_never_receives_the_worker_bearer() {
        let (result, requests) = crate::test_support::http_fixture(
            &["GET /v1/health HTTP/1.1"],
            |_, _| DENIED.into(),
            |base| async move {
                WorkerClient::new(&base, TOKEN.into())
                    .unwrap()
                    .status(false)
                    .await
            },
        )
        .await;
        assert!(result.is_err());
        assert_no_bearer(&requests);
    }

    #[tokio::test]
    async fn security_signed_responses_reject_changed_body_and_status() {
        for (status, body, accepted) in [
            (200u16, "null", true),
            (201, "null", false),
            (200, "\"modified\"", false),
        ] {
            let (result, _) = crate::test_support::http_fixture(
                &["GET /v1/health HTTP/1.1", "GET /v1/progress HTTP/1.1"],
                |index, request| {
                    if index == 0 { return HEALTH.into(); }
                    let authorization = request.lines().find(|line| line.to_ascii_lowercase().starts_with("authorization:")).unwrap();
                    let nonce: uuid::Uuid = authorization.split_whitespace().nth(2).unwrap().split(':').nth(2).unwrap().parse().unwrap();
                    let tag = crate::test_support::worker_mac(TOKEN, b"response-v1", &[nonce.as_bytes(), &200u16.to_le_bytes(), b"null"]);
                    format!("HTTP/1.1 {status} OK\r\nx-gale-response: {tag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                },
                |base| async move {
                    let client = WorkerClient::new(&base, TOKEN.into()).unwrap();
                    client.send::<Option<String>>(client.http.get(format!("{base}/v1/progress"))).await
                },
            ).await;
            assert_eq!(
                result.is_ok(),
                accepted,
                "response status {status}, body {body}"
            );
        }
    }

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
