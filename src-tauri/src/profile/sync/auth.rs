use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use eyre::{Context, OptionExt, Result, bail, eyre};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tauri::{AppHandle, Emitter, Manager, Url};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tracing::{debug, error, info, warn};

use crate::{db::Db, state::ManagerExt};

pub struct State {
    creds: Mutex<Option<AuthCredentials>>,
    refresh_lock: AsyncMutex<()>,
    /// The `gale://auth/callback` URL carries no flow identifier, so
    /// OAuth flows are exclusive and each callback resolves at most one
    /// of them. The slot frees itself when the waiting flow is dropped.
    oauth_callback: Mutex<Option<oneshot::Sender<String>>>,
}

impl State {
    pub fn new(stored_creds: Option<AuthCredentials>) -> Self {
        Self {
            creds: Mutex::new(stored_creds),
            refresh_lock: AsyncMutex::new(()),
            oauth_callback: Mutex::new(None),
        }
    }

    fn creds(&'_ self) -> MutexGuard<'_, Option<AuthCredentials>> {
        self.creds.lock().unwrap()
    }

    pub fn set_creds(&self, creds: Option<AuthCredentials>, db: &Db) -> Result<()> {
        let mut stored = self.creds();
        db.save_auth(creds.as_ref())?;
        *stored = creds;
        Ok(())
    }

    fn clear_if_current(&self, refresh_token: &str, db: &Db) -> Result<bool> {
        let mut stored = self.creds();
        if !stored
            .as_ref()
            .is_some_and(|creds| creds.refresh_token == refresh_token)
        {
            return Ok(false);
        }
        db.save_auth(None)?;
        *stored = None;
        Ok(true)
    }

    /// Registers the waiting end of a new OAuth flow. A dropped
    /// (timed-out or cancelled) flow closes its sender, so an abandoned
    /// flow never blocks the next sign-in.
    fn begin_oauth(&self) -> Result<oneshot::Receiver<String>> {
        let mut slot = self.oauth_callback.lock().unwrap();
        if slot.as_ref().is_some_and(|sender| !sender.is_closed()) {
            bail!(
                "another Gale sync sign-in is already waiting for the browser. Finish it or wait a minute, then try again"
            );
        }
        let (sender, receiver) = oneshot::channel();
        *slot = Some(sender);
        Ok(receiver)
    }

    /// Delivers the callback to the one waiting flow. The URL carries
    /// tokens, so it must never appear in an error or a log.
    fn complete_oauth(&self, url: String) -> Result<()> {
        let Some(sender) = self.oauth_callback.lock().unwrap().take() else {
            bail!("no Gale sync sign-in is waiting for this callback");
        };
        sender
            .send(url)
            .map_err(|_| eyre!("the Gale sync sign-in this callback belongs to has already ended"))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthCredentials {
    user: User,
    access_token: String,
    token_expiry: i64,
    refresh_token: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub discord_id: String,
    pub name: String,
    pub display_name: String,
    pub avatar: Option<String>,
}

impl AuthCredentials {
    fn from_tokens(access_token: String, refresh_token: String) -> Result<Self> {
        let JwtPayload { exp, user } = decode_jwt(&access_token).context("failed to decode jwt")?;

        Ok(Self {
            access_token,
            refresh_token,
            token_expiry: exp,
            user,
        })
    }

    /// The refresh token is the worker's long-lived credential: the sync
    /// service rotates it on every grant and the worker journal persists
    /// each rotation, so the seeded value only needs to be valid once.
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }
}

const OAUTH_TIMEOUT: Duration = Duration::from_mins(1);

pub async fn login_with_oauth(app: &AppHandle) -> Result<User> {
    let creds = oauth_credentials(app).await?;
    let user = creds.user.clone();

    info!("logged in as {}", user.name);

    let _refresh_guard = app.sync_auth().refresh_lock.lock().await;
    app.sync_auth().set_creds(Some(creds), app.db())?;

    Ok(user)
}

/// Runs the browser OAuth flow and returns the issued credentials
/// *without* storing them as the desktop session. The managed worker uses
/// this to get its own credential chain: the sync service rotates the
/// refresh token on every grant, so handing the worker a copy of the
/// desktop's token would break whichever side refreshed second.
pub async fn oauth_credentials(app: &AppHandle) -> Result<AuthCredentials> {
    // Register before opening the browser so the callback can never
    // arrive before — or be routed to — another flow.
    let callback = app.sync_auth().begin_oauth()?;
    let url = format!("{}/auth/login", *super::API_URL);
    open::that_detached(url).context("failed to open url in browser")?;

    tokio::select! {
        url = callback => {
         let url = url.context("the sign-in was abandoned")?;
         let url = Url::parse(&url).context("invalid url")?;
         let query: HashMap<_, _> = url.query_pairs().collect();

         let access_token = query
             .get("access_token")
             .ok_or_eyre("access_token parameter is missing")?
             .clone()
             .into_owned();

         let refresh_token = query
             .get("refresh_token")
             .ok_or_eyre("refresh_token parameter is missing")?
             .clone()
             .into_owned();

         app.get_webview_window("main").unwrap().set_focus().ok();

         AuthCredentials::from_tokens(access_token, refresh_token)
        }
        () = tokio::time::sleep(OAUTH_TIMEOUT) => {
            Err(eyre!("auth callback timed out"))
        }
    }
}

/// Forces a token grant with the desktop's stored refresh token. A second
/// OAuth login may invalidate the earlier chain, so worker provisioning
/// calls this afterwards to find out whether the desktop session survived.
pub async fn verify_session(app: &AppHandle) -> Result<()> {
    let _refresh_guard = app.sync_auth().refresh_lock.lock().await;
    let refresh_token = {
        let state = app.sync_auth();
        let creds = state.creds();
        creds
            .as_ref()
            .map(|creds| creds.refresh_token.clone())
            .ok_or_eyre("not logged in")?
    };

    refresh_desktop_token(refresh_token, app).await.map(|_| ())
}

pub async fn logout(app: &AppHandle) -> Result<()> {
    let _refresh_guard = app.sync_auth().refresh_lock.lock().await;
    app.sync_auth().set_creds(None, app.db())
}

pub fn handle_callback(url: String, app: &AppHandle) -> Result<()> {
    app.sync_auth().complete_oauth(url)
}

#[derive(Debug, Deserialize)]
struct JwtPayload {
    exp: i64,

    #[serde(flatten)]
    user: User,
}

fn decode_jwt<T: DeserializeOwned>(token: &str) -> Result<T> {
    let payload = token.split('.').nth(1).ok_or_eyre("token is malformed")?;

    let bytes = BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .context("failed to decode base64")?;

    serde_json::from_slice(&bytes).context("failed to deserialize json")
}

/// The `exp` claim of a sync access token, as issued by the sync service.
#[cfg(feature = "worker")]
pub(crate) fn access_token_expiry(access_token: &str) -> Result<DateTime<Utc>> {
    #[derive(Debug, Deserialize)]
    struct Expiry {
        exp: i64,
    }

    let Expiry { exp } = decode_jwt(access_token).context("failed to decode jwt")?;
    DateTime::from_timestamp(exp, 0).ok_or_eyre("access token expiry is out of range")
}

pub fn user_info(app: &AppHandle) -> Option<User> {
    app.sync_auth()
        .creds()
        .as_ref()
        .map(|state| state.user.clone())
}

/// The `POST /auth/token` answer. Every grant rotates the refresh token.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
}

pub async fn access_token(app: &AppHandle) -> Result<Option<String>> {
    let _refresh_guard = app.sync_auth().refresh_lock.lock().await;
    let (refresh_token, access_token, expiry) = {
        let state = app.sync_auth();
        let creds = state.creds.lock().unwrap();
        let Some(creds) = creds.as_ref() else {
            return Ok(None);
        };
        (
            creds.refresh_token.clone(),
            creds.access_token.clone(),
            DateTime::from_timestamp(creds.token_expiry, 0),
        )
    };

    let Some(expiry) = expiry else {
        warn!("token expiry date is invalid");
        expire_session(app, &refresh_token)?;
        bail!("sync session expired; sign in again");
    };
    if Utc::now() < expiry {
        return Ok(Some(access_token));
    }

    refresh_desktop_token(refresh_token, app).await.map(Some)
}

fn is_refresh_rejected(error: &eyre::Report) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|error| {
            matches!(
                error.status(),
                Some(StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED)
            )
        })
}

