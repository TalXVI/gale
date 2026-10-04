//! Storing a profile's server settings and credentials, for a profile
//! captured before any asynchronous work.

use serde::Deserialize;
use tauri::AppHandle;

use super::{
    secrets::{ServerSecret, ServerSecrets},
    settings::{ProfileServerSettings, RemoteServerSettings},
};
use crate::state::ManagerExt;

/// Settings + optional credentials, saved together from the settings
/// page. Empty credential fields leave stored credentials untouched;
/// each `remember*` flag controls only its own credential, `false`
/// clears that one and nothing else.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveServerSettingsRequest {
    pub settings: ProfileServerSettings,
    #[serde(default)]
    pub game_password: String,
    #[serde(default)]
    pub remember_game_password: bool,
    #[serde(default)]
    pub remote_password: String,
    #[serde(default)]
    pub remember_remote_password: bool,
    #[serde(default)]
    pub worker_token: String,
    #[serde(default)]
    pub remember_worker_token: bool,
    #[serde(default)]
    pub dat_host_password: String,
    #[serde(default)]
    pub remember_dat_host_password: bool,
}

/// Stores `settings` for the profile along with the request's credentials.
pub(crate) fn persist_settings(
    app: &AppHandle,
    profile_id: i64,
    secrets: &ServerSecrets,
    mut settings: ProfileServerSettings,
    request: &SaveServerSettingsRequest,
) -> eyre::Result<()> {
    persist_credential(
        secrets,
        Some(ServerSecret::GamePassword),
        &request.game_password,
        request.remember_game_password,
    )?;

    let remote = settings.remote.clone();
    update_settings_for(app, profile_id, |stored| {
        // A settings page opened earlier must not overwrite newer Sync-dialog
        // defaults. That dedicated command owns this client-local field.
        settings.sync_dialog = std::mem::take(&mut stored.sync_dialog);
        *stored = settings;
    })?;
    persist_remote_credentials(secrets, &remote, request)
}

/// Persists each remote credential independently: a provided value is
/// stored per its own `remember` flag, an empty one leaves the stored
/// value alone unless `remember` is off, which clears just that
/// credential and nothing else.
fn persist_remote_credentials(
    secrets: &ServerSecrets,
    remote: &RemoteServerSettings,
    request: &SaveServerSettingsRequest,
) -> eyre::Result<()> {
    persist_credential(
        secrets,
        ServerSecret::required_by(&remote.transport),
        &request.remote_password,
        request.remember_remote_password,
    )?;
    persist_credential(
        secrets,
        Some(ServerSecret::WorkerToken),
        &request.worker_token,
        request.remember_worker_token,
    )?;
    persist_credential(
        secrets,
        Some(ServerSecret::DatHostPassword),
        &request.dat_host_password,
        request.remember_dat_host_password,
    )
}

pub(crate) fn persist_credential(
    secrets: &ServerSecrets,
    secret: Option<ServerSecret>,
    value: &str,
    remember: bool,
) -> eyre::Result<()> {
    let Some(secret) = secret else {
        return Ok(());
    };
    if !value.is_empty() || !remember {
        secrets.persist(secret, value, remember)?;
    }
    Ok(())
}

