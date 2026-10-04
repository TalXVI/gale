//! FTP transport, upgraded to TLS (FTPS) where the server supports it.

use std::{
    fs::File,
    io::{Cursor, ErrorKind, Read, Write},
    net::{Shutdown, TcpStream},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use eyre::{Context, Result, bail, ensure};
use suppaftp::{
    DataStream, FtpError, RustlsConnector, RustlsFtpStream, Status, TlsStream,
    rustls::{ClientConnection, StreamOwned},
    types::FileType,
};
use tracing::{debug, info, warn};

use super::{
    CONNECT_TIMEOUT, ConnectionTestResult, IO_TIMEOUT, Reached, ReaderConnector, RemoteEntry,
    RemoteOps, Retrieve, bound_io, connect_socket, connector_for,
    ftps_verifier::{FtpsCertVerifier, ftps_client_config},
    inaccessible_directory,
};
use crate::profile::server::{
    paths::RemotePath,
    settings::{RemoteProtocol, TransportSettings},
};

/// Bound on draining an upload's data channel to EOF after the TLS/TCP
/// close handshake. A healthy server closes its side as soon as the
/// client's FIN arrives, so exceeding this means the peer is holding the
/// channel open. The upload must fail rather than stall the deployment.
const FTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Live failures begin near 40 seconds or 170 transfers on one control
/// connection. Retire it before either observed boundary.
const FTP_MAX_CONNECTION_AGE: Duration = Duration::from_secs(30);
const FTP_MAX_TRANSFERS: u64 = 100;

pub(super) struct FtpConnection {
    ftp: RustlsFtpStream,
    settings: TransportSettings,
    password: String,
    encrypted: bool,
    connected_at: Instant,
    transfer_count: u64,
}

impl FtpConnection {
    /// Connects and logs in, unless the server presents a certificate that
    /// is not trusted.
    pub(super) fn connect(settings: &TransportSettings, password: &str) -> Result<Reached<Self>> {
        ensure!(!password.is_empty(), "FTP password is required");

        let stream = connect_ftp_control(settings)?;

        let (verifier, observed) = FtpsCertVerifier::new(settings.trusted_certificate.clone())?;
        let connector = ftps_client_config(verifier);

        let (mut ftp, encrypted) = match stream.into_secure(
            RustlsConnector::from(Arc::new(connector)),
            settings.host.trim(),
        ) {
            Ok(stream) => (stream, true),
            Err(error) => {
                if let Some(fingerprint) = observed.lock().unwrap().take() {
                    return Ok(Reached::Untrusted { fingerprint });
                }
                // Plain FTP servers reject AUTH TLS entirely; reconnect and
                // stay unencrypted instead of failing.
                ensure!(
                    allows_plaintext_ftp_fallback(settings, &error),
                    "FTPS TLS negotiation failed: {error}"
                );
                warn!(
                    host = settings.host.trim(),
                    "FTP server does not support TLS; continuing unencrypted"
                );
                (connect_ftp_control(settings)?, false)
            }
        };

        ftp.login(settings.username.trim(), password)
            .context("FTP authentication failed")?;

        // Binary mode is required for byte-exact transfers. ASCII mode
        // may newline-mangle payloads, and some servers (e.g. DatHost's
        // ProFTPD) refuse SIZE entirely while in ASCII mode, which would
        // make existing files look absent.
        ftp.transfer_type(FileType::Binary)
            .context("FTP server refused binary transfer mode")?;

        Ok(Reached::Trusted(Self {
            ftp,
            settings: settings.clone(),
            password: password.to_owned(),
            encrypted,
            connected_at: Instant::now(),
            transfer_count: 0,
        }))
    }

    /// Connects and checks that the server directory can be entered.
    pub(super) fn test(
        settings: &TransportSettings,
        password: &str,
    ) -> Result<ConnectionTestResult> {
        let mut connection = match Self::connect(settings, password)? {
            Reached::Trusted(connection) => connection,
            Reached::Untrusted { fingerprint } => {
                return Ok(ConnectionTestResult::CertificateUntrusted { fingerprint });
            }
        };
        let directory = settings.server_directory()?;
        connection
            .check_directory(&directory)
            .with_context(|| inaccessible_directory(settings))?;

        Ok(ConnectionTestResult::Connected {
            fingerprint: None,
            encrypted: connection.encrypted,
        })
    }

    fn check_directory(&mut self, path: &RemotePath) -> Result<()> {
        let original = self.ftp.pwd()?;
        self.ftp.cwd(path.as_str())?;
        self.ftp.cwd(original)?;
        Ok(())
    }

    /// Replaces the control connection before the server would drop it.
    fn renew_if_needed(&mut self) -> Result<()> {
        if self.connected_at.elapsed() >= FTP_MAX_CONNECTION_AGE
            || self.transfer_count >= FTP_MAX_TRANSFERS
        {
            self.reconnect()
                .context("failed to renew FTP control connection")?;
        }
        Ok(())
    }
}

impl Retrieve for FtpConnection {
    fn retrieve(
        &mut self,
        path: &RemotePath,
        limit: u64,
        sink: &mut dyn Write,
    ) -> Result<Option<u64>> {
        self.renew_if_needed()?;
        self.transfer_count += 1;
        let transfer_count = self.transfer_count;
        let connection_age_ms = self.connected_at.elapsed().as_millis();
        let copied = match self.ftp.retr_as_stream(path.as_str()) {
            Ok(mut stream) => {
                let started = Instant::now();
                let read = std::io::copy(&mut (&mut stream).take(limit.saturating_add(1)), sink);
                // The control channel owes a completion reply even
                // after a failed data read, so always finalize.
                let finalize = self.ftp.finalize_retr_stream(stream);
                if read.is_err() || finalize.is_err() {
                    warn!(command = "RETR", path = %path, transfer_count, connection_age_ms, transfer_duration_ms = started.elapsed().as_millis(), data_error = ?read.as_ref().err(), completion_error = ?finalize.as_ref().err(), "FTP transfer failed");
                }
                let copied =
                    read.with_context(|| format!("FTP RETR data read failed for {path}"))?;
                finalize.with_context(|| format!("FTP RETR completion failed for {path}"))?;
                debug!(command = "RETR", path = %path, transfer_count, connection_age_ms, transfer_duration_ms = started.elapsed().as_millis(), bytes = copied, "FTP transfer completed");
                Some(copied)
            }
            Err(err) if is_ftp_not_found(&err) => None,
            Err(err) => {
                warn!(command = "RETR", path = %path, transfer_count, connection_age_ms, error = %err, "FTP transfer command failed");
                return Err(err).with_context(|| format!("FTP RETR failed for {path}"));
            }
        };

        // A RETR 550 conflates "absent" with "the server refuses to return
        // this file": filtering hosts use the same status for both.
        // Cross-check existence so a refused read surfaces as an error
        // rather than as absence.
        if copied.is_none() && self.is_file(path)? {
            bail!("remote server refuses to return the existing file {path}");
        }
        Ok(copied)
    }
}

impl RemoteOps for FtpConnection {
    fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
        self.renew_if_needed()?;
        match ftp_mlst(&mut self.ftp, path.as_str())? {
            Mlst::Facts(facts) => Ok(facts.is_dir),
            Mlst::Absent => Ok(false),
            Mlst::Unsupported => {
                let original = self.ftp.pwd()?;
                match self.ftp.cwd(path.as_str()) {
                    Ok(()) => {
                        self.ftp.cwd(original)?;
                        Ok(true)
                    }
                    Err(err) if is_ftp_not_found(&err) => {
                        self.ftp.cwd(original)?;
                        Ok(false)
                    }
                    Err(err) => Err(err.into()),
                }
            }
        }
    }

    fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
        self.renew_if_needed()?;
        match ftp_mlst(&mut self.ftp, path.as_str())? {
            Mlst::Facts(facts) => Ok(!facts.is_dir),
            Mlst::Absent => Ok(false),
            Mlst::Unsupported => ftp_size(&mut self.ftp, path.as_str()).map(|size| size.is_some()),
        }
    }

    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
        self.renew_if_needed()?;
        match ftp_mlst(&mut self.ftp, path.as_str())? {
            Mlst::Absent => Ok(None),
            Mlst::Facts(facts) if facts.is_dir => Ok(None),
            Mlst::Facts(facts) => match facts.size {
                Some(size) => Ok(Some(size)),
                None => ftp_size(&mut self.ftp, path.as_str()),
            },
            Mlst::Unsupported => ftp_size(&mut self.ftp, path.as_str()),
        }
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        self.renew_if_needed()?;
        self.transfer_count += 1;
        debug!(phase = "snapshot.list", command = "LIST", path = %dir, transfer_count = self.transfer_count, "listing FTP directory");
        match self.ftp.list(Some(dir.as_str())) {
            Ok(entries) => ftp_list_entries(entries),
            Err(err) if is_ftp_not_found(&err) => Ok(Vec::new()),
            Err(err) => {
                warn!(phase = "snapshot.list", command = "LIST", path = %dir, transfer_count = self.transfer_count, connection_age_ms = self.connected_at.elapsed().as_millis(), error = %err, "FTP directory listing failed");
                Err(err.into())
            }
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
        self.renew_if_needed()?;
        self.transfer_count += 1;
        debug!(phase = "write", command = "STOR", path = %path, transfer_count = self.transfer_count, "starting FTP upload");
        ftp_store(
            &mut self.ftp,
            path.as_str(),
            &mut Cursor::new(bytes),
            bytes.len() as u64,
            FTP_DRAIN_TIMEOUT,
        )
    }

    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
        self.renew_if_needed()?;
        let mut file = File::open(local)
            .with_context(|| format!("failed to read staged file {}", local.display()))?;
        let expected_len = file
            .metadata()
            .with_context(|| format!("failed to stat staged file {}", local.display()))?
            .len();

        self.transfer_count += 1;
        debug!(phase = "upload", command = "STOR", path = %remote, transfer_count = self.transfer_count, "starting FTP upload");
        ftp_store(
            &mut self.ftp,
            remote.as_str(),
            &mut file,
            expected_len,
            FTP_DRAIN_TIMEOUT,
        )
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.renew_if_needed()?;
        self.ftp.rename(from.as_str(), to.as_str())?;
        Ok(())
    }

    fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
        self.renew_if_needed()?;
        let deleted = match self.ftp.rm(path.as_str()) {
            Ok(()) => true,
            Err(err) if is_ftp_not_found(&err) => false,
            Err(err) => return Err(err.into()),
        };
        if !deleted && self.is_file(path)? {
            bail!("remote server refuses to delete the existing file {path}");
        }
        Ok(deleted)
    }

    fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        self.renew_if_needed()?;
        match self.ftp.rmdir(path.as_str()) {
            Ok(()) => Ok(()),
            Err(err) if is_ftp_not_found(&err) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
        self.renew_if_needed()?;
        let original = self.ftp.pwd()?;
        if self.ftp.cwd(path.as_str()).is_ok() {
            self.ftp.cwd(original)?;
            return Ok(());
        }
        self.ftp.cwd(&original)?;
        self.ftp.mkdir(path.as_str())?;
        Ok(())
    }

    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
        self.renew_if_needed()?;
        match self.ftp.mkdir(path.as_str()) {
            Ok(()) => Ok(true),
            Err(error) => {
                let original = self.ftp.pwd()?;
                match self.ftp.cwd(path.as_str()) {
                    Ok(()) => {
                        self.ftp.cwd(original)?;
                        Ok(false)
                    }
                    Err(_) => {
                        self.ftp.cwd(&original).ok();
                        Err(error.into())
                    }
                }
            }
        }
    }

    fn reconnect(&mut self) -> Result<()> {
        info!(
            phase = "reconnect",
            transfer_count = self.transfer_count,
            connection_age_ms = self.connected_at.elapsed().as_millis(),
            "replacing remote connection"
        );
        *self = Self::connect(&self.settings, &self.password)?.trusted("reconnecting")?;
        Ok(())
    }
}

