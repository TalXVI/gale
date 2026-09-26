//! The worker's standalone Gale sync client.
//!
//! Mirrors the desktop's `profile::sync` transport but takes credentials
//! from the journal and environment rather than `AppHandle`/keyring, so
//! it can run unattended. Token rotation is persisted to the journal.
//! Archive validation goes through the same `publication_from_archive`
//! path the desktop uses, so a "canonical publication" is the identical
//! artifact on both sides.

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

/// What the worker fetched for one poll cycle: metadata only when the
/// archive is unchanged (cheap revision check), or the full validated
/// publication.
pub enum PublicationProbe {
    /// No publication has ever been pushed for this profile.
    None,
    /// The remote revision did not advance past `since`.
    Unchanged(SyncProfileMetadata),
    /// A new publication revision, downloaded and validated.
    New(FetchedPublication),
}

pub struct SyncClient {
    config: WorkerConfig,
    /// The seed refresh token from the worker's secrets, used until the
    /// journal holds a rotated one.
    seed_refresh_token: Option<String>,
    http: reqwest::Client,
    /// Refresh tokens rotate once per grant; serialize reading and replacing them.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl SyncClient {
    pub fn new(config: WorkerConfig, seed_refresh_token: Option<String>) -> Self {
        Self {
            config,
            seed_refresh_token,
            http: reqwest::Client::new(),
            refresh_lock: tokio::sync::Mutex::new(()),
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

    /// Ensures a usable access token exists, refreshing through
    /// `POST /auth/token` when needed and persisting the rotated refresh
    /// token back into the journal.
    async fn access_token(&self, journal: &Journal) -> Result<String> {
        let _guard = self.refresh_lock.lock().await;
        let refresh = {
            let state = journal.state.lock().await;
            state.refresh_token.clone()
        }
        .or_else(|| self.seed_refresh_token.clone())
        .ok_or_eyre(format!(
            "no refresh token in journal and {} is unset",
            secrets::ENV_REFRESH_TOKEN
        ))?;

        let response = self
            .http
            .post(format!("{}/auth/token", self.api_url()))
            .json(&GrantTokenRequest {
                refresh_token: refresh,
            })
            .send()
            .await
            .context("failed to reach the sync service")?;

        if response.status() == StatusCode::UNAUTHORIZED {
            bail!(
                "sync refresh token was rejected; re-seed {}",
                secrets::ENV_REFRESH_TOKEN
            );
        }
        let response = response
            .error_for_status()
            .context("sync token request failed")?;

        let tokens: TokenResponse = response
            .json()
            .await
            .context("sync token response was malformed")?;

        let mut state = journal.state.lock().await;
        state.refresh_token = Some(tokens.refresh_token);
        journal.save(&state)?;

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
    /// it. One token grant serves both requests. A refresh rotates the
    /// refresh token, so requesting a second grant per poll would be
    /// wasted work.
    pub async fn poll(
        &self,
        journal: &Journal,
        since: Option<DateTime<Utc>>,
    ) -> Result<PublicationProbe> {
        let token = self.access_token(journal).await?;

        let Some(metadata) = self.manifest(&token).await? else {
            return Ok(PublicationProbe::None);
        };

        if let Some(seen) = since
            && metadata.updated_at <= seen
        {
            return Ok(PublicationProbe::Unchanged(metadata));
        }

        let bytes = self.archive_bytes(&token, &metadata).await?;
        let publication = sync::publication_from_archive(&metadata, &bytes)
            .context("canonical publication failed validation")?;

        Ok(PublicationProbe::New(publication))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        extract::{Path, State},
        http::StatusCode as HttpStatus,
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use serde_json::json;

    use super::*;
    use crate::{
        profile::{
            export::{ProfileManifest, R2Mod},
            sync::{SyncProfileMetadata, auth::User},
        },
        thunderstore::{Backend, PackageIdent},
    };

    const PROFILE_ID: &str = "sync-profile-1";

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
            "accessToken": "access-1",
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
            ("GET", "/profile/sync-profile-1" | "/profile/sync-profile-1/meta") => {
                request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    == Some("Bearer access-1")
            }
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

        assert!(matches!(probe, PublicationProbe::None));
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

        let PublicationProbe::New(publication) = probe else {
            panic!("expected a new publication");
        };
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

        assert!(matches!(probe, PublicationProbe::Unchanged(_)));
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
    async fn a_rejected_refresh_token_fails_clearly() {
        let api = MockApi {
            fail_auth: true,
            ..Default::default()
        };
        let url = serve(api).await;
        let (_dir, journal) = journal().await;

        let result = SyncClient::new(config(url.url.clone()), None)
            .poll(&journal, None)
            .await;
        let error = result.err().unwrap();
        assert!(
            error
                .to_string()
                .contains("sync refresh token was rejected")
        );
        assert_eq!(
            journal.state.lock().await.refresh_token.as_deref(),
            Some("refresh-seed")
        );
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
        assert!(matches!(
            client.poll(&journal, None).await.unwrap(),
            PublicationProbe::None
        ));
        assert_eq!(
            *api.refresh_tokens.lock().await,
            ["refresh-seed"],
            "the retry uses the existing credential"
        );
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
        assert_eq!(
            *api.refresh_tokens.lock().await,
            ["refresh-seed", "refresh-rotated"]
        );
    }
}
