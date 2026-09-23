use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::paths::RemotePathBuf;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ServerLocation {
    #[default]
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RemoteAuthentication {
    #[default]
    Password,
    PrivateKey,
    Agent,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RemoteProtocol {
    #[default]
    Sftp,
    Ftp,
    Ftps,
}

/// How remote synchronization is executed.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SyncMode {
    /// Gale on this device connects to the server and deploys directly.
    #[default]
    Local,
    /// An independently running gale-worker performs the deployment.
    Worker,
}

/// What may happen to the game server process after files are deployed.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RestartPolicy {
    /// Deploy files only; a restart is reported as required but never done.
    #[default]
    Manual,
    /// Restart immediately after a successful deployment.
    Immediate,
    /// Restart only when the server reports zero players. Requires a host
    /// provider that can report player presence; otherwise blocked.
    WhenEmpty,
}

/// Client-local defaults for manual deployments from the Sync dialog.
/// The worker's unattended restart policy remains `remote.restart_policy`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeployScope {
    Mods,
    Configs,
    #[default]
    Both,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncDialogPreferences {
    pub scope: DeployScope,
    pub restart_policy: RestartPolicy,
}

/// Hosting provider integration used for restart and presence checks.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HostProvider {
    /// No provider integration: restarts stay manual.
    #[default]
    None,
    /// DatHost game-server API (https://dathost.net/api/0.1).
    DatHost,
}

/// Connection details for the worker that executes deployments remotely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkerSettings {
    /// Base URL of the worker's HTTP API, e.g. `http://192.168.1.10:8472`.
    pub address: String,
    /// Whether `address` points at the Gale-managed Windows service on
    /// this machine (`gale-worker` installed under ProgramData) rather
    /// than an independently hosted worker. Provisioning sets it;
    /// uninstall clears it.
    pub hosted: bool,
    /// Whether the worker may synchronize new publications on its own.
    /// Manual Deploy Now requests work regardless of this toggle.
    pub auto_sync: bool,
    /// When `auto_sync` is on, whether published mod revisions may deploy
    /// automatically or always wait for a manual request. Config files keep
    /// following their per-file policies either way.
    pub auto_mods: bool,
}

/// Hosting-provider configuration used for restart/presence operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HostSettings {
    pub provider: HostProvider,
    /// DatHost game-server id.
    pub dat_host_server_id: String,
    /// DatHost API account name (email). The password is kept in the keyring.
    pub dat_host_username: String,
}

/// Server settings stored by a profile.
///
/// `local` and `remote` are kept separate because they describe unrelated
/// operations: local settings drive launching a server on this machine, while
/// remote settings drive deploying the profile over FTP/SFTP. Both are
/// remembered when switching `location` back and forth.
///
/// The local fields are flattened into this struct when serialized, which
/// keeps the representation identical to the original flat settings format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileServerSettings {
    #[serde(default)]
    pub location: ServerLocation,

    #[serde(flatten)]
    pub local: LocalServerSettings,

    #[serde(default)]
    pub remote: RemoteServerSettings,

    /// Saved only in this client's profile database; never in publications.
    #[serde(default)]
    pub sync_dialog: SyncDialogPreferences,
}