fn expire_session(app: &AppHandle, refresh_token: &str) -> Result<()> {
    if app.sync_auth().clear_if_current(refresh_token, app.db())?
        && let Err(error) = app.emit("sync_session_expired", ())
    {
        warn!(%error, "failed to notify the UI that the sync session expired");
    }
    Ok(())
}

async fn refresh_desktop_token(refresh_token: String, app: &AppHandle) -> Result<String> {
    match request_token(refresh_token.clone(), app).await {
        Ok(token) => Ok(token),
        Err(error) if is_refresh_rejected(&error) => {
            error!("failed to refresh access token: {error:#}");
            expire_session(app, &refresh_token)?;
            bail!("sync session expired; sign in again");
        }
        Err(error) => Err(error.wrap_err("could not refresh sync session; try again")),
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrantTokenRequest {
    pub(crate) refresh_token: String,
}

async fn request_token(refresh_token: String, app: &AppHandle) -> Result<String> {
    debug!("refreshing access token");

    let response = token_grant(app.http(), super::API_URL.as_ref(), &refresh_token).await?;

    let creds =
        AuthCredentials::from_tokens(response.access_token.clone(), response.refresh_token)?;

    app.sync_auth().set_creds(Some(creds), app.db())?;

    Ok(response.access_token)
}

async fn token_grant(
    client: &reqwest_middleware::ClientWithMiddleware,
    api_url: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    client
        .post(format!("{api_url}/auth/token"))
        .json(&GrantTokenRequest {
            refresh_token: refresh_token.to_owned(),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn rejected_refresh_is_detected_as_expired_session() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /auth/token HTTP/1.1"));
            stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let error = token_grant(&client, &url, "invalid-refresh")
            .await
            .expect_err("the fake auth server rejects the refresh token");
        assert!(is_refresh_rejected(&error));
        server.await.unwrap();
    }

    /// The callback URL carries no flow identifier, so one callback
    /// resolves exactly one flow and a second concurrent sign-in is
    /// refused rather than sharing the callback.
    #[tokio::test]
    async fn an_oauth_callback_resolves_exactly_one_waiting_flow() {
        let state = State::new(None);
        let first = state.begin_oauth().unwrap();
        assert!(
            state.begin_oauth().is_err(),
            "a second flow must not wait on the same callback"
        );

        state
            .complete_oauth("gale://auth/callback?access_token=a".to_owned())
            .unwrap();
        assert_eq!(first.await.unwrap(), "gale://auth/callback?access_token=a");

        // The callback was consumed; nothing is left for a late flow.
        assert!(
            state
                .complete_oauth("gale://auth/callback?x".to_owned())
                .is_err()
        );
    }

    /// A dropped receiver (timed-out or cancelled sign-in) frees the
    /// slot so the next flow can begin and be resolved normally.
    #[tokio::test]
    async fn an_ended_oauth_flow_frees_the_slot() {
        let state = State::new(None);
        drop(state.begin_oauth().unwrap());

        let second = state.begin_oauth().unwrap();
        state
            .complete_oauth("gale://auth/callback?access_token=b".to_owned())
            .unwrap();
        assert_eq!(second.await.unwrap(), "gale://auth/callback?access_token=b");
    }
}
