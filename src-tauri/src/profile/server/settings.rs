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
        LocalServerSettings, ProfileServerSettings, RemoteProtocol, RemoteServerSettings,
        ServerLocation,
    };

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
        };

        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["serverName"], "Name");
        assert_eq!(value["port"], 2456);
        assert!(value.get("local").is_none());
    }
}
