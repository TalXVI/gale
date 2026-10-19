use keyring::{credential::CredentialApi, mock::MockCredential};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// An external credential store whose write failures do not affect reads.
#[derive(Clone, Default)]
pub(crate) struct CredentialStore(Arc<Store>);

#[derive(Default)]
struct Store {
    credential: MockCredential,
    blocked: AtomicBool,
}

impl CredentialStore {
    pub fn entry(&self) -> keyring::Entry {
        keyring::Entry::new_with_credential(Box::new(self.clone()))
    }

    pub fn block_writes(&self, blocked: bool) {
        self.0.blocked.store(blocked, Ordering::SeqCst);
    }
}

impl CredentialApi for CredentialStore {
    fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
        if self.0.blocked.load(Ordering::SeqCst) {
            return Err(keyring::Error::NoStorageAccess(Box::new(
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )));
        }
        self.0.credential.set_secret(secret)
    }

    fn get_secret(&self) -> keyring::Result<Vec<u8>> {
        self.0.credential.get_secret()
    }

    fn delete_credential(&self) -> keyring::Result<()> {
        self.0.credential.delete_credential()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Runs a bounded external HTTP fixture and the real client concurrently.
pub(crate) async fn http_fixture<T, F, Fut>(
    paths: &[&str],
    mut reply: impl FnMut(usize, &str) -> String,
    client: F,
) -> (T, Vec<String>)
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (finished, mut done) = tokio::sync::oneshot::channel();
    let run_client = async {
        let result = client(base).await;
        finished.send(()).unwrap();
        result
    };
    let serve = async {
        let mut requests = Vec::new();
        loop {
            let (mut socket, _) = tokio::select! {
                result = listener.accept() => result.unwrap(),
                _ = &mut done => break,
            };
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0, "client closed before completing its headers");
                request.extend_from_slice(&chunk[..count]);
                assert!(request.len() <= 65536, "oversized fixture request");
            }
            let request = String::from_utf8(request).unwrap();
            let index = requests.len();
            let expected = paths.get(index).expect("unexpected extra HTTP request");
            assert_eq!(request.lines().next().unwrap(), *expected);
            let response = reply(index, &request);
            requests.push(request);
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        assert_eq!(
            requests.len(),
            paths.len(),
            "client omitted an expected request"
        );
        requests
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run_client, serve)
    })
    .await
    .expect("HTTP fixture exceeded its deadline")
}

/// Independent encoding of the documented Worker MAC, for adversarial wire inputs.
pub(crate) fn worker_mac(token: &str, domain: &[u8], fields: &[&[u8]]) -> blake3::Hash {
    let key = blake3::derive_key("Gale Worker HTTP authentication v1", token.as_bytes());
    let mut hash = blake3::Hasher::new_keyed(&key);
    hash.update(domain);
    for field in fields {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    hash.finalize()
}
