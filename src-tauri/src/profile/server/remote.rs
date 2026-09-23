use std::{
    fs::File,
    io::{Cursor, ErrorKind, Read, Write},
    net::{Shutdown, TcpStream, ToSocketAddrs},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
use suppaftp::{FtpError, RustlsConnector, RustlsFtpStream, Status, types::FileType};
use tracing::{debug, info, warn};

use super::{
    paths::RemotePath,
    settings::{RemoteAuthentication, RemoteProtocol, RemoteServerSettings},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on draining an upload's data channel to EOF after the TLS/TCP
/// close handshake. A healthy server closes its side as soon as the
/// client's FIN arrives, so exceeding this means the peer is holding the
/// channel open — the upload must fail rather than stall the deployment.
const FTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Live failures begin near 40 seconds or 170 transfers on one control
/// connection. Retire it before either observed boundary.
const FTP_MAX_CONNECTION_AGE: Duration = Duration::from_secs(30);
const FTP_MAX_TRANSFERS: u64 = 100;
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
    /// Atomically creates `path` as a directory: `true` when created,
    /// `false` when it already existed. The deployment lease uses it
    /// because `MKD`/mkdir is atomic on both FTP and SFTP servers.
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
    connected_at: Instant,
    transfer_count: u64,
    ftp_max_age: Duration,
    ftp_max_transfers: u64,
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

/// The rustls provider for FTPS client configuration. Prefer the
/// process-wide provider when the application installed one; fall back
/// to aws-lc-rs, which this crate's feature graph always compiles in.
/// Resolving it explicitly keeps this module working in processes that
/// never install a default, where `ClientConfig::builder()` panics on
/// the ambiguous feature set.
fn crypto_provider() -> Arc<suppaftp::rustls::crypto::CryptoProvider> {
    suppaftp::rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(suppaftp::rustls::crypto::aws_lc_rs::default_provider()))
}

/// The `ClientConfig` behind an FTPS session. Provider and protocol
/// versions are chosen explicitly for the same reason as
/// [`crypto_provider`]: the plain builder relies on process state this
/// module cannot assume.
fn ftps_client_config(verifier: FtpsCertVerifier) -> ClientConfig {
    ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("the aws-lc-rs provider supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth()
}

/// FTPS certificate verification. When a fingerprint pin is configured it
/// is an exact-match requirement: the certificate the server presents
/// must match it whether or not normal CA validation would have
/// succeeded, so a CA-valid *replacement* certificate cannot silently
/// satisfy the pin. Without a pin, normal CA validation decides. When
/// that fails, the observed fingerprint is recorded so the caller can
/// show it and ask the user to trust it explicitly.
///
/// Handshake-signature verification always goes through webpki. Trusting
/// a certificate must never skip checking that the server holds its
/// private key.
#[derive(Debug)]
struct FtpsCertVerifier {
    webpki: Arc<suppaftp::rustls::client::WebPkiServerVerifier>,
    pinned: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
}

impl FtpsCertVerifier {
    fn new(pinned: Option<String>) -> Result<(Self, Arc<Mutex<Option<String>>>)> {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots, pinned)
    }

    fn with_roots(
        roots: RootCertStore,
        pinned: Option<String>,
    ) -> Result<(Self, Arc<Mutex<Option<String>>>)> {
        let webpki = suppaftp::rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            crypto_provider(),
        )
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
        let fingerprint = certificate_fingerprint(end_entity);
        *self.observed.lock().unwrap() = Some(fingerprint.clone());

        // The pin is the whole trust decision when configured: exact match
        // or rejection, independent of what CA validation thinks.
        if let Some(pinned) = self.pinned.as_deref() {
            return if pinned == fingerprint {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(TlsError::InvalidCertificate(
                    suppaftp::rustls::CertificateError::Other(suppaftp::rustls::OtherError(
                        Arc::new(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "certificate fingerprint does not match the pinned value",
                        )),
                    )),
                ))
            };
        }

        self.webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

/// Fingerprint of the certificate the server presents, used for pin
/// comparison. blake3 is already a dependency and gives a stable 256-bit
/// digest.
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
    /// `settings` is expected to have passed
    /// [`RemoteServerSettings::validate_connection`] already; this is the
    /// transport layer, so it only parses the paths it actually needs.
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
            connected_at: Instant::now(),
            transfer_count: 0,
            ftp_max_age: FTP_MAX_CONNECTION_AGE,
            ftp_max_transfers: FTP_MAX_TRANSFERS,
        }
    }

    fn renew_ftp_if_needed(&mut self) -> Result<()> {
        if matches!(self.client, RemoteClient::Ftp(_))
            && (self.connected_at.elapsed() >= self.ftp_max_age
                || self.transfer_count >= self.ftp_max_transfers)
        {
            self.reconnect()
                .context("failed to renew FTP control connection")?;
        }
        Ok(())
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
        let connector = ftps_client_config(verifier);

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
                if allows_plaintext_ftp_fallback(settings, &error) {
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

        // Binary mode is required for byte-exact transfers — ASCII mode
        // may newline-mangle payloads — and some servers (e.g. DatHost's
        // ProFTPD) refuse SIZE entirely while in ASCII mode, which would
        // make existing files look absent.
        stream
            .transfer_type(FileType::Binary)
            .context("FTP server refused binary transfer mode")
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
        self.renew_ftp_if_needed()?;
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.stat(Path::new(path.as_str())) {
                Ok(stat) => Ok(stat.is_dir()),
                Err(err) if is_sftp_not_found(&err) => Ok(false),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp_mlst(ftp, path.as_str())? {
                Mlst::Facts(facts) => Ok(facts.is_dir),
                Mlst::Absent => Ok(false),
                Mlst::Unsupported => {
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
            },
        }
    }

    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
        self.renew_ftp_if_needed()?;
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.stat(Path::new(path.as_str())) {
                Ok(stat) if stat.is_dir() => Ok(None),
                Ok(stat) => Ok(stat.size),
                Err(err) if is_sftp_not_found(&err) => Ok(None),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp_mlst(ftp, path.as_str())? {
                Mlst::Absent => Ok(None),
                Mlst::Facts(facts) if facts.is_dir => Ok(None),
                Mlst::Facts(facts) => match facts.size {
                    Some(size) => Ok(Some(size)),
                    None => ftp_size(ftp, path.as_str()),
                },
                Mlst::Unsupported => ftp_size(ftp, path.as_str()),
            },
        }
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        self.renew_ftp_if_needed()?;
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
            RemoteClient::Ftp(ftp) => {
                self.transfer_count += 1;
                debug!(phase = "snapshot.list", command = "LIST", path = %dir, transfer_count = self.transfer_count, "listing FTP directory");
                match ftp.list(Some(dir.as_str())) {
                    Ok(entries) => ftp_list_entries(entries),
                    Err(err) if is_ftp_not_found(&err) => Ok(Vec::new()),
                    Err(err) => {
                        warn!(phase = "snapshot.list", command = "LIST", path = %dir, transfer_count = self.transfer_count, connection_age_ms = self.connected_at.elapsed().as_millis(), error = %err, "FTP directory listing failed");
                        Err(err.into())
                    }
                }
            }
        }
    }

    fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
        // A concrete size lets oversized downloads be refused before the
        // transfer starts. When the server cannot report one — `SIZE` is
        // refused in FTP ASCII mode on some hosts, and an absent file
        // reports the same way — the transfer itself decides existence
        // and the cap is enforced on the received bytes.
        let size = self.file_size(path).inspect_err(|err| {
            if matches!(self.client, RemoteClient::Ftp(_)) {
                warn!(phase = "read.metadata", path = %path, transfer_count = self.transfer_count, connection_age_ms = self.connected_at.elapsed().as_millis(), error = %err, "FTP metadata probe failed before RETR");
            }
        })?;
        if let Some(size) = size {
            ensure!(
                size <= max,
                "remote file {path} is {size} bytes, exceeding the {max}-byte limit"
            );
        }

        self.renew_ftp_if_needed()?;

        let transfer = if matches!(self.client, RemoteClient::Ftp(_)) {
            self.transfer_count += 1;
            Some((self.transfer_count, self.connected_at.elapsed().as_millis()))
        } else {
            None
        };

        let result: Option<Vec<u8>> = match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.open(Path::new(path.as_str())) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(max + 1).read_to_end(&mut bytes)?;
                    ensure!(
                        bytes.len() as u64 <= max,
                        "remote file {path} exceeds the {max}-byte limit"
                    );
                    Some(bytes)
                }
                Err(err) if is_sftp_not_found(&err) => None,
                Err(err) => return Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp.retr_as_stream(path.as_str()) {
                Ok(mut stream) => {
                    let (transfer_count, connection_age_ms) = transfer.unwrap();
                    debug!(phase = "read.data", command = "RETR", path = %path, transfer_count, connection_age_ms, "FTP data transfer started");
                    let transfer_started = Instant::now();
                    let mut bytes = Vec::new();
                    let read = (&mut stream)
                        .take(max.saturating_add(1))
                        .read_to_end(&mut bytes);
                    let finalize = ftp.finalize_retr_stream(stream);
                    if let Err(err) = &read {
                        warn!(phase = "read.data", command = "RETR", path = %path, transfer_count, connection_age_ms, transfer_duration_ms = transfer_started.elapsed().as_millis(), error = %err, "FTP data channel read failed");
                    }
                    if let Err(err) = &finalize {
                        warn!(phase = "read.completion", command = "RETR", path = %path, transfer_count, connection_age_ms, transfer_duration_ms = transfer_started.elapsed().as_millis(), error = %err, "FTP control channel did not confirm transfer completion");
                    }
                    read.with_context(|| format!("FTP RETR data read failed for {path}"))?;
                    finalize.with_context(|| format!("FTP RETR completion failed for {path}"))?;
                    ensure!(
                        bytes.len() as u64 <= max,
                        "remote file {path} exceeds the {max}-byte limit"
                    );
                    debug!(phase = "read.completion", command = "RETR", path = %path, transfer_count, connection_age_ms, transfer_duration_ms = transfer_started.elapsed().as_millis(), bytes = bytes.len(), "FTP control channel confirmed transfer completion");
                    Some(bytes)
                }
                Err(err) if is_ftp_not_found(&err) => None,
                Err(err) => {
                    let (transfer_count, connection_age_ms) = transfer.unwrap();
                    warn!(phase = "read.start", command = "RETR", path = %path, transfer_count, connection_age_ms, error = %err, "FTP transfer command failed");
                    return Err(err).with_context(|| format!("FTP RETR failed for {path}"));
                }
            },
        };

        // A RETR 550 conflates "absent" with "the server refuses to
        // return this file" — filtering hosts use the same status for
        // both. Cross-check existence so a refused read surfaces as an
        // error rather than a silently empty one.
        if result.is_none() && matches!(self.client, RemoteClient::Ftp(_)) && self.is_file(path)? {
            bail!("remote server refuses to return the existing file {path}");
        }

        Ok(result)
    }

    fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
        self.renew_ftp_if_needed()?;
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.stat(Path::new(path.as_str())) {
                Ok(stat) => Ok(!stat.is_dir()),
                Err(err) if is_sftp_not_found(&err) => Ok(false),
                Err(err) => Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp_mlst(ftp, path.as_str())? {
                Mlst::Facts(facts) => Ok(!facts.is_dir),
                Mlst::Absent => Ok(false),
                Mlst::Unsupported => ftp_size(ftp, path.as_str()).map(|size| size.is_some()),
            },
        }
    }

    fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
        self.renew_ftp_if_needed()?;
        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => {
                let mut file = sftp.create(Path::new(path.as_str()))?;
                file.write_all(bytes)?;
                file.flush()?;
                Ok(())
            }
            RemoteClient::Ftp(ftp) => {
                self.transfer_count += 1;
                debug!(phase = "write", command = "STOR", path = %path, transfer_count = self.transfer_count, "starting FTP upload");
                ftp_store(
                    ftp,
                    path.as_str(),
                    &mut Cursor::new(bytes),
                    bytes.len() as u64,
                    FTP_DRAIN_TIMEOUT,
                )
            }
        }
    }

    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
        self.renew_ftp_if_needed()?;
        let mut file = File::open(local)
            .with_context(|| format!("failed to read staged file {}", local.display()))?;
        let expected_len = file
            .metadata()
            .with_context(|| format!("failed to stat staged file {}", local.display()))?
            .len();

        match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => {
                let mut remote_file = sftp.create(Path::new(remote.as_str()))?;
                std::io::copy(&mut file, &mut remote_file)?;
                remote_file.flush()?;
                Ok(())
            }
            RemoteClient::Ftp(ftp) => {
                self.transfer_count += 1;
                debug!(phase = "upload", command = "STOR", path = %remote, transfer_count = self.transfer_count, "starting FTP upload");
                ftp_store(
                    ftp,
                    remote.as_str(),
                    &mut file,
                    expected_len,
                    FTP_DRAIN_TIMEOUT,
                )
            }
        }
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.renew_ftp_if_needed()?;
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
        self.renew_ftp_if_needed()?;
        let deleted = match &mut self.client {
            RemoteClient::Sftp { sftp, .. } => match sftp.unlink(Path::new(path.as_str())) {
                Ok(()) => true,
                Err(err) if is_sftp_not_found(&err) => false,
                Err(err) => return Err(err.into()),
            },
            RemoteClient::Ftp(ftp) => match ftp.rm(path.as_str()) {
                Ok(()) => true,
                Err(err) if is_ftp_not_found(&err) => false,
                Err(err) => return Err(err.into()),
            },
        };
        if !deleted && matches!(self.client, RemoteClient::Ftp(_)) && self.is_file(path)? {
            bail!("remote server refuses to delete the existing file {path}");
        }
        Ok(deleted)
    }

    fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        self.renew_ftp_if_needed()?;
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
        self.renew_ftp_if_needed()?;
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
        self.renew_ftp_if_needed()?;
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
        info!(
            phase = "reconnect",
            transfer_count = self.transfer_count,
            connection_age_ms = self.connected_at.elapsed().as_millis(),
            "replacing remote connection"
        );
        let attempt = Self::connect(&self.settings, &self.password)?;
        match attempt {
            ConnectionAttempt::Connected(connection) => {
                self.client = connection.client;
                self.fingerprint = connection.fingerprint;
                self.encrypted = connection.encrypted;
                self.connected_at = Instant::now();
                self.transfer_count = 0;
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

/// Whether the server does not implement the command at all (500/502),
/// meaning callers should use their pre-RFC-3659 fallback probe.
fn is_ftp_command_unsupported(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response) if matches!(response.status, Status::NotImplemented | Status::BadCommand))
}

