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

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::{Context, OptionExt, Result, ensure};
use reqwest::StatusCode;

use crate::profile::sync::{
    self, FetchedPublication, SyncProfileMetadata,
    auth::{GrantTokenRequest, TokenResponse},
};
use crate::worker::{config::WorkerConfig, journal::Journal, secrets};

/// Bounds how long a hung grant can hold the refresh lock — and delay
/// shutdown, which waits on it through `settle`.
const GRANT_TIMEOUT: Duration = Duration::from_secs(30);

/// Surfaced as the poll error when the sync service rejects the worker's
/// refresh token; the text tells the operator how to sign the worker in
/// again.
pub const SYNC_REAUTHORIZATION_REQUIRED: &str = "Gale sync rejected the Worker's sign-in (it expired or was revoked). Sign the Worker in again: run 'Set up worker' for a Worker hosted on this PC, or give a manually run Worker a new GALE_WORKER_REFRESH_TOKEN and restart it.";

/// The error a poll fails with while the sync service rejects the
/// worker's sign-in. Typed so the journal can classify the poll error
/// without matching its text.
#[derive(Debug)]
pub struct SyncReauthorizationRequired;

impl std::fmt::Display for SyncReauthorizationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(SYNC_REAUTHORIZATION_REQUIRED)
    }
}

impl std::error::Error for SyncReauthorizationRequired {}

/// Surfaced as the poll error when a rotated credential could not be
/// persisted; the text tells the operator what a restart would lose.
pub const SYNC_CREDENTIAL_NOT_SAVED: &str = "The Worker received a new Gale sync sign-in but could not save it to its state directory, so it has paused syncing and keeps retrying the save. If the Worker restarts before the save succeeds, sign it in again.";

/// A granted access token, reused until shortly before its `exp`.
struct CachedAccessToken {
    token: String,
    expires_at: DateTime<Utc>,
}

/// The credentials guarded by `refresh_lock`: the cached access token
/// plus a marker for a rotation the journal has not persisted yet.
struct CredentialSlot {
    access: Option<CachedAccessToken>,
    unsaved: bool,
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
    refresh_lock: Arc<tokio::sync::Mutex<CredentialSlot>>,
}

