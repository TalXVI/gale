//! Shared constants for the Gale-managed local worker (the "Host worker
//! on this PC" mode). Windows-only: the desktop uses these paths to
//! provision and query the service, and `gale-worker` uses them when it
//! installs or runs as that service. Keeping them in one place keeps the
//! two binaries' on-disk contract identical.

use eyre::{Context, Result, bail};
use std::{
    ops::RangeInclusive,
    time::{Duration, Instant},
};
use windows_service::service::ServiceState;

const TRANSITION_TIMEOUT: Duration = Duration::from_secs(45);

/// SCM service name for the managed worker. One service exists per
/// machine, which is also the first line of defense against duplicate
/// workers managing the same server.
pub const SERVICE_NAME: &str = "GaleWorker";

/// The managed worker's fixed identity in lease records and status
/// responses.
pub const WORKER_ID: &str = "this-pc";

/// Preferred loopback port for the managed worker's HTTP API.
pub const DEFAULT_PORT: u16 = 8472;

/// Ports probed when the default is taken. The provisioned port is
/// written into both the worker config and the profile's worker address.
pub const PORT_RANGE: RangeInclusive<u16> = DEFAULT_PORT..=8496;

pub const CONFIG_FILE: &str = "gale-worker.json";
pub const SECRETS_FILE: &str = "secrets.env";
pub const STATUS_FILE: &str = "status.json";
/// A staged copy of the user's SSH private key — the service runs as
/// LocalSystem and cannot read keys under `%USERPROFILE%`.
pub const SSH_KEY_FILE: &str = "ssh.key";
#[cfg(feature = "worker")]
pub const LOG_FILE: &str = "worker.log";
/// The durable state (`gale-worker-state.json`) and `secrets.env` live in
/// this subdirectory, which is ACL'd so only SYSTEM and administrators
/// can read it.
pub const PRIVATE_DIR: &str = "private";

/// The worker binary's name next to `gale.exe` / in `target/`.
pub const WORKER_EXE: &str = "gale-worker.exe";

/// Machine-wide worker root, `%ProgramData%\Gale\worker`. The service
/// must keep its state where it can reach it before and without any user
/// signing in, so a per-user directory cannot work.
pub fn root_dir() -> std::path::PathBuf {
    let base = std::env::var_os("ProgramData")
        .unwrap_or_else(|| std::ffi::OsString::from(r"C:\ProgramData"));
    std::path::PathBuf::from(base).join("Gale").join("worker")
}

/// `%ProgramData%\Gale\worker\private` — secrets, journal, and lock.
pub fn private_dir() -> std::path::PathBuf {
    root_dir().join(PRIVATE_DIR)
}

pub fn stop_and_wait(service: &windows_service::service::Service) -> Result<()> {
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    if let Err(err) = service.stop()
        && service.query_status()?.current_state != ServiceState::Stopped
    {
        return Err(err).context("failed to stop the service");
    }
    wait_for_state(service, ServiceState::Stopped)
}

pub fn wait_for_state(
    service: &windows_service::service::Service,
    target: ServiceState,
) -> Result<()> {
    let deadline = Instant::now() + TRANSITION_TIMEOUT;
    loop {
        let state = service.query_status()?.current_state;
        if state == target {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "service did not reach {target:?} within {TRANSITION_TIMEOUT:?} (state: {state:?})"
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Confirms the worker's listen address accepts TCP connections. SCM
/// state alone cannot prove the API survived past the Running report —
/// a crashed process can linger in Running briefly.
pub fn await_listen_ready(listen: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last_err = None;
    while Instant::now() < deadline {
        match std::net::TcpStream::connect(listen) {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out")))
        .context(format!(
            "the worker service reports running but {listen} is not answering"
        ))
}