fn is_ftp_size_unsupported(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response) if FTP_SIZE_UNAVAILABLE.contains(&response.status))
}

fn is_ftp_tls_unsupported(error: &FtpError) -> bool {
    is_ftp_command_unsupported(error)
}

/// What an RFC 3659 `MLST` probe learned about a path. `MLST` reports
/// `type=` and `size=` facts for one path in a single command, which makes
/// it the most reliable existence probe on FTP servers that restrict
/// `SIZE` (e.g. ProFTPD's "SIZE not allowed in ASCII mode") or filter
/// files out of listings entirely.
enum Mlst {
    /// The path exists; the server's facts describe it.
    Facts(MlstFacts),
    /// The server reported the path does not exist.
    Absent,
    /// The server does not implement `MLST`; callers should use their
    /// fallback probe (CWD for directories, SIZE for files).
    Unsupported,
}

struct MlstFacts {
    /// `type=` was `dir`, `cdir`, or `pdir`.
    is_dir: bool,
    /// The `size=` fact, when the server reported one.
    size: Option<u64>,
}

fn ftp_mlst(ftp: &mut RustlsFtpStream, path: &str) -> Result<Mlst> {
    debug!(
        phase = "metadata",
        command = "MLST",
        path,
        "probing FTP path"
    );
    match ftp.mlst(Some(path)) {
        Ok(line) => Ok(Mlst::Facts(parse_mlst_facts(&line))),
        Err(err) if is_ftp_not_found(&err) => Ok(Mlst::Absent),
        Err(err) if is_ftp_command_unsupported(&err) => Ok(Mlst::Unsupported),
        Err(err) => {
            warn!(phase = "metadata", command = "MLST", path, error = %err, "FTP metadata command failed");
            Err(err).with_context(|| format!("FTP MLST failed for {path}"))
        }
    }
}

/// Parses an MLST fact line like `modify=..;size=227;type=file; <path>`:
/// the fact list runs to the first space, then the pathname follows.
fn parse_mlst_facts(line: &str) -> MlstFacts {
    let facts = line.split_once(' ').map_or(line, |(facts, _)| facts);
    let mut parsed = MlstFacts {
        is_dir: false,
        size: None,
    };

    for fact in facts.split(';') {
        match fact.split_once('=') {
            Some(("type", kind)) => {
                parsed.is_dir = matches!(kind, "dir" | "cdir" | "pdir");
            }
            Some(("size", size)) => parsed.size = size.trim().parse().ok(),
            _ => {}
        }
    }

    parsed
}

/// `SIZE` as an existence/size probe — the pre-RFC-3659 fallback. A 550
/// or 500/502 response means the size cannot be determined this way,
/// either because the path is absent or because the server refuses the
/// command (as with ProFTPD in ASCII mode).
fn ftp_size(ftp: &mut RustlsFtpStream, path: &str) -> Result<Option<u64>> {
    debug!(
        phase = "metadata",
        command = "SIZE",
        path,
        "probing FTP size"
    );
    match ftp.size(path) {
        Ok(size) => Ok(Some(size as u64)),
        Err(err) if is_ftp_not_found(&err) || is_ftp_size_unsupported(&err) => Ok(None),
        Err(err) => {
            warn!(phase = "metadata", command = "SIZE", path, error = %err, "FTP metadata command failed");
            Err(err).with_context(|| format!("FTP SIZE failed for {path}"))
        }
    }
}

/// Owns an FTPS upload stream and explicitly closes its TCP write side after
/// the TLS stream has sent `close_notify`.
///
/// SuppaFTP 11's synchronous `finalize_put_stream` drops the data stream and
/// immediately reads the control-channel reply. Its rustls stream emits
/// `close_notify` from `Drop`, but the underlying socket is only closed as a
/// side effect of dropping the last handle. Some ProFTPD installations can
/// persist the first 8 KiB received when that implicit close races the final
/// TLS records. Keeping a socket clone lets this wrapper make the lifecycle
/// explicit: dropping the TLS stream sends `close_notify`, then `shutdown`
/// sends the TCP FIN that terminates the data channel before `226` is read.
struct FtpUploadStream<S> {
    stream: Option<S>,
    socket: TcpStream,
    closed: bool,
}

impl<S: Write> FtpUploadStream<S> {
    fn new(stream: S, socket: TcpStream) -> Self {
        Self {
            stream: Some(stream),
            socket,
            closed: false,
        }
    }

    /// Send the TLS close notification, half-close the TCP write side, and
    /// consume every response record before the socket is dropped.
    ///
    /// `timeout` bounds the whole drain: a peer that never sends its FIN
    /// must not block the upload indefinitely.
    fn close_tls_and_drain(&mut self, timeout: Duration) -> Result<()> {
        drop(self.stream.take());
        self.socket
            .shutdown(Shutdown::Write)
            .context("failed to shut down FTP data socket")?;

        // FTPS servers may send post-handshake TLS records (notably TLS 1.3
        // session tickets) on the data channel. Leaving those bytes unread
        // makes Windows and some POSIX stacks reset the socket when the last
        // handle is dropped, which can discard data the server received just
        // before the reset. The data channel has no application response, so
        // draining it to EOF is the complete close handshake.
        let deadline = Instant::now() + timeout;
        let mut discarded = [0u8; 16 * 1024];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for the FTP server to close the data connection");
            }
            self.socket
                .set_read_timeout(Some(remaining))
                .context("failed to bound the FTP data socket drain")?;
            match self.socket.read(&mut discarded) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    bail!("timed out waiting for the FTP server to close the data connection")
                }
                Err(error) => return Err(error).context("failed to drain FTP data socket"),
            }
        }
        self.closed = true;
        Ok(())
    }
}

impl<S: Write> Write for FtpUploadStream<S> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.stream
            .as_mut()
            .expect("FTP upload stream was already closed")
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream
            .as_mut()
            .expect("FTP upload stream was already closed")
            .flush()
    }
}

impl<S> Drop for FtpUploadStream<S> {
    fn drop(&mut self) {
        if !self.closed {
            // Best effort cleanup for an error path. The normal path calls
            // `close_tls_and_drain` so drain errors can be reported.
            drop(self.stream.take());
            let _ = self.socket.shutdown(Shutdown::Write);
        }
    }
}

/// `STOR` with an explicit data-stream flush and byte-count check.
///
/// SuppaFTP's synchronous `finalize_put_stream` only drops the data stream
/// before reading `226`. Over FTPS, the rustls stream can have unread
/// post-handshake records from the server; dropping a socket with those bytes
/// pending may reset the connection and make ProFTPD discard the tail. Flush
/// the application records, complete the TLS/TCP close handshake, and compare
/// the stored length so a truncated copy is never accepted.
fn ftp_store(
    ftp: &mut RustlsFtpStream,
    path: &str,
    reader: &mut impl Read,
    expected_len: u64,
    drain_timeout: Duration,
) -> Result<()> {
    let stream = ftp.put_with_stream(path)?;
    let socket = stream
        .get_ref()
        .try_clone()
        .context("failed to duplicate FTP data socket")?;
    let mut stream = FtpUploadStream::new(stream, socket);
    let written = std::io::copy(reader, &mut stream).context("failed to write FTP data stream")?;
    ensure!(
        written == expected_len,
        "FTP upload for {path} copied {written} bytes, expected {expected_len}"
    );
    stream.flush().context("failed to flush FTP data stream")?;
    stream.close_tls_and_drain(drain_timeout)?;
    ftp.finalize_put_stream(stream)?;
    // A flushed socket only proves local delivery. Some servers still
    // send 226 after aborting the data channel; check the stored length
    // wherever the protocol exposes it.
    let stored = match ftp_mlst(ftp, path)? {
        Mlst::Facts(facts) => facts.size,
        Mlst::Absent => bail!("FTP upload for {path} completed but the file is absent"),
        Mlst::Unsupported => ftp_size(ftp, path)?,
    };
    if let Some(stored) = stored {
        ensure!(
            stored == expected_len,
            "FTP upload for {path} stored {stored} bytes, expected {expected_len}"
        );
    }
    Ok(())
}

