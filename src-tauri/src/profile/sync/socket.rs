use std::{collections::BTreeSet, time::Duration};

use eyre::{OptionExt, Result, bail};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tokio::{sync::watch, time};
use tracing::{debug, error, info, warn};

use crate::{
    profile::{ModManager, Profile},
    state::ManagerExt,
};

type WebSocket =
    tokio_websockets::WebSocketStream<tokio_websockets::MaybeTlsStream<tokio::net::TcpStream>>;

const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "event", content = "payload", rename_all = "camelCase")]
enum ServerMessage {
    ProfileUpdated {
        metadata: Box<super::SyncProfileMetadata>,
    },

    ProfileDeleted {
        id: String,
    },

    ProfileNotFound {
        id: String,
    },

    Error {
        message: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", content = "payload", rename_all = "camelCase")]
enum ClientMessage<'a> {
    #[serde(rename_all = "camelCase")]
    Subscribe { profile_id: &'a str },

    #[serde(rename_all = "camelCase")]
    Unsubscribe { profile_id: &'a str },
}

pub struct State {
    subscriptions: watch::Sender<BTreeSet<String>>,
}

impl State {
    pub fn new(app: AppHandle) -> Self {
        let (subscriptions, rx) = watch::channel(BTreeSet::new());

        tauri::async_runtime::spawn(async move {
            let url = format!("{}/socket/connect", super::API_URL.replace("http", "ws"));
            if let Err(err) = connect(&url, rx, |message| handle_message(&app, message)).await {
                error!(?err, "sync socket task failed");
            }
        });

        Self { subscriptions }
    }

    pub fn subscribe(&self, profile: &Profile) {
        if let Some(info) = &profile.sync {
            self.subscriptions
                .send_if_modified(|profiles| profiles.insert(info.id.clone()));
        }
    }

    pub fn unsubscribe(&self, profile: &Profile) {
        if let Some(info) = &profile.sync {
            self.subscriptions
                .send_if_modified(|profiles| profiles.remove(&info.id));
        }
    }
}

async fn connect(
    url: &str,
    mut subscriptions: watch::Receiver<BTreeSet<String>>,
    mut on_message: impl FnMut(ServerMessage),
) -> Result<()> {
    let client = tokio_websockets::ClientBuilder::new().uri(url)?;
    let mut retry_delay = INITIAL_RETRY_DELAY;

    loop {
        if subscriptions
            .wait_for(|profiles| !profiles.is_empty())
            .await
            .is_err()
            || subscriptions.has_changed().is_err()
        {
            return Ok(());
        }

        info!("connecting to sync server socket at {url}");
        let result = async {
            let (socket, _) = tokio::select! {
                result = time::timeout(CONNECT_TIMEOUT, client.connect()) => result??,
                _ = subscriptions.wait_for(BTreeSet::is_empty) => return Ok(()),
            };
            retry_delay = INITIAL_RETRY_DELAY;
            exchange(socket, &mut subscriptions, &mut on_message).await
        }
        .await;

        if let Err(err) = result {
            warn!(?err, ?retry_delay, "sync socket disconnected; retrying");
            tokio::select! {
                () = time::sleep(retry_delay) => {},
                _ = subscriptions.wait_for(BTreeSet::is_empty) => {},
            }
            retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
        }
    }
}

async fn exchange(
    socket: WebSocket,
    subscriptions: &mut watch::Receiver<BTreeSet<String>>,
    on_message: &mut impl FnMut(ServerMessage),
) -> Result<()> {
    let (mut sender, mut receiver) = socket.split();
    let mut subscribed = BTreeSet::new();

    loop {
        let desired = subscriptions.borrow_and_update().clone();
        for profile_id in subscribed.difference(&desired) {
            send_message(&mut sender, ClientMessage::Unsubscribe { profile_id }).await?;
        }
        for profile_id in desired.difference(&subscribed) {
            send_message(&mut sender, ClientMessage::Subscribe { profile_id }).await?;
        }
        subscribed = desired;
        if subscribed.is_empty() {
            return Ok(());
        }

        tokio::select! {
            changed = subscriptions.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            },
            item = receiver.next() => {
                let message = item.ok_or_eyre("sync socket stream closed")??;
                if message.is_close() {
                    bail!("sync server closed the socket");
                }
                if let Some(text) = message.as_text() {
                    on_message(serde_json::from_str(text)?);
                }
            },
        }
    }
}

async fn send_message(
    sender: &mut SplitSink<WebSocket, tokio_websockets::Message>,
    message: ClientMessage<'_>,
) -> Result<()> {
    debug!("sending {message:?}");
    sender
        .send(tokio_websockets::Message::text(serde_json::to_string(
            &message,
        )?))
        .await?;
    Ok(())
}

fn handle_message(app: &AppHandle, message: ServerMessage) {
    match message {
        ServerMessage::ProfileUpdated { metadata } => {
            info!("got sync profile update event for {}", metadata.id);
            let mut manager = app.lock_manager();
            for profile in sync_profiles_with_id(&mut manager, &metadata.id) {
                profile.sync.as_mut().unwrap().observe_metadata(&metadata);
                if let Err(err) = profile.save(app, true) {
                    error!(?err, "failed to save sync profile update");
                }
            }
        }
        ServerMessage::ProfileNotFound { id } | ServerMessage::ProfileDeleted { id } => {
            info!("got sync profile delete event for {}", id);
            let mut manager = app.lock_manager();
            for profile in sync_profiles_with_id(&mut manager, &id) {
                profile.sync.as_mut().unwrap().missing = true;
                if let Err(err) = profile.save(app, true) {
                    error!(?err, "failed to save missing sync profile");
                }
            }
        }
        ServerMessage::Error { message } => {
            error!("got error from socket: {message}");
        }
    }
}

fn sync_profiles_with_id<'a>(
    manager: &'a mut ModManager,
    id: &'a str,
) -> impl Iterator<Item = &'a mut Profile> + 'a {
    manager
        .games
        .values_mut()
        .flat_map(|game| game.profiles.iter_mut())
        .filter(move |profile| profile.sync.as_ref().is_some_and(|info| info.id == id))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::{io::AsyncWriteExt, net::TcpListener, sync::mpsc};
    use tokio_websockets::{Message, ServerBuilder, WebSocketStream};

    use super::*;

    type ServerSocket = WebSocketStream<tokio::net::TcpStream>;

    async fn accept(listener: &TcpListener) -> ServerSocket {
        time::timeout(Duration::from_secs(10), async {
            let (stream, _) = listener.accept().await.unwrap();
            ServerBuilder::new().accept(stream).await.unwrap().1
        })
        .await
        .expect("sync client did not reconnect")
    }

    async fn receive(socket: &mut ServerSocket) -> Value {
        let message = time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("missing subscription message")
            .unwrap()
            .unwrap();
        serde_json::from_str(message.as_text().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reconnects_after_handshake_failure_and_restores_current_subscriptions() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/socket/connect", listener.local_addr().unwrap());
        let (subscriptions, rx) = watch::channel(BTreeSet::from(["first".to_owned()]));
        let (observations, mut received) = mpsc::unbounded_channel();
        let client = tokio::spawn(async move {
            connect(&url, rx, |message| observations.send(message).unwrap()).await
        });

        let (mut rejected, _) = time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        rejected
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        drop(rejected);

        let mut socket = accept(&listener).await;
        assert_eq!(
            receive(&mut socket).await,
            json!({"event": "subscribe", "payload": {"profileId": "first"}})
        );
        subscriptions.send_replace(BTreeSet::from(["second".to_owned()]));
        assert_eq!(
            receive(&mut socket).await,
            json!({"event": "unsubscribe", "payload": {"profileId": "first"}})
        );
        assert_eq!(
            receive(&mut socket).await,
            json!({"event": "subscribe", "payload": {"profileId": "second"}})
        );

        drop(socket);
        subscriptions.send_replace(BTreeSet::from(["current".to_owned()]));
        let mut socket = accept(&listener).await;
        assert_eq!(
            receive(&mut socket).await,
            json!({"event": "subscribe", "payload": {"profileId": "current"}})
        );
        socket
            .send(Message::text(
                json!({
                    "event": "profileUpdated",
                    "payload": {"metadata": {
                        "id": "current",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-10-17T00:00:00Z",
                        "owner": {"discordId": "1", "name": "owner", "displayName": "Owner", "avatar": null},
                        "manifest": {"profileName": "Test", "mods": []}
                    }}
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let observation = time::timeout(Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap();
        let ServerMessage::ProfileUpdated { metadata } = observation else {
            panic!("expected publication metadata");
        };
        assert_eq!(metadata.id, "current");
        assert_eq!(
            metadata.updated_at.to_rfc3339(),
            "2026-10-17T00:00:00+00:00"
        );

        drop(subscriptions);
        time::timeout(Duration::from_secs(10), client)
            .await
            .expect("sync client did not stop")
            .unwrap()
            .unwrap();
    }
}
