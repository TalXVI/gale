//! Authentication for loopback HTTP. The shared token never crosses the socket.
//! BLAKE3 keyed hashes bind each request and response to a fresh nonce; the
//! server admits each nonce once within a short validity window.

use blake3::Hash;
use chrono::Utc;
use uuid::Uuid;

pub const CHALLENGE_HEADER: &str = "x-gale-challenge";
pub const RESPONSE_HEADER: &str = "x-gale-response";
#[cfg(feature = "worker")]
pub const VALID_SECS: i64 = 30;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub struct Proof {
    epoch: Uuid,
    timestamp: i64,
    nonce: Uuid,
    tag: Hash,
}

fn mac(token: &str, domain: &[u8], fields: &[&[u8]]) -> Hash {
    let key = blake3::derive_key("Gale Worker HTTP authentication v1", token.as_bytes());
    let mut hash = blake3::Hasher::new_keyed(&key);
    hash.update(domain);
    for field in fields {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    hash.finalize()
}

impl Proof {
    pub fn sign(token: &str, epoch: Uuid, method: &str, target: &str, body: &[u8]) -> Self {
        let timestamp = Utc::now().timestamp();
        let nonce = Uuid::new_v4();
        Self {
            epoch,
            timestamp,
            nonce,
            tag: Self::request_tag(token, epoch, timestamp, &nonce, method, target, body),
        }
    }

    fn request_tag(
        token: &str,
        epoch: Uuid,
        timestamp: i64,
        nonce: &Uuid,
        method: &str,
        target: &str,
        body: &[u8],
    ) -> Hash {
        mac(
            token,
            b"request-v1",
            &[
                epoch.as_bytes(),
                &timestamp.to_le_bytes(),
                nonce.as_bytes(),
                method.as_bytes(),
                target.as_bytes(),
                body,
            ],
        )
    }

    pub fn authorization(&self) -> String {
        format!(
            "Gale-v1 {}:{}:{}:{}",
            self.epoch,
            self.timestamp,
            self.nonce,
            self.tag.to_hex()
        )
    }

    #[cfg(feature = "worker")]
    pub fn verify(
        token: &str,
        epoch: Uuid,
        authorization: &str,
        method: &str,
        target: &str,
        body: &[u8],
    ) -> Option<Self> {
        let mut fields = authorization.strip_prefix("Gale-v1 ")?.split(':');
        if fields.next()?.parse::<Uuid>().ok()? != epoch {
            return None;
        }
        let timestamp = fields.next()?.parse::<i64>().ok()?;
        let nonce = fields.next()?.parse::<Uuid>().ok()?;
        let tag = Hash::from_hex(fields.next()?).ok()?;
        if fields.next().is_some()
            || Utc::now().timestamp().abs_diff(timestamp) > VALID_SECS as u64
            || tag != Self::request_tag(token, epoch, timestamp, &nonce, method, target, body)
        {
            return None;
        }
        Some(Self {
            epoch,
            timestamp,
            nonce,
            tag,
        })
    }

    #[cfg(feature = "worker")]
    pub fn nonce(&self) -> Uuid {
        self.nonce
    }

    pub fn response_tag(&self, token: &str, status: u16, body: &[u8]) -> Hash {
        mac(
            token,
            b"response-v1",
            &[self.nonce.as_bytes(), &status.to_le_bytes(), body],
        )
    }

    pub fn verify_response(&self, token: &str, status: u16, body: &[u8], tag: &str) -> bool {
        Hash::from_hex(tag).is_ok_and(|tag| tag == self.response_tag(token, status, body))
    }
}

#[cfg(feature = "worker")]
mod server;
#[cfg(feature = "worker")]
pub(crate) use server::{ServerAuth, authenticate, health};
