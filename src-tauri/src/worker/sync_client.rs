//! The worker's standalone Gale sync client.
//!
//! Mirrors the desktop's `profile::sync` transport but takes credentials
//! from the journal and environment rather than `AppHandle`/keyring, so
//! it can run unattended. Refresh tokens are single-use and rotate on
//! every grant; each rotation is persisted to the journal, and the issued
//! access token is reused until shortly before it expires. A 400/401
//! answer means the credential itself is dead — the journal latches
//! `sync_reauthorization_required` and no further token requests are made
//! until a new sign-in replaces it. Archive validation goes through the
//! same `publication_from_archive` path the desktop uses, so a
//! "canonical publication" is the identical artifact on both sides.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use eyre::{Context, OptionExt, Result, bail, ensure};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::profile::sync::{self, FetchedPublication, SyncProfileMetadata};
use crate::worker::{config::WorkerConfig, journal::Journal, secrets};

static DEFAULT_API_URL: &str = "https://gale.kesomannen.com/api";

/// Same bound as the desktop's `download_profile_bytes`.
const MAX_DOWNLOAD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrantTokenRequest {
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

/// Surfaced as the poll error when the sync service rejects the worker's
/// refresh token; the text tells the operator how to sign the worker in
/// again.
pub const SYNC_REAUTHORIZATION_REQUIRED: &str = "Gale sync rejected the Worker's sign-in (it expired or was revoked). Sign the Worker in again: run 'Set up worker' for a Worker hosted on this PC, or give a manually run Worker a new GALE_WORKER_REFRESH_TOKEN and restart it.";

/// A granted access token, reused until shortly before its `exp`.
struct CachedAccessToken {
    token: String,
    expires_at: DateTime<Utc>,
}

pub struct SyncClient {
    config: WorkerConfig,
    /// The seed refresh token from the worker's secrets, used until the
    /// journal holds a rotated one. When the journal's credential was
    /// rejected, a seed that differs from it is a new sign-in and is
    /// tried instead.
    seed_refresh_token: Option<String>,
    http: reqwest::Client,
    /// Refresh tokens rotate once per grant, so reading and replacing
    /// them is serialized here. The slot also caches the current access
    /// token: one grant serves as many polls as its expiry allows.
    refresh_lock: tokio::sync::Mutex<Option<CachedAccessToken>>,
}

impl SyncClient {
    pub fn new(config: WorkerConfig, seed_refresh_token: Option<String>) -> Self {
        Self {
            config,
            seed_refresh_token,
            http: reqwest::Client::new(),
            refresh_lock: tokio::sync::Mutex::new(None),
        }
    }

    fn api_url(&self) -> &str {
        static ENV_URL: LazyLock<Option<String>> =
            LazyLock::new(|| std::env::var("GALE_SYNC_URL").ok());

        self.config
            .sync_url
            .as_deref()
            .or(ENV_URL.as_deref())
            .unwrap_or(DEFAULT_API_URL)
    }

    /// Ensures a usable access token exists, reusing the cached one while
    /// it remains valid and granting through `POST /auth/token`
    /// otherwise. Every grant rotates the refresh token, which is
    /// persisted back into the journal.
    ///
    /// A 400/401 answer means the credential itself was rejected — there
    /// is no recovery short of a new sign-in, so the journal latches
    /// `sync_reauthorization_required` and no further token requests are
    /// made until setup installs a fresh credential or a different
    /// `GALE_WORKER_REFRESH_TOKEN` seed is supplied. Every other failure
    /// (network, 429, 5xx, malformed body) stays retryable: the journal
    /// is left untouched and the next poll tries again.
    async fn access_token(&self, journal: &Journal) -> Result<String> {
        let mut cache = self.refresh_lock.lock().await;
        if let Some(cached) = cache.as_ref()
            && Utc::now() + chrono::Duration::seconds(60) < cached.expires_at
        {
            return Ok(cached.token.clone());
        }

        let candidate = {
            let state = journal.state.lock().await;
            if state.sync_reauthorization_required {
                // The journal holds the rejected credential; a seed equal
                // to it is the same dead token. Only a differing seed is
                // a newly supplied sign-in worth one attempt.
                match self.seed_refresh_token.as_deref() {
                    Some(seed) if Some(seed) != state.refresh_token.as_deref() => seed.to_owned(),
                    _ => bail!("{SYNC_REAUTHORIZATION_REQUIRED}"),
                }
            } else {
                state
                    .refresh_token
                    .clone()
                    .or_else(|| self.seed_refresh_token.clone())
                    .ok_or_eyre(format!(
                        "no refresh token in journal and {} is unset",
                        secrets::ENV_REFRESH_TOKEN
                    ))?
            }
        };

        let response = self
            .http
            .post(format!("{}/auth/token", self.api_url()))
            .json(&GrantTokenRequest {
                refresh_token: candidate.clone(),
            })
            .send()
            .await
            .context("failed to reach the sync service")?;

        if matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED
        ) {
            // Persist the rejected value — including a seed the journal
            // never held — so the same credential is recognized as spent
            // instead of being retried on every poll.
            let mut state = journal.state.lock().await;
            state.refresh_token = Some(candidate);
            state.sync_reauthorization_required = true;
            journal.save(&state)?;
            *cache = None;
            bail!("{SYNC_REAUTHORIZATION_REQUIRED}");
        }

        let tokens: TokenResponse = response
            .error_for_status()
            .context("sync token request failed")?
            .json()
            .await
            .context("sync token response was malformed")?;

        let mut state = journal.state.lock().await;
        state.refresh_token = Some(tokens.refresh_token);
        state.sync_reauthorization_required = false;
        journal.save(&state)?;

        // A token whose expiry cannot be decoded is used once and never
        // cached; that decode failure must not fail the poll.
        *cache = sync::auth::access_token_expiry(&tokens.access_token)
            .map(|expires_at| CachedAccessToken {
                token: tokens.access_token.clone(),
                expires_at,
            })
            .ok();

        Ok(tokens.access_token)
    }

    /// Fetches the profile's current metadata. Returns `None` when the
    /// profile does not exist.
    async fn manifest(&self, token: &str) -> Result<Option<SyncProfileMetadata>> {
        let response = self
            .http
            .get(format!(
                "{}/profile/{}/meta",
                self.api_url(),
                self.config.profile_id
            ))
            .bearer_auth(token)
            .send()
            .await
            .context("failed to reach the sync service")?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .context("failed to fetch sync profile metadata")?;

        let metadata: SyncProfileMetadata = response
            .json()
            .await
            .context("sync profile metadata was malformed")?;

        let game = metadata.manifest.game.as_deref().unwrap_or_default();
        ensure!(
            game == self.config.game,
            "sync profile is for game '{game}', but the worker is configured for '{}'",
            self.config.game
        );

        Ok(Some(metadata))
    }

    /// Downloads the profile archive with the desktop's size bound.
    async fn archive_bytes(&self, token: &str, metadata: &SyncProfileMetadata) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(format!("{}/profile/{}", self.api_url(), metadata.id))
            .bearer_auth(token)
            .send()
            .await
            .context("failed to reach the sync service")?
            .error_for_status()
            .context("failed to download the publication")?;

        if let Some(len) = response.content_length() {
            ensure!(
                len <= MAX_DOWNLOAD_BYTES as u64,
                "sync archive exceeds the download size limit"
            );
        }

        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_DOWNLOAD_BYTES,
                "sync archive exceeds the download size limit"
            );
            bytes.extend_from_slice(&chunk);
        }

        Ok(bytes)
    }

    /// Polls for the canonical publication. When `since` is `Some`, the
    /// archive is only downloaded if the remote revision advanced past
    /// it. One access token serves both requests, and the token cache
    /// means most polls need no grant at all.
    pub async fn poll(
        &self,
        journal: &Journal,
        since: Option<DateTime<Utc>>,
    ) -> Result<Option<FetchedPublication>> {
        let token = self.access_token(journal).await?;

        let Some(metadata) = self.manifest(&token).await? else {
            return Ok(None);
        };

        if let Some(seen) = since
            && metadata.updated_at <= seen
        {
            return Ok(None);
        }

        let bytes = self.archive_bytes(&token, &metadata).await?;
        let publication = sync::publication_from_archive(&metadata, &bytes)
            .context("canonical publication failed validation")?;

        Ok(Some(publication))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashSet;
    use std::sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        extract::{Path, State},
        http::StatusCode as HttpStatus,
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use base64::Engine;
    use serde_json::json;

    use super::*;
    use crate::{
        profile::{
            export::{ModRevision, ProfileManifest, R2Mod},
            server::{
                settings::RestartPolicy,
                state::{
                    ExecutorKind, OperationKind, OperationRecord, OperationStatus,
                    OperationSummary, RestartOutcome,
                },
            },
            sync::{SyncProfileMetadata, auth::User},
        },
        thunderstore::{Backend, PackageIdent},
        worker::journal::{PendingWork, WorkerJournal},
    };

    const PROFILE_ID: &str = "sync-profile-1";

    /// A structurally valid JWT with the given `exp` claim, matching the
    /// shape the sync service issues.
    fn jwt_with_expiry(exp: i64) -> String {
        format!(
            "eyJhbGciOiJIUzI1NiJ9.{}.sig",
            base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#))
        )
    }

    /// The access token every `MockApi` grant issues, expiring in 2100.
    static ACCESS_JWT: LazyLock<String> = LazyLock::new(|| jwt_with_expiry(4102444800));

    #[test]
    fn the_mock_access_token_carries_a_real_expiry() {
        let expiry = crate::profile::sync::auth::access_token_expiry(&ACCESS_JWT)
            .expect("the mock token must decode");
        assert_eq!(expiry, DateTime::from_timestamp(4102444800, 0).unwrap());
    }

    #[derive(Clone, Default)]
    pub(crate) struct MockApi {
        pub(crate) meta: Option<SyncProfileMetadata>,
        pub(crate) archive: Vec<u8>,
        pub(crate) fail_auth: bool,
        pub(crate) token_status: Arc<AtomicU16>,
        pub(crate) chunked: bool,
        pub(crate) violations: Arc<std::sync::Mutex<Vec<String>>>,
        pub(crate) token_hits: Arc<AtomicUsize>,
        pub(crate) meta_hits: Arc<AtomicUsize>,
        pub(crate) archive_hits: Arc<AtomicUsize>,
        pub(crate) refresh_tokens: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    async fn token(
        State(api): State<MockApi>,
        request: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
    ) -> Response {
        let request = match request {
            Ok(Json(request)) => request,
            Err(error) => {
                api.violations
                    .lock()
                    .unwrap()
                    .push(format!("invalid token body: {error}"));
                return HttpStatus::BAD_REQUEST.into_response();
            }
        };
        let mut tokens = api.refresh_tokens.lock().await;
        let expected = if tokens.is_empty() {
            "refresh-seed"
        } else {
            "refresh-rotated"
        };
        if request != json!({"refreshToken": expected}) {
            api.violations
                .lock()
                .unwrap()
                .push(format!("unexpected token request: {request}"));
            return HttpStatus::BAD_REQUEST.into_response();
        }
        api.token_hits.fetch_add(1, Ordering::Relaxed);
        if api.fail_auth {
            return HttpStatus::UNAUTHORIZED.into_response();
        }
        let status = api.token_status.load(Ordering::Relaxed);
        if status != 0 {
            return HttpStatus::from_u16(status).unwrap().into_response();
        }
        tokens.push(expected.to_owned());
        Json(json!({
            "accessToken": ACCESS_JWT.as_str(),
            "refreshToken": "refresh-rotated"
        }))
        .into_response()
    }

    async fn meta(State(api): State<MockApi>, Path(_id): Path<String>) -> Response {
        api.meta_hits.fetch_add(1, Ordering::Relaxed);
        match &api.meta {
            Some(meta) => Json(meta.clone()).into_response(),
            None => HttpStatus::NOT_FOUND.into_response(),
        }
    }

    async fn archive(State(api): State<MockApi>, Path(_id): Path<String>) -> Response {
        api.archive_hits.fetch_add(1, Ordering::Relaxed);
        if api.chunked {
            let chunks: Vec<_> = api
                .archive
                .chunks(64 * 1024)
                .map(|chunk| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(chunk)))
                .collect();
            axum::body::Body::from_stream(futures_util::stream::iter(chunks)).into_response()
        } else {
            api.archive.clone().into_response()
        }
    }

    pub(crate) struct MockServer {
        pub(crate) url: String,
        task: tokio::task::JoinHandle<()>,
        api: MockApi,
    }

    impl MockServer {
        pub(crate) fn metadata_requests(&self) -> usize {
            self.api.meta_hits.load(Ordering::Relaxed)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.task.abort();
            if !std::thread::panicking() {
                let violations = self.api.violations.lock().unwrap();
                assert!(
                    violations.is_empty(),
                    "unexpected sync API requests: {violations:?}"
                );
            }
        }
    }

    async fn validate_request(
        State(api): State<MockApi>,
        request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> Response {
        let path = request.uri().path();
        let valid = match (request.method().as_str(), path) {
            ("POST", "/auth/token") => true,
            ("GET", "/profile/sync-profile-1" | "/profile/sync-profile-1/meta") => request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|header| header == format!("Bearer {}", *ACCESS_JWT)),
            _ => false,
        } && request.uri().query().is_none();
        if !valid {
            api.violations
                .lock()
                .unwrap()
                .push(format!("{} {}", request.method(), request.uri()));
            return HttpStatus::BAD_REQUEST.into_response();
        }
        next.run(request).await
    }

    pub(crate) async fn serve(api: MockApi) -> MockServer {
        let app = Router::new()
            .route("/auth/token", post(token))
            .route("/profile/{id}/meta", get(meta))
            .route("/profile/{id}", get(archive))
            .layer(axum::middleware::from_fn_with_state(
                api.clone(),
                validate_request,
            ))
            .with_state(api.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockServer {
            url: format!("http://{addr}"),
            task,
            api,
        }
    }

    /// A minimal single-use-rotation auth service for the incident
    /// regression: each grant consumes the presented token and mints the
    /// next (`rt-{n}`), `lose_next_rotation` consumes the token but
    /// answers 500 without minting (the rotation is lost), and unknown
    /// tokens get a 400. Issued access tokens are already expired so
    /// every poll performs a grant.
    #[derive(Clone)]
    struct RotatingAuth {
        /// Refresh tokens the service currently accepts.
        valid: Arc<tokio::sync::Mutex<HashSet<String>>>,
        /// Minting counter; the next token is `rt-{n}`.
        next: Arc<AtomicUsize>,
        /// Total `/auth/token` requests received.
        grants: Arc<AtomicUsize>,
        lose_next_rotation: Arc<AtomicBool>,
    }

    impl RotatingAuth {
        /// Marks a token as currently valid, as a newly issued sign-in
        /// credential would be.
        async fn register(&self, token: &str) {
            self.valid.lock().await.insert(token.to_owned());
        }
    }

    async fn rotating_grant(
        State(auth): State<RotatingAuth>,
        Json(request): Json<serde_json::Value>,
    ) -> Response {
        auth.grants.fetch_add(1, Ordering::Relaxed);
        let presented = request["refreshToken"].as_str().unwrap_or_default();
        let mut valid = auth.valid.lock().await;
        if !valid.remove(presented) {
            return HttpStatus::BAD_REQUEST.into_response();
        }
        if auth.lose_next_rotation.swap(false, Ordering::Relaxed) {
            return HttpStatus::INTERNAL_SERVER_ERROR.into_response();
        }
        let minted = format!("rt-{}", auth.next.fetch_add(1, Ordering::Relaxed));
        valid.insert(minted.clone());
        Json(json!({
            "accessToken": jwt_with_expiry(1),
            "refreshToken": minted,
        }))
        .into_response()
    }

    /// Serves the rotating-auth mock. `/profile/{id}/meta` always 404s,
    /// so a successful grant resolves to `None` without archive work.
    async fn serve_rotating_auth(
        seed_tokens: &[&str],
    ) -> (RotatingAuth, String, tokio::task::JoinHandle<()>) {
        let auth = RotatingAuth {
            valid: Arc::new(tokio::sync::Mutex::new(
                seed_tokens.iter().map(|token| token.to_string()).collect(),
            )),
            next: Arc::new(AtomicUsize::new(1)),
            grants: Arc::new(AtomicUsize::new(0)),
            lose_next_rotation: Arc::new(AtomicBool::new(false)),
        };
        let app = Router::new()
            .route("/auth/token", post(rotating_grant))
            .route(
                "/profile/{id}/meta",
                get(|| async { HttpStatus::NOT_FOUND }),
            )
            .with_state(auth.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (auth, format!("http://{addr}"), task)
    }

    fn mod_rev(byte: char) -> ModRevision {
        ModRevision::try_from(byte.to_string().repeat(64)).unwrap()
    }

    fn operation(id: &str) -> OperationRecord {
        OperationRecord {
            id: id.to_owned(),
            executor: ExecutorKind::Worker,
            kind: OperationKind::Automatic,
            worker_id: Some("test".to_owned()),
            publication_revision: None,
            mods_revision: None,
            status: OperationStatus::Succeeded,
            summary: OperationSummary::default(),
            restart: RestartOutcome::NotRequired,
            error: None,
            started_at: Utc::now(),
            finished_at: Utc::now(),
        }
    }

    /// The journal minus its credential fields, so a test can assert a
    /// credential lifecycle left all operational state untouched.
    fn without_credentials(state: &WorkerJournal) -> serde_json::Value {
        let mut value = serde_json::to_value(state).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("refreshToken");
        object.remove("syncReauthorizationRequired");
        value
    }

    pub(crate) fn metadata(game: &str, updated_at: DateTime<Utc>) -> SyncProfileMetadata {
        SyncProfileMetadata {
            id: PROFILE_ID.to_owned(),
            created_at: Utc::now(),
            updated_at,
            owner: User {
                discord_id: "1".to_owned(),
                name: "owner".to_owned(),
                display_name: "Owner".to_owned(),
                avatar: None,
            },
            manifest: manifest(game),
        }
    }

    pub(crate) fn manifest(game: &str) -> ProfileManifest {
        ProfileManifest {
            name: "Pack".to_owned(),
            mods: vec![R2Mod {
                ident: PackageIdent::from(("Author", "Mod")),
                version: semver::Version::new(1, 0, 0).into(),
                enabled: true,
                source: Backend::Thunderstore,
            }],
            game: Some(game.to_owned()),
            ignored_version_updates: Vec::new(),
            ignored_package_updates: Vec::new(),
            sync: None,
        }
    }

    /// A legacy-format archive: `export.r2x` manifest plus config payloads.
    pub(crate) fn archive_zip(manifest: &ProfileManifest) -> Vec<u8> {
        use std::io::{Cursor, Write};
        use zip::{ZipWriter, write::SimpleFileOptions};

        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            zip.start_file("export.r2x", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(serde_yaml::to_string(manifest).unwrap().as_bytes())
                .unwrap();
            zip.start_file("BepInEx/config/mod.cfg", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"v1").unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    fn config(sync_url: String) -> WorkerConfig {
        WorkerConfig {
            profile_id: PROFILE_ID.to_owned(),
            game: "valheim".to_owned(),
            sync_url: Some(sync_url),
            ..Default::default()
        }
    }

    async fn journal() -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        {
            let mut state = journal.state.lock().await;
            state.refresh_token = Some("refresh-seed".to_owned());
            journal.save(&state).unwrap();
        }
        (dir, journal)
    }

    #[tokio::test]
    async fn poll_reports_none_without_a_publication() {
        let api = MockApi::default();
        let url = serve(api.clone()).await;
        let (_dir, journal) = journal().await;

        let probe = SyncClient::new(config(url.url.clone()), None)
            .poll(&journal, None)
            .await
            .unwrap();

        assert!(probe.is_none());
        assert_eq!(api.token_hits.load(Ordering::Relaxed), 1);
        assert_eq!(api.archive_hits.load(Ordering::Relaxed), 0);
        // The rotated refresh token was persisted even though nothing was
        // published.
        assert_eq!(
            journal.state.lock().await.refresh_token.as_deref(),
            Some("refresh-rotated")
        );
    }

    #[tokio::test]
    async fn poll_fetches_and_validates_a_new_publication() {
        let updated = Utc::now();
        let api = MockApi {
            meta: Some(metadata("valheim", updated)),
            archive: archive_zip(&manifest("valheim")),
            ..Default::default()
        };
        let url = serve(api.clone()).await;
        let (_dir, journal) = journal().await;

        let probe = SyncClient::new(config(url.url.clone()), None)
            .poll(&journal, None)
            .await
            .unwrap();

        let publication = probe.expect("expected a new publication");
        assert_eq!(publication.revision, updated);
        assert_eq!(publication.manifest.mods.len(), 1);
        assert_eq!(publication.config.len(), 1);
        // One token grant served both requests.
        assert_eq!(api.token_hits.load(Ordering::Relaxed), 1);
        assert_eq!(api.archive_hits.load(Ordering::Relaxed), 1);
        assert_eq!(
            journal.state.lock().await.refresh_token.as_deref(),
            Some("refresh-rotated")
        );
    }

    #[tokio::test]
    async fn unchanged_revision_skips_the_archive_download() {
        let updated = Utc::now();
        let api = MockApi {
            meta: Some(metadata("valheim", updated)),
            ..Default::default()
        };
        let url = serve(api.clone()).await;
        let (_dir, journal) = journal().await;

        let probe = SyncClient::new(config(url.url.clone()), None)
            .poll(&journal, Some(updated))
            .await
            .unwrap();

        assert!(probe.is_none());
        assert_eq!(api.archive_hits.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_foreign_game_publication_is_rejected() {
        let api = MockApi {
            meta: Some(metadata("not-valheim", Utc::now())),
            archive: archive_zip(&manifest("not-valheim")),
            ..Default::default()
        };
        let url = serve(api.clone()).await;
        let (_dir, journal) = journal().await;

        let result = SyncClient::new(config(url.url.clone()), None)
            .poll(&journal, None)
            .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("sync profile is for game")
        );
        assert_eq!(api.archive_hits.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn an_oversized_archive_is_refused() {
        for chunked in [false, true] {
            // A valid ZIP with a large archive comment/prefix padding: ZIP readers allow
            // prepended data. Rejection must be the transport bound, not ZIP parsing.
            let mut archive = vec![0; MAX_DOWNLOAD_BYTES + 1];
            archive.extend(archive_zip(&manifest("valheim")));
            zip::ZipArchive::new(std::io::Cursor::new(&archive)).unwrap();
            let api = MockApi {
                meta: Some(metadata("valheim", Utc::now())),
                archive,
                chunked,
                ..Default::default()
            };
            let server = serve(api).await;
            let (_dir, journal) = journal().await;
            let error = SyncClient::new(config(server.url.clone()), None)
                .poll(&journal, None)
                .await
                .err()
                .unwrap();
            assert_eq!(
                error.to_string(),
                "sync archive exceeds the download size limit"
            );
        }
    }

    #[tokio::test]
    async fn the_access_token_is_reused_until_it_expires() {
        let api = MockApi::default();
        let url = serve(api.clone()).await;
        let (_dir, journal) = journal().await;
        let client = SyncClient::new(config(url.url.clone()), None);

        client.poll(&journal, None).await.unwrap();
        client.poll(&journal, None).await.unwrap();

        // Two polls, one grant: the cached access token served the
        // second poll.
        assert_eq!(api.token_hits.load(Ordering::Relaxed), 1);
        assert_eq!(api.meta_hits.load(Ordering::Relaxed), 2);
        assert_eq!(
            journal.state.lock().await.refresh_token.as_deref(),
            Some("refresh-rotated")
        );
    }

    /// 400 and 401 both mean the credential is dead: the poll fails with
    /// the reauthorization message, the journal latches, and a restarted
    /// worker fast-fails without making another token request.
    #[tokio::test]
    async fn a_rejected_refresh_token_latches_until_a_new_sign_in() {
        for status in [400_u16, 401] {
            let api = MockApi::default();
            api.token_status.store(status, Ordering::Relaxed);
            let server = serve(api.clone()).await;
            let (dir, journal) = journal().await;
            let client = SyncClient::new(config(server.url.clone()), None);

            let error = client.poll(&journal, None).await.err().unwrap();
            assert_eq!(error.to_string(), SYNC_REAUTHORIZATION_REQUIRED);
            assert_eq!(api.token_hits.load(Ordering::Relaxed), 1);
            drop(journal);

            // The latch is durable: a restarted worker keeps the rejected
            // credential on record and does not present it again.
            let journal = Journal::load(dir.path()).unwrap();
            {
                let state = journal.state.lock().await;
                assert!(state.sync_reauthorization_required, "{status}");
                assert_eq!(state.refresh_token.as_deref(), Some("refresh-seed"));
            }
            let error = SyncClient::new(config(server.url.clone()), None)
                .poll(&journal, None)
                .await
                .err()
                .unwrap();
            assert_eq!(error.to_string(), SYNC_REAUTHORIZATION_REQUIRED);
            assert_eq!(api.token_hits.load(Ordering::Relaxed), 1, "{status}");
        }
    }

    /// Every failure other than a 400/401 rejection is retryable: the
    /// credential and the latch stay untouched and the next poll grants
    /// again.
    #[tokio::test]
    async fn transient_token_failures_stay_retryable() {
        for status in [429_u16, 500, 503] {
            let api = MockApi::default();
            api.token_status.store(status, Ordering::Relaxed);
            let server = serve(api.clone()).await;
            let (_dir, journal) = journal().await;
            let client = SyncClient::new(config(server.url.clone()), None);

            assert!(client.poll(&journal, None).await.is_err());
            {
                let state = journal.state.lock().await;
                assert!(!state.sync_reauthorization_required, "{status}");
                assert_eq!(state.refresh_token.as_deref(), Some("refresh-seed"));
            }
            // Still failing, but the next poll presents the same
            // credential again rather than condemning it.
            assert!(client.poll(&journal, None).await.is_err());
            assert_eq!(api.token_hits.load(Ordering::Relaxed), 2, "{status}");
        }

        // An unreachable sync service behaves the same: the credential is
        // not condemned and works against a reachable server afterwards.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (_dir, journal) = journal().await;
        assert!(
            SyncClient::new(config(format!("http://127.0.0.1:{port}")), None)
                .poll(&journal, None)
                .await
                .is_err()
        );
        {
            let state = journal.state.lock().await;
            assert!(!state.sync_reauthorization_required);
            assert_eq!(state.refresh_token.as_deref(), Some("refresh-seed"));
        }
        let api = MockApi::default();
        let server = serve(api.clone()).await;
        SyncClient::new(config(server.url.clone()), None)
            .poll(&journal, None)
            .await
            .unwrap();
        assert_eq!(api.token_hits.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_server_error_preserves_the_refresh_token_for_retry() {
        let api = MockApi::default();
        api.token_status.store(500, Ordering::Relaxed);
        let server = serve(api.clone()).await;
        let (_dir, journal) = journal().await;
        let client = SyncClient::new(config(server.url.clone()), None);

        let error = client.poll(&journal, None).await.err().unwrap();
        assert_eq!(error.to_string(), "sync token request failed");
        assert!(format!("{error:#}").contains("500 Internal Server Error"));
        assert_eq!(
            journal.state.lock().await.refresh_token.as_deref(),
            Some("refresh-seed")
        );

        api.token_status.store(0, Ordering::Relaxed);
        assert!(client.poll(&journal, None).await.unwrap().is_none());
        assert_eq!(
            *api.refresh_tokens.lock().await,
            ["refresh-seed"],
            "the retry uses the existing credential"
        );
    }

    /// The production incident end to end: the service consumes a
    /// refresh token but loses the rotation behind a generic 500, so the
    /// next grant is an unknown-token 400. That latches
    /// `sync_reauthorization_required`, polls stop contacting the
    /// service, a fresh sign-in recovers, and every non-credential
    /// journal field survives untouched.
    #[tokio::test]
    async fn a_lost_rotation_latches_until_a_new_sign_in() {
        let (auth, url, _task) = serve_rotating_auth(&["rt-0"]).await;

        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        let revision = Utc::now();
        let deployed_revision = revision - chrono::Duration::hours(1);
        let baseline = {
            let mut state = journal.state.lock().await;
            state.pending = Some(PendingWork::new(revision, mod_rev('b')));
            state.last_deployed_revision = Some(deployed_revision);
            state.deployed_mods_revision = Some(mod_rev('a'));
            state.auto_deploy_mods = true;
            state.restart_policy = RestartPolicy::WhenEmpty;
            state.automation_seeded = true;
            state.last_operation = Some(operation("op-1"));
            state.last_error = Some("automatic deployment failed: upload failed".to_owned());
            state.refresh_token = Some("rt-0".to_owned());
            journal.save(&state).unwrap();
            without_credentials(&state)
        };
        let client = SyncClient::new(config(url.clone()), None);

        // A healthy grant rotates rt-0 to rt-1.
        assert!(client.poll(&journal, None).await.unwrap().is_none());
        assert_eq!(
            journal.state.lock().await.refresh_token.as_deref(),
            Some("rt-1")
        );

        // The rotation is lost behind a 500: retryable, no latch, and the
        // (now dead) token stays in the journal.
        auth.lose_next_rotation.store(true, Ordering::Relaxed);
        assert!(client.poll(&journal, None).await.is_err());
        {
            let state = journal.state.lock().await;
            assert!(!state.sync_reauthorization_required);
            assert_eq!(state.refresh_token.as_deref(), Some("rt-1"));
        }

        // Presenting the consumed token is a permanent rejection.
        let error = client.poll(&journal, None).await.err().unwrap();
        assert_eq!(error.to_string(), SYNC_REAUTHORIZATION_REQUIRED);
        assert!(journal.state.lock().await.sync_reauthorization_required);

        // From here on the service is not contacted at all.
        let grants = auth.grants.load(Ordering::Relaxed);
        let error = client.poll(&journal, None).await.err().unwrap();
        assert_eq!(error.to_string(), SYNC_REAUTHORIZATION_REQUIRED);
        assert_eq!(auth.grants.load(Ordering::Relaxed), grants);

        // Same-binding setup installs a fresh credential; the poll
        // succeeds and the latch clears.
        auth.register("fresh").await;
        {
            let mut state = journal.state.lock().await;
            state.replace_sync_credential("fresh".to_owned());
            journal.save(&state).unwrap();
        }
        assert!(client.poll(&journal, None).await.unwrap().is_none());
        {
            let state = journal.state.lock().await;
            assert!(!state.sync_reauthorization_required);
            assert_eq!(state.refresh_token.as_deref(), Some("rt-2"));
            // Nothing but the credential changed.
            assert_eq!(without_credentials(&state), baseline);
        }

        // The recovered state is durable on disk too.
        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert!(!state.sync_reauthorization_required);
        assert_eq!(without_credentials(&state), baseline);
    }

    /// While the latch is set, a seed identical to the rejected
    /// credential is never retried, but a different seed is treated as a
    /// newly supplied sign-in — the manual-worker's recovery path.
    #[tokio::test]
    async fn a_different_seed_is_the_manual_recovery_path() {
        let (auth, url, _task) = serve_rotating_auth(&[]).await;
        let (_dir, journal) = journal().await;

        // The seeded token is unknown to the service: permanent
        // rejection, recorded into the journal.
        let error = SyncClient::new(config(url.clone()), None)
            .poll(&journal, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), SYNC_REAUTHORIZATION_REQUIRED);
        assert_eq!(auth.grants.load(Ordering::Relaxed), 1);

        // Restarting with the same seed makes no request.
        let error = SyncClient::new(config(url.clone()), Some("refresh-seed".to_owned()))
            .poll(&journal, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), SYNC_REAUTHORIZATION_REQUIRED);
        assert_eq!(auth.grants.load(Ordering::Relaxed), 1);

        // Restarting with a new seed presents it once and clears the
        // latch.
        auth.register("operator-supplied").await;
        let client = SyncClient::new(config(url.clone()), Some("operator-supplied".to_owned()));
        assert!(client.poll(&journal, None).await.unwrap().is_none());
        let state = journal.state.lock().await;
        assert!(!state.sync_reauthorization_required);
        assert_eq!(state.refresh_token.as_deref(), Some("rt-1"));
    }

    #[tokio::test]
    async fn concurrent_polls_use_the_rotated_token() {
        let api = MockApi::default();
        let url = serve(api.clone()).await;
        let client = SyncClient::new(config(url.url.clone()), None);
        let (_dir, journal) = journal().await;
        let (first, second) =
            tokio::join!(client.poll(&journal, None), client.poll(&journal, None));
        first.unwrap();
        second.unwrap();
        // The grant is serialized, and whichever poll ran second reused
        // the cached access token rather than rotating again.
        assert_eq!(*api.refresh_tokens.lock().await, ["refresh-seed"]);
        assert_eq!(api.meta_hits.load(Ordering::Relaxed), 2);
        assert_eq!(
            journal.state.lock().await.refresh_token.as_deref(),
            Some("refresh-rotated")
        );
    }
}
