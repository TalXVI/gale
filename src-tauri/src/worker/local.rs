//! Shared constants for the Gale-managed local worker (the "Host worker
//! on this PC" mode). Windows-only: the desktop uses these paths to
//! provision and query the service, and `gale-worker` uses them when it
//! installs or runs as that service. Keeping them in one place keeps the
//! two binaries' on-disk contract identical.

use eyre::{Context, Result, bail};
use std::ffi::OsStr;
use std::{
    ops::RangeInclusive,
    path::Path,
    time::{Duration, Instant},
};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

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
pub const TRAY_EXE: &str = "gale-worker-tray.exe";

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

/// The Server page and tray companion share this exact byte comparison.
/// Development builds have no independent Worker version metadata.
pub fn worker_update_available(candidate: &Path) -> bool {
    binaries_differ(candidate, &root_dir().join(WORKER_EXE))
}

pub fn binaries_differ(candidate: &Path, installed: &Path) -> bool {
    match (std::fs::read(candidate), std::fs::read(installed)) {
        (Ok(candidate), Ok(installed)) => candidate != installed,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    Running,
    StopPending,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceControlAction {
    Start,
    Stop,
    Restart,
}

fn service_manager() -> Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to open the service control manager")
}

fn is_not_installed(err: &windows_service::Error) -> bool {
    matches!(err, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(1060))
}

pub fn service_state() -> Result<ManagedServiceState> {
    let service = match service_manager()?.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => service,
        Err(err) if is_not_installed(&err) => return Ok(ManagedServiceState::NotInstalled),
        Err(err) => return Err(err).context("failed to open the worker service"),
    };
    Ok(match service.query_status()?.current_state {
        ServiceState::Stopped => ManagedServiceState::Stopped,
        ServiceState::StartPending => ManagedServiceState::StartPending,
        ServiceState::Running => ManagedServiceState::Running,
        ServiceState::StopPending => ManagedServiceState::StopPending,
        _ => ManagedServiceState::Other,
    })
}

trait ServiceOperations {
    fn stop_and_wait(&mut self) -> Result<()>;
    fn start_and_wait(&mut self) -> Result<()>;
}

fn perform_control(
    operations: &mut impl ServiceOperations,
    action: ServiceControlAction,
) -> Result<()> {
    if action != ServiceControlAction::Start {
        operations.stop_and_wait()?;
    }
    if action != ServiceControlAction::Stop {
        operations.start_and_wait()?;
    }
    Ok(())
}

struct ScmServiceOperations {
    service: windows_service::service::Service,
    listen: Option<String>,
}

impl ServiceOperations for ScmServiceOperations {
    fn stop_and_wait(&mut self) -> Result<()> {
        stop_and_wait(&self.service)
    }

    fn start_and_wait(&mut self) -> Result<()> {
        self.service
            .start(&[] as &[&OsStr])
            .context("failed to start the service")?;
        wait_for_state(&self.service, ServiceState::Running)?;
        await_listen_ready(
            self.listen
                .as_deref()
                .ok_or_else(|| eyre::eyre!("the installed Worker listen address is missing"))?,
        )
    }
}

/// Controls the managed service through the same stop/wait/start/readiness
/// sequence used by Gale's Server page and the notification-area companion.
pub fn control_service(action: ServiceControlAction) -> Result<()> {
    let access = match action {
        ServiceControlAction::Start => ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        ServiceControlAction::Stop => ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        ServiceControlAction::Restart => {
            ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS
        }
    };
    let service = service_manager()?
        .open_service(SERVICE_NAME, access)
        .context("the managed worker is not installed")?;
    let listen = if action == ServiceControlAction::Stop {
        None
    } else {
        let config = std::fs::read(root_dir().join(CONFIG_FILE))
            .context("failed to read the installed Worker config")?;
        Some(
            serde_json::from_slice::<super::config::WorkerConfig>(&config)
                .context("the installed Worker config is invalid")?
                .listen,
        )
    };
    perform_control(&mut ScmServiceOperations { service, listen }, action)
}

/// Runs the bundled Worker elevated. The caller supplies Gale's bundled
/// candidate explicitly; a tray-helper path must never be used here.
pub fn run_elevated_worker(exe: &Path, params: &str) -> Result<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HSTRING, PCWSTR, w};

    let exe_w = HSTRING::from(exe.as_os_str());
    let params_w = HSTRING::from(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: Default::default(),
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        lpDirectory: PCWSTR::null(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };

    unsafe { ShellExecuteExW(&mut info) }.context(
        "the elevation prompt was cancelled or failed — managing the worker needs elevation",
    )?;
    if info.hProcess.is_invalid() {
        bail!("the elevated worker command produced no process handle");
    }
    let code = unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code
    };
    Ok(code)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeOperations(Vec<&'static str>);

    impl ServiceOperations for FakeOperations {
        fn stop_and_wait(&mut self) -> Result<()> {
            self.0.push("stop_and_wait");
            Ok(())
        }

        fn start_and_wait(&mut self) -> Result<()> {
            self.0.push("start_and_wait");
            Ok(())
        }
    }

    #[test]
    fn restart_uses_the_shared_stop_then_start_semantics() {
        let mut operations = FakeOperations::default();
        perform_control(&mut operations, ServiceControlAction::Restart).unwrap();
        assert_eq!(operations.0, ["stop_and_wait", "start_and_wait"]);
    }

    #[test]
    fn stop_waits_until_the_service_is_stopped() {
        let mut operations = FakeOperations::default();
        perform_control(&mut operations, ServiceControlAction::Stop).unwrap();
        assert_eq!(operations.0, ["stop_and_wait"]);
    }
}