impl SyncClient {
    pub fn new(config: WorkerConfig, seed_refresh_token: Option<String>) -> Self {
        Self {
            config,
            seed_refresh_token,
            http: reqwest::Client::new(),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(CredentialSlot {
                access: None,
                unsaved: false,
            })),
        }
    }

    /// The configured sync service, or the desktop's.
    fn api_url(&self) -> &str {
        self.config
            .sync_url
            .as_deref()
            .unwrap_or(sync::API_URL.as_ref())
    }

    /// Ensures a usable access token exists, reusing the cached one while
    /// it remains valid and granting through `POST /auth/token`
    /// otherwise. Every grant rotates the refresh token, which is
    /// persisted back into the journal.
    ///
    /// Once a grant may have rotated the credential it runs to
    /// completion independently of whoever triggered it: the work is
    /// spawned holding an owned lock guard, so a dropped caller (the
    /// shutdown select, a cancelled request handler) cannot strand a
    /// rotation, and shutdown waits for it through `settle`. The request
    /// itself is bounded by `GRANT_TIMEOUT`; a response lost to the
    /// timeout after the service rotated is the protocol's unavoidable
    /// window.
    ///
    /// A 400/401 answer means the credential itself was rejected — there
    /// is no recovery short of a new sign-in, so the journal latches
    /// `sync_reauthorization_required` and no further token requests are
    /// made until setup installs a fresh credential or a different
    /// `GALE_WORKER_REFRESH_TOKEN` seed is supplied. Every other failure
    /// (network, 429, 5xx, malformed body) stays retryable: the journal
    /// is left untouched and the next poll tries again.
    async fn access_token(&self, journal: &Arc<Journal>) -> Result<String> {
        let mut slot = self.refresh_lock.clone().lock_owned().await;

        // A credential the service issued but the journal could not
        // persist is the only credential that still works, so it must
        // reach disk before anything else happens.
        if slot.unsaved {
            let state = journal.state.lock().await;
            if let Err(err) = journal.save(&state) {
                return Err(err.wrap_err(SYNC_CREDENTIAL_NOT_SAVED));
            }
            slot.unsaved = false;
        }

        if let Some(cached) = slot.access.as_ref()
            && Utc::now() + chrono::Duration::seconds(60) < cached.expires_at
        {
            return Ok(cached.token.clone());
        }

        tokio::spawn(grant(
            self.http.clone(),
            format!("{}/auth/token", self.api_url()),
            self.seed_refresh_token.clone(),
            journal.clone(),
            slot,
        ))
        .await
        .context("sync token grant task failed")?
    }

    /// Waits for a grant already running to commit: the spawned task
    /// holds the refresh lock, so acquiring it here means the journal
    /// is settled. Shutdown calls this before the process exits.
    pub async fn settle(&self) {
        let _guard = self.refresh_lock.lock().await;
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
        let Some(metadata) = sync::read_profile_meta(response).await? else {
            return Ok(None);
        };

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
        sync::read_archive(response).await
    }

    /// Polls for the canonical publication. When `since` is `Some`, the
    /// archive is only downloaded if the remote revision advanced past
    /// it. One access token serves both requests, and the token cache
    /// means most polls need no grant at all.
    pub async fn poll(
        &self,
        journal: &Arc<Journal>,
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

/// Runs one credential grant to completion. The owned guard keeps the
/// refresh lock held until the journal commit finishes, so the commit
/// cannot be lost when the future that triggered the grant is dropped.
async fn grant(
    http: reqwest::Client,
    url: String,
    seed: Option<String>,
    journal: Arc<Journal>,
    mut slot: tokio::sync::OwnedMutexGuard<CredentialSlot>,
) -> Result<String> {
    let candidate = {
        let state = journal.state.lock().await;
        if state.sync_reauthorization_required {
            // The journal holds the rejected credential; a seed equal
            // to it is the same dead token. Only a differing seed is
            // a newly supplied sign-in worth one attempt.
            match seed.as_deref() {
                Some(seed) if Some(seed) != state.refresh_token.as_deref() => seed.to_owned(),
                _ => return Err(SyncReauthorizationRequired.into()),
            }
        } else {
            state
                .refresh_token
                .clone()
                .or_else(|| seed.clone())
                .ok_or_eyre(format!(
                    "no refresh token in journal and {} is unset",
                    secrets::ENV_REFRESH_TOKEN
                ))?
        }
    };

    let response = http
        .post(url)
        .json(&GrantTokenRequest {
            refresh_token: candidate.clone(),
        })
        .timeout(GRANT_TIMEOUT)
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
        slot.access = None;
        return Err(SyncReauthorizationRequired.into());
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
    if let Err(err) = journal.save(&state) {
        // The commit point is a durable journal save. The rotated token
        // is the only credential that still works, so it stays in memory
        // for the next recommit and `unsaved` pauses syncing until the
        // save lands: a Worker that is syncing normally is always
        // restart-safe.
        slot.unsaved = true;
        slot.access = None;
        return Err(err.wrap_err(SYNC_CREDENTIAL_NOT_SAVED));
    }

    // A token whose expiry cannot be decoded is used once and never
    // cached; that decode failure must not fail the poll.
    slot.access = sync::auth::access_token_expiry(&tokens.access_token)
        .map(|expires_at| CachedAccessToken {
            token: tokens.access_token.clone(),
            expires_at,
        })
        .ok();

    Ok(tokens.access_token)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashSet;
    use std::sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
    };
    use std::time::Duration;

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

    type MockPublication = (SyncProfileMetadata, Vec<u8>);
    type ArchiveGate = (usize, Arc<tokio::sync::Semaphore>);

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
        pub(crate) publication: Arc<std::sync::Mutex<Option<MockPublication>>>,
        pub(crate) archive_gate: Arc<std::sync::Mutex<Option<ArchiveGate>>>,
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
        if let Some((meta, _)) = &*api.publication.lock().unwrap() {
            return Json(meta.clone()).into_response();
        }
        match &api.meta {
            Some(meta) => Json(meta.clone()).into_response(),
            None => HttpStatus::NOT_FOUND.into_response(),
        }
    }

    async fn archive(State(api): State<MockApi>, Path(_id): Path<String>) -> Response {
        let hit = api.archive_hits.fetch_add(1, Ordering::Relaxed) + 1;
        let gate = api.archive_gate.lock().unwrap().clone();
        if let Some((after, gate)) = gate
            && hit >= after
        {
            gate.acquire().await.unwrap().forget();
        }
        if let Some((_, archive)) = &*api.publication.lock().unwrap() {
            return archive.clone().into_response();
        }
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

        pub(crate) fn archive_requests(&self) -> usize {
            self.api.archive_hits.load(Ordering::Relaxed)
        }

        pub(crate) fn publish(&self, meta: SyncProfileMetadata, archive: Vec<u8>) {
            *self.api.publication.lock().unwrap() = Some((meta, archive));
        }

        /// Holds deployment's archive fetch after observation has fetched it once.
        pub(crate) fn hold_deployment_archive(&self) -> Arc<tokio::sync::Semaphore> {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            *self.api.archive_gate.lock().unwrap() =
                Some((self.archive_requests() + 2, gate.clone()));
            gate
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
    /// tokens get a 400. `respond_delay` holds the response back *after*
    /// the rotation so a caller can be dropped mid-commit. Issued access
    /// tokens are already expired so every poll performs a grant.
    #[derive(Clone)]
    pub(crate) struct RotatingAuth {
        /// Refresh tokens the service currently accepts.
        valid: Arc<tokio::sync::Mutex<HashSet<String>>>,
        /// Minting counter; the next token is `rt-{n}`.
        next: Arc<AtomicUsize>,
        /// Total `/auth/token` requests received.
        pub(crate) grants: Arc<AtomicUsize>,
        lose_next_rotation: Arc<AtomicBool>,
        respond_delay: Duration,
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
        // The token is already consumed and its successor registered, so
        // the delay must not hold the lock that a second request needs.
        drop(valid);
        if !auth.respond_delay.is_zero() {
            tokio::time::sleep(auth.respond_delay).await;
        }
        Json(json!({
            "accessToken": jwt_with_expiry(1),
            "refreshToken": minted,
        }))
        .into_response()
    }

    /// Serves the rotating-auth mock. `/profile/{id}/meta` always 404s,
    /// so a successful grant resolves to `None` without archive work.
    pub(crate) async fn serve_rotating_auth(
        seed_tokens: &[&str],
        respond_delay: Duration,
    ) -> (RotatingAuth, String, tokio::task::JoinHandle<()>) {
        let auth = RotatingAuth {
            valid: Arc::new(tokio::sync::Mutex::new(
                seed_tokens.iter().map(|token| token.to_string()).collect(),
            )),
            next: Arc::new(AtomicUsize::new(1)),
            grants: Arc::new(AtomicUsize::new(0)),
            lose_next_rotation: Arc::new(AtomicBool::new(false)),
            respond_delay,
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
            ignored_version_updates: Default::default(),
            ignored_package_updates: Default::default(),
            excluded_files: Default::default(),
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

    async fn journal() -> (tempfile::TempDir, Arc<Journal>) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::load(dir.path()).unwrap());
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
            let mut archive = vec![0; sync::MAX_DOWNLOAD_BYTES + 1];
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
            let journal = Arc::new(Journal::load(dir.path()).unwrap());
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
        let (auth, url, _task) = serve_rotating_auth(&["rt-0"], Duration::ZERO).await;

        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::load(dir.path()).unwrap());
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
        let (auth, url, _task) = serve_rotating_auth(&[], Duration::ZERO).await;
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

    /// A caller dropped mid-grant (the shutdown select or a cancelled
    /// request handler) must not strand a rotation the service already
    /// made: the grant still commits to the journal, so the next poll
    /// uses the rotated token rather than presenting the consumed one.
    #[tokio::test]
    async fn a_dropped_caller_cannot_strand_a_rotation() {
        let (auth, url, _task) = serve_rotating_auth(&["rt-0"], Duration::from_millis(500)).await;
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::load(dir.path()).unwrap());
        {
            let mut state = journal.state.lock().await;
            state.refresh_token = Some("rt-0".to_owned());
            journal.save(&state).unwrap();
        }
        let client = SyncClient::new(config(url), None);

        // The service mints rt-1, then holds the response past this
        // timeout: the caller is gone before the grant could commit.
        let elapsed =
            tokio::time::timeout(Duration::from_millis(100), client.poll(&journal, None)).await;
        assert!(elapsed.is_err(), "the delayed grant outlives its caller");

        assert!(client.poll(&journal, None).await.unwrap().is_none());
        assert_eq!(auth.grants.load(Ordering::Relaxed), 2);
        assert!(!journal.state.lock().await.sync_reauthorization_required);

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(state.refresh_token.as_deref(), Some("rt-2"));
        assert!(!state.sync_reauthorization_required);
    }

    /// A rotation whose journal commit fails pauses syncing instead of
    /// moving on: the new credential lives only in memory until the
    /// save succeeds, so no further grant is attempted and no cached
    /// access token is served.
    #[tokio::test]
    async fn an_unsaved_rotation_pauses_sync_until_it_is_saved() {
        let (auth, url, _task) = serve_rotating_auth(&["rt-0"], Duration::ZERO).await;
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::load(dir.path()).unwrap());
        let revision = Utc::now();
        let baseline = {
            let mut state = journal.state.lock().await;
            state.pending = Some(PendingWork::new(revision, mod_rev('b')));
            state.last_deployed_revision = Some(revision - chrono::Duration::hours(1));
            state.deployed_mods_revision = Some(mod_rev('a'));
            state.auto_deploy_mods = true;
            state.restart_policy = RestartPolicy::WhenEmpty;
            state.last_operation = Some(operation("op-1"));
            state.refresh_token = Some("rt-0".to_owned());
            journal.save(&state).unwrap();
            without_credentials(&state)
        };
        let client = SyncClient::new(config(url), None);

        // A directory at the temp path makes every journal save fail.
        std::fs::create_dir(dir.path().join("gale-worker-state.json.tmp")).unwrap();
        let error = client.poll(&journal, None).await.err().unwrap();
        assert_eq!(error.to_string(), SYNC_CREDENTIAL_NOT_SAVED);
        assert_eq!(auth.grants.load(Ordering::Relaxed), 1);

        // Syncing pauses on the unsaved credential rather than granting
        // again from memory.
        let error = client.poll(&journal, None).await.err().unwrap();
        assert_eq!(error.to_string(), SYNC_CREDENTIAL_NOT_SAVED);
        assert_eq!(auth.grants.load(Ordering::Relaxed), 1);

        // Once the recommit lands, syncing resumes on the rotated chain.
        std::fs::remove_dir(dir.path().join("gale-worker-state.json.tmp")).unwrap();
        assert!(client.poll(&journal, None).await.unwrap().is_none());
        assert_eq!(auth.grants.load(Ordering::Relaxed), 2);

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(state.refresh_token.as_deref(), Some("rt-2"));
        assert!(!state.sync_reauthorization_required);
        assert_eq!(without_credentials(&state), baseline);
    }
}
