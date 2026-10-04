//! An in-memory FTP endpoint for end-to-end tests. Unlike
//! [`memory::MemoryRemote`], which stubs [`RemoteOps`] directly, this
//! serves the actual wire protocol: USER/PASS/TYPE/PWD/CWD/PASV/LIST/
//! MLST/SIZE/MDTM/RETR/STOR/RNFR/RNTO/DELE/MKD/RMD, so tests exercise
//! the FTP transport's real command and error handling.
//!
//! The knobs reproduce observed hosting behaviors: DatHost's ProFTPD
//! refuses `SIZE` while in ASCII mode, and a deeper filtering host could
//! refuse `RETR` on existing files while still proving their existence
//! through `MLST`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use suppaftp::rustls::{
    ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer,
};

use super::ftps_verifier::{certificate_fingerprint, crypto_provider};

/// Behaviors the fake server can exhibit.
#[derive(Default, Clone, Copy)]
pub struct Options {
    /// Refuse `SIZE` until the client sends `TYPE I`. DatHost's
    /// ProFTPD answers `550 SIZE not allowed in ASCII mode`.
    pub size_requires_binary: bool,
    /// Refuse all SIZE requests, including binary mode.
    pub refuse_size: bool,
    /// Refuse `RETR` even for files that exist and are provable via
    /// `MLST`. This models a deeper read filter.
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
    /// still answering `226 transfer complete`. The client must not
    /// report this mid-transfer abort as success.
    pub abort_stor_after_tls_handshake: bool,
    /// On `STOR`, keep only this prefix of the received payload but
    /// still answer `226 transfer complete`. This models a host that
    /// reports success after saving a truncated file.
    pub truncate_stor_to: Option<usize>,
    /// On `STOR`, hold the data connection open after the payload
    /// arrives. No `close_notify`. No FIN, so the client's close
    /// handshake never sees EOF: a wedged server the drain bound
    /// must turn into an upload failure.
    pub hold_stor_eof: bool,
    /// On `RETR`, open the data connection but never send the payload
    /// or close it, while still answering `226`: a wedged data channel
    /// the client's I/O bound must turn into an error.
    pub stall_retr_data: bool,
    /// Expire an individual control connection after this age.
    pub expire_control_after: Option<Duration>,
    /// Expire an individual control connection after this many data transfers.
    pub expire_control_after_transfers: Option<usize>,
    /// Start `LIST` output with `.` and `..`, as `ls -a` style servers do.
    pub list_dot_entries: bool,
}

/// A path-scoped RETR interruption armed by
/// [`FakeFtp::reset_on_retr_of`]: the next `remaining` RETRs of exactly
/// `path` kill their control connection, either before or after the
/// `226` completion reply.
struct PathReset {
    path: String,
    remaining: usize,
    after_completion: bool,
}

/// Everything the accept loop and each connection thread share. One
/// clone hands a connection all of it; `FakeFtp` derefs to it so
/// tests can seed and inspect the same handles directly.
#[derive(Clone)]
pub struct Shared {
    /// Every `VERB arg` line received, for protocol assertions.
    pub commands: Arc<Mutex<Vec<String>>>,
    pub sent_bytes: Arc<std::sync::atomic::AtomicUsize>,
    /// One entry per `STOR` data channel the client closed without a
    /// clean close: under TLS, its `close_notify` must arrive before the
    /// TCP FIN.
    unclean_stor_closes: Arc<Mutex<Vec<String>>>,
    retr_count: Arc<std::sync::atomic::AtomicUsize>,
    reset_retr_at: Arc<std::sync::atomic::AtomicUsize>,
    reset_retr_remaining: Arc<std::sync::atomic::AtomicUsize>,
    reset_after_completion: Arc<std::sync::atomic::AtomicBool>,
    path_reset: Arc<Mutex<Option<PathReset>>>,
    fs: Arc<Mutex<Fs>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    options: Options,
    tls: Option<Tls>,
}

