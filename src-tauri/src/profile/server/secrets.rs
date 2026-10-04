use eyre::{Context, Result};
use keyring::Entry;

use super::settings::TransportSettings;

const SERVICE: &str = "com.kesomannen.gale.dedicated-server";

#[derive(Debug, Clone, Copy)]
pub enum ServerSecret {
    GamePassword,
    SftpPassword,
    FtpPassword,
    SshKeyPassphrase,
    DatHostPassword,
    WorkerToken,
}

// `ServerSecrets` indexes its entries by discriminant, so `ALL` must list
// every secret in declaration order.
const _: () = {
    let mut index = 0;
    while index < ServerSecret::ALL.len() {
        assert!(ServerSecret::ALL[index] as usize == index);
        index += 1;
    }
};

/// Access to a profile's server credentials in the OS credential store.
/// The keyring entries are initialized once per server operation and reused
/// for every credential it touches.
pub struct ServerSecrets {
    /// One entry per secret, indexed by `ServerSecret`.
    entries: Vec<Entry>,
}

impl ServerSecret {
    const ALL: [Self; 6] = [
        Self::GamePassword,
        Self::SftpPassword,
        Self::FtpPassword,
        Self::SshKeyPassphrase,
        Self::DatHostPassword,
        Self::WorkerToken,
    ];

    /// The credential a remote connection requires, if any. SSH agent
    /// authentication needs no stored credential.
    pub fn required_by(settings: &TransportSettings) -> Option<Self> {
        use super::settings::{RemoteAuthentication, RemoteProtocol};

        match (settings.protocol, settings.authentication) {
            (RemoteProtocol::Sftp, RemoteAuthentication::Password) => Some(Self::SftpPassword),
            (RemoteProtocol::Sftp, RemoteAuthentication::PrivateKey) => {
                Some(Self::SshKeyPassphrase)
            }
            (RemoteProtocol::Sftp, RemoteAuthentication::Agent) => None,
            (_, _) => Some(Self::FtpPassword),
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::GamePassword => "game-password",
            Self::SftpPassword => "sftp-password",
            Self::FtpPassword => "ftp-password",
            Self::SshKeyPassphrase => "ssh-key-passphrase",
            Self::DatHostPassword => "dathost-password",
            Self::WorkerToken => "worker-token",
        }
    }

    fn entry(self, profile_id: i64) -> Result<Entry> {
        Entry::new(SERVICE, &format!("profile-{profile_id}-{}", self.suffix()))
            .context("failed to access OS credential store")
    }
}

impl ServerSecrets {
    pub fn for_profile(profile_id: i64) -> Result<Self> {
        let entries = ServerSecret::ALL
            .iter()
            .map(|secret| secret.entry(profile_id))
            .collect::<Result<_>>()?;
        Ok(Self { entries })
    }

    fn entry(&self, secret: ServerSecret) -> &Entry {
        &self.entries[secret as usize]
    }

    /// Returns `provided` when given, otherwise the stored credential.
    pub fn resolve(&self, secret: ServerSecret, provided: &str) -> Result<String> {
        if !provided.is_empty() {
            return Ok(provided.to_owned());
        }

        self.get(secret).map(|value| value.unwrap_or_default())
    }

    pub fn get(&self, secret: ServerSecret) -> Result<Option<String>> {
        match self.entry(secret).get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(err).context("failed to read credential"),
        }
    }

    /// Whether a credential is stored, without exposing its value.
    pub fn has(&self, secret: ServerSecret) -> Result<bool> {
        self.get(secret).map(|value| value.is_some())
    }

    /// Stores `value` when `remember` is set, clears it otherwise.
    pub fn persist(&self, secret: ServerSecret, value: &str, remember: bool) -> Result<()> {
        if remember && !value.is_empty() {
            self.set(secret, value)
        } else {
            self.remove(secret)
        }
    }

    pub fn set(&self, secret: ServerSecret, value: &str) -> Result<()> {
        self.entry(secret)
            .set_password(value)
            .context("failed to save credential")
    }

    pub fn remove(&self, secret: ServerSecret) -> Result<()> {
        match self.entry(secret).delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err).context("failed to remove credential"),
        }
    }
}

#[cfg(test)]
pub(crate) fn in_memory_secrets() -> ServerSecrets {
    use keyring::mock::MockCredential;

    ServerSecrets {
        entries: ServerSecret::ALL
            .iter()
            .map(|_| Entry::new_with_credential(Box::new(MockCredential::default())))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::server::settings_store::persist_credential;
    use keyring::mock::MockCredential;

    #[test]
    fn saved_game_password_can_be_reused_replaced_and_forgotten() {
        let secrets = in_memory_secrets();
        let password = Some(ServerSecret::GamePassword);
        secrets
            .set(ServerSecret::WorkerToken, "unrelated-token")
            .unwrap();

        persist_credential(&secrets, password, "first-password", true).unwrap();
        assert_eq!(
            secrets.resolve(ServerSecret::GamePassword, "").unwrap(),
            "first-password"
        );
        persist_credential(&secrets, password, "", true).unwrap();
        assert_eq!(
            secrets.resolve(ServerSecret::GamePassword, "").unwrap(),
            "first-password"
        );
        persist_credential(&secrets, password, "replacement", true).unwrap();
        assert_eq!(
            secrets.resolve(ServerSecret::GamePassword, "").unwrap(),
            "replacement"
        );
        persist_credential(&secrets, password, "session-only", false).unwrap();
        assert_eq!(secrets.get(ServerSecret::GamePassword).unwrap(), None);
        assert_eq!(
            secrets
                .resolve(ServerSecret::GamePassword, "session-only")
                .unwrap(),
            "session-only"
        );
        assert_eq!(
            secrets.get(ServerSecret::WorkerToken).unwrap().as_deref(),
            Some("unrelated-token")
        );
    }

    #[test]
    fn credential_storage_failure_is_reported_without_replacing_the_password() {
        let secrets = in_memory_secrets();
        secrets
            .set(ServerSecret::GamePassword, "saved-password")
            .unwrap();
        secrets
            .entry(ServerSecret::GamePassword)
            .get_credential()
            .downcast_ref::<MockCredential>()
            .unwrap()
            .set_error(keyring::Error::NoStorageAccess(Box::new(
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )));

        assert!(
            persist_credential(
                &secrets,
                Some(ServerSecret::GamePassword),
                "new-password",
                true
            )
            .is_err()
        );
        assert_eq!(
            secrets.resolve(ServerSecret::GamePassword, "").unwrap(),
            "saved-password"
        );
    }
}
