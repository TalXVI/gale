use std::{
    fs::File,
    io::{Cursor, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use eyre::{Context, OptionExt, Result, bail, ensure};
use serde::Serialize;
use ssh2::{Error as SshError, ErrorCode, HashType, RenameFlags, Sftp};
use suppaftp::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use suppaftp::{FtpError, RustlsConnector, RustlsFtpStream, Status};

use super::{
    paths::RemotePath,
    settings::{RemoteAuthentication, RemoteProtocol, RemoteServerSettings},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SSH_TIMEOUT: Duration = Duration::from_millis(15_000);
const SFTP_NO_SUCH_FILE: i32 = 2;
/// FTP servers do not always implement `SIZE` for directories.
const FTP_SIZE_UNAVAILABLE: &[Status] = &[Status::FileUnavailable, Status::BadCommand];

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectionTestResult {
    Connected {
        fingerprint: Option<String>,
        encrypted: bool,
    },
    HostKeyUntrusted {
        fingerprint: String,
    },
    CertificateUntrusted {
        fingerprint: String,
    },
}

pub enum ConnectionAttempt {
    Connected(RemoteConnection),
    HostKeyUntrusted { fingerprint: String },
    CertificateUntrusted { fingerprint: String },
}

/// The transport-level operations the deployment engine needs, abstracted so
/// the engine can run against a real FTP/SFTP connection or an in-memory
/// remote in tests.
///
/// All paths are absolute remote paths; deploy-path mapping happens in the
/// engine. Every method takes `&mut self` because real transports are
/// single-stream stateful protocols.
///
/// `Send` is required so sessions and heartbeat connections can move
/// across `spawn_blocking`/thread boundaries.
pub trait RemoteOps: Send {
    /// Whether `path` exists and is a directory.
    fn is_dir(&mut self, path: &RemotePath) -> Result<bool>;
    /// Whether `path` exists and is not a directory.
    fn is_file(&mut self, path: &RemotePath) -> Result<bool>;
    /// Size of the remote file, or `None` when absent or not a file.
    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>>;
    /// Entries of a directory. Missing directories yield an empty list.
    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>>;
    /// File contents, bounded to `max` bytes. Implementations must refuse to
    /// download files larger than `max` instead of truncating them.
    fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>>;
    /// Writes bytes to `path`, creating or truncating it.
    fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()>;
    /// Uploads a local file to `path`, streaming its contents.
    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()>;
    /// Renames `from` to `to`, replacing `to` when it exists where the
    /// protocol supports it.
    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()>;
    /// Deletes a file. Returns whether anything was removed.
    fn delete_file(&mut self, path: &RemotePath) -> Result<bool>;
    /// Removes an empty directory.
    fn delete_dir(&mut self, path: &RemotePath) -> Result<()>;
    /// Creates `path` and any missing parents it can create.
    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()>;
    /// Atomically creates `path` as a directory: `true` when created, `false`
    /// when it already existed. Used as the deployment-lease primitive because
    /// `MKD`/mkdir is atomic on both FTP and SFTP servers.
    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool>;
    /// Re-establishes the underlying transport after an error.
    fn reconnect(&mut self) -> Result<()>;
}

pub struct RemoteConnection {
    client: RemoteClient,
    settings: RemoteServerSettings,
    password: String,
    pub fingerprint: Option<String>,
    pub encrypted: bool,
}

pub struct RemoteEntry {
    pub name: String,
    pub is_directory: bool,
    /// File size where the listing reports it (always present for SFTP).
    pub size: Option<u64>,
}

enum RemoteClient {
    Sftp { sftp: Sftp, _session: ssh2::Session },
    Ftp(RustlsFtpStream),
}

/// FTPS certificate verification: normal CA validation first, then an exact
/// certificate fingerprint pin as the only fallback trust. When validation
/// fails the observed fingerprint is recorded so the caller can surface it
/// for a trust decision — subsequent arbitrary certificates for the same
/// host stay rejected.
#[derive(Debug)]
struct FtpsCertVerifier {
    webpki: Arc<suppaftp::rustls::client::WebPkiServerVerifier>,
    pinned: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
}

impl FtpsCertVerifier {
    fn new(pinned: Option<String>) -> Result<(Self, Arc<Mutex<Option<String>>>)> {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let webpki = suppaftp::rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .context("failed to build certificate verifier")?;
        let observed = Arc::new(Mutex::new(None));

        Ok((
            Self {
                webpki,
                pinned,
                observed: observed.clone(),
            },
            observed,
        ))
    }
}

impl ServerCertVerifier for FtpsCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => return Ok(verified),
            Err(error) => {
                let fingerprint = certificate_fingerprint(end_entity);
                *self.observed.lock().unwrap() = Some(fingerprint.clone());

                if self.pinned.as_deref() == Some(fingerprint.as_str()) {
                    return Ok(ServerCertVerified::assertion());
                }

                Err(error)
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        if self.pinned.is_some() {
            return Ok(HandshakeSignatureValid::assertion());
        }

        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        if self.pinned.is_some() {
            return Ok(HandshakeSignatureValid::assertion());
        }

        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

/// Fingerprint of an end-entity certificate for pin comparison. blake3 is
/// already a dependency and yields a stable 256-bit digest.
fn certificate_fingerprint(cert: &CertificateDer<'_>) -> String {
    blake3::hash(cert.as_ref()).to_hex().to_string()
}

pub fn test_connection(
    settings: &RemoteServerSettings,
    password: &str,
) -> Result<ConnectionTestResult> {
    match RemoteConnection::connect(settings, password)? {
        ConnectionAttempt::Connected(mut connection) => {
            let directory = settings.server_directory()?;
            connection.check_directory(&directory).with_context(|| {
                format!(
                    "connected successfully, but server directory '{}' could not be accessed",
                    settings.server_directory
                )
            })?;

            Ok(ConnectionTestResult::Connected {
                fingerprint: connection.fingerprint,
                encrypted: connection.encrypted,
            })
        }
        ConnectionAttempt::HostKeyUntrusted { fingerprint } => {
            Ok(ConnectionTestResult::HostKeyUntrusted { fingerprint })
        }
        ConnectionAttempt::CertificateUntrusted { fingerprint } => {
            Ok(ConnectionTestResult::CertificateUntrusted { fingerprint })
        }
    }
}

impl RemoteConnection {
    /// Connects and authenticates against the remote server.
    ///
    /// `settings` is expected to have passed [`RemoteServerSettings::validate`]
    /// already; this is the transport layer, so it only parses the paths it
    /// actually needs.
    pub fn connect(settings: &RemoteServerSettings, password: &str) -> Result<ConnectionAttempt> {
        if settings.protocol != RemoteProtocol::Sftp {
            return match Self::connect_ftp(settings, password) {
                Ok((client, encrypted)) => Ok(ConnectionAttempt::Connected(Self::new(
                    client, settings, password, None, encrypted,
                ))),
                Err(FtpConnectError::Untrusted { fingerprint }) => {
                    Ok(ConnectionAttempt::CertificateUntrusted { fingerprint })
                }
                Err(FtpConnectError::Other(error)) => Err(error),
            };
        }

        let mut session = connect_tcp(settings)?;
        session.handshake().context("SSH handshake failed")?;

        let fingerprint = host_key_fingerprint(&session)?;

        match settings.trusted_host_key.as_deref() {
            Some(expected) => ensure!(
                expected == fingerprint,
                "SSH host key has changed. Expected {expected}, received {fingerprint}. Refusing to send credentials."
            ),
            None => return Ok(ConnectionAttempt::HostKeyUntrusted { fingerprint }),
        }

        authenticate(&session, settings, password)?;

        let sftp = session
            .sftp()
            .context("connected over SSH, but the server did not provide an SFTP subsystem")?;

        Ok(ConnectionAttempt::Connected(Self::new(
            RemoteClient::Sftp {
                sftp,
                _session: session,
            },
            settings,
            password,
            Some(fingerprint),
            true,
        )))
    }

    fn new(
        client: RemoteClient,
        settings: &RemoteServerSettings,
        password: &str,
        fingerprint: Option<String>,
        encrypted: bool,
    ) -> Self {
        Self {
            client,
            settings: settings.clone(),
            password: password.to_owned(),
            fingerprint,
            encrypted,
        }
    }

    fn connect_ftp(
        settings: &RemoteServerSettings,
        password: &str,
    ) -> std::result::Result<(RemoteClient, bool), FtpConnectError> {
        if password.is_empty() {
            return Err(FtpConnectError::Other(eyre::eyre!(
                "FTP password is required"
            )));
        }

        let address = format!("{}:{}", settings.host.trim(), settings.port);
        let stream = RustlsFtpStream::connect(address.as_str())
            .with_context(|| format!("failed to connect to {}", settings.host))
            .map_err(FtpConnectError::Other)?;

        let (verifier, observed) = FtpsCertVerifier::new(settings.trusted_certificate.clone())
            .map_err(FtpConnectError::Other)?;
        let connector = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();

        let (mut stream, encrypted) = match stream.into_secure(
            RustlsConnector::from(Arc::new(connector)),
            settings.host.trim(),
        ) {
            Ok(stream) => (stream, true),
            Err(error) => {
                if let Some(fingerprint) = observed.lock().unwrap().take() {
                    return Err(FtpConnectError::Untrusted { fingerprint });
                }
                // Plain FTP servers reject AUTH TLS entirely; reconnect and
                // stay unencrypted instead of failing.
                if settings.protocol == RemoteProtocol::Ftp && is_ftp_tls_unsupported(&error) {
                    (
                        RustlsFtpStream::connect(address.as_str())
                            .with_context(|| format!("failed to reconnect to {}", settings.host))
                            .map_err(FtpConnectError::Other)?,
                        false,
                    )
                } else {
                    return Err(FtpConnectError::Other(eyre::eyre!(
                        "FTPS TLS negotiation failed: {error}"
                    )));
                }
            }
        };

        stream
            .login(settings.username.trim(), password)
            .context("FTP authentication failed")
            .map_err(FtpConnectError::Other)?;

        Ok((RemoteClient::Ftp(stream), encrypted))
    }

    pub fn check_directory(&mut self, path: &RemotePath) -> Result<()> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => {
                sftp.stat(Path::new(path.as_str()))?;
                Ok(())
            }
            RemoteClient::Ftp(ftp) => {
                let original = ftp.pwd()?;
                ftp.cwd(path.as_str())?;
                ftp.cwd(original)?;
                Ok(())
            }
        }
    }
}

enum FtpConnectError {
    Untrusted { fingerprint: String },
    Other(eyre::Report),
}

impl RemoteOps for RemoteConnection {
    fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.stat(Path::new(path.as_str())) {
                Ok(stat) => Ok(stat.is_dir()),
                Err(err) if is_sftp_not_found(&err) => Ok(false),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => {
                let original = ftp.pwd()?;
                match ftp.cwd(path.as_str()) {
                    Ok(()) => {
                        ftp.cwd(original)?;
                        Ok(true)
                    }
                    Err(err) if is_ftp_not_found(&err) => {
                        ftp.cwd(original)?;
                        Ok(false)
                    }
                    Err(err) => Err(err.into()),
                }
            }
        }
    }

    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.stat(Path::new(path.as_str())) {
                Ok(stat) if stat.is_dir() => Ok(None),
                Ok(stat) => Ok(stat.size),
                Err(err) if is_sftp_not_found(&err) => Ok(None),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp.size(path.as_str()) {
                Ok(size) => Ok(Some(size as u64)),
                Err(err) if is_ftp_not_found(&err) || is_ftp_size_unsupported(&err) => Ok(None),
                Err(err) => Err(err.into()),
            },
        }
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.readdir(Path::new(dir.as_str())) {
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
            },
            RemoteClient::Ftp(ftp) => match ftp.list(Some(dir.as_str())) {
                Ok(entries) => ftp_list_entries(entries),
                Err(err) if is_ftp_not_found(&err) => Ok(Vec::new()),
                Err(err) => Err(err.into()),
            },
        }
    }

    fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
        // Check the size first so oversized files are never downloaded.
        match self.file_size(path)? {
            Some(size) if size > max => {
                bail!("remote file {path} is {size} bytes, exceeding the {max}-byte limit")
            }
            Some(_) => {}
            None => {
                if !self.is_file(path)? {
                    return Ok(None);
                }
            }
        }

        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.open(Path::new(path.as_str())) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(max + 1).read_to_end(&mut bytes)?;
                    ensure!(
                        bytes.len() as u64 <= max,
                        "remote file {path} exceeds the {max}-byte limit"
                    );
                    Ok(Some(bytes))
                }
                Err(err) if is_sftp_not_found(&err) => Ok(None),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp.retr_as_buffer(path.as_str()) {
                Ok(bytes) => {
                    let bytes = bytes.into_inner();
                    ensure!(
                        bytes.len() as u64 <= max,
                        "remote file {path} exceeds the {max}-byte limit"
                    );
                    Ok(Some(bytes))
                }
                Err(err) if is_ftp_not_found(&err) => Ok(None),
                Err(err) => Err(err.into()),
            },
        }
    }

    fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.stat(Path::new(path.as_str())) {
                Ok(stat) => Ok(!stat.is_dir()),
                Err(err) if is_sftp_not_found(&err) => Ok(false),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp.size(path.as_str()) {
                Ok(_) => Ok(true),
                Err(err) if is_ftp_not_found(&err) || is_ftp_size_unsupported(&err) => Ok(false),
                Err(err) => Err(err.into()),
            },
        }
    }

    fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => {
                let mut file = sftp.create(Path::new(path.as_str()))?;
                file.write_all(bytes)?;
                file.flush()?;
                Ok(())
            }
            RemoteClient::Ftp(ftp) => {
                ftp.put_file(path.as_str(), &mut Cursor::new(bytes))?;
                Ok(())
            }
        }
    }

    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
        let mut file = File::open(local)
            .with_context(|| format!("failed to read staged file {}", local.display()))?;

        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => {
                let mut remote_file = sftp.create(Path::new(remote.as_str()))?;
                std::io::copy(&mut file, &mut remote_file)?;
                remote_file.flush()?;
                Ok(())
            }
            RemoteClient::Ftp(ftp) => {
                ftp.put_file(remote.as_str(), &mut file)?;
                Ok(())
            }
        }
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => {
                sftp.rename(
                    Path::new(from.as_str()),
                    Path::new(to.as_str()),
                    Some(RenameFlags::ATOMIC | RenameFlags::OVERWRITE | RenameFlags::NATIVE),
                )?;
                Ok(())
            }
            RemoteClient::Ftp(ftp) => {
                ftp.rename(from.as_str(), to.as_str())?;
                Ok(())
            }
        }
    }

    fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.unlink(Path::new(path.as_str())) {
                Ok(()) => Ok(true),
                Err(err) if is_sftp_not_found(&err) => Ok(false),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp.rm(path.as_str()) {
                Ok(()) => Ok(true),
                Err(err) if is_ftp_not_found(&err) => Ok(false),
                Err(err) => Err(err.into()),
            },
        }
    }

    fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.rmdir(Path::new(path.as_str())) {
                Ok(()) => Ok(()),
                Err(err) if is_sftp_not_found(&err) => Ok(()),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp.rmdir(path.as_str()) {
                Ok(()) => Ok(()),
                Err(err) if is_ftp_not_found(&err) => Ok(()),
                Err(err) => Err(err.into()),
            },
        }
    }

    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.stat(Path::new(path.as_str())) {
                Ok(_) => Ok(()),
                Err(err) if is_sftp_not_found(&err) => {
                    sftp.mkdir(Path::new(path.as_str()), 0o755)?;
                    Ok(())
                }
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => {
                let original = ftp.pwd()?;
                if ftp.cwd(path.as_str()).is_ok() {
                    ftp.cwd(original)?;
                    return Ok(());
                }
                ftp.cwd(&original)?;
                ftp.mkdir(path.as_str())?;
                Ok(())
            }
        }
    }

    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.mkdir(Path::new(path.as_str()), 0o755) {
                Ok(()) => Ok(true),
                Err(error) => match sftp.stat(Path::new(path.as_str())) {
                    Ok(stat) if stat.is_dir() => Ok(false),
                    _ => Err(error.into()),
                },
            },
            RemoteClient::Ftp(ftp) => match ftp.mkdir(path.as_str()) {
                Ok(()) => Ok(true),
                Err(error) => {
                    let original = ftp.pwd()?;
                    match ftp.cwd(path.as_str()) {
                        Ok(()) => {
                            ftp.cwd(original)?;
                            Ok(false)
                        }
                        Err(_) => {
                            ftp.cwd(&original).ok();
                            Err(error.into())
                        }
                    }
                }
            },
        }
    }

    fn reconnect(&mut self) -> Result<()> {
        let attempt = Self::connect(&self.settings, &self.password)?;
        match attempt {
            ConnectionAttempt::Connected(connection) => {
                self.client = connection.client;
                self.fingerprint = connection.fingerprint;
                self.encrypted = connection.encrypted;
                Ok(())
            }
            ConnectionAttempt::HostKeyUntrusted { .. }
            | ConnectionAttempt::CertificateUntrusted { .. } => {
                bail!("server trust verification failed while reconnecting")
            }
        }
    }
}