/// Settings used when launching a dedicated server on this machine.
///
/// The game's launch-arg builder decides which fields it uses; not every
/// field is meaningful for every game. Defaults are intentionally neutral.
/// The frontend builds initial values with real game context, like the
/// default port and game name, instead of relying on a `Default` impl.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalServerSettings {
    /// Advertised server name. Valheim: `-name`.
    pub server_name: String,
    /// World save to load. Valheim-specific.
    pub world: String,
    /// UDP port the server listens on.
    pub port: u16,
    /// Whether the server is publicly listed. Valheim: `-public`.
    pub public_server: bool,
    /// Valheim-specific: enables crossplay backend.
    pub crossplay: bool,
    /// Extra raw arguments appended to the launch command.
    pub extra_args: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteServerSettings {
    pub protocol: RemoteProtocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Directory on the remote server the profile is deployed into. Kept as a
    /// string since it is user-facing input; parse into a [`RemotePathBuf`]
    /// with [`Self::server_directory`] before use.
    pub server_directory: String,
    pub authentication: RemoteAuthentication,
    /// Local filesystem path to the SSH private key.
    pub private_key_path: String,
    /// SHA-256 fingerprint of an SFTP host key the user has trusted.
    pub trusted_host_key: Option<String>,
    /// SHA-256 fingerprint of an FTPS certificate the user has pinned. Unlike
    /// hostname-wide trust this only accepts the exact certificate seen at
    /// trust time, so subsequent arbitrary certificates are rejected.
    #[serde(default)]
    pub trusted_certificate: Option<String>,
    /// Which executor performs remote synchronization.
    #[serde(default)]
    pub sync_mode: SyncMode,
    /// Worker connection and automation settings (`syncMode == "worker"`).
    #[serde(default)]
    pub worker: WorkerSettings,
    /// Hosting provider used for restarts and player-presence checks.
    #[serde(default)]
    pub host_control: HostSettings,
    /// What may happen to the server process after deployment.
    #[serde(default)]
    pub restart_policy: RestartPolicy,
}

impl ProfileServerSettings {
    /// The kind of validation that matters for the next operation, based on
    /// `location`.
    pub fn validate(&self) -> Result<()> {
        match self.location {
            ServerLocation::Local => self.local.validate(),
            ServerLocation::Remote => self.remote.validate(),
        }
    }
}

impl LocalServerSettings {
    /// Game-independent checks. Game-specific rules (e.g. Valheim's password
    /// policy) are enforced by the launch-arg builder.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.server_name.trim().is_empty(),
            "server name cannot be empty"
        );
        ensure!(self.port != 0, "server port cannot be 0");

        if !self.extra_args.is_empty() {
            ensure!(
                !self.extra_args.trim().is_empty(),
                "additional launch arguments cannot be only whitespace"
            );
        }

        Ok(())
    }
}

impl RemoteServerSettings {
    /// The checks a file-transfer connection needs: reachable host,
    /// credentials to authenticate with, and a valid remote directory.
    ///
    /// Worker and host-provider fields are deliberately excluded. They
    /// configure who executes deployments and what happens afterwards,
    /// so requiring them here would block testing the FTP/SFTP settings
    /// before a worker is provisioned or while an external worker is
    /// still being configured.
    pub fn validate_connection(&self) -> Result<()> {
        ensure!(!self.host.trim().is_empty(), "remote host cannot be empty");
        ensure!(self.port != 0, "remote port cannot be 0");
        ensure!(
            !self.username.trim().is_empty(),
            "remote username cannot be empty"
        );

        self.server_directory()?;

        if self.protocol == RemoteProtocol::Sftp
            && self.authentication == RemoteAuthentication::PrivateKey
        {
            ensure!(
                !self.private_key_path.trim().is_empty(),
                "SSH private key file is required"
            );
        }

        Ok(())
    }

    /// Full validation for operations that act on the whole
    /// configuration: saving settings, deploying, or testing the worker.
    /// Worker mode requires a bound worker address here; connection-only
    /// operations use [`Self::validate_connection`] instead.
    pub fn validate(&self) -> Result<()> {
        self.validate_connection()?;

        if self.sync_mode == SyncMode::Worker {
            let address = self.worker.address.trim();
            ensure!(!address.is_empty(), "worker address cannot be empty");
            ensure!(
                address.starts_with("http://") || address.starts_with("https://"),
                "worker address must be an http:// or https:// URL"
            );
        }

        if self.host_control.provider == HostProvider::DatHost {
            ensure!(
                !self.host_control.dat_host_server_id.trim().is_empty(),
                "DatHost server id is required"
            );
            ensure!(
                !self.host_control.dat_host_username.trim().is_empty(),
                "DatHost account email is required"
            );
        }

        Ok(())
    }