/// Whether a failed AUTH TLS negotiation may retry the connection
/// unencrypted. Only automatic `ftp` mode degrades; an explicit `ftps`
/// selection fails instead of sending credentials in the clear.
fn allows_plaintext_ftp_fallback(settings: &RemoteServerSettings, error: &FtpError) -> bool {
    settings.protocol == RemoteProtocol::Ftp && is_ftp_tls_unsupported(error)
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
        /// Simulates a dropped transport: reads and directory checks fail.
        pub connection_dead: bool,
        /// Return FTP 550 when rename cannot find its source or overwrite.
        pub ftp_rename_semantics: bool,
        pub write_events: Option<std::sync::mpsc::Sender<()>>,
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
                connection_dead: false,
                ftp_rename_semantics: false,
                write_events: None,
            }
        }

        /// Places a file and registers its parent directories.
        pub fn put_file(&mut self, path: &str, bytes: &[u8]) {
            self.files.insert(path.to_owned(), bytes.to_vec());
            self.register_parents(path);
        }

        pub fn contents(&self, path: &str) -> Option<&[u8]> {
            self.files.get(path).map(Vec::as_slice)
        }

        fn register_parents(&mut self, path: &str) {
            let mut prefix = path.rmatch_indices('/');
            // Skip the file itself, then take all directory prefixes.
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
            if self.connection_dead {
                bail!("connection is dead");
            }
            Ok(self.dirs.contains(path.as_str()))
        }

        fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
            if self.connection_dead {
                bail!("connection is dead");
            }
            Ok(self.files.contains_key(path.as_str()))
        }

        fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
            if self.connection_dead {
                bail!("connection is dead");
            }
            Ok(self.files.get(path.as_str()).map(|b| b.len() as u64))
        }

        fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
            if self.connection_dead {
                bail!("connection is dead");
            }
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
            if self.connection_dead {
                bail!("connection is dead");
            }
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
            if let Some(events) = &self.write_events {
                let _ = events.send(());
            }
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
            if self.ftp_rename_semantics
                && (!self.files.contains_key(from.as_str()) || self.files.contains_key(to.as_str()))
            {
                return Err(
                    suppaftp::FtpError::UnexpectedResponse(suppaftp::types::Response::new(
                        suppaftp::Status::FileUnavailable,
                        b"550 rename refused".to_vec(),
                    ))
                    .into(),
                );
            }
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

    /// Wraps a shared remote and hides dot-segment paths
    /// (`.gale-deploy.lock`, `.gale-server-state.json`, ...) from read
    /// commands: `read`/`file_size`/`is_file` report them missing and
    /// `list` omits them, while writes, mkdir, deletes, and `is_dir` still
    /// work. That mirrors hosts which filter hidden paths from FTP read
    /// commands — the condition behind a false "lease taken over" abort.
    ///
    /// `filter_lists` also hides them from directory listings, the
    /// strictest observed variant; with it off, listings still expose
    /// holder markers so competing claimants can report a busy lease.
    #[derive(Clone)]
    pub(crate) struct FilteredReads {
        pub inner: std::sync::Arc<std::sync::Mutex<MemoryRemote>>,
        pub filter_lists: bool,
    }

    impl FilteredReads {
        pub fn new(inner: std::sync::Arc<std::sync::Mutex<MemoryRemote>>) -> Self {
            Self {
                inner,
                filter_lists: true,
            }
        }

        fn hidden(path: &RemotePath) -> bool {
            path.as_str()
                .split('/')
                .any(|segment| segment.starts_with('.'))
        }
    }

    impl RemoteOps for FilteredReads {
        fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
            self.inner.lock().unwrap().is_dir(path)
        }

        fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
            if Self::hidden(path) {
                return Ok(false);
            }
            self.inner.lock().unwrap().is_file(path)
        }

        fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
            if Self::hidden(path) {
                return Ok(None);
            }
            self.inner.lock().unwrap().file_size(path)
        }

        fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
            if !self.filter_lists {
                return self.inner.lock().unwrap().list(dir);
            }
            // Listing a hidden directory itself yields nothing.
            if Self::hidden(dir) {
                return Ok(Vec::new());
            }
            Ok(self
                .inner
                .lock()
                .unwrap()
                .list(dir)?
                .into_iter()
                .filter(|entry| !entry.name.starts_with('.'))
                .collect())
        }

        fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
            if Self::hidden(path) {
                return Ok(None);
            }
            self.inner.lock().unwrap().read(path, max)
        }

        fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
            self.inner.lock().unwrap().write(path, bytes)
        }

        fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
            self.inner.lock().unwrap().upload(local, remote)
        }

        fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
            self.inner.lock().unwrap().rename(from, to)
        }

        fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
            self.inner.lock().unwrap().delete_file(path)
        }

        fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
            self.inner.lock().unwrap().delete_dir(path)
        }

        fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
            self.inner.lock().unwrap().ensure_dir(path)
        }

        fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
            self.inner.lock().unwrap().claim_dir(path)
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

/// An in-memory FTP endpoint for end-to-end tests. Unlike
/// [`memory::MemoryRemote`], which stubs [`RemoteOps`] directly, this
/// serves the actual wire protocol — USER/PASS/TYPE/PWD/CWD/PASV/LIST/
/// MLST/SIZE/MDTM/RETR/STOR/RNFR/RNTO/DELE/MKD/RMD — so tests exercise
/// `RemoteConnection`'s real command and error handling.
///
/// The knobs reproduce observed hosting behaviors: DatHost's ProFTPD
/// refuses `SIZE` while in ASCII mode, and a deeper filtering host could
/// refuse `RETR` on existing files while still proving their existence
/// through `MLST`.
#[cfg(test)]
pub(crate) mod fake_ftp {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use suppaftp::rustls::{
        ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer,
    };

    use super::{certificate_fingerprint, crypto_provider};

    /// Behaviors the fake server can exhibit.
    #[derive(Default, Clone, Copy)]
    pub struct Options {
        /// Refuse `SIZE` until the client sends `TYPE I` — DatHost's
        /// ProFTPD answers `550 SIZE not allowed in ASCII mode`.
        pub size_requires_binary: bool,
        /// Refuse all SIZE requests, including binary mode.
        pub refuse_size: bool,
        /// Refuse `RETR` even for files that exist and are provable via
        /// `MLST` — a deeper read filter.
        pub refuse_retr: bool,
        /// Refuse deletion with the same status used for absent files.
        pub refuse_dele: bool,
        /// Do not implement `MLST` (a pre-RFC-3659 host).
        pub no_mlst: bool,
        /// Require explicit FTPS: `AUTH TLS` upgrades the control
        /// connection and `PROT P` protects the data connections with
        /// a self-signed certificate.
        pub tls: bool,
        /// On `STOR`, complete the data-channel TLS handshake, then
        /// kill the socket before the payload can be received while
        /// still answering `226 transfer complete` — a mid-transfer
        /// abort the client must not report as success.
        pub abort_stor_after_tls_handshake: bool,
        /// On `STOR`, keep only this prefix of the received payload but
        /// still answer `226 transfer complete` — the observed host
        /// behavior where a store reports success while persisting a
        /// truncated file.
        pub truncate_stor_to: Option<usize>,
        /// On `STOR`, hold the data connection open after the payload
        /// arrives — no `close_notify`, no FIN — so the client's close
        /// handshake never sees EOF: a wedged server the drain bound
        /// must turn into an upload failure.
        pub hold_stor_eof: bool,
        /// Expire an individual control connection after this age.
        pub expire_control_after: Option<Duration>,
        /// Expire an individual control connection after this many data transfers.
        pub expire_control_after_transfers: Option<usize>,
    }

    /// A running fake server. `fs` is shared with every accepted
    /// connection, so tests observe and seed the remote filesystem
    /// directly.
    pub struct FakeFtp {
        pub addr: SocketAddr,
        /// Every `VERB arg` line received, for protocol assertions.
        pub commands: Arc<Mutex<Vec<String>>>,
        pub sent_bytes: Arc<std::sync::atomic::AtomicUsize>,
        retr_count: Arc<std::sync::atomic::AtomicUsize>,
        reset_retr_at: Arc<std::sync::atomic::AtomicUsize>,
        reset_retr_remaining: Arc<std::sync::atomic::AtomicUsize>,
        reset_after_completion: Arc<std::sync::atomic::AtomicBool>,
        /// The blake3 fingerprint of the self-signed certificate the
        /// server presents when `Options::tls` is on; `None` otherwise.
        certificate_fingerprint: Option<String>,
        fs: Arc<Mutex<Fs>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    #[derive(Default)]
    struct Fs {
        dirs: BTreeSet<String>,
        files: BTreeMap<String, Vec<u8>>,
    }

    impl Fs {
        fn exists(&self, path: &str) -> bool {
            self.dirs.contains(path) || self.files.contains_key(path)
        }

        /// Immediate children of `dir` as `(name, is_dir)` pairs.
        fn children(&self, dir: &str) -> Vec<(String, bool)> {
            let prefix = format!("{}/", dir.trim_end_matches('/'));
            let mut children = BTreeMap::new();

            let mut collect = |path: &String, is_dir: bool| {
                let Some(rest) = path.strip_prefix(&prefix) else {
                    return;
                };
                if rest.is_empty() {
                    return;
                }
                match rest.split_once('/') {
                    None => {
                        children.insert(rest.to_owned(), is_dir);
                    }
                    Some((head, _)) => {
                        children.insert(head.to_owned(), true);
                    }
                }
            };

            for dir in &self.dirs {
                collect(dir, true);
            }
            for file in self.files.keys() {
                collect(file, false);
            }
            children.into_iter().collect()
        }
    }

    /// Normalizes `path` to an absolute canonical form (`//`, `.`, `..`
    /// resolved lexically; FTP has no symlinks here).
    fn normalize(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for segment in path.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        format!("/{}", parts.join("/"))
    }

    fn resolve(cwd: &str, arg: &str) -> String {
        if arg.starts_with('/') {
            normalize(arg)
        } else {
            normalize(&format!("{}/{}", cwd.trim_end_matches('/'), arg))
        }
    }

    /// Per-connection TLS configs: `control` covers the control channel
    /// and ordinary data transfers, while `data` covers the data channel
    /// when `abort_stor_after_tls_handshake` is on — it is pinned to
    /// TLS 1.2 so the server owns the final handshake flight and can
    /// reset the socket before the client's payload write is attempted.
    /// (Under TLS 1.3 the client sends Finished and payload back-to-back,
    /// leaving no window for the abort to precede the write.)
    struct Tls {
        control: Arc<ServerConfig>,
        data: Arc<ServerConfig>,
    }

