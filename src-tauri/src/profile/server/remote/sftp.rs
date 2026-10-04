//! SFTP transport over SSH.

use std::{
    io::{Read, Write},
    path::Path,
};

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use eyre::{Context, OptionExt, Result, ensure};
use ssh2::{Error as SshError, ErrorCode, FileStat, HashType, RenameFlags, Sftp};
use tracing::info;

use super::{
    CONNECT_TIMEOUT, ConnectionTestResult, IO_TIMEOUT, Reached, ReaderConnector, RemoteEntry,
    RemoteOps, Retrieve, connect_socket, connector_for, inaccessible_directory,
};
use crate::profile::server::{
    paths::RemotePath,
    settings::{RemoteAuthentication, TransportSettings},
};

const SFTP_NO_SUCH_FILE: i32 = 2;

pub(super) struct SftpConnection {
    sftp: Sftp,
    _session: ssh2::Session,
    settings: TransportSettings,
    password: String,
    fingerprint: String,
}

impl SftpConnection {
    /// Connects and authenticates, unless the server's host key is not the
    /// trusted one.
    pub(super) fn connect(settings: &TransportSettings, password: &str) -> Result<Reached<Self>> {
        let mut session = connect_tcp(settings)?;
        session.handshake().context("SSH handshake failed")?;

        let fingerprint = host_key_fingerprint(&session)?;

        match settings.trusted_host_key.as_deref() {
            Some(expected) => ensure!(
                expected == fingerprint,
                "SSH host key has changed. Expected {expected}, received {fingerprint}. Refusing to send credentials."
            ),
            None => return Ok(Reached::Untrusted { fingerprint }),
        }

        authenticate(&session, settings, password)?;

        let sftp = session
            .sftp()
            .context("connected over SSH, but the server did not provide an SFTP subsystem")?;

        Ok(Reached::Trusted(Self {
            sftp,
            _session: session,
            settings: settings.clone(),
            password: password.to_owned(),
            fingerprint,
        }))
    }

    /// Connects and checks that the server directory can be accessed.
    pub(super) fn test(
        settings: &TransportSettings,
        password: &str,
    ) -> Result<ConnectionTestResult> {
        let connection = match Self::connect(settings, password)? {
            Reached::Trusted(connection) => connection,
            Reached::Untrusted { fingerprint } => {
                return Ok(ConnectionTestResult::HostKeyUntrusted { fingerprint });
            }
        };
        let directory = settings.server_directory()?;
        connection
            .sftp
            .stat(Path::new(directory.as_str()))
            .with_context(|| inaccessible_directory(settings))?;

        Ok(ConnectionTestResult::Connected {
            fingerprint: Some(connection.fingerprint),
            encrypted: true,
        })
    }

