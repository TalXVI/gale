//! File access on the remote server, over SFTP or FTP(S).

use std::{
    io::Write,
    net::{TcpStream, ToSocketAddrs},
    ops::DerefMut,
    path::Path,
    sync::Arc,
    time::Duration,
};

use eyre::{Context, Result, bail, ensure};
use serde::Serialize;

use self::{ftp::FtpConnection, sftp::SftpConnection};
use super::{
    paths::RemotePath,
    settings::{RemoteProtocol, TransportSettings},
};

#[cfg(test)]
pub(crate) mod fake_ftp;
mod ftp;
mod ftps_verifier;
#[cfg(test)]
pub(crate) mod memory;
mod sftp;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on any single blocking read or write. A peer that stays silent
/// this long is treated as stalled: the operation fails instead of holding
/// the deployment (and the worker's operation lock) forever.
const IO_TIMEOUT: Duration = Duration::from_millis(15_000);

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
    Connected(Box<dyn RemoteOps>),
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
    /// Streams the file at `path` into `sink`, bounded to `listed_size`
    /// bytes from the listing that supplied it. `Ok(false)` means the file
    /// is absent; `Ok(true)` means bytes were written (fewer than
    /// `listed_size` if the file shrank. The caller detects divergence
    /// through the hash). A file larger than its listed size is an error.
    ///
    /// The default implementation buffers through [`Self::read`]:
    /// semantically identical, just without streaming. Transports override
    /// it to skip the metadata probe because the listing reported the size,
    /// and to hash the bytes as they arrive.
    fn read_listed(
        &mut self,
        path: &RemotePath,
        listed_size: u64,
        sink: &mut dyn Write,
    ) -> Result<bool> {
        match self.read(path, listed_size)? {
            Some(bytes) => {
                sink.write_all(&bytes)
                    .with_context(|| format!("failed to consume remote file {path}"))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    /// A factory for extra read-only connections used by snapshot scanning
    /// and verification. `None` means this transport cannot open helpers,
    /// the snapshot falls back to the authoritative connection.
    fn reader_connector(&self) -> Option<ReaderConnector> {
        None
    }
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
    /// Creates the directory `path` unless it already exists. Missing
    /// parents are not created.
    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()>;
    /// Atomically creates `path` as a directory: `true` when created,
    /// `false` when it already existed. The deployment lease uses it
    /// because `MKD`/mkdir is atomic on both FTP and SFTP servers.
    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool>;
    /// Re-establishes the underlying transport after an error.
    fn reconnect(&mut self) -> Result<()>;
}

/// Read-only operations used by snapshot helper connections. No mutating
/// methods exist, so a helper connection cannot change the server.
pub trait RemoteReader: Send {
    /// See [`RemoteOps::list`].
    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>>;
    /// See [`RemoteOps::read_listed`].
    fn read_listed(
        &mut self,
        path: &RemotePath,
        listed_size: u64,
        sink: &mut dyn Write,
    ) -> Result<bool>;
    /// See [`RemoteOps::reconnect`].
    fn reconnect(&mut self) -> Result<()>;
}

/// Opens another read-only connection to the same server.
pub type ReaderConnector = Arc<dyn Fn() -> Result<Box<dyn RemoteReader>> + Send + Sync>;

/// Exposes only the read-only subset of a [`RemoteOps`]. Wraps anything
/// that dereferences to one: an owned `Box<dyn RemoteOps>` for helper
/// connections, or `&mut dyn RemoteOps` to lend the authoritative
/// connection to the serial fallback path.
pub struct ReadOnly<T>(pub T);

impl<T, R> RemoteReader for ReadOnly<T>
where
    T: DerefMut<Target = R> + Send,
    R: RemoteOps + ?Sized,
{
    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        self.0.list(dir)
    }

    fn read_listed(
        &mut self,
        path: &RemotePath,
        listed_size: u64,
        sink: &mut dyn Write,
    ) -> Result<bool> {
        self.0.read_listed(path, listed_size, sink)
    }

    fn reconnect(&mut self) -> Result<()> {
        self.0.reconnect()
    }
}

pub struct RemoteEntry {
    pub name: String,
    pub is_directory: bool,
    /// File size where the listing reports it (always present for SFTP).
    pub size: Option<u64>,
}

/// Connects and authenticates against the remote server.
///
/// `settings` is expected to have passed
/// [`TransportSettings::validate`] already; this is the
/// transport layer, so it only parses the paths it actually needs.
pub fn connect(settings: &TransportSettings, password: &str) -> Result<ConnectionAttempt> {
    Ok(match settings.protocol {
        RemoteProtocol::Sftp => match SftpConnection::connect(settings, password)? {
            Reached::Trusted(connection) => ConnectionAttempt::Connected(Box::new(connection)),
            Reached::Untrusted { fingerprint } => {
                ConnectionAttempt::HostKeyUntrusted { fingerprint }
            }
        },
        RemoteProtocol::Ftp | RemoteProtocol::Ftps => {
            match FtpConnection::connect(settings, password)? {
                Reached::Trusted(connection) => ConnectionAttempt::Connected(Box::new(connection)),
                Reached::Untrusted { fingerprint } => {
                    ConnectionAttempt::CertificateUntrusted { fingerprint }
                }
            }
        }
    })
}

pub fn test_connection(
    settings: &TransportSettings,
    password: &str,
) -> Result<ConnectionTestResult> {
    match settings.protocol {
        RemoteProtocol::Sftp => SftpConnection::test(settings, password),
        RemoteProtocol::Ftp | RemoteProtocol::Ftps => FtpConnection::test(settings, password),
    }
}

fn inaccessible_directory(settings: &TransportSettings) -> String {
    format!(
        "connected successfully, but server directory '{}' could not be accessed",
        settings.server_directory
    )
}

/// The outcome of reaching a server: a connection, or the fingerprint the
/// user has to trust before any credentials are sent.
enum Reached<C> {
    Trusted(C),
    Untrusted { fingerprint: String },
}

impl<C> Reached<C> {
    /// The connection, failing when trust would have to be asked for.
    /// Reconnects and helper connections have no user to ask.
    fn trusted(self, while_doing: &str) -> Result<C> {
        match self {
            Self::Trusted(connection) => Ok(connection),
            Self::Untrusted { .. } => {
                bail!("server trust verification failed while {while_doing}");
            }
        }
    }
}

/// Snapshot helpers are full connections to the same server, opened with
/// the same settings; each keeps its own renewal and reconnect handling.
/// Trust prompts fail the helper rather than silently sending credentials.
fn connector_for<C: RemoteOps + 'static>(
    settings: &TransportSettings,
    password: &str,
    connect: fn(&TransportSettings, &str) -> Result<Reached<C>>,
) -> ReaderConnector {
    let settings = settings.clone();
    let password = password.to_owned();
    Arc::new(move || {
        let connection = connect(&settings, &password)?.trusted("opening a snapshot reader")?;
        Ok(Box::new(ReadOnly(Box::new(connection))) as Box<dyn RemoteReader>)
    })
}