/// Opens the FTP control connection. Every socket the session opens, this
/// one and each passive data connection, is bounded by [`IO_TIMEOUT`].
fn connect_ftp_control(settings: &TransportSettings) -> Result<RustlsFtpStream> {
    let stream = connect_socket(settings, IO_TIMEOUT)?;
    let control = RustlsFtpStream::connect_with_stream(stream)
        .with_context(|| format!("failed to connect to {}", settings.host))?;
    Ok(control.passive_stream_builder(|address| {
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(FtpError::ConnectionError)?;
        bound_io(&stream, IO_TIMEOUT).map_err(FtpError::ConnectionError)?;
        Ok(stream)
    }))
}

fn is_ftp_not_found(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response) if response.status == Status::FileUnavailable)
}

/// Whether the server does not implement the command at all (500/502),
/// meaning callers should use their pre-RFC-3659 fallback probe.
fn is_ftp_command_unsupported(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response) if matches!(response.status, Status::NotImplemented | Status::BadCommand))
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

/// `SIZE` as an existence/size probe for servers without RFC 3659. A 550
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
        Err(err) if is_ftp_not_found(&err) || is_ftp_command_unsupported(&err) => Ok(None),
        Err(err) => {
            warn!(phase = "metadata", command = "SIZE", path, error = %err, "FTP metadata command failed");
            Err(err).with_context(|| format!("FTP SIZE failed for {path}"))
        }
    }
}