/// A running fake server. `shared.fs` is shared with every accepted
/// connection, so tests observe and seed the remote filesystem
/// directly.
pub struct FakeFtp {
    pub addr: SocketAddr,
    shared: Shared,
    /// The most control connections the server has held at once.
    peak_connection_count: Arc<std::sync::atomic::AtomicUsize>,
    /// The fingerprint of the self-signed certificate the server
    /// presents when `Options::tls` is on; `None` otherwise.
    certificate_fingerprint: Option<String>,
    /// That certificate's DER encoding.
    certificate_der: Option<Vec<u8>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::ops::Deref for FakeFtp {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.shared
    }
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
/// when `abort_stor_after_tls_handshake` is on. It is pinned to
/// TLS 1.2 so the server owns the final handshake flight and can
/// reset the socket before the client's payload write is attempted.
/// (Under TLS 1.3 the client sends Finished and payload back-to-back,
/// leaving no window for the abort to precede the write.)
#[derive(Clone)]
struct Tls {
    control: Arc<ServerConfig>,
    data: Arc<ServerConfig>,
}

/// Builds the self-signed server identity used when
/// `Options::tls` is on. Returns the `Tls` configs plus the
/// certificate's fingerprint so tests can pin it exactly the way a
/// user trusts a certificate in Gale.
fn tls_identity(tls12_data: bool) -> (Tls, String, Vec<u8>) {
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

    (
        Tls { control, data },
        fingerprint,
        certified.cert.der().to_vec(),
    )
}

impl FakeFtp {
    pub fn spawn(options: Options) -> Self {
        let tls = options
            .tls
            .then(|| tls_identity(options.abort_stor_after_tls_handshake));
        let certificate_fingerprint = tls.as_ref().map(|(_, fingerprint, _)| fingerprint.clone());
        let certificate_der = tls.as_ref().map(|(_, _, der)| der.clone());
        let shared = Shared {
            fs: Arc::new(Mutex::new(Fs {
                dirs: ["/".to_owned()].into_iter().collect(),
                ..Default::default()
            })),
            commands: Arc::new(Mutex::new(Vec::new())),
            sent_bytes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            unclean_stor_closes: Arc::new(Mutex::new(Vec::new())),
            retr_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            reset_retr_at: Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
            reset_retr_remaining: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            reset_after_completion: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            path_reset: Arc::new(Mutex::new(None)),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            options,
            tls: tls.map(|(tls, _, _)| tls),
        };
        let active_connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak_connection_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        let thread = {
            let shared = shared.clone();
            let stopping = shared.stop.clone();
            let active_connections = active_connections.clone();
            let peak_connection_count = peak_connection_count.clone();
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
                            let shared = shared.clone();
                            let active_connections = active_connections.clone();
                            peak_connection_count.fetch_max(
                                active_connections
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                                    + 1,
                                std::sync::atomic::Ordering::SeqCst,
                            );
                            threads.push(std::thread::spawn(move || {
                                let shutdown = stream.try_clone().unwrap();
                                serve(stream, &shared);
                                let _ = shutdown.shutdown(std::net::Shutdown::Both);
                                active_connections
                                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
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
            shared,
            peak_connection_count,
            certificate_fingerprint,
            certificate_der,
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

    /// Break the control connection on the next `count` RETRs of
    /// exactly `path`. This is the path-scoped counterpart of
    /// [`Self::reset_on_retrs`], which cannot identify a file once
    /// payload reads run on parallel connections.
    pub fn reset_on_retr_of(&self, path: &str, count: usize, after_completion: bool) {
        assert!(count > 0);
        *self.path_reset.lock().unwrap() = Some(PathReset {
            path: normalize(path),
            remaining: count,
            after_completion,
        });
    }

    /// The most control connections the server has held at once.
    pub fn peak_connections(&self) -> usize {
        self.peak_connection_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many received command lines start with `prefix`
    /// (e.g. `RETR `, include the trailing space to exclude
    /// similarly-named commands).
    pub fn count(&self, prefix: &str) -> usize {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with(prefix))
            .count()
    }

    /// Clears the command log so counts can be attributed to a single
    /// phase of a test.
    pub fn clear_commands(&self) {
        self.commands.lock().unwrap().clear();
    }

    /// Removes a seeded file to simulate a remote deletion between
    /// Preview and Deploy in staleness tests.
    pub fn remove_file(&self, path: &str) {
        self.fs.lock().unwrap().files.remove(&normalize(path));
    }

    pub fn has_dir(&self, path: &str) -> bool {
        self.fs.lock().unwrap().dirs.contains(&normalize(path))
    }

    /// The fingerprint to pin as `trusted_certificate` when
    /// `Options::tls` is on.
    pub fn trusted_certificate(&self) -> Option<String> {
        self.certificate_fingerprint.clone()
    }

    /// The DER encoding of the certificate presented under
    /// `Options::tls`.
    pub fn certificate_der(&self) -> &[u8] {
        self.certificate_der
            .as_deref()
            .expect("the fake presents a certificate only with Options::tls")
    }

    /// `STOR` data channels the client did not close cleanly, as
    /// `path: error kind`.
    pub fn unclean_stor_closes(&self) -> Vec<String> {
        self.unclean_stor_closes.lock().unwrap().clone()
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

fn accept_data(listener: TcpListener, stop: &std::sync::atomic::AtomicBool) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !stop.load(std::sync::atomic::Ordering::SeqCst) && std::time::Instant::now() < deadline {
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
/// session negotiated `PROT P`. The TLS variant is boxed so a plain
/// `DataSocket` stays pointer-sized.
enum DataSocket {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
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
    /// `write_tls` flushes the alert without reading, `flush` would
    /// block waiting for client data that never comes.
    fn close(self) {
        if let Self::Tls(mut conn) = self {
            conn.conn.send_close_notify();
            let _ = conn.conn.write_tls(&mut conn.sock);
        }
    }
}

fn serve(stream: TcpStream, shared: &Shared) {
    let Shared {
        fs,
        commands,
        options,
        tls,
        stop,
        sent_bytes,
        unclean_stor_closes,
        retr_count,
        reset_retr_at,
        reset_retr_remaining,
        reset_after_completion,
        path_reset,
    } = shared;
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
        () => {{
            passive
                .take()
                .and_then(|listener| accept_data(listener, stop))
                .and_then(|mut stream| match (protected, tls.as_ref()) {
                    (true, Some(tls)) => tls_accept(&mut stream, &tls.data)
                        .map(|conn| DataSocket::Tls(Box::new(StreamOwned::new(conn, stream)))),
                    _ => Some(DataSocket::Plain(stream)),
                })
        }};
    }

    /// The client connects the passive data socket before reading
    /// the command's final response, so a refused command must still
    /// accept and close it. A rustls data stream that is dropped
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
                Some(_) if options.stall_retr_data => {
                    transfers += 1;
                    if !send(&mut writer, "150 opening data connection") {
                        return;
                    }
                    held_data.extend(data!());
                    "226 transfer complete".to_owned()
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
                            sent_bytes.fetch_add(chunk.len(), std::sync::atomic::Ordering::SeqCst);
                        }
                        data.close();
                    }
                    let sequence = retr_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    let mut reset = (sequence
                        >= reset_retr_at.load(std::sync::atomic::Ordering::SeqCst)
                        && reset_retr_remaining
                            .fetch_update(
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                                |remaining| remaining.checked_sub(1),
                            )
                            .is_ok())
                    .then(|| reset_after_completion.load(std::sync::atomic::Ordering::SeqCst));
                    if reset.is_none() {
                        let mut guard = path_reset.lock().unwrap();
                        if let Some(armed) = guard.as_mut()
                            && armed.path == path
                            && armed.remaining > 0
                        {
                            armed.remaining -= 1;
                            reset = Some(armed.after_completion);
                        }
                    }
                    if let Some(after_completion) = reset {
                        if after_completion {
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
                    // The TLS 1.2 handshake completed inside `data!`,
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
                        // rustls reports EOF without `close_notify` as
                        // `UnexpectedEof`, so a clean read is the ordered
                        // close: `close_notify`, then FIN.
                        if let Err(error) = data.read_to_end(&mut bytes) {
                            unclean_stor_closes
                                .lock()
                                .unwrap()
                                .push(format!("{path}: {:?}", error.kind()));
                        }
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
                        let dots = (options.list_dot_entries && verb == "LIST")
                            .then(|| [(".".to_owned(), true), ("..".to_owned(), true)]);
                        Some(
                            dots.into_iter()
                                .flatten()
                                .chain(fs.children(&path))
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
