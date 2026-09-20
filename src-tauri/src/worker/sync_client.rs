//! The worker's standalone Gale sync client.
//!
//! Mirrors the desktop's `profile::sync` transport but sources credentials
//! from the journal/environment rather than `AppHandle`/keyring, so it can
//! run unattended. Token rotation is persisted to the journal; archive
//! validation goes through the same `publication_from_archive` path the
//! desktop uses, so a "canonical publication" means the identical artifact
//! on both sides.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use eyre::{Context, OptionExt, Result, bail, ensure};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::profile::sync::{self, FetchedPublication, SyncProfileMetadata};
use crate::worker::{config::WorkerConfig, journal::Journal};

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
    http: reqwest::Client,
}

impl SyncClient {
    pub fn new(config: WorkerConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
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
        let refresh = {
            let state = journal.state.lock().await;
            state.refresh_token.clone()
        }
        .or_else(WorkerConfig::seed_refresh_token)
        .ok_or_eyre("no refresh token in journal and GALE_WORKER_REFRESH_TOKEN is unset")?;

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
            bail!("sync refresh token was rejected; re-seed GALE_WORKER_REFRESH_TOKEN");
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

    /// Polls for the canonical publication. When `since` is `Some`, only
    /// downloads the archive if the remote revision advanced past it. One
    /// token grant serves both requests — a refresh rotates the refresh
    /// token, so doubling it per poll is pure waste.
    pub async fn poll(
        &self,
        journal: &Journal,
        since: Option<DateTime<Utc>>,
    ) -> Result<PublicationProbe> {
        let token = self.access_token(journal).await?;

        let Some(metadata) = self.manifest(&token).await? else {
            return Ok(PublicationProbe::None);
        };

        if let Some(seen) = since {
            if metadata.updated_at <= seen {
                return Ok(PublicationProbe::Unchanged(metadata));
            }
        }

        let bytes = self.archive_bytes(&token, &metadata).await?;
        let publication = sync::publication_from_archive(&metadata, &bytes)
            .context("canonical publication failed validation")?;

        Ok(PublicationProbe::New(publication))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
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
    struct MockApi {
        meta: Option<SyncProfileMetadata>,
        archive: Vec<u8>,
        fail_auth: bool,
        token_hits: Arc<AtomicUsize>,
        meta_hits: Arc<AtomicUsize>,
        archive_hits: Arc<AtomicUsize>,
    }

    async fn token(State(api): State<MockApi>) -> Response {
        api.token_hits.fetch_add(1, Ordering::Relaxed);
        if api.fail_auth {
            return HttpStatus::UNAUTHORIZED.into_response();
        }
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
        api.archive.clone().into_response()
    }

    /// Serves the mock sync API on an ephemeral localhost port.
    async fn serve(api: MockApi) -> String {
        let app = Router::new()
            .route("/auth/token", post(token))
            .route("/profile/{id}/meta", get(meta))
            .route("/profile/{id}", get(archive))
            .with_state(api);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn metadata(game: &str, updated_at: DateTime<Utc>) -> SyncProfileMetadata {
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

    fn manifest(game: &str) -> ProfileManifest {
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
    fn archive_zip(manifest: &ProfileManifest) -> Vec<u8> {
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

        let probe = SyncClient::new(config(url))
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

        let probe = SyncClient::new(config(url))
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

        let probe = SyncClient::new(config(url))
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
            ..Default::default()
        };
        let url = serve(api).await;
        let (_dir, journal) = journal().await;

        let result = SyncClient::new(config(url)).poll(&journal, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn an_oversized_archive_is_refused() {
        let api = MockApi {
            meta: Some(metadata("valheim", Utc::now())),
            archive: vec![0u8; MAX_DOWNLOAD_BYTES + 1],
            ..Default::default()
        };
        let url = serve(api).await;
        let (_dir, journal) = journal().await;

        let result = SyncClient::new(config(url)).poll(&journal, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn a_rejected_refresh_token_fails_clearly() {
        let api = MockApi {
            fail_auth: true,
            ..Default::default()
        };
        let url = serve(api).await;
        let (_dir, journal) = journal().await;

        let result = SyncClient::new(config(url)).poll(&journal, None).await;
        assert!(result.is_err());
    }
}