/// Updates the settings of a profile captured before any asynchronous
/// work, so an active-profile switch cannot redirect the write.
pub(crate) fn update_settings_for(
    app: &AppHandle,
    profile_id: i64,
    update: impl FnOnce(&mut ProfileServerSettings),
) -> eyre::Result<()> {
    let mut manager = app.lock_manager();
    let (_, profile) = manager.profile_by_id_mut(profile_id)?;
    update(profile.server_settings.get_or_insert_default());
    profile.save(app, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::server::settings::TransportSettings;

    /// Every credential stored, in the order the assertion tuples use.
    fn fully_stocked_secrets() -> ServerSecrets {
        use super::super::secrets::in_memory_secrets;

        let secrets = in_memory_secrets();
        for secret in [
            ServerSecret::GamePassword,
            ServerSecret::SftpPassword,
            ServerSecret::FtpPassword,
            ServerSecret::SshKeyPassphrase,
            ServerSecret::DatHostPassword,
            ServerSecret::WorkerToken,
        ] {
            secrets.set(secret, "stored").unwrap();
        }
        secrets
    }

    /// Presence of every secret, in `fully_stocked_secrets` order.
    fn stored(secrets: &ServerSecrets) -> [bool; 6] {
        [
            secrets.has(ServerSecret::GamePassword).unwrap(),
            secrets.has(ServerSecret::SftpPassword).unwrap(),
            secrets.has(ServerSecret::FtpPassword).unwrap(),
            secrets.has(ServerSecret::SshKeyPassphrase).unwrap(),
            secrets.has(ServerSecret::DatHostPassword).unwrap(),
            secrets.has(ServerSecret::WorkerToken).unwrap(),
        ]
    }

    fn save_request(remote: RemoteServerSettings) -> SaveServerSettingsRequest {
        SaveServerSettingsRequest {
            settings: ProfileServerSettings {
                remote,
                ..ProfileServerSettings::default()
            },
            game_password: String::new(),
            remember_game_password: true,
            remote_password: String::new(),
            remember_remote_password: true,
            worker_token: String::new(),
            remember_worker_token: true,
            dat_host_password: String::new(),
            remember_dat_host_password: true,
        }
    }

    const ALL: [bool; 6] = [true; 6];
    const SFTP: usize = 1;
    const FTP: usize = 2;
    const KEY: usize = 3;
    const DATHOST: usize = 4;
    const TOKEN: usize = 5;

    /// Only the flag's own credential is dropped. Index `cleared` is the
    /// one slot that flips to false.
    fn assert_only_cleared(cleared: usize, stored: [bool; 6]) {
        let mut expected = ALL;
        expected[cleared] = false;
        assert_eq!(stored, expected, "only slot {cleared} should be cleared");
    }

    #[test]
    fn forgetting_one_remote_credential_leaves_all_others() {
        use super::super::settings::{RemoteAuthentication, RemoteProtocol};

        // SFTP password auth clears only the SFTP password.
        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remember_remote_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(SFTP, stored(&secrets));

        // Private-key auth clears only the key passphrase.
        let secrets = fully_stocked_secrets();
        let remote = RemoteServerSettings {
            transport: TransportSettings {
                authentication: RemoteAuthentication::PrivateKey,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut request = save_request(remote);
        request.remember_remote_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(KEY, stored(&secrets));

        // FTP/FTPS clears only the FTP password.
        for protocol in [RemoteProtocol::Ftp, RemoteProtocol::Ftps] {
            let secrets = fully_stocked_secrets();
            let remote = RemoteServerSettings {
                transport: TransportSettings {
                    protocol,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut request = save_request(remote);
            request.remember_remote_password = false;
            persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
            assert_only_cleared(FTP, stored(&secrets));
        }

        // Agent auth has no transport credential: nothing is cleared.
        let secrets = fully_stocked_secrets();
        let remote = RemoteServerSettings {
            transport: TransportSettings {
                authentication: RemoteAuthentication::Agent,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut request = save_request(remote);
        request.remember_remote_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_eq!(stored(&secrets), ALL);

        // Worker token and DatHost password are independent flags.
        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remember_worker_token = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(TOKEN, stored(&secrets));

        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remember_dat_host_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(DATHOST, stored(&secrets));
    }

    #[test]
    fn remembered_credentials_stay_untouched_or_replace_only_themselves() {
        // All remember flags on with empty values: nothing changes.
        let secrets = fully_stocked_secrets();
        let request = save_request(RemoteServerSettings::default());
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_eq!(stored(&secrets), ALL);

        // A typed value replaces only its own secret.
        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remote_password = "new-remote-password".to_owned();
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_eq!(stored(&secrets), ALL);
        assert_eq!(
            secrets.get(ServerSecret::SftpPassword).unwrap().as_deref(),
            Some("new-remote-password")
        );
        assert_eq!(
            secrets.get(ServerSecret::WorkerToken).unwrap().as_deref(),
            Some("stored")
        );
    }
}