fn connect_tcp(settings: &RemoteServerSettings) -> Result<ssh2::Session> {
    let address = format!("{}:{}", settings.host.trim(), settings.port);
    let addresses = address
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve {}", settings.host))?
        .collect::<Vec<_>>();

    ensure!(!addresses.is_empty(), "host resolved to no addresses");

    let mut last_error = None;

    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(CONNECT_TIMEOUT)).ok();
                stream.set_write_timeout(Some(CONNECT_TIMEOUT)).ok();

                let mut session = ssh2::Session::new().context("failed to create SSH session")?;
                session.set_tcp_stream(stream);
                session.set_timeout(SSH_TIMEOUT.as_millis() as u32);

                return Ok(session);
            }
            Err(err) => last_error = Some(err),
        }
    }

    bail!(
        "failed to connect to {}: {}",
        address,
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown network error".to_owned())
    )
}

fn host_key_fingerprint(session: &ssh2::Session) -> Result<String> {
    let hash = session
        .host_key_hash(HashType::Sha256)
        .ok_or_eyre("server did not provide a SHA256 host-key fingerprint")?;

    Ok(format!("SHA256:{}", STANDARD_NO_PAD.encode(hash)))
}

fn authenticate(
    session: &ssh2::Session,
    settings: &RemoteServerSettings,
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

fn is_ftp_not_found(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response) if response.status == Status::FileUnavailable)
}