    /// Parses `server_directory` into a validated remote path.
    pub fn server_directory(&self) -> Result<RemotePathBuf> {
        RemotePathBuf::new(self.server_directory.trim())
            .map_err(|_| eyre::eyre!("remote server directory is not a valid remote path"))
    }

    /// A normalized "protocol://host:port/directory" identity string.
    /// The plan hash binds an approval to it, so a settings change that
    /// retargets the server invalidates pending approvals.
    pub fn describe_target(&self) -> String {
        let protocol = match self.protocol {
            RemoteProtocol::Sftp => "sftp",
            RemoteProtocol::Ftp => "ftp",
            RemoteProtocol::Ftps => "ftps",
        };
        format!(
            "{protocol}://{}:{}{}",
            self.host.trim().to_lowercase(),
            self.port,
            self.server_directory.trim()
        )
    }

    /// A copy with the file-transfer fields taken from `tested`, keeping
    /// this settings' executor and host-control configuration.
    ///
    /// A successful connection test persists the proven transport this
    /// way: it must not activate the executor the dialog happened to
    /// hold, like an unprovisioned hosted worker or an incompletely
    /// configured external one, nor change automation or restart policy.
    pub fn with_tested_transport(&self, tested: &Self) -> Self {
        let mut merged = self.clone();
        merged.protocol = tested.protocol;
        merged.host = tested.host.clone();
        merged.port = tested.port;
        merged.username = tested.username.clone();
        merged.server_directory = tested.server_directory.clone();
        merged.authentication = tested.authentication;
        merged.private_key_path = tested.private_key_path.clone();
        merged.trusted_host_key = tested.trusted_host_key.clone();
        merged.trusted_certificate = tested.trusted_certificate.clone();
        merged
    }

    pub fn trust_host_key(&mut self, fingerprint: String) {
        self.trusted_host_key = Some(fingerprint);
    }

    pub fn trust_certificate(&mut self, fingerprint: String) {
        self.trusted_certificate = Some(fingerprint);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LocalServerSettings, ProfileServerSettings, RemoteAuthentication, RemoteProtocol,
        RemoteServerSettings, RestartPolicy, ServerLocation, SyncMode, WorkerSettings,
    };

    /// The fresh-profile provisioning sequence relies on this contract:
    /// the dialog saves transport settings in `local` mode before the
    /// worker exists (no address yet), and provisioning itself flips the
    /// profile to hosted worker mode. Worker mode with no address must
    /// keep failing — that's how a misconfigured external worker is
    /// caught — while local mode tolerates the empty address.
    #[test]
    fn worker_mode_requires_an_address_but_local_does_not() {
        let transport = RemoteServerSettings {
            host: "h".into(),
            port: 22,
            username: "u".into(),
            server_directory: "/srv/valheim".into(),
            ..Default::default()
        };

        let unprovisioned = RemoteServerSettings {
            sync_mode: SyncMode::Worker,
            worker: WorkerSettings {
                hosted: true,
                address: String::new(),
                ..Default::default()
            },
            ..transport.clone()
        };
        assert!(unprovisioned.validate().is_err());

        let mut saved_first = transport.clone();
        assert!(saved_first.validate().is_ok());
        saved_first.worker.auto_sync = true;
        saved_first.worker.auto_mods = true;
        assert!(saved_first.validate().is_ok());

        // An already-configured external worker is untouched.
        let external = RemoteServerSettings {
            sync_mode: SyncMode::Worker,
            worker: WorkerSettings {
                address: "http://127.0.0.1:8472".into(),
                ..Default::default()
            },
            ..transport
        };
        assert!(external.validate().is_ok());
    }