/// A transport that streams file contents, which gives it
/// [`RemoteOps::read`] and [`RemoteOps::read_listed`].
trait Retrieve: RemoteOps {
    /// Streams at most `limit + 1` bytes of `path` into `sink` and returns
    /// how many arrived, or `None` when the file is absent. Reading one byte
    /// past `limit` lets callers tell an oversized file from an exact fit.
    fn retrieve(
        &mut self,
        path: &RemotePath,
        limit: u64,
        sink: &mut dyn Write,
    ) -> Result<Option<u64>>;

    /// See [`RemoteOps::read`].
    fn read_capped(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
        // A concrete size lets oversized downloads be refused before the
        // transfer starts. When the server cannot report one, `SIZE` is
        // refused in FTP ASCII mode on some hosts, and an absent file
        // reports the same way. The transfer itself decides existence
        // and the cap is enforced on the received bytes.
        if let Some(size) = self.file_size(path)? {
            ensure!(
                size <= max,
                "remote file {path} is {size} bytes, exceeding the {max}-byte limit"
            );
        }

        let mut bytes = Vec::new();
        if self.retrieve(path, max, &mut bytes)?.is_none() {
            return Ok(None);
        }
        ensure!(
            bytes.len() as u64 <= max,
            "remote file {path} exceeds the {max}-byte limit"
        );
        Ok(Some(bytes))
    }

    /// The streaming payload read used by snapshot verification. Unlike
    /// [`Self::read_capped`] it does not probe the file first. The listing
    /// that selected this file already reported its size, and it hashes the
    /// data channel straight into `sink` instead of a buffer.
    fn read_within_listing(
        &mut self,
        path: &RemotePath,
        listed_size: u64,
        sink: &mut dyn Write,
    ) -> Result<bool> {
        let Some(copied) = self.retrieve(path, listed_size, sink)? else {
            return Ok(false);
        };
        // A file that grew past its listing since the snapshot was taken
        // is remote drift, not a truncated download.
        ensure!(
            copied <= listed_size,
            "remote file {path} grew past its listed {listed_size} bytes"
        );
        Ok(true)
    }
}

/// Connects to the first reachable address of the configured host, with
/// reads and writes bounded by `io_timeout`.
fn connect_socket(settings: &TransportSettings, io_timeout: Duration) -> Result<TcpStream> {
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
                bound_io(&stream, io_timeout)
                    .with_context(|| format!("failed to configure the connection to {address}"))?;
                return Ok(stream);
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
    );
}

fn bound_io(stream: &TcpStream, timeout: Duration) -> std::io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}
