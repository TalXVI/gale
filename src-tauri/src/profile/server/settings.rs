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
    /// The Gale-managed gale-worker service on this machine (installed
    /// under ProgramData) performs the deployment. Provisioning selects
    /// it; uninstall returns to `Local`.
    HostedWorker,
    /// An independently hosted gale-worker performs the deployment.
    Worker,
}

impl SyncMode {
    /// Whether a worker, rather than this process, executes deployments.
    pub fn uses_worker(self) -> bool {
        matches!(self, Self::HostedWorker | Self::Worker)
    }
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
/// The worker's unattended restart policy remains
/// `remote.automation.restart_policy`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncDialogPreferences {
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

/// Who executes deployments.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct ExecutorSettings {
    pub mode: SyncMode,
    /// Base URL of the worker's HTTP API, e.g. `http://192.168.1.10:8472`.
    /// Kept while `mode` is `Local` so switching back restores it.
    pub worker_address: String,
}

/// What a worker does without being asked.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct AutomationSettings {
    /// Whether owed mod payloads may deploy without a manual request.
    pub auto_deploy_mods: bool,
    /// What may happen to the server process after a deployment that
    /// does not choose a policy itself.
    pub restart_policy: RestartPolicy,
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
/// The local fields are flattened into this struct when serialized.
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

/// How Gale reaches a remote server's files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct TransportSettings {
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
    pub trusted_certificate: Option<String>,
}

/// Settings for deploying a profile to a remote server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "RemoteSettingsRepr", rename_all = "camelCase")]
pub struct RemoteServerSettings {
    pub transport: TransportSettings,
    pub executor: ExecutorSettings,
    /// Hosting provider used for restarts and player-presence checks.
    pub host_control: HostSettings,
    pub automation: AutomationSettings,
}

/// Every shape `RemoteServerSettings` has been stored in. Settings saved
/// before the fields were grouped are flat, and describe the hosted
/// worker as `syncMode: "worker"` plus `worker.hosted`.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RemoteSettingsRepr {
    transport: Option<TransportSettings>,
    executor: Option<ExecutorSettings>,
    host_control: HostSettings,
    automation: Option<AutomationSettings>,
    #[serde(flatten)]
    flat_transport: TransportSettings,
    sync_mode: SyncMode,
    worker: FlatWorkerSettings,
    restart_policy: RestartPolicy,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct FlatWorkerSettings {
    address: String,
    hosted: bool,
    auto_deploy_mods: bool,
}

impl From<RemoteSettingsRepr> for RemoteServerSettings {
    fn from(repr: RemoteSettingsRepr) -> Self {
        let mode = match repr.sync_mode {
            SyncMode::Worker if repr.worker.hosted => SyncMode::HostedWorker,
            mode => mode,
        };
        Self {
            transport: repr.transport.unwrap_or(repr.flat_transport),
            executor: repr.executor.unwrap_or(ExecutorSettings {
                mode,
                worker_address: repr.worker.address,
            }),
            host_control: repr.host_control,
            automation: repr.automation.unwrap_or(AutomationSettings {
                auto_deploy_mods: repr.worker.auto_deploy_mods,
                restart_policy: repr.restart_policy,
            }),
        }
    }
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

impl TransportSettings {
    /// The checks a file-transfer connection needs: reachable host,
    /// credentials to authenticate with, and a valid remote directory.
    pub fn validate(&self) -> Result<()> {
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
}

impl ExecutorSettings {
    /// A worker mode needs the address of the worker to reach.
    pub fn validate(&self) -> Result<()> {
        if self.mode.uses_worker() {
            let address = self.worker_address.trim();
            ensure!(!address.is_empty(), "worker address cannot be empty");
            ensure!(
                address.starts_with("http://") || address.starts_with("https://"),
                "worker address must be an http:// or https:// URL"
            );
        }

        Ok(())
    }
}

impl HostSettings {
    /// Validates the hosting-provider settings that restarts and presence
    /// checks use.
    pub fn validate(&self) -> Result<()> {
        if self.provider == HostProvider::DatHost {
            ensure!(
                !self.dat_host_server_id.trim().is_empty(),
                "DatHost server id is required"
            );
            ensure!(
                !self.dat_host_username.trim().is_empty(),
                "DatHost account email is required"
            );
        }

        Ok(())
    }
}

impl RemoteServerSettings {
    /// Full validation for operations that act on the whole
    /// configuration: saving settings, deploying, or testing the worker.
    /// Connection-only operations validate [`Self::transport`] alone, so
    /// file transfer can be tested before a worker is provisioned.
    pub fn validate(&self) -> Result<()> {
        self.transport.validate()?;
        self.executor.validate()?;
        self.host_control.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutomationSettings, ExecutorSettings, LocalServerSettings, ProfileServerSettings,
        RemoteAuthentication, RemoteProtocol, RemoteServerSettings, RestartPolicy, ServerLocation,
        SyncMode, TransportSettings,
    };