    /// Builds the self-signed server identity used when
    /// `Options::tls` is on. Returns the `Tls` configs plus the
    /// certificate's fingerprint so tests can pin it exactly the way a
    /// user trusts a certificate in Gale.
    fn tls_identity(tls12_data: bool) -> (Tls, String) {
        use suppaftp::rustls::version::{TLS12, TLS13};

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let fingerprint = certificate_fingerprint(certified.cert.der());
        let build = |versions: &[&'static suppaftp::rustls::SupportedProtocolVersion]| {
            let key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
            let mut config = ServerConfig::builder_with_provider(crypto_provider())
                .with_protocol_versions(versions)
                .expect("the aws-lc-rs provider supports these TLS versions")
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key.into())
                .unwrap();
            // Keep post-handshake tickets enabled: an FTPS server can send
            // these records on an upload data channel even though the client
            // has no application response to read. The upload regression
            // exercises draining those records before the TCP socket closes.
            config.send_tls13_tickets = 2;
            config
        };
        let control = Arc::new(build(&[&TLS13, &TLS12]));
        let data = if tls12_data {
            Arc::new(build(&[&TLS12]))
        } else {
            control.clone()
        };

        (Tls { control, data }, fingerprint)
    }

    impl FakeFtp {
        pub fn spawn(options: Options) -> Self {
            let fs = Arc::new(Mutex::new(Fs {
                dirs: ["/".to_owned()].into_iter().collect(),
                ..Default::default()
            }));
            let commands = Arc::new(Mutex::new(Vec::new()));
            let sent_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let retr_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let reset_retr_at = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
            let reset_retr_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let reset_after_completion = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let tls = options
                .tls
                .then(|| tls_identity(options.abort_stor_after_tls_handshake));
            let certificate_fingerprint = tls.as_ref().map(|(_, fingerprint)| fingerprint.clone());

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();

            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopping = stop.clone();
            let thread = {
                let fs = fs.clone();
                let commands = commands.clone();
                let sent_bytes = sent_bytes.clone();
                let retr_count = retr_count.clone();
                let reset_retr_at = reset_retr_at.clone();
                let reset_retr_remaining = reset_retr_remaining.clone();
                let reset_after_completion = reset_after_completion.clone();
                std::thread::spawn(move || {
                    let mut sockets = Vec::new();
                    let mut threads = Vec::new();
                    while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream.set_nonblocking(false).unwrap();
                                stream
                                    .set_write_timeout(Some(Duration::from_secs(2)))
                                    .unwrap();
                                sockets.push(stream.try_clone().unwrap());
                                let fs = fs.clone();
                                let commands = commands.clone();
                                let sent_bytes = sent_bytes.clone();
                                let retr_count = retr_count.clone();
                                let reset_retr_at = reset_retr_at.clone();
                                let reset_retr_remaining = reset_retr_remaining.clone();
                                let reset_after_completion = reset_after_completion.clone();
                                let stop = stopping.clone();
                                let tls = tls.as_ref().map(|(tls, _)| Tls {
                                    control: tls.control.clone(),
                                    data: tls.data.clone(),
                                });
                                threads.push(std::thread::spawn(move || {
                                    let shutdown = stream.try_clone().unwrap();
                                    serve(
                                        stream,
                                        &fs,
                                        &commands,
                                        &options,
                                        tls,
                                        &stop,
                                        &sent_bytes,
                                        &retr_count,
                                        &reset_retr_at,
                                        &reset_retr_remaining,
                                        &reset_after_completion,
                                    );
                                    let _ = shutdown.shutdown(std::net::Shutdown::Both);
                                }));
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(error) => panic!("FTP accept failed: {error}"),
                        }
                    }
                    drop(listener);
                    for socket in sockets {
                        let _ = socket.shutdown(std::net::Shutdown::Both);
                    }
                    for thread in threads {
                        thread.join().unwrap();
                    }
                })
            };