    /// Metadata of `path`, or `None` when it does not exist.
    fn stat(&self, path: &RemotePath) -> Result<Option<FileStat>> {
        match self.sftp.stat(Path::new(path.as_str())) {
            Ok(stat) => Ok(Some(stat)),
            Err(err) if is_sftp_not_found(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

impl Retrieve for SftpConnection {
    fn retrieve(
        &mut self,
        path: &RemotePath,
        limit: u64,
        sink: &mut dyn Write,
    ) -> Result<Option<u64>> {
        match self.sftp.open(Path::new(path.as_str())) {
            Ok(file) => Ok(Some(
                std::io::copy(&mut file.take(limit.saturating_add(1)), sink)
                    .with_context(|| format!("SFTP read failed for {path}"))?,
            )),
            Err(err) if is_sftp_not_found(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

impl RemoteOps for SftpConnection {
    fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
        Ok(self.stat(path)?.is_some_and(|stat| stat.is_dir()))
    }

    fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
        Ok(self.stat(path)?.is_some_and(|stat| !stat.is_dir()))
    }

    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
        Ok(self
            .stat(path)?
            .filter(|stat| !stat.is_dir())
            .and_then(|stat| stat.size))
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        match self.sftp.readdir(Path::new(dir.as_str())) {
            Ok(entries) => Ok(entries
                .into_iter()
                .filter_map(|(path, stat)| {
                    Some(RemoteEntry {
                        name: path.file_name()?.to_str()?.to_owned(),
                        is_directory: stat.is_dir(),
                        size: stat.size,
                    })
                })
                .collect()),
            Err(err) if is_sftp_not_found(&err) => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }

    fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
        self.read_capped(path, max)
    }

    fn read_listed(
        &mut self,
        path: &RemotePath,
        listed_size: u64,
        sink: &mut dyn Write,
    ) -> Result<bool> {
        self.read_within_listing(path, listed_size, sink)
    }

    fn reader_connector(&self) -> Option<ReaderConnector> {
        Some(connector_for(&self.settings, &self.password, Self::connect))
    }

    fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
        let mut file = self.sftp.create(Path::new(path.as_str()))?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    }

    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
        let mut file = std::fs::File::open(local)
            .with_context(|| format!("failed to read staged file {}", local.display()))?;
        let mut remote_file = self.sftp.create(Path::new(remote.as_str()))?;
        std::io::copy(&mut file, &mut remote_file)?;
        remote_file.flush()?;
        Ok(())
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.sftp.rename(
            Path::new(from.as_str()),
            Path::new(to.as_str()),
            Some(RenameFlags::ATOMIC | RenameFlags::OVERWRITE | RenameFlags::NATIVE),
        )?;
        Ok(())
    }

    fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
        match self.sftp.unlink(Path::new(path.as_str())) {
            Ok(()) => Ok(true),
            Err(err) if is_sftp_not_found(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        match self.sftp.rmdir(Path::new(path.as_str())) {
            Ok(()) => Ok(()),
            Err(err) if is_sftp_not_found(&err) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
        if self.stat(path)?.is_none() {
            self.sftp.mkdir(Path::new(path.as_str()), 0o755)?;
        }
        Ok(())
    }

    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
        match self.sftp.mkdir(Path::new(path.as_str()), 0o755) {
            Ok(()) => Ok(true),
            Err(error) => match self.sftp.stat(Path::new(path.as_str())) {
                Ok(stat) if stat.is_dir() => Ok(false),
                _ => Err(error.into()),
            },
        }
    }

    fn reconnect(&mut self) -> Result<()> {
        info!(phase = "reconnect", "replacing remote connection");
        *self = Self::connect(&self.settings, &self.password)?.trusted("reconnecting")?;
        Ok(())
    }
}

fn connect_tcp(settings: &TransportSettings) -> Result<ssh2::Session> {
    let stream = connect_socket(settings, CONNECT_TIMEOUT)?;

    let mut session = ssh2::Session::new().context("failed to create SSH session")?;
    session.set_tcp_stream(stream);
    session.set_timeout(IO_TIMEOUT.as_millis() as u32);

    Ok(session)
}

fn host_key_fingerprint(session: &ssh2::Session) -> Result<String> {
    let hash = session
        .host_key_hash(HashType::Sha256)
        .ok_or_eyre("server did not provide a SHA256 host-key fingerprint")?;

    Ok(format!("SHA256:{}", STANDARD_NO_PAD.encode(hash)))
}

fn authenticate(
    session: &ssh2::Session,
    settings: &TransportSettings,
    credential: &str,
) -> Result<()> {
    match settings.authentication {
        RemoteAuthentication::Password => {
            ensure!(!credential.is_empty(), "SFTP password is required");
            session
                .userauth_password(&settings.username, credential)
                .context("SSH password authentication failed")?;
        }
        RemoteAuthentication::PrivateKey => {
            let private_key = Path::new(&settings.private_key_path);
            ensure!(private_key.is_file(), "SSH private key file does not exist");
            session
                .userauth_pubkey_file(
                    &settings.username,
                    None,
                    private_key,
                    (!credential.is_empty()).then_some(credential),
                )
                .context("SSH private-key authentication failed")?;
        }
        RemoteAuthentication::Agent => session
            .userauth_agent(&settings.username)
            .context("SSH agent authentication failed")?,
    }

    ensure!(session.authenticated(), "SSH authentication was rejected");

    Ok(())
}

fn is_sftp_not_found(error: &SshError) -> bool {
    matches!(error.code(), ErrorCode::SFTP(SFTP_NO_SUCH_FILE))
}