fn is_ftp_size_unsupported(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response) if FTP_SIZE_UNAVAILABLE.contains(&response.status))
}

fn is_ftp_tls_unsupported(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response) if matches!(response.status, Status::NotImplemented | Status::BadCommand))
}

fn ftp_list_entries(entries: Vec<String>) -> Result<Vec<RemoteEntry>> {
    entries
        .into_iter()
        .map(|entry| {
            entry
                .parse::<suppaftp::list::File>()
                .map(|file| RemoteEntry {
                    name: file.name().to_owned(),
                    is_directory: file.is_directory(),
                    size: Some(file.size() as u64),
                })
                .with_context(|| format!("failed to parse FTP LIST entry: {entry}"))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod memory {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use eyre::{Context, Result, bail};

    use super::{RemoteEntry, RemoteOps};
    use crate::profile::server::paths::RemotePath;

    /// In-memory `RemoteOps` for engine/state/lease tests. Supplies remote
    /// filesystem state; contains no deployment logic of its own.
    pub(crate) struct MemoryRemote {
        pub files: BTreeMap<String, Vec<u8>>,
        pub dirs: BTreeSet<String>,
        /// Paths whose next write/upload/delete fails once, then clears.
        pub fail_once: BTreeSet<String>,
        /// Paths whose write/upload/delete always fails.
        pub fail_always: BTreeSet<String>,
    }

    impl MemoryRemote {
        pub fn new() -> Self {
            let mut dirs = BTreeSet::new();
            dirs.insert("/".to_owned());
            Self {
                files: BTreeMap::new(),
                dirs,
                fail_once: BTreeSet::new(),
                fail_always: BTreeSet::new(),
            }
        }

        /// Convenience: place a file, registering its parent directories.
        pub fn put_file(&mut self, path: &str, bytes: &[u8]) {
            self.files.insert(path.to_owned(), bytes.to_vec());
            self.register_parents(path);
        }

        pub fn contents(&self, path: &str) -> Option<&[u8]> {
            self.files.get(path).map(Vec::as_slice)
        }

        fn register_parents(&mut self, path: &str) {
            let mut prefix = path.rmatch_indices('/');
            // Skip the file itself: take all directory prefixes.
            prefix.next();
            for (idx, _) in prefix {
                if idx > 0 {
                    self.dirs.insert(path[..idx].to_owned());
                }
            }
        }

        fn maybe_fail(&mut self, path: &RemotePath) -> Result<()> {
            if self.fail_always.contains(path.as_str()) || self.fail_once.remove(path.as_str()) {
                bail!("injected remote failure on {}", path.as_str());
            }
            Ok(())
        }
    }

    impl RemoteOps for MemoryRemote {
        fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
            Ok(self.dirs.contains(path.as_str()))
        }

        fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
            Ok(self.files.contains_key(path.as_str()))
        }

        fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
            Ok(self.files.get(path.as_str()).map(|b| b.len() as u64))
        }

        fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
            let base = dir.as_str().trim_end_matches('/');
            let mut entries = Vec::new();
            let mut seen = BTreeSet::new();

            for (path, bytes) in &self.files {
                if let Some(rest) = path.strip_prefix(&format!("{base}/")) {
                    let name = rest.split('/').next().unwrap().to_owned();
                    if seen.insert(name.clone()) {
                        entries.push(RemoteEntry {
                            is_directory: rest.contains('/'),
                            name,
                            size: Some(bytes.len() as u64),
                        });
                    }
                }
            }
            for dir_path in &self.dirs {
                if let Some(rest) = dir_path.strip_prefix(&format!("{base}/")) {
                    let name = rest.split('/').next().unwrap().to_owned();
                    if !name.is_empty() && seen.insert(name.clone()) {
                        entries.push(RemoteEntry {
                            name,
                            is_directory: true,
                            size: None,
                        });
                    }
                }
            }
            Ok(entries)
        }

        fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
            let Some(bytes) = self.files.get(path.as_str()) else {
                return Ok(None);
            };
            if bytes.len() as u64 > max {
                bail!(
                    "remote file {} exceeds the {}-byte read bound",
                    path.as_str(),
                    max
                );
            }
            Ok(Some(bytes.clone()))
        }

        fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
            self.maybe_fail(path)?;
            self.put_file(path.as_str(), bytes);
            Ok(())
        }

        fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
            self.maybe_fail(remote)?;
            let bytes =
                std::fs::read(local).with_context(|| format!("failed to read {local:?}"))?;
            self.put_file(remote.as_str(), &bytes);
            Ok(())
        }

        fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
            let bytes = self
                .files
                .remove(from.as_str())
                .ok_or_else(|| eyre::eyre!("rename source {} missing", from.as_str()))?;
            self.put_file(to.as_str(), &bytes);
            Ok(())
        }

        fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
            self.maybe_fail(path)?;
            Ok(self.files.remove(path.as_str()).is_some())
        }

        fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
            let base = format!("{}/", path.as_str().trim_end_matches('/'));
            if self.files.keys().any(|f| f.starts_with(&base))
                || self
                    .dirs
                    .iter()
                    .any(|d| d != path.as_str() && d.starts_with(&base))
            {
                bail!("directory {} is not empty", path.as_str());
            }
            self.dirs.remove(path.as_str());
            Ok(())
        }

        fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
            self.dirs.insert(path.as_str().to_owned());
            self.register_parents(&format!("{}/x", path.as_str().trim_end_matches('/')));
            Ok(())
        }

        fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
            Ok(self.dirs.insert(path.as_str().to_owned()))
        }

        fn reconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// A shared handle so tests can keep observing the remote after it is
    /// boxed into a [`crate::profile::server::engine::Session`].
    impl RemoteOps for std::sync::Arc<std::sync::Mutex<MemoryRemote>> {
        fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
            self.lock().unwrap().is_dir(path)
        }

        fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
            self.lock().unwrap().is_file(path)
        }

        fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
            self.lock().unwrap().file_size(path)
        }

        fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
            self.lock().unwrap().list(dir)
        }

        fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
            self.lock().unwrap().read(path, max)
        }

        fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
            self.lock().unwrap().write(path, bytes)
        }

        fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
            self.lock().unwrap().upload(local, remote)
        }

        fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
            self.lock().unwrap().rename(from, to)
        }

        fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
            self.lock().unwrap().delete_file(path)
        }

        fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
            self.lock().unwrap().delete_dir(path)
        }

        fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
            self.lock().unwrap().ensure_dir(path)
        }

        fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
            self.lock().unwrap().claim_dir(path)
        }

        fn reconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use suppaftp::{FtpError, Status, types::Response};

    use super::{ftp_list_entries, is_ftp_not_found, is_ftp_tls_unsupported};

    #[test]
    fn parses_ftp_directory_responses() {
        let entries = ftp_list_entries(vec![
            "-rw-r--r-- 1 owner group 42 Sep 11 12:00 File name.dll.old".to_owned(),
        ])
        .unwrap();
        assert_eq!(entries[0].name, "File name.dll.old");
        assert!(!entries[0].is_directory);
        assert_eq!(entries[0].size, Some(42));
    }

    #[test]
    fn recognizes_missing_ftp_paths() {
        let missing = FtpError::UnexpectedResponse(Response::new(
            Status::from(550),
            b"550 path does not exists".to_vec(),
        ));
        assert!(is_ftp_not_found(&missing));
    }

    #[test]
    fn recognizes_ftp_servers_without_tls() {
        let unsupported = FtpError::UnexpectedResponse(Response::new(
            Status::from(502),
            b"502 AUTH TLS not implemented".to_vec(),
        ));

        assert!(is_ftp_tls_unsupported(&unsupported));
    }
}