            Self {
                addr,
                commands,
                sent_bytes,
                retr_count,
                reset_retr_at,
                reset_retr_remaining,
                reset_after_completion,
                certificate_fingerprint,
                fs,
                stop,
                thread: Some(thread),
            }
        }

        /// A server pre-seeded like a Valheim install, Gale-metadata
        /// absent: the layout a first deployment sees.
        pub fn valheim_host(options: Options) -> Self {
            let server = Self::spawn(options);
            server.seed_dir("/BepInEx");
            server.seed_dir("/BepInEx/config");
            server.seed_dir("/BepInEx/plugins");
            server.seed_dir("/BepInEx/core");
            server.seed_file("/BepInEx/config/BepInEx.cfg", b"[settings]\n");
            server
        }

        pub fn seed_dir(&self, path: &str) {
            self.fs.lock().unwrap().dirs.insert(normalize(path));
        }

        pub fn seed_file(&self, path: &str, bytes: &[u8]) {
            self.fs
                .lock()
                .unwrap()
                .files
                .insert(normalize(path), bytes.to_vec());
        }

        pub fn file(&self, path: &str) -> Option<Vec<u8>> {
            self.fs.lock().unwrap().files.get(&normalize(path)).cloned()
        }

        /// Break the control connection on the nth later RETR, either
        /// before or just after its 226 completion reply.
        pub fn reset_on_retr(&self, nth: usize, after_completion: bool) {
            self.reset_on_retrs(nth, 1, after_completion);
        }

        pub fn reset_on_retrs(&self, nth: usize, count: usize, after_completion: bool) {
            assert!(nth > 0);
            assert!(count > 0);
            use std::sync::atomic::Ordering::SeqCst;
            self.retr_count.store(0, SeqCst);
            self.reset_after_completion.store(after_completion, SeqCst);
            self.reset_retr_at.store(nth, SeqCst);
            self.reset_retr_remaining.store(count, SeqCst);
        }

        pub fn has_dir(&self, path: &str) -> bool {
            self.fs.lock().unwrap().dirs.contains(&normalize(path))
        }

        /// The fingerprint to pin as `trusted_certificate` when
        /// `Options::tls` is on.
        pub fn trusted_certificate(&self) -> Option<String> {
            self.certificate_fingerprint.clone()
        }

        /// Whether the client ever sent a command starting with
        /// `prefix` (e.g. `TYPE I`).
        pub fn saw(&self, prefix: &str) -> bool {
            self.commands
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with(prefix))
        }
    }

    impl Drop for FakeFtp {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let result = self.thread.take().unwrap().join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }

    fn accept_data(
        listener: TcpListener,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Option<TcpStream> {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !stop.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    return Some(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(_) => return None,
            }
        }
        None
    }

    fn send(writer: &mut impl Write, text: &str) -> bool {
        writer.write_all(format!("{text}\r\n").as_bytes()).is_ok()
    }

    /// The last space-separated argument, or the whole line when the
    /// command carries none.
    fn arg_of(line: &str) -> &str {
        line.split_once(' ').map_or("", |(_, arg)| arg).trim()
    }

    /// The server side of a TLS handshake driven to completion over
    /// `socket`. Returns the established connection, or `None` when the
    /// peer goes away mid-handshake.
    fn tls_accept(socket: &mut TcpStream, config: &Arc<ServerConfig>) -> Option<ServerConnection> {
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .ok()?;
        let mut conn = ServerConnection::new(config.clone()).ok()?;
        while conn.is_handshaking() {
            if conn.complete_io(socket).is_err() {
                return None;
            }
        }
        socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        Some(conn)
    }

    /// A cloneable handle over a TLS stream so the control channel can
    /// hold independent read and write ends.
    #[derive(Clone)]
    struct TlsSocket(Arc<Mutex<StreamOwned<ServerConnection, TcpStream>>>);

    impl Read for TlsSocket {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().read(buf)
        }
    }

    impl Write for TlsSocket {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }

    /// An accepted passive data connection: plain TCP, or TLS once the
    /// session negotiated `PROT P`.
    enum DataSocket {
        Plain(TcpStream),
        Tls(StreamOwned<ServerConnection, TcpStream>),
    }

    impl Read for DataSocket {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self {
                Self::Plain(stream) => stream.read(buf),
                Self::Tls(stream) => stream.read(buf),
            }
        }
    }

    impl Write for DataSocket {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                Self::Plain(stream) => stream.write(buf),
                Self::Tls(stream) => stream.write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self {
                Self::Plain(stream) => stream.flush(),
                Self::Tls(stream) => stream.flush(),
            }
        }
    }

    impl DataSocket {
        /// rustls does not emit `close_notify` on drop; without it the
        /// client's read reports `UnexpectedEof` after the payload.
        /// `write_tls` flushes the alert without reading — `flush` would
        /// block waiting for client data that never comes.
        fn close(self) {
            if let Self::Tls(mut conn) = self {
                conn.conn.send_close_notify();
                let _ = conn.conn.write_tls(&mut conn.sock);
            }
        }
    }

    fn serve(
        stream: TcpStream,
        fs: &Arc<Mutex<Fs>>,
        commands: &Arc<Mutex<Vec<String>>>,
        options: &Options,
        tls: Option<Tls>,
        stop: &std::sync::atomic::AtomicBool,
        sent_bytes: &std::sync::atomic::AtomicUsize,
        retr_count: &std::sync::atomic::AtomicUsize,
        reset_retr_at: &std::sync::atomic::AtomicUsize,
        reset_retr_remaining: &std::sync::atomic::AtomicUsize,
        reset_after_completion: &std::sync::atomic::AtomicBool,
    ) {
        // `socket` keeps a handle to the control connection so `AUTH TLS`
        // can upgrade it in place; the reader/writer work on clones until
        // then and on the TLS stream afterwards.
        let mut socket = Some(stream.try_clone().unwrap());
        let mut writer: Box<dyn Write> = Box::new(stream.try_clone().unwrap());
        let mut reader: Box<dyn BufRead> = Box::new(BufReader::new(stream));
        let mut passive: Option<TcpListener> = None;
        let mut cwd = "/".to_owned();
        let mut binary = false;
        let mut protected = false;
        let mut rename_from: Option<String> = None;
        // STOR data sockets kept open under `hold_stor_eof`; they are
        // dropped when the control session ends.
        let mut held_data: Vec<DataSocket> = Vec::new();
        let mut line = String::new();
        let connected_at = Instant::now();
        let mut transfers = 0usize;

        /// Opens the pending passive data connection, if any, and wraps
        /// it in TLS when the session negotiated `PROT P`.
        macro_rules! data {
            () => {
                passive
                    .take()
                    .and_then(|listener| accept_data(listener, stop))
                    .and_then(|mut stream| match (protected, tls.as_ref()) {
                        (true, Some(tls)) => tls_accept(&mut stream, &tls.data)
                            .map(|conn| DataSocket::Tls(StreamOwned::new(conn, stream))),
                        _ => Some(DataSocket::Plain(stream)),
                    })
            };
        }

        /// The client connects the passive data socket before reading
        /// the command's final response, so a refused command must still
        /// accept and close it — a rustls data stream that is dropped
        /// before its handshake runs `complete_io` and waits on a
        /// channel that was never accepted.
        macro_rules! reap_data {
            () => {
                if let Some(listener) = passive.take() {
                    let _ = accept_data(listener, stop);
                }
            };
        }

        if !send(&mut writer, "220 fake ftp ready") {
            return;
        }

        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let command = line.trim_end().to_owned();
            commands.lock().unwrap().push(command.clone());
            if options
                .expire_control_after
                .is_some_and(|age| connected_at.elapsed() >= age)
                || options
                    .expire_control_after_transfers
                    .is_some_and(|limit| transfers >= limit)
            {
                return;
            }
            let verb = command
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            let path = resolve(&cwd, arg_of(&command));

            let response = match verb.as_str() {
                "AUTH" => match (tls.as_ref(), arg_of(&command).to_ascii_uppercase().as_str()) {
                    (Some(tls), "TLS" | "SSL") => {
                        if !send(&mut writer, "234 AUTH TLS successful") {
                            return;
                        }
                        let Some(mut sock) = socket.take() else {
                            return;
                        };
                        let Some(conn) = tls_accept(&mut sock, &tls.control) else {
                            return;
                        };
                        // Idle control channels live until fixture shutdown; data
                        // transfers retain their bounded read timeout.
                        sock.set_read_timeout(None).unwrap();
                        let secure = TlsSocket(Arc::new(Mutex::new(StreamOwned::new(conn, sock))));
                        reader = Box::new(BufReader::new(secure.clone()));
                        writer = Box::new(secure);
                        continue;
                    }
                    _ => "502 TLS is not supported".to_owned(),
                },
                "PBSZ" => "200 PBSZ=0".to_owned(),
                "PROT" => {
                    protected = arg_of(&command).eq_ignore_ascii_case("P");
                    "200 protection level set".to_owned()
                }
                "USER" => "331 password required".to_owned(),
                "PASS" => "230 logged in".to_owned(),
                "SYST" => "215 UNIX Type: L8".to_owned(),
                "TYPE" => match arg_of(&command).to_ascii_uppercase().as_str() {
                    "I" => {
                        binary = true;
                        "200 binary mode".to_owned()
                    }
                    "A" => {
                        binary = false;
                        "200 ascii mode".to_owned()
                    }
                    _ => "504 unsupported type".to_owned(),
                },
                "MODE" | "STRU" | "NOOP" | "OPTS" => "200 ok".to_owned(),
                "PWD" | "XPWD" => format!("257 \"{cwd}\" is the current directory"),
                "CWD" => {
                    if fs.lock().unwrap().dirs.contains(&path) {
                        cwd = path;
                        "250 directory changed".to_owned()
                    } else {
                        "550 no such directory".to_owned()
                    }
                }
                "CDUP" => {
                    cwd = resolve(&cwd, "..");
                    "250 directory changed".to_owned()
                }
                "PASV" => {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    let port = listener.local_addr().unwrap().port();
                    passive = Some(listener);
                    format!(
                        "227 Entering Passive Mode (127,0,0,1,{},{})",
                        port / 256,
                        port % 256
                    )
                }
                "MLST" => {
                    if options.no_mlst {
                        "502 MLST not implemented".to_owned()
                    } else {
                        let fs = fs.lock().unwrap();
                        // RFC 3659 multi-line form: the facts line is what
                        // `mlst` returns to the caller.
                        let facts = if let Some(bytes) = fs.files.get(&path) {
                            Some(format!(
                                " type=file;size={};modify=20240101000000; {path}",
                                bytes.len()
                            ))
                        } else if fs.dirs.contains(&path) {
                            Some(format!(" type=dir;modify=20240101000000; {path}"))
                        } else {
                            None
                        };
                        match facts {
                            None => "550 no such file or directory".to_owned(),
                            Some(facts) => {
                                if !send(&mut writer, "250-MLST") || !send(&mut writer, &facts) {
                                    return;
                                }
                                "250 End".to_owned()
                            }
                        }
                    }
                }
                "SIZE" => {
                    if options.refuse_size || (options.size_requires_binary && !binary) {
                        "550 SIZE not allowed in ASCII mode.".to_owned()
                    } else {
                        match fs.lock().unwrap().files.get(&path) {
                            Some(bytes) => format!("213 {}", bytes.len()),
                            None => "550 is not retrievable".to_owned(),
                        }
                    }
                }
                "MDTM" => {
                    if fs.lock().unwrap().files.contains_key(&path) {
                        "213 20240101000000".to_owned()
                    } else {
                        "550 is not retrievable".to_owned()
                    }
                }
                "RETR" => match fs.lock().unwrap().files.get(&path).cloned() {
                    Some(_) if options.refuse_retr => {
                        reap_data!();
                        "550 refused".to_owned()
                    }
                    Some(bytes) => {
                        transfers += 1;
                        if !send(&mut writer, "150 opening data connection") {
                            return;
                        }
                        if let Some(mut data) = data!() {
                            for chunk in bytes.chunks(4096) {
                                if data.write_all(chunk).is_err() {
                                    break;
                                }
                                sent_bytes
                                    .fetch_add(chunk.len(), std::sync::atomic::Ordering::SeqCst);
                            }
                            data.close();
                        }
                        let sequence =
                            retr_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if sequence >= reset_retr_at.load(std::sync::atomic::Ordering::SeqCst)
                            && reset_retr_remaining
                                .fetch_update(
                                    std::sync::atomic::Ordering::SeqCst,
                                    std::sync::atomic::Ordering::SeqCst,
                                    |remaining| remaining.checked_sub(1),
                                )
                                .is_ok()
                        {
                            if reset_after_completion.load(std::sync::atomic::Ordering::SeqCst) {
                                let _ = send(&mut writer, "226 transfer complete");
                            }
                            return;
                        }
                        "226 transfer complete".to_owned()
                    }
                    None => {
                        reap_data!();
                        "550 no such file or directory".to_owned()
                    }
                },
                "STOR" => {
                    transfers += 1;
                    if !send(&mut writer, "150 opening data connection") {
                        return;
                    }
                    let mut bytes = Vec::new();
                    match data!() {
                        // The TLS 1.2 handshake completed inside `data!` —
                        // the server just sent the final flight, so the
                        // client is still finishing its side and has not
                        // written its payload yet. Resetting the socket now
                        // means the payload write lands on a dead
                        // connection: rustls swallows that error and defers
                        // it to the stream's next flush.
                        Some(DataSocket::Tls(conn)) if options.abort_stor_after_tls_handshake => {
                            let socket = tokio::net::TcpSocket::from_std_stream(conn.sock);
                            let _ = socket.set_zero_linger();
                            drop(socket);
                        }
                        Some(mut data) => {
                            let _ = data.read_to_end(&mut bytes);
                            // Withhold EOF: dropping `data` here would FIN
                            // the channel and complete the client's drain.
                            if options.hold_stor_eof {
                                held_data.push(data);
                            }
                        }
                        None => {}
                    }
                    if let Some(limit) = options.truncate_stor_to {
                        bytes.truncate(limit);
                    }
                    fs.lock().unwrap().files.insert(path.clone(), bytes);
                    "226 transfer complete".to_owned()
                }
                "LIST" | "NLST" => {
                    let lines = {
                        let fs = fs.lock().unwrap();
                        if !fs.dirs.contains(&path) {
                            None
                        } else {
                            Some(
                                fs.children(&path)
                                    .into_iter()
                                    .map(|(name, is_dir)| {
                                        if verb == "NLST" {
                                            format!("{name}\r\n")
                                        } else {
                                            let child = normalize(&format!(
                                                "{}/{}",
                                                path.trim_end_matches('/'),
                                                name
                                            ));
                                            let (perm, size) = if is_dir {
                                                ("drwxrwxrwx", 4096)
                                            } else {
                                                (
                                                    "-rw-rw-rw-",
                                                    fs.files.get(&child).map_or(0, Vec::len),
                                                )
                                            };
                                            format!(
                                                "{perm}   1 dathost  users {size:>8} Sep 09 12:01 {name}\r\n"
                                            )
                                        }
                                    })
                                    .collect::<Vec<_>>(),
                            )
                        }
                    };
                    match lines {
                        None => {
                            reap_data!();
                            "550 no such directory".to_owned()
                        }
                        Some(lines) => {
                            transfers += 1;
                            if !send(&mut writer, "150 opening data connection") {
                                return;
                            }
                            if let Some(mut data) = data!() {
                                for line in lines {
                                    let _ = data.write_all(line.as_bytes());
                                }
                                data.close();
                            }
                            "226 transfer complete".to_owned()
                        }
                    }
                }
                "RNFR" => {
                    if fs.lock().unwrap().exists(&path) {
                        rename_from = Some(path.clone());
                        "350 ready for destination".to_owned()
                    } else {
                        rename_from = None;
                        "550 no such file".to_owned()
                    }
                }
                "RNTO" => match rename_from.take() {
                    Some(from) => {
                        let mut fs = fs.lock().unwrap();
                        if let Some(bytes) = fs.files.remove(&from) {
                            fs.files.insert(path.clone(), bytes);
                        } else if fs.dirs.remove(&from) {
                            fs.dirs.insert(path.clone());
                        }
                        "250 renamed".to_owned()
                    }
                    None => "503 RNFR first".to_owned(),
                },
                "DELE" => {
                    if options.refuse_dele {
                        "550 refused".to_owned()
                    } else if fs.lock().unwrap().files.remove(&path).is_some() {
                        "250 deleted".to_owned()
                    } else {
                        "550 no such file".to_owned()
                    }
                }
                "MKD" => {
                    let mut fs = fs.lock().unwrap();
                    let parent = resolve("/", &format!("{}/..", path));
                    if fs.exists(&path) || !fs.dirs.contains(&parent) {
                        "550 unavailable".to_owned()
                    } else {
                        fs.dirs.insert(path.clone());
                        format!("257 \"{path}\" created")
                    }
                }
                "RMD" => {
                    let mut fs = fs.lock().unwrap();
                    let empty = fs.children(&path).is_empty();
                    if fs.dirs.contains(&path) && empty {
                        fs.dirs.remove(&path);
                        "250 removed".to_owned()
                    } else {
                        "550 unavailable".to_owned()
                    }
                }
                "QUIT" => {
                    let _ = send(&mut writer, "221 goodbye");
                    return;
                }
                _ => "502 not implemented".to_owned(),
            };

            if !send(&mut writer, &response) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use suppaftp::rustls::{
        RootCertStore, SignatureScheme,
        client::danger::ServerCertVerifier,
        pki_types::{CertificateDer, ServerName, UnixTime},
    };
    use suppaftp::{FtpError, Status, types::Response};

    use super::{
        ConnectionAttempt, FtpsCertVerifier, RemoteClient, RemoteConnection, RemoteOps,
        RemoteProtocol, RemoteServerSettings, allows_plaintext_ftp_fallback,
        certificate_fingerprint, ftp_list_entries, ftp_store, ftps_client_config, is_ftp_not_found,
        is_ftp_tls_unsupported,
    };
    use eyre::Result;

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

    /// Only automatic `ftp` mode may drop to plaintext when the server
    /// refuses AUTH TLS. A strict `ftps` selection must surface the
    /// failure instead of logging in unencrypted.
    #[test]
    fn strict_ftps_never_falls_back_to_plaintext() {
        let tls_refused = FtpError::UnexpectedResponse(Response::new(
            Status::from(502),
            b"502 AUTH TLS not implemented".to_vec(),
        ));
        let unrelated = FtpError::UnexpectedResponse(Response::new(
            Status::from(550),
            b"550 unrelated".to_vec(),
        ));

        let mut settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftp,
            ..Default::default()
        };
        assert!(allows_plaintext_ftp_fallback(&settings, &tls_refused));
        assert!(!allows_plaintext_ftp_fallback(&settings, &unrelated));

        settings.protocol = RemoteProtocol::Ftps;
        assert!(!allows_plaintext_ftp_fallback(&settings, &tls_refused));
    }

    /// The connector must initialize in a fresh process without a global provider.
    #[test]
    fn ftps_config_builds_without_a_process_default_provider() {
        const CHILD_ENV: &str = "GALE_FTPS_PROVIDER_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            assert!(suppaftp::rustls::crypto::CryptoProvider::get_default().is_none());
            let (verifier, _) = FtpsCertVerifier::new(None).unwrap();
            let _ = ftps_client_config(verifier);
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "profile::server::remote::tests::ftps_config_builds_without_a_process_default_provider"])
            .env(CHILD_ENV, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    // ---------- FTPS certificate verification ----------

    fn self_signed(name: &str) -> rcgen::CertifiedKey {
        rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap()
    }

    /// A CA plus a leaf it signed. webpki accepts the leaf when the CA is
    /// in the root store.
    fn ca_signed_leaf(name: &str) -> (RootCertStore, rcgen::CertifiedKey) {
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let leaf_params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();

        (
            roots,
            rcgen::CertifiedKey {
                cert: leaf_cert,
                key_pair: leaf_key,
            },
        )
    }

    fn verifier(
        mut roots: RootCertStore,
        pinned: Option<String>,
    ) -> (FtpsCertVerifier, Arc<std::sync::Mutex<Option<String>>>) {
        // WebPkiServerVerifier requires at least one trust anchor even
        // when a pin bypasses it, so add a throwaway CA to satisfy it.
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        roots
            .add(params.self_signed(&ca_key).unwrap().der().clone())
            .unwrap();
        FtpsCertVerifier::with_roots(roots, pinned).unwrap()
    }

    fn verify(
        verifier: &FtpsCertVerifier,
        cert: &CertificateDer<'static>,
        name: &str,
    ) -> Result<(), suppaftp::rustls::Error> {
        verifier
            .verify_server_cert(
                cert,
                &[],
                &ServerName::try_from(name.to_owned()).unwrap(),
                &[],
                UnixTime::now(),
            )
            .map(|_| ())
    }

    #[test]
    fn pinned_self_signed_cert_is_accepted() {
        // Explicit trust: an exact fingerprint pin accepts a certificate
        // CA validation would reject.
        let certified = self_signed("ftps.local");
        let pin = certificate_fingerprint(certified.cert.der());
        let (verifier, _) = verifier(RootCertStore::empty(), Some(pin));

        verify(&verifier, certified.cert.der(), "ftps.local").unwrap();
    }

    #[test]
    fn ca_valid_replacement_cannot_satisfy_a_pin() {
        // The certificate chains to a trusted CA, but it is not the pinned
        // one. A pin is exact-match, not "any CA-valid cert".
        let (roots, certified) = ca_signed_leaf("ftps.example.com");
        let other = self_signed("other");
        let (verifier, _) = verifier(roots, Some(certificate_fingerprint(other.cert.der())));

        assert!(verify(&verifier, certified.cert.der(), "ftps.example.com").is_err());
    }

    #[test]
    fn ca_valid_cert_is_accepted_without_a_pin() {
        let (roots, certified) = ca_signed_leaf("ftps.example.com");
        let (verifier, _) = verifier(roots, None);

        verify(&verifier, certified.cert.der(), "ftps.example.com").unwrap();
    }

    #[test]
    fn unpinned_self_signed_cert_is_rejected_and_observed() {
        let certified = self_signed("ftps.local");
        let (verifier, observed) = verifier(RootCertStore::empty(), None);

        assert!(verify(&verifier, certified.cert.der(), "ftps.local").is_err());
        // The fingerprint is surfaced so the user can make an explicit
        // trust decision.
        assert_eq!(
            observed.lock().unwrap().as_deref(),
            Some(certificate_fingerprint(certified.cert.der()).as_str())
        );
    }

    #[test]
    fn mismatched_pin_rejects_even_when_self_signed() {
        let certified = self_signed("ftps.local");
        let other = self_signed("other.local");
        let (verifier, _) = verifier(
            RootCertStore::empty(),
            Some(certificate_fingerprint(other.cert.der())),
        );

        assert!(verify(&verifier, certified.cert.der(), "ftps.local").is_err());
    }

    // ---------- real TLS handshakes through the verifier ----------

    use suppaftp::rustls::{
        ClientConfig, ClientConnection, ServerConfig, ServerConnection,
        pki_types::PrivatePkcs8KeyDer,
        server::ResolvesServerCert,
        sign::{CertifiedKey, Signer, SigningKey},
    };

    fn provider() -> Arc<suppaftp::rustls::crypto::CryptoProvider> {
        Arc::new(suppaftp::rustls::crypto::aws_lc_rs::default_provider())
    }

    fn client_config(verifier: FtpsCertVerifier) -> Arc<ClientConfig> {
        Arc::new(
            ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth(),
        )
    }

    fn client_config_tls12(verifier: FtpsCertVerifier) -> Arc<ClientConfig> {
        Arc::new(
            ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(&[&suppaftp::rustls::version::TLS12])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth(),
        )
    }

    fn server_config(certified: &rcgen::CertifiedKey) -> Arc<ServerConfig> {
        let key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        Arc::new(
            ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key.into())
                .unwrap(),
        )
    }

    /// A signing key that produces well-formed but wrong ECDSA signatures.
    /// The server presents a valid certificate but cannot prove possession
    /// of its private key.
    #[derive(Debug)]
    struct ForgedKey;

    impl SigningKey for ForgedKey {
        fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
            offered
                .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
                .then(|| Box::new(ForgedKey) as Box<dyn Signer>)
        }

        fn algorithm(&self) -> suppaftp::rustls::SignatureAlgorithm {
            suppaftp::rustls::SignatureAlgorithm::ECDSA
        }
    }

    impl Signer for ForgedKey {
        fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, suppaftp::rustls::Error> {
            // DER-shaped ECDSA signature (SEQUENCE of two INTEGERs) whose
            // scalars cannot be the real transcript signature.
            let mut signature = vec![0x30, 0x44, 0x02, 0x20];
            signature.extend_from_slice(&[7u8; 32]);
            signature.extend_from_slice(&[0x02, 0x20]);
            signature.extend_from_slice(&[9u8; 32]);
            Ok(signature)
        }

        fn scheme(&self) -> SignatureScheme {
            SignatureScheme::ECDSA_NISTP256_SHA256
        }
    }

    #[derive(Debug)]
    struct FixedResolver(Arc<CertifiedKey>);

    impl ResolvesServerCert for FixedResolver {
        fn resolve(
            &self,
            _client_hello: suppaftp::rustls::server::ClientHello<'_>,
        ) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    fn forged_server_config(certified: &rcgen::CertifiedKey) -> Arc<ServerConfig> {
        let key = Arc::new(CertifiedKey::new(
            vec![certified.cert.der().clone()],
            Arc::new(ForgedKey),
        ));
        Arc::new(
            ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(FixedResolver(key))),
        )
    }

    /// Pumps TLS records between an in-memory client and server until the
    /// handshake completes or a side errors.
    fn pump(
        client: &mut ClientConnection,
        server: &mut ServerConnection,
    ) -> Result<(), suppaftp::rustls::Error> {
        loop {
            while client.wants_write() {
                let mut buf = Vec::new();
                client.write_tls(&mut buf).unwrap();
                server.read_tls(&mut &buf[..]).unwrap();
            }
            server.process_new_packets()?;
            while server.wants_write() {
                let mut buf = Vec::new();
                server.write_tls(&mut buf).unwrap();
                client.read_tls(&mut &buf[..]).unwrap();
            }
            client.process_new_packets()?;
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
            assert!(
                client.wants_write() || server.wants_write(),
                "handshake stalled"
            );
        }
    }

    fn handshake(
        client_config: Arc<ClientConfig>,
        server_config: Arc<ServerConfig>,
        name: &str,
    ) -> Result<(), suppaftp::rustls::Error> {
        let mut client = ClientConnection::new(
            client_config,
            ServerName::try_from(name.to_owned()).unwrap(),
        )
        .unwrap();
        let mut server = ServerConnection::new(server_config).unwrap();
        pump(&mut client, &mut server)
    }

    #[test]
    fn pinned_cert_completes_a_real_tls_handshake() {
        // A pinned self-signed certificate, with a server holding its
        // private key, completes the handshake under both TLS versions.
        for config in [client_config, client_config_tls12] {
            let certified = self_signed("ftps.local");
            let pin = certificate_fingerprint(certified.cert.der());
            let (verifier, _) = verifier(RootCertStore::empty(), Some(pin));

            handshake(config(verifier), server_config(&certified), "ftps.local")
                .expect("pinned handshake failed");
        }
    }

    #[test]
    fn forged_handshake_signature_fails_even_when_cert_is_pinned() {
        // A pin must not turn into `HandshakeSignatureValid::assertion()`.
        // The certificate is pinned, but the server signs with garbage.
        // The handshake must fail, proving verify_tls1x_signature still does real crypto.
        for config in [client_config, client_config_tls12] {
            let certified = self_signed("ftps.local");
            let pin = certificate_fingerprint(certified.cert.der());
            let (verifier, _) = verifier(RootCertStore::empty(), Some(pin));

            assert!(
                handshake(
                    config(verifier),
                    forged_server_config(&certified),
                    "ftps.local"
                )
                .is_err(),
                "forged handshake signature was accepted"
            );
        }
    }

    #[test]
    fn unpinned_cert_fails_the_handshake() {
        let certified = self_signed("ftps.local");
        let (verifier, _) = verifier(RootCertStore::empty(), None);

        assert!(
            handshake(
                client_config(verifier),
                server_config(&certified),
                "ftps.local"
            )
            .is_err()
        );
    }

    // ---------- live-protocol tests against the in-memory FTP server ----------

    use super::fake_ftp::{FakeFtp, Options as FakeFtpOptions};
    use crate::profile::server::paths::RemotePathBuf;
    use crate::profile::server::settings::RemoteAuthentication;

    fn ftp_connect(server: &FakeFtp) -> RemoteConnection {
        let settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftp,
            host: "127.0.0.1".to_owned(),
            port: server.addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            ..Default::default()
        };
        match RemoteConnection::connect(&settings, "pw").unwrap() {
            ConnectionAttempt::Connected(conn) => conn,
            _ => panic!("plaintext fallback should connect to the fake"),
        }
    }

    fn ftps_connect(server: &FakeFtp) -> RemoteConnection {
        let settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftps,
            host: "127.0.0.1".to_owned(),
            port: server.addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            trusted_certificate: server.trusted_certificate(),
            ..Default::default()
        };
        match RemoteConnection::connect(&settings, "pw").unwrap() {
            ConnectionAttempt::Connected(conn) => {
                assert!(conn.encrypted, "FTPS connection must be encrypted");
                conn
            }
            _ => panic!("pinned certificate should connect to the fake over TLS"),
        }
    }

    fn remote_path(path: &str) -> RemotePathBuf {
        RemotePathBuf::new(path).unwrap()
    }

    #[test]
    fn ftps_renews_before_control_connection_expires_by_age() {
        let server = FakeFtp::spawn(FakeFtpOptions {
            tls: true,
            expire_control_after: Some(Duration::from_millis(250)),
            ..Default::default()
        });
        server.seed_file("/state.json", b"state");

        let mut expired = ftps_connect(&server);
        expired.ftp_max_age = Duration::from_secs(60);
        std::thread::sleep(Duration::from_millis(270));
        assert!(
            expired
                .read(remote_path("/state.json").as_path(), 64)
                .is_err()
        );
        drop(expired);

        let mut conn = ftps_connect(&server);
        conn.ftp_max_age = Duration::from_millis(100);
        std::thread::sleep(Duration::from_millis(270));

        assert_eq!(
            conn.read(remote_path("/state.json").as_path(), 64).unwrap(),
            Some(b"state".to_vec())
        );
        assert_eq!(
            server
                .commands
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.starts_with("USER "))
                .count(),
            3
        );
    }

    #[test]
    fn ftps_renews_before_control_connection_expires_by_transfer_count() {
        let server = FakeFtp::spawn(FakeFtpOptions {
            tls: true,
            expire_control_after_transfers: Some(4),
            ..Default::default()
        });
        server.seed_file("/state.json", b"state");

        let mut expired = ftps_connect(&server);
        expired.ftp_max_transfers = 100;
        for _ in 0..4 {
            assert_eq!(
                expired
                    .read(remote_path("/state.json").as_path(), 64)
                    .unwrap(),
                Some(b"state".to_vec())
            );
        }
        assert!(
            expired
                .read(remote_path("/state.json").as_path(), 64)
                .is_err()
        );
        drop(expired);

        let mut conn = ftps_connect(&server);
        conn.ftp_max_transfers = 4;
        for _ in 0..5 {
            assert_eq!(
                conn.read(remote_path("/state.json").as_path(), 64).unwrap(),
                Some(b"state".to_vec())
            );
        }
        assert_eq!(
            server
                .commands
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.starts_with("USER "))
                .count(),
            3
        );
    }

    #[test]
    fn ftp_reads_enforce_the_cap_without_size_metadata() {
        for tls in [false, true] {
            let server = FakeFtp::spawn(FakeFtpOptions {
                tls,
                refuse_size: true,
                no_mlst: true,
                ..Default::default()
            });
            // Valid JSON with trailing whitespace: parsing cannot mask a missing cap.
            let mut bytes = br#"{"version":1}"#.to_vec();
            bytes.resize(16 * 1024 * 1024, b' ');
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
            server.seed_file("/state.json", &bytes);
            let mut conn = if tls {
                ftps_connect(&server)
            } else {
                ftp_connect(&server)
            };
            let path = remote_path("/state.json");
            assert_eq!(conn.file_size(path.as_path()).unwrap(), None);
            let error = conn.read(path.as_path(), 64).unwrap_err();
            assert_eq!(
                error.to_string(),
                "remote file /state.json exceeds the 64-byte limit"
            );
            assert!(server.saw("RETR /state.json"));
            drop(conn);
            let sent = server.sent_bytes.clone();
            drop(server); // Join the transfer before measuring consumption.
            assert!(sent.load(std::sync::atomic::Ordering::SeqCst) < bytes.len());
        }
    }

    #[test]
    fn ftp_connection_survives_an_oversized_read() {
        let server = FakeFtp::spawn(FakeFtpOptions {
            refuse_size: true,
            no_mlst: true,
            ..Default::default()
        });
        server.seed_file("/oversized.bin", &vec![b'x'; 256 * 1024]);
        server.seed_file("/small.txt", b"still connected");
        let mut conn = ftp_connect(&server);

        let oversized = remote_path("/oversized.bin");
        let error = conn.read(oversized.as_path(), 64).unwrap_err();
        assert_eq!(
            error.to_string(),
            "remote file /oversized.bin exceeds the 64-byte limit"
        );

        assert!(conn.is_dir(remote_path("/").as_path()).unwrap());
        let small = remote_path("/small.txt");
        assert_eq!(
            conn.read(small.as_path(), 64).unwrap(),
            Some(b"still connected".to_vec())
        );
    }

    #[test]
    fn ftp_fixture_releases_listener_even_during_unwinding() {
        for unwind in [false, true] {
            let mut addr = None;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let server = FakeFtp::spawn(FakeFtpOptions::default());
                addr = Some(server.addr);
                let _idle_client = std::net::TcpStream::connect(server.addr).unwrap();
                if unwind {
                    panic!("deliberate fixture unwind");
                }
            }));
            assert_eq!(result.is_err(), unwind);
            let _listener = std::net::TcpListener::bind(addr.unwrap()).unwrap();
        }
    }

    /// The DatHost profile verified against the live server: `SIZE` is
    /// refused in ASCII mode (`550 SIZE not allowed in ASCII mode`),
    /// while dot-prefixed paths are fully visible to LIST/MLST/MDTM/RETR.
    /// Before the fix, `read` gated on `file_size`/`is_file`, so a
    /// retrievable file looked absent — the false "lease taken over".
    #[test]
    fn reads_stay_correct_when_size_is_refused_in_ascii() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            size_requires_binary: true,
            ..Default::default()
        });
        server.seed_dir("/BepInEx/config/.gale-deploy.lock");
        server.seed_file(
            "/BepInEx/config/.gale-deploy.lock/lease.json",
            b"{\"owner\":\"x\"}",
        );

        let mut conn = ftp_connect(&server);
        assert!(server.saw("TYPE I"), "binary mode must be negotiated");

        let lease = remote_path("/BepInEx/config/.gale-deploy.lock/lease.json");
        assert_eq!(
            conn.read(lease.as_path(), 64 * 1024).unwrap(),
            Some(b"{\"owner\":\"x\"}".to_vec())
        );
        assert!(conn.is_file(lease.as_path()).unwrap());
        assert_eq!(conn.file_size(lease.as_path()).unwrap(), Some(13));
        assert!(
            conn.is_dir(remote_path("/BepInEx/config/.gale-deploy.lock").as_path())
                .unwrap()
        );

        // Genuinely absent paths still read as absent — not confused
        // with the SIZE refusal.
        let absent = remote_path("/BepInEx/config/.gale-server-state.json");
        assert_eq!(conn.read(absent.as_path(), 64 * 1024).unwrap(), None);
        assert!(!conn.is_file(absent.as_path()).unwrap());
        assert_eq!(conn.file_size(absent.as_path()).unwrap(), None);

        // Listings include dot-prefixed entries.
        let entries = conn.list(remote_path("/BepInEx/config").as_path()).unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.name == ".gale-deploy.lock" && entry.is_directory)
        );
    }

    /// On a host without RFC 3659 the same probes fall back to the
    /// pre-3659 commands and still work once binary mode is on.
    #[test]
    fn reads_work_without_mlst() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            size_requires_binary: true,
            no_mlst: true,
            ..Default::default()
        });
        server.seed_file("/BepInEx/config/file.cfg", b"abc");

        let mut conn = ftp_connect(&server);
        let path = remote_path("/BepInEx/config/file.cfg");
        assert!(conn.is_file(path.as_path()).unwrap());
        assert_eq!(conn.file_size(path.as_path()).unwrap(), Some(3));
        assert_eq!(
            conn.read(path.as_path(), 64 * 1024).unwrap(),
            Some(b"abc".to_vec())
        );
        assert!(
            conn.is_dir(remote_path("/BepInEx/config").as_path())
                .unwrap()
        );
    }

    /// A host that refuses to RETR an existing file must produce an
    /// error, not a silently empty read: `Ok(None)` is reserved for
    /// confirmed absence.
    #[test]
    fn a_refused_read_is_not_silently_absent() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            refuse_retr: true,
            ..Default::default()
        });
        server.seed_file("/BepInEx/config/locked.dat", b"data");

        let mut conn = ftp_connect(&server);
        let refused = remote_path("/BepInEx/config/locked.dat");
        let err = conn.read(refused.as_path(), 64 * 1024).unwrap_err();
        assert!(
            format!("{err}").contains("refuses to return"),
            "expected a refusal error, got: {err}"
        );

        // Genuine absence still reads as absent on the same connection.
        let absent = remote_path("/BepInEx/config/missing.dat");
        assert_eq!(conn.read(absent.as_path(), 64 * 1024).unwrap(), None);
    }

    #[test]
    fn a_refused_deletion_is_not_silently_absent() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            refuse_dele: true,
            ..Default::default()
        });
        server.seed_file("/BepInEx/config/locked.dat", b"data");
        let mut conn = ftp_connect(&server);
        let err = conn
            .delete_file(remote_path("/BepInEx/config/locked.dat").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("refuses to delete"));
        assert!(
            !conn
                .delete_file(remote_path("/BepInEx/config/missing.dat").as_path())
                .unwrap()
        );
    }

    #[test]
    fn ftps_uploads_preserve_binary_payloads_across_record_boundaries() {
        // The fake TLS server leaves TLS 1.3 post-handshake tickets enabled;
        // every payload below therefore exercises the data-channel drain.
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            ..Default::default()
        });
        let mut conn = ftps_connect(&server);

        for (sequence, size) in [1usize, 8_191, 8_192, 8_193, 19_968, 32_769]
            .into_iter()
            .enumerate()
        {
            let bytes: Vec<u8> = (0..size)
                .map(|index| ((index * 131 + sequence * 17) % 251) as u8)
                .collect();
            let path = remote_path(&format!("/BepInEx/config/upload-{sequence}-{size}.bin"));

            conn.write(path.as_path(), &bytes).unwrap();

            let stored = server.file(path.as_str()).expect("uploaded file is absent");
            assert_eq!(stored.len(), bytes.len());
            assert_eq!(blake3::hash(&stored), blake3::hash(&bytes));
            assert_eq!(stored, bytes);
        }
    }

    /// Regression test for an FTPS data channel that dies after the TLS
    /// handshake: the fake completes the handshake, kills the socket
    /// before the payload lands, then still answers `226 transfer
    /// complete`. `write` must surface the aborted transfer instead of
    /// reporting success for bytes the server never stored.
    #[test]
    fn ftps_write_reports_an_aborted_data_transfer() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            abort_stor_after_tls_handshake: true,
            ..Default::default()
        });
        let mut conn = ftps_connect(&server);
        let path = remote_path("/BepInEx/config/state.tmp");
        let expected = vec![b'x'; 8 * 1024];

        let result = conn.write(path.as_path(), &expected);

        assert!(
            result.is_err(),
            "an aborted FTPS data transfer must not report success"
        );
        assert_ne!(
            server.file(path.as_str()).as_deref(),
            Some(expected.as_slice())
        );
    }

    /// Regression test for an FTPS server that accepts the payload but
    /// never closes its end of the data connection: the close handshake
    /// must fail once the drain bound elapses instead of stalling the
    /// deployment, so the temporary file is never promoted.
    #[test]
    fn ftps_upload_fails_when_the_server_withholds_eof() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            hold_stor_eof: true,
            ..Default::default()
        });
        let mut conn = ftps_connect(&server);
        let RemoteClient::Ftp(ftp) = &mut conn.client else {
            panic!("expected an FTP connection");
        };

        let payload = vec![b'x'; 8 * 1024];
        let error = ftp_store(
            ftp,
            "/BepInEx/config/state.tmp",
            &mut std::io::Cursor::new(payload.as_slice()),
            payload.len() as u64,
            std::time::Duration::from_millis(500),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("timed out"),
            "expected a drain timeout, got: {error}"
        );
        assert!(server.saw("STOR /BepInEx/config/state.tmp"));
    }

    /// Exercises Gale's production FTPS transport against a real server.
    ///
    /// The ignored test is run in CI or locally with a disposable ProFTPD
    /// endpoint. It deliberately crosses the 8 KiB copy boundary, performs
    /// consecutive transfers, and verifies downloaded bytes and hashes.
    #[test]
    #[ignore = "requires GALE_TEST_FTPS_* for a disposable FTPS server"]
    fn real_ftps_round_trips_varied_binary_payloads() {
        let host = std::env::var("GALE_TEST_FTPS_HOST").expect("GALE_TEST_FTPS_HOST is required");
        let port = std::env::var("GALE_TEST_FTPS_PORT")
            .unwrap_or_else(|_| "21".to_owned())
            .parse()
            .expect("GALE_TEST_FTPS_PORT must be a port number");
        let username =
            std::env::var("GALE_TEST_FTPS_USER").expect("GALE_TEST_FTPS_USER is required");
        let password =
            std::env::var("GALE_TEST_FTPS_PASSWORD").expect("GALE_TEST_FTPS_PASSWORD is required");
        let base = std::env::var("GALE_TEST_FTPS_DIR").unwrap_or_else(|_| "/".to_owned());
        let mut settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftps,
            host,
            port,
            username,
            server_directory: base.clone(),
            authentication: RemoteAuthentication::Password,
            ..Default::default()
        };

        let fingerprint = match RemoteConnection::connect(&settings, &password).unwrap() {
            ConnectionAttempt::CertificateUntrusted { fingerprint } => fingerprint,
            ConnectionAttempt::Connected(_) => {
                panic!("test server certificate should be untrusted")
            }
            ConnectionAttempt::HostKeyUntrusted { .. } => panic!("FTPS returned an SSH host key"),
        };
        settings.trusted_certificate = Some(fingerprint);
        let mut conn = match RemoteConnection::connect(&settings, &password).unwrap() {
            ConnectionAttempt::Connected(conn) => conn,
            _ => panic!("pinned FTPS certificate was not accepted"),
        };

        let sizes = [1usize, 8_191, 8_192, 8_193, 19_968, 1_048_613];
        for (sequence, size) in sizes.into_iter().enumerate() {
            let bytes: Vec<u8> = (0..size)
                .map(|index| ((index * 131 + sequence * 17) % 251) as u8)
                .collect();
            let path = remote_path(&format!(
                "{}/gale-ftps-{sequence}-{size}.bin",
                base.trim_end_matches('/')
            ));

            conn.write(path.as_path(), &bytes).unwrap();
            let stored = conn
                .read(path.as_path(), size as u64 + 1)
                .unwrap()
                .expect("uploaded file is absent");

            assert_eq!(stored.len(), size, "server stored the wrong length");
            assert_eq!(blake3::hash(&stored), blake3::hash(&bytes));
            assert_eq!(stored, bytes, "server stored different bytes");
            assert!(conn.delete_file(path.as_path()).unwrap());
        }
    }
}