/// Owns an FTP upload stream and closes its data channel explicitly before
/// the `226` reply is read.
///
/// SuppaFTP 11's synchronous `finalize_put_stream` drops the data stream and
/// immediately reads the control-channel reply. Its rustls stream emits
/// `close_notify` from `Drop`, and the TCP connection closes only as a side
/// effect of dropping the socket. Some ProFTPD installations can persist the
/// first 8 KiB received when that implicit close races the final TLS
/// records. This wrapper makes the lifecycle explicit: it sends
/// `close_notify`, half-closes the TCP write side, and drains the channel to
/// EOF before the stream is handed back.
///
/// Every step runs on the stream's own socket handle. A duplicated handle
/// is not equivalent on Windows: closing the original can disconnect the
/// shared socket, leaving the duplicate unable to complete the close after
/// the server already received the whole payload.
struct FtpUploadStream<T>
where
    T: TlsStream<InnerStream = StreamOwned<ClientConnection, TcpStream>>,
{
    stream: DataStream<T>,
    closed: bool,
}

impl<T> FtpUploadStream<T>
where
    T: TlsStream<InnerStream = StreamOwned<ClientConnection, TcpStream>>,
{
    fn new(stream: DataStream<T>) -> Self {
        Self {
            stream,
            closed: false,
        }
    }

    /// Sends the TLS close notification, if any, and half-closes the TCP
    /// write side, returning the socket for the drain.
    fn close_write(&mut self) -> std::io::Result<&mut TcpStream> {
        let socket = match &mut self.stream {
            DataStream::Tcp(socket) => socket,
            DataStream::Ssl(tls) => {
                let tls = tls.mut_ref();
                tls.flush()?;
                tls.conn.send_close_notify();
                while tls.conn.wants_write() {
                    tls.conn.write_tls(&mut tls.sock)?;
                }
                &mut tls.sock
            }
        };
        socket.shutdown(Shutdown::Write)?;
        Ok(socket)
    }

    /// Send the TLS close notification, half-close the TCP write side, and
    /// consume every response record before the socket is dropped.
    ///
    /// `timeout` bounds the whole drain: a peer that never sends its FIN
    /// must not block the upload indefinitely.
    fn close_tls_and_drain(&mut self, timeout: Duration) -> Result<()> {
        let socket = self
            .close_write()
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
            socket
                .set_read_timeout(Some(remaining))
                .context("failed to bound the FTP data socket drain")?;
            match socket.read(&mut discarded) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    bail!("timed out waiting for the FTP server to close the data connection");
                }
                Err(error) => return Err(error).context("failed to drain FTP data socket"),
            }
        }
        self.closed = true;
        Ok(())
    }
}

