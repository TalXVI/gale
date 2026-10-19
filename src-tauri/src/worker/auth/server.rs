//! Server-side authentication state and HTTP admission.

use super::{CHALLENGE_HEADER, MAX_RESPONSE_BYTES, Proof, RESPONSE_HEADER, VALID_SECS};
use crate::worker::api::ErrorResponse;
use axum::{
    Json,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

pub(crate) struct ServerAuth {
    token: String,
    epoch: Uuid,
    nonces: Mutex<HashMap<Uuid, Instant>>,
}

impl ServerAuth {
    pub fn new(token: String) -> Self {
        Self {
            token,
            epoch: Uuid::new_v4(),
            nonces: Mutex::new(HashMap::new()),
        }
    }
}

pub(crate) async fn health(State(auth): State<Arc<ServerAuth>>) -> impl IntoResponse {
    (
        StatusCode::NO_CONTENT,
        [(CHALLENGE_HEADER, auth.epoch.to_string())],
    )
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            error: "missing or invalid bearer token".into(),
        }),
    )
        .into_response()
}

/// Authenticates signed loopback requests or a TLS proxy's bearer token
/// before any handler reads the request.
pub(crate) async fn authenticate(
    State(auth): State<Arc<ServerAuth>>,
    request: Request,
    next: Next,
) -> Response {
    let authorization = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if authorization.starts_with("Gale-v1 ") {
        let (parts, body) = request.into_parts();
        let Ok(body) = axum::body::to_bytes(body, 2 * 1024 * 1024).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        let target = parts
            .uri
            .path_and_query()
            .map_or("/", |target| target.as_str());
        let Some(proof) = Proof::verify(
            &auth.token,
            auth.epoch,
            &authorization,
            parts.method.as_str(),
            target,
            &body,
        ) else {
            return unauthorized();
        };
        {
            let mut nonces = auth.nonces.lock().await;
            nonces.retain(|_, seen| seen.elapsed().as_secs() <= (VALID_SECS * 2) as u64);
            if nonces.contains_key(&proof.nonce()) || nonces.len() >= 4096 {
                return unauthorized();
            }
            nonces.insert(proof.nonce(), std::time::Instant::now());
        }
        let response = next
            .run(Request::from_parts(parts, axum::body::Body::from(body)))
            .await;
        let (mut parts, body) = response.into_parts();
        let Ok(body) = axum::body::to_bytes(body, MAX_RESPONSE_BYTES).await else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        parts.headers.insert(
            RESPONSE_HEADER,
            proof
                .response_tag(&auth.token, parts.status.as_u16(), &body)
                .to_hex()
                .as_str()
                .parse()
                .expect("a hex tag is a valid header"),
        );
        return Response::from_parts(parts, axum::body::Body::from(body));
    }
    // Bearer authentication remains available for clients behind a TLS proxy.
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        // blake3 hashes compare in constant time, so the response time
        // reveals nothing about how much of a guessed token was right.
        .is_some_and(|token| blake3::hash(token.as_bytes()) == blake3::hash(auth.token.as_bytes()));
    if !authorized {
        return unauthorized();
    }
    next.run(request).await
}