    /// Connection testing uses the narrower check: the file transfer
    /// must be testable while the dialog's sync mode names a worker
    /// that has no address yet. Deployment-level `validate` stays
    /// strict for the same settings.
    #[test]
    fn connection_validation_ignores_worker_configuration() {
        let unprovisioned_hosted = RemoteServerSettings {
            host: "ftp.example.com".into(),
            port: 21,
            username: "u".into(),
            server_directory: "/".into(),
            sync_mode: SyncMode::Worker,
            worker: WorkerSettings {
                hosted: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(unprovisioned_hosted.validate_connection().is_ok());
        assert!(unprovisioned_hosted.validate().is_err());

        let unconfigured_external = RemoteServerSettings {
            worker: WorkerSettings::default(),
            ..unprovisioned_hosted.clone()
        };
        assert!(unconfigured_external.validate_connection().is_ok());
        assert!(unconfigured_external.validate().is_err());
    }

    /// Worker independence cannot weaken the checks the transport
    /// itself needs; bad credentials or connection settings still fail.
    #[test]
    fn connection_validation_still_checks_the_transport() {
        let base = RemoteServerSettings {
            host: "ftp.example.com".into(),
            port: 21,
            username: "u".into(),
            server_directory: "/".into(),
            sync_mode: SyncMode::Worker,
            ..Default::default()
        };

        for broken in [
            RemoteServerSettings {
                host: "  ".into(),
                ..base.clone()
            },
            RemoteServerSettings {
                port: 0,
                ..base.clone()
            },
            RemoteServerSettings {
                username: String::new(),
                ..base.clone()
            },
            RemoteServerSettings {
                server_directory: "/srv/../escape".into(),
                ..base.clone()
            },
            RemoteServerSettings {
                protocol: RemoteProtocol::Sftp,
                authentication: RemoteAuthentication::PrivateKey,
                private_key_path: String::new(),
                ..base.clone()
            },
        ] {
            assert!(broken.validate_connection().is_err());
            assert!(broken.validate().is_err());
        }
    }

    /// A successful connection test persists the proven transport onto
    /// the stored settings; whatever executor the dialog held must not
    /// leak in. With nothing stored, the merge cannot activate an
    /// unprovisioned worker, and a stored worker keeps its binding.
    #[test]
    fn tested_transport_does_not_activate_unsaved_executor() {
        let tested = RemoteServerSettings {
            protocol: RemoteProtocol::Ftps,
            host: "ftp.example.com".into(),
            port: 21,
            username: "u".into(),
            server_directory: "/".into(),
            trusted_certificate: Some("cert".into()),
            sync_mode: SyncMode::Worker,
            worker: WorkerSettings {
                hosted: true,
                auto_sync: true,
                auto_mods: true,
                ..Default::default()
            },
            restart_policy: RestartPolicy::Immediate,
            ..Default::default()
        };

        let merged = RemoteServerSettings::default().with_tested_transport(&tested);
        assert_eq!(merged.protocol, RemoteProtocol::Ftps);
        assert_eq!(merged.host, "ftp.example.com");
        assert_eq!(merged.trusted_certificate.as_deref(), Some("cert"));
        assert_eq!(merged.sync_mode, SyncMode::Local);
        assert!(!merged.worker.hosted);
        assert!(!merged.worker.auto_sync);
        assert_eq!(merged.restart_policy, RestartPolicy::Manual);
        assert!(merged.validate().is_ok());

        let stored = RemoteServerSettings {
            sync_mode: SyncMode::Worker,
            worker: WorkerSettings {
                address: "http://127.0.0.1:8472".into(),
                hosted: true,
                auto_sync: true,
                ..Default::default()
            },
            restart_policy: RestartPolicy::WhenEmpty,
            ..Default::default()
        };
        let merged = stored.with_tested_transport(&tested);
        assert_eq!(merged.host, "ftp.example.com");
        assert_eq!(merged.sync_mode, SyncMode::Worker);
        assert_eq!(merged.worker.address, "http://127.0.0.1:8472");
        assert!(merged.worker.auto_sync);
        assert!(!merged.worker.auto_mods);
        assert_eq!(merged.restart_policy, RestartPolicy::WhenEmpty);
        assert!(merged.validate().is_ok());
    }

    #[test]
    fn deserializes_legacy_flat_shape() {
        // Settings written by the original implementation keep working: the
        // flattened `local` fields land in the right place and `location` is
        // respected.
        let legacy = serde_json::json!({
            "location": "remote",
            "serverName": "My Server",
            "world": "Dedicated",
            "port": 2456,
            "publicServer": true,
            "crossplay": false,
            "extraArgs": "-savedir /data",
            "remote": {
                "protocol": "sftp",
                "host": "example.com",
                "port": 22,
                "username": "u",
                "serverDirectory": "/srv/valheim"
            }
        });

        let settings: ProfileServerSettings = serde_json::from_value(legacy).unwrap();

        assert_eq!(settings.location, ServerLocation::Remote);
        assert_eq!(settings.local.server_name, "My Server");
        assert_eq!(settings.local.port, 2456);
        assert_eq!(settings.remote.host, "example.com");
        assert_eq!(settings.sync_dialog, Default::default());
    }

    #[test]
    fn manual_dialog_defaults_round_trip_per_local_profile() {
        let mut first = ProfileServerSettings::default();
        first.sync_dialog.scope = super::DeployScope::Configs;
        first.sync_dialog.restart_policy = RestartPolicy::WhenEmpty;
        first.remote.restart_policy = RestartPolicy::Immediate;
        let second = ProfileServerSettings::default();

        let first: ProfileServerSettings =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        let second: ProfileServerSettings =
            serde_json::from_str(&serde_json::to_string(&second).unwrap()).unwrap();
        assert_eq!(first.sync_dialog.scope, super::DeployScope::Configs);
        assert_eq!(first.sync_dialog.restart_policy, RestartPolicy::WhenEmpty);
        assert_eq!(first.remote.restart_policy, RestartPolicy::Immediate);
        assert_eq!(second.sync_dialog, Default::default());
    }

    /// `"ftps"` is a distinct protocol on the wire: it must not collapse
    /// into `ftp` when settings round-trip, or a strict-TLS selection
    /// would silently gain plaintext fallback.
    #[test]
    fn ftps_round_trips_as_a_distinct_protocol() {
        let settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftps,
            ..Default::default()
        };

        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["protocol"], "ftps");

        let parsed: RemoteServerSettings = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.protocol, RemoteProtocol::Ftps);
    }

    #[test]
    fn tolerates_partial_payloads() {
        let settings: ProfileServerSettings =
            serde_json::from_str(r#"{"location":"remote","remote":{"host":"h"}}"#).unwrap();

        assert_eq!(settings.location, ServerLocation::Remote);
        assert_eq!(settings.local.port, 0);
        assert_eq!(settings.remote.protocol, RemoteProtocol::Sftp);
    }

    #[test]
    fn validates_remote_directory() {
        let settings = RemoteServerSettings {
            host: "h".into(),
            port: 22,
            username: "u".into(),
            server_directory: "/srv/../escape".into(),
            ..Default::default()
        };

        assert!(settings.validate().is_err());

        let valid = RemoteServerSettings {
            server_directory: "/srv/valheim".into(),
            ..settings
        };

        assert!(valid.validate().is_ok());
        assert_eq!(valid.server_directory().unwrap().as_str(), "/srv/valheim");
    }

    #[test]
    fn serializes_to_flat_shape() {
        let settings = ProfileServerSettings {
            location: ServerLocation::Local,
            local: LocalServerSettings {
                server_name: "Name".into(),
                port: 2456,
                ..Default::default()
            },
            remote: RemoteServerSettings::default(),
            sync_dialog: super::SyncDialogPreferences::default(),
        };

        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["serverName"], "Name");
        assert_eq!(value["port"], 2456);
        assert!(value.get("local").is_none());
    }
}