impl<T> Write for FtpUploadStream<T>
where
    T: TlsStream<InnerStream = StreamOwned<ClientConnection, TcpStream>>,
{
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

impl<T> Drop for FtpUploadStream<T>
where
    T: TlsStream<InnerStream = StreamOwned<ClientConnection, TcpStream>>,
{
    fn drop(&mut self) {
        if !self.closed {
            // Best effort cleanup for an error path. The normal path calls
            // `close_tls_and_drain` so drain errors can be reported.
            let _ = self.close_write();
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
    let mut stream = FtpUploadStream::new(ftp.put_with_stream(path)?);
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
        Mlst::Absent => {
            bail!("FTP upload for {path} completed but the file is absent");
        }
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
/// unencrypted. Only automatic `ftp` mode degrades, and only for a server
/// that was never trusted over TLS: an explicit `ftps` selection or a
/// pinned certificate fails instead of sending credentials in the clear.
fn allows_plaintext_ftp_fallback(settings: &TransportSettings, error: &FtpError) -> bool {
    settings.protocol == RemoteProtocol::Ftp
        && settings.trusted_certificate.is_none()
        && is_ftp_command_unsupported(error)
}

/// Parses `LIST` output. Some servers list `.` and `..` like `ls -a`; they
/// are not children of the directory, so they are left out.
fn ftp_list_entries(entries: Vec<String>) -> Result<Vec<RemoteEntry>> {
    let mut parsed = Vec::with_capacity(entries.len());
    for entry in entries {
        let file = entry
            .parse::<suppaftp::list::File>()
            .with_context(|| format!("failed to parse FTP LIST entry: {entry}"))?;
        if matches!(file.name(), "." | "..") {
            continue;
        }
        parsed.push(RemoteEntry {
            name: file.name().to_owned(),
            is_directory: file.is_directory(),
            size: Some(file.size() as u64),
        });
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        FTP_MAX_CONNECTION_AGE, FTP_MAX_TRANSFERS, FtpConnection, ftp_list_entries, ftp_store,
    };
    use crate::profile::server::remote::{
        ConnectionAttempt, IO_TIMEOUT, Reached, RemoteOps, connect,
    };
    use crate::profile::server::settings::{RemoteProtocol, TransportSettings};

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

    /// Only automatic `ftp` mode may drop to plaintext when the server
    /// refuses AUTH TLS. A strict `ftps` selection must report the
    /// failure instead of logging in unencrypted.
    #[test]
    fn strict_ftps_never_falls_back_to_plaintext() {
        let server = FakeFtp::spawn(FakeFtpOptions::default());
        let ftp = ftp_connect(&server);
        let mut settings = ftp.settings.clone();
        drop(ftp);
        server.clear_commands();
        settings.protocol = RemoteProtocol::Ftps;
        assert!(connect(&settings, "pw").is_err());
        assert_eq!(server.count("AUTH TLS"), 1);
        assert_eq!(server.count("PASS "), 0);
    }

    /// A pinned certificate records that this server spoke TLS when the user
    /// trusted it. A later refusal of AUTH TLS is a downgrade, so automatic
    /// `ftp` mode must fail instead of logging in unencrypted.
    #[test]
    fn a_pinned_certificate_never_falls_back_to_plaintext() {
        let server = FakeFtp::spawn(FakeFtpOptions::default());
        let mut settings = ftp_connect(&server).settings.clone();
        server.clear_commands();
        settings.trusted_certificate = Some("AB:CD".to_owned());

        assert!(connect(&settings, "pw").is_err());
        assert_eq!(server.count("AUTH TLS"), 1);
        assert_eq!(server.count("PASS "), 0);
    }

    /// The fingerprint a user is asked to trust is the certificate's SHA-256
    /// digest in the colon-separated form `openssl x509 -fingerprint -sha256`
    /// and browsers show, so it can be checked against the host's records.
    #[test]
    fn an_untrusted_certificate_reports_its_sha256_fingerprint() {
        use sha2::Digest;

        let server = FakeFtp::spawn(FakeFtpOptions {
            tls: true,
            ..Default::default()
        });
        let settings = TransportSettings {
            protocol: RemoteProtocol::Ftps,
            host: "127.0.0.1".to_owned(),
            port: server.addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            ..Default::default()
        };

        let ConnectionAttempt::CertificateUntrusted { fingerprint } =
            connect(&settings, "pw").unwrap()
        else {
            panic!("a self-signed certificate must need explicit trust");
        };
        let expected = sha2::Sha256::digest(server.certificate_der())
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(fingerprint, expected);
    }

    // ---------- live-protocol tests against the in-memory FTP server ----------

    use crate::profile::server::paths::RemotePathBuf;
    use crate::profile::server::remote::fake_ftp::{FakeFtp, Options as FakeFtpOptions};
    use crate::profile::server::settings::RemoteAuthentication;

    fn ftp_connect(server: &FakeFtp) -> FtpConnection {
        let settings = TransportSettings {
            protocol: RemoteProtocol::Ftp,
            host: "127.0.0.1".to_owned(),
            port: server.addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            ..Default::default()
        };
        match FtpConnection::connect(&settings, "pw").unwrap() {
            Reached::Trusted(conn) => conn,
            Reached::Untrusted { .. } => panic!("plaintext fallback should connect to the fake"),
        }
    }

    fn ftps_connect(server: &FakeFtp) -> FtpConnection {
        let settings = TransportSettings {
            protocol: RemoteProtocol::Ftps,
            host: "127.0.0.1".to_owned(),
            port: server.addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            trusted_certificate: server.trusted_certificate(),
            ..Default::default()
        };
        match FtpConnection::connect(&settings, "pw").unwrap() {
            Reached::Trusted(conn) => {
                assert!(conn.encrypted, "FTPS connection must be encrypted");
                conn
            }
            Reached::Untrusted { .. } => {
                panic!("pinned certificate should connect to the fake over TLS")
            }
        }
    }

    fn remote_path(path: &str) -> RemotePathBuf {
        RemotePathBuf::new(path).unwrap()
    }

    /// Waits for `operation` on another thread, failing the test instead of
    /// hanging it when the operation never returns.
    fn within_io_bound<T: Send + 'static>(operation: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(operation());
        });
        outcome
            .recv_timeout(IO_TIMEOUT + Duration::from_secs(10))
            .expect("the FTP operation hung past its I/O bound")
    }

    /// A host that accepts the TCP connection but never sends its `220`
    /// greeting must fail the connection attempt, not hang it.
    #[test]
    fn ftp_connect_fails_when_the_server_never_greets() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let settings = TransportSettings {
            protocol: RemoteProtocol::Ftp,
            host: "127.0.0.1".to_owned(),
            port: listener.local_addr().unwrap().port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            ..Default::default()
        };

        let silent = std::thread::spawn(move || listener.accept().unwrap());
        let result = within_io_bound(move || connect(&settings, "pw").map(|_| ()));
        assert!(result.is_err(), "a silent server cannot complete a login");
        drop(silent.join().unwrap());
    }

    /// A data connection that never delivers its payload must fail the
    /// transfer, not hang it.
    #[test]
    fn ftp_read_fails_when_the_data_channel_stalls() {
        let server = FakeFtp::spawn(FakeFtpOptions {
            stall_retr_data: true,
            ..Default::default()
        });
        server.seed_file("/state.json", b"state");
        let mut conn = ftp_connect(&server);

        let result = within_io_bound(move || {
            conn.read_listed(remote_path("/state.json").as_path(), 5, &mut Vec::new())
                .map(|_| ())
        });
        assert!(result.is_err(), "a stalled payload cannot be read");
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
        std::thread::sleep(Duration::from_millis(270));
        assert!(
            expired
                .read(remote_path("/state.json").as_path(), 64)
                .is_err()
        );
        drop(expired);

        // Stands in for the connection having been open for the full
        // renewal age.
        let mut conn = ftps_connect(&server);
        conn.connected_at = Instant::now().checked_sub(FTP_MAX_CONNECTION_AGE).unwrap();
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
        let short_lived = FakeFtp::spawn(FakeFtpOptions {
            tls: true,
            expire_control_after_transfers: Some(4),
            ..Default::default()
        });
        short_lived.seed_file("/state.json", b"state");

        let mut expired = ftps_connect(&short_lived);
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

        let server = FakeFtp::spawn(FakeFtpOptions {
            tls: true,
            expire_control_after_transfers: Some(FTP_MAX_TRANSFERS as usize),
            ..Default::default()
        });
        server.seed_file("/state.json", b"state");
        let mut conn = ftps_connect(&server);
        for _ in 0..=FTP_MAX_TRANSFERS {
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
            2
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
    fn ftp_listings_leave_out_the_dot_entries() {
        let server = FakeFtp::spawn(FakeFtpOptions {
            list_dot_entries: true,
            ..Default::default()
        });
        server.seed_dir("/plugins");
        server.seed_file("/plugins/Mod.dll", b"dll");
        let mut conn = ftp_connect(&server);

        let names: Vec<_> = conn
            .list(remote_path("/plugins").as_path())
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["Mod.dll"]);
    }

    /// The DatHost profile verified against the live server: `SIZE` is
    /// refused in ASCII mode (`550 SIZE not allowed in ASCII mode`),
    /// while dot-prefixed paths are fully visible to LIST/MLST/MDTM/RETR.
    /// Before the fix, `read` gated on `file_size`/`is_file`, so a
    /// retrievable file looked absent and caused a false "lease taken over" error.
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

        // Genuinely absent paths still read as absent, not confused
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

    /// `read_listed` trusts the listing's size: it streams RETR straight
    /// into the sink without the MLST probe `read` performs first.
    #[test]
    fn ftps_read_listed_streams_without_a_metadata_probe() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            ..Default::default()
        });
        server.seed_file("/BepInEx/plugins/Mod.dll", b"payload-bytes");

        let mut conn = ftps_connect(&server);
        server.clear_commands();
        let mut sink = Vec::new();
        assert!(
            conn.read_listed(
                remote_path("/BepInEx/plugins/Mod.dll").as_path(),
                13,
                &mut sink
            )
            .unwrap()
        );
        assert_eq!(sink, b"payload-bytes");
        assert_eq!(server.count("MLST"), 0);
    }

    /// An absent path reports `false`; the only MLST is the post-550
    /// existence cross-check, which runs after RETR.
    #[test]
    fn ftps_read_listed_reports_absent_without_probing_first() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            ..Default::default()
        });

        let mut conn = ftps_connect(&server);
        server.clear_commands();
        let mut sink = Vec::new();
        assert!(
            !conn
                .read_listed(
                    remote_path("/BepInEx/plugins/Missing.dll").as_path(),
                    4,
                    &mut sink
                )
                .unwrap()
        );
        assert!(sink.is_empty());

        let commands = server.commands.lock().unwrap();
        let retr = commands
            .iter()
            .position(|command| command == "RETR /BepInEx/plugins/Missing.dll")
            .expect("the absent path must still be attempted with RETR");
        assert!(
            !commands[..retr]
                .iter()
                .any(|command| command.starts_with("MLST")),
            "read_listed must not probe metadata before RETR"
        );
        assert!(
            commands[retr..]
                .iter()
                .any(|command| command == "MLST /BepInEx/plugins/Missing.dll"),
            "the 550 ambiguity must be resolved by an existence check"
        );
    }

    /// A host that refuses to RETR an existing file must fail closed:
    /// `Ok(false)` is reserved for confirmed absence.
    #[test]
    fn ftps_read_listed_fails_closed_when_retr_is_refused() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            refuse_retr: true,
            ..Default::default()
        });
        server.seed_file("/BepInEx/plugins/Mod.dll", b"payload-bytes");

        let mut conn = ftps_connect(&server);
        let mut sink = Vec::new();
        let error = conn
            .read_listed(
                remote_path("/BepInEx/plugins/Mod.dll").as_path(),
                13,
                &mut sink,
            )
            .unwrap_err();
        assert!(format!("{error}").contains("refuses to return"));
    }

    /// Remote drift is an error, not a truncated hash: a file larger than
    /// its listing size is refused; a smaller one streams what arrived so
    /// the caller's hash comparison reports divergence.
    #[test]
    fn ftps_read_listed_bounds_the_stream_to_the_listed_size() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            ..Default::default()
        });
        server.seed_file("/BepInEx/plugins/Grew.dll", b"0123456789");
        server.seed_file("/BepInEx/plugins/Shrank.dll", b"12345");

        let mut conn = ftps_connect(&server);
        let mut sink = Vec::new();
        let error = conn
            .read_listed(
                remote_path("/BepInEx/plugins/Grew.dll").as_path(),
                5,
                &mut sink,
            )
            .unwrap_err();
        assert!(format!("{error}").contains("grew past its listed 5 bytes"));

        let mut sink = Vec::new();
        assert!(
            conn.read_listed(
                remote_path("/BepInEx/plugins/Shrank.dll").as_path(),
                10,
                &mut sink
            )
            .unwrap()
        );
        assert_eq!(sink, b"12345");
    }

    /// A dropped control connection mid-RETR surfaces as an error; the
    /// engine-level retry reconnects separately.
    #[test]
    fn ftps_read_listed_surfaces_a_control_reset_during_transfer() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            ..Default::default()
        });
        server.seed_file("/BepInEx/plugins/Mod.dll", b"payload-bytes");
        server.reset_on_retr_of("/BepInEx/plugins/Mod.dll", 1, false);

        let mut conn = ftps_connect(&server);
        let mut sink = Vec::new();
        assert!(
            conn.read_listed(
                remote_path("/BepInEx/plugins/Mod.dll").as_path(),
                13,
                &mut sink
            )
            .is_err()
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

    /// Some ProFTPD hosts keep only the first 8 KiB when the data channel's
    /// final TLS records race its TCP close. The upload must close the
    /// channel in order, `close_notify` before FIN, whatever the payload
    /// size, rather than leave the alert to the stream's drop.
    #[test]
    fn ftps_uploads_send_close_notify_before_closing_the_data_channel() {
        let server = FakeFtp::valheim_host(FakeFtpOptions {
            tls: true,
            ..Default::default()
        });
        let mut conn = ftps_connect(&server);

        for size in [1usize, 8_192, 8_193, 32_769] {
            let path = remote_path(&format!("/BepInEx/config/ordered-{size}.bin"));
            conn.write(path.as_path(), &vec![b'x'; size]).unwrap();
        }

        assert_eq!(server.unclean_stor_closes(), Vec::<String>::new());
    }

    /// Regression test for an FTPS data channel that dies after the TLS
    /// handshake: the fake completes the handshake, kills the socket
    /// before the payload lands, then still answers `226 transfer
    /// complete`. `write` must report the aborted transfer instead of
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
        let ftp = &mut conn.ftp;

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
}
