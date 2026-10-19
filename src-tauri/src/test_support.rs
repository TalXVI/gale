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