/// Read-only diagnostic for a live FTP server, used to characterize
/// hosts that filter dot-prefixed paths. Distinguishes "absent" from
/// "present but refused" across LIST/NLST/MLSD/MLST/SIZE/MDTM/RETR/CWD
/// and across absolute versus CWD-relative paths. Never issues a
/// mutating command (no STOR/MKD/RMD/DELE/RNTO).
///
/// The password comes from the OS credential store under the same
/// service/account Gale writes and is never printed.
///
///     $env:GALE_FTP_PROBE = "1"
///     $env:GALE_PROBE_PROFILE_ID = "6"
///     $env:GALE_PROBE_HOST = "<host>"
///     $env:GALE_PROBE_USER = "<ftp username>"
///     $env:GALE_PROBE_BASE = "/BepInEx/config"   # probe dir; default shown
///     cargo run --features diagnostics --example ftp-probe
#[cfg(feature = "diagnostics")]
pub fn ftp_probe_dotpath_visibility() -> Result<()> {
    use crate::profile::server::settings::RemoteAuthentication;
    use eyre::bail;

    if std::env::var("GALE_FTP_PROBE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set GALE_FTP_PROBE=1 and GALE_PROBE_* vars");
        return Ok(());
    }

    let profile_id =
        std::env::var("GALE_PROBE_PROFILE_ID").expect("GALE_PROBE_PROFILE_ID required");
    let host = std::env::var("GALE_PROBE_HOST").expect("GALE_PROBE_HOST required");
    let user = std::env::var("GALE_PROBE_USER").expect("GALE_PROBE_USER required");
    let dir = std::env::var("GALE_PROBE_DIR").unwrap_or_else(|_| "/".to_owned());
    let port = std::env::var("GALE_PROBE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(21u16);

    let password = keyring::Entry::new(
        "com.kesomannen.gale.dedicated-server",
        &format!("profile-{profile_id}-ftp-password"),
    )?
    .get_password()?;

    let mut settings = RemoteServerSettings {
        protocol: RemoteProtocol::Ftps,
        host,
        port,
        username: user,
        server_directory: dir,
        authentication: RemoteAuthentication::Password,
        ..Default::default()
    };

    let mut conn = match RemoteConnection::connect(&settings, &password)? {
        ConnectionAttempt::Connected(conn) => conn,
        ConnectionAttempt::CertificateUntrusted { fingerprint } => {
            eprintln!("[probe] pinning observed certificate fingerprint {fingerprint}");
            settings.trusted_certificate = Some(fingerprint);
            match RemoteConnection::connect(&settings, &password)? {
                ConnectionAttempt::Connected(conn) => conn,
                _ => bail!("certificate still untrusted after pinning"),
            }
        }
        ConnectionAttempt::HostKeyUntrusted { .. } => {
            bail!("unexpected host-key prompt on an FTP connection")
        }
    };

    eprintln!("[probe] connected encrypted={}", conn.encrypted);

    let ftp = match &mut conn.client {
        RemoteClient::Ftp(ftp) => ftp,
        RemoteClient::Sftp { .. } => bail!("probe only supports FTP connections"),
    };

    macro_rules! probe {
        ($label:expr, $op:expr) => {
            match $op {
                Ok(value) => eprintln!("[probe] {:<58} OK   {:?}", $label, value),
                Err(error) => eprintln!("[probe] {:<58} ERR  {error}", $label),
            }
        };
    }

    let config_dir =
        std::env::var("GALE_PROBE_BASE").unwrap_or_else(|_| "/BepInEx/config".to_owned());
    let dot_state = format!("{config_dir}/.gale-server-state.json");
    let plain_state = format!("{config_dir}/gale-server-state.json");
    let dot_lock = format!("{config_dir}/.gale-deploy.lock");
    let dot_lease = format!("{dot_lock}/lease.json");

    probe!("SIZE lease.json (ascii)", ftp.size(&dot_lease));
    probe!(
        "TYPE I",
        ftp.transfer_type(suppaftp::types::FileType::Binary)
    );
    probe!("SIZE lease.json (binary)", ftp.size(&dot_lease));
    probe!("SIZE dot-state (binary)", ftp.size(&dot_state));

    probe!("PWD", ftp.pwd());
    probe!("LIST /", ftp.list(Some("/")));
    probe!("NLST /", ftp.nlst(Some("/")));
    probe!("MLSD /", ftp.mlsd(Some("/")));

    probe!("LIST base", ftp.list(Some(&config_dir)));
    probe!("NLST base", ftp.nlst(Some(&config_dir)));
    probe!("MLSD base", ftp.mlsd(Some(&config_dir)));

    probe!("MLST dot-state", ftp.mlst(Some(&dot_state)));
    probe!("SIZE dot-state", ftp.size(&dot_state));
    probe!("MDTM dot-state", ftp.mdtm(&dot_state));
    probe!("MLST plain-state", ftp.mlst(Some(&plain_state)));
    probe!("SIZE plain-state", ftp.size(&plain_state));

    probe!("MLST dot-lock", ftp.mlst(Some(&dot_lock)));
    probe!("CWD dot-lock (dir probe)", ftp.cwd(&dot_lock));
    probe!("LIST dot-lock", ftp.list(Some(&dot_lock)));
    probe!("NLST dot-lock", ftp.nlst(Some(&dot_lock)));
    probe!("MLSD dot-lock", ftp.mlsd(Some(&dot_lock)));

    probe!("MLST lease.json", ftp.mlst(Some(&dot_lease)));
    probe!("SIZE lease.json", ftp.size(&dot_lease));
    probe!("MDTM lease.json", ftp.mdtm(&dot_lease));
    probe!("RETR lease.json", {
        ftp.retr_as_buffer(&dot_lease).map(|b| b.into_inner().len())
    });
    probe!("RETR dot-state", {
        ftp.retr_as_buffer(&dot_state).map(|b| b.into_inner().len())
    });

    probe!("CWD base", ftp.cwd(&config_dir));
    probe!("LIST (cwd=config)", ftp.list(None));
    probe!("NLST (cwd=config)", ftp.nlst(None));
    probe!("MLSD (cwd=config)", ftp.mlsd(None));
    probe!(
        "MLST .gale-server-state.json (rel)",
        ftp.mlst(Some(".gale-server-state.json"))
    );
    probe!("SIZE .gale-server-state.json (rel)", {
        ftp.size(".gale-server-state.json")
    });
    probe!("MLST .gale-deploy.lock (rel)", {
        ftp.mlst(Some(".gale-deploy.lock"))
    });
    probe!("CWD .. (restore)", ftp.cwd(".."));

    Ok(())
}