    /// The fresh-profile provisioning sequence relies on this contract:
    /// the dialog saves transport settings in `local` mode before the
    /// worker exists (no address yet), and provisioning itself flips the
    /// profile to hosted worker mode. Worker mode with no address must
    /// keep failing. That's how a misconfigured external worker is
    /// caught, while local mode tolerates the empty address.
    #[test]
    fn worker_mode_requires_an_address_but_local_does_not() {
        let transport = RemoteServerSettings {
            transport: TransportSettings {
                host: "h".into(),
                port: 22,
                username: "u".into(),
                server_directory: "/srv/valheim".into(),
                ..Default::default()
            },
            ..Default::default()
        };

        let unprovisioned = RemoteServerSettings {
            executor: ExecutorSettings {
                mode: SyncMode::HostedWorker,
                worker_address: String::new(),
            },
            ..transport.clone()
        };
        assert!(unprovisioned.validate().is_err());

        let mut saved_first = transport.clone();
        assert!(saved_first.validate().is_ok());
        saved_first.automation.auto_deploy_mods = true;
        assert!(saved_first.validate().is_ok());

        // An already-configured external worker is untouched.
        let external = RemoteServerSettings {
            executor: ExecutorSettings {
                mode: SyncMode::Worker,
                worker_address: "http://127.0.0.1:8472".into(),
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
            transport: TransportSettings {
                host: "ftp.example.com".into(),
                port: 21,
                username: "u".into(),
                server_directory: "/".into(),
                ..Default::default()
            },
            executor: ExecutorSettings {
                mode: SyncMode::HostedWorker,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(unprovisioned_hosted.transport.validate().is_ok());
        assert!(unprovisioned_hosted.validate().is_err());

        let unconfigured_external = RemoteServerSettings {
            executor: ExecutorSettings {
                mode: SyncMode::Worker,
                ..Default::default()
            },
            ..unprovisioned_hosted.clone()
        };
        assert!(unconfigured_external.transport.validate().is_ok());
        assert!(unconfigured_external.validate().is_err());
    }

    /// Worker independence cannot weaken the checks the transport
    /// itself needs; bad credentials or connection settings still fail.
    #[test]
    fn connection_validation_still_checks_the_transport() {
        let base = TransportSettings {
            host: "ftp.example.com".into(),
            port: 21,
            username: "u".into(),
            server_directory: "/".into(),
            ..Default::default()
        };
        let with = |transport| RemoteServerSettings {
            transport,
            executor: ExecutorSettings {
                mode: SyncMode::Worker,
                ..Default::default()
            },
            ..Default::default()
        };

        for broken in [
            TransportSettings {
                host: "  ".into(),
                ..base.clone()
            },
            TransportSettings {
                port: 0,
                ..base.clone()
            },
            TransportSettings {
                username: String::new(),
                ..base.clone()
            },
            TransportSettings {
                server_directory: "/srv/../escape".into(),
                ..base.clone()
            },
            TransportSettings {
                protocol: RemoteProtocol::Sftp,
                authentication: RemoteAuthentication::PrivateKey,
                private_key_path: String::new(),
                ..base.clone()
            },
        ] {
            let broken = with(broken);
            assert!(broken.transport.validate().is_err());
            assert!(broken.validate().is_err());
        }
    }

    #[test]
    fn deserializes_flat_settings() {
        let value = serde_json::json!({
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

        let settings: ProfileServerSettings = serde_json::from_value(value).unwrap();

        assert_eq!(settings.location, ServerLocation::Remote);
        assert_eq!(settings.local.server_name, "My Server");
        assert_eq!(settings.local.port, 2456);
        assert_eq!(settings.remote.transport.host, "example.com");
        assert_eq!(settings.sync_dialog, Default::default());
    }

    /// `"ftps"` is a distinct protocol on the wire: it must not collapse
    /// into `ftp` when settings round-trip, or a strict-TLS selection
    /// would silently gain plaintext fallback.
    #[test]
    fn ftps_round_trips_as_a_distinct_protocol() {
        let settings = RemoteServerSettings {
            transport: TransportSettings {
                protocol: RemoteProtocol::Ftps,
                ..Default::default()
            },
            ..Default::default()
        };

        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["transport"]["protocol"], "ftps");

        let parsed: RemoteServerSettings = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.transport.protocol, RemoteProtocol::Ftps);
    }

    #[test]
    fn tolerates_partial_payloads() {
        let settings: ProfileServerSettings =
            serde_json::from_str(r#"{"location":"remote","remote":{"host":"h"}}"#).unwrap();

        assert_eq!(settings.location, ServerLocation::Remote);
        assert_eq!(settings.local.port, 0);
        assert_eq!(settings.remote.transport.protocol, RemoteProtocol::Sftp);
    }

    /// Settings saved before the fields were grouped stay readable, and
    /// the hosted worker they described as `syncMode: "worker"` plus
    /// `worker.hosted` keeps pointing at the managed service rather than
    /// turning into an external worker.
    #[test]
    fn settings_saved_in_the_flat_shape_keep_their_executor_and_automation() {
        let flat = |hosted| {
            serde_json::json!({
                "protocol": "ftp",
                "host": "example.com",
                "port": 21,
                "username": "u",
                "serverDirectory": "/srv",
                "syncMode": "worker",
                "worker": {
                    "address": "http://127.0.0.1:8472",
                    "hosted": hosted,
                    "autoDeployMods": true
                },
                "hostControl": { "provider": "datHost", "datHostServerId": "s" },
                "restartPolicy": "whenEmpty"
            })
        };

        for (hosted, mode) in [(true, SyncMode::HostedWorker), (false, SyncMode::Worker)] {
            let settings: RemoteServerSettings = serde_json::from_value(flat(hosted)).unwrap();

            assert_eq!(settings.transport.protocol, RemoteProtocol::Ftp);
            assert_eq!(settings.transport.host, "example.com");
            assert_eq!(settings.transport.server_directory, "/srv");
            assert_eq!(
                settings.executor,
                ExecutorSettings {
                    mode,
                    worker_address: "http://127.0.0.1:8472".into(),
                }
            );
            assert_eq!(
                settings.automation,
                AutomationSettings {
                    auto_deploy_mods: true,
                    restart_policy: RestartPolicy::WhenEmpty,
                }
            );
            assert_eq!(settings.host_control.dat_host_server_id, "s");

            // Saved again, the settings take the grouped shape and read
            // back unchanged.
            let saved = serde_json::to_value(&settings).unwrap();
            assert!(saved.get("syncMode").is_none() && saved.get("worker").is_none());
            let reread: RemoteServerSettings = serde_json::from_value(saved).unwrap();
            assert_eq!(reread.transport, settings.transport);
            assert_eq!(reread.executor, settings.executor);
            assert_eq!(reread.automation, settings.automation);
        }
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
