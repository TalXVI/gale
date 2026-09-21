//! The Gale-managed local worker: `gale-worker` running as a Windows
//! service on the same machine as Gale ("Host worker on this PC").
//!
//! Provisioning writes `%ProgramData%\Gale\worker` (config, secrets,
//! journal, status file), installs the service through one elevated step,
//! and points the profile's `syncMode: "worker"` at the loopback API.
//! Once installed the service is independent of the Gale process: it
//! starts with Windows, restarts after crashes through SCM failure
//! actions, and recovers pending work through its journal. Day-to-day
//! start/stop/status calls go through the SCM unelevated because install
//! grants interactive users control rights.

use eyre::{Result, bail};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::worker::api::{StatusResponse, WorkerRunReport};

#[cfg(windows)]
use eyre::{Context, OptionExt, ensure};
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use uuid::Uuid;

#[cfg(windows)]
use crate::{
    profile::{
        server::{
            commands::{
                persist_credential, remote_credential, save_remote_settings_for, sync_target,
                worker_client,
            },
            secrets::{ServerSecret, ServerSecrets},
            settings::{
                HostProvider, RemoteAuthentication, RemoteServerSettings, SyncMode, WorkerSettings,
            },
        },
        sync::auth,
    },
    state::ManagerExt,
    worker::{api::WorkerRunPhase, config::WorkerConfig, local, secrets::Secrets},
};

/// What the service control manager reports, projected for the UI.
/// `NotInstalled` is a state so the dialog can offer Install again after
/// an out-of-band removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    Running,
    StopPending,
    /// Any other SCM state (pause states the worker never enters).
    Other,
}

/// Who the installed worker is bound to, read from its config file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerBinding {
    pub worker_id: String,
    /// The sync-profile id the worker serves — compare against the
    /// profile's own sync id to detect a binding to another profile.
    pub profile_id: String,
    pub listen: String,
    /// The worker's API base URL, derived from `listen`.
    pub address: String,
}

/// Everything the dialog needs to render the managed worker.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalWorkerStatus {
    /// `false` off Windows — provisioning is unavailable there.
    pub supported: bool,
    pub service: ServiceState,
    /// The installed worker's binding, when a config exists.
    pub binding: Option<WorkerBinding>,
    /// The worker's last reported run phase; a `running` report while the
    /// service is stopped means the process died unexpectedly.
    pub run: Option<WorkerRunReport>,
    /// Live API status, present when the service is running and the
    /// profile holds the worker token.
    pub worker: Option<StatusResponse>,
    /// Why `worker` is absent (unreachable, bad token, ...).
    pub worker_error: Option<String>,
    /// True when the run report says `shutdown` — the machine went down;
    /// the service comes back with the next boot.
    pub stopped_for_shutdown: bool,
    /// True when the bundled `gale-worker.exe` differs from the copy the
    /// installed service runs — an app update left a newer worker behind.
    /// `update` swaps it in without re-running the OAuth provisioning.
    pub update_available: bool,
    /// Provisioning warnings that are not fatal (e.g. the worker sign-in
    /// invalidated the desktop session).
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl LocalWorkerStatus {
    #[cfg(not(windows))]
    fn unsupported() -> Self {
        Self {
            supported: false,
            service: ServiceState::NotInstalled,
            binding: None,
            run: None,
            worker: None,
            worker_error: None,
            stopped_for_shutdown: false,
            update_available: false,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LocalWorkerAction {
    Start,
    Stop,
    Restart,
}

// ---------- status ----------

/// Assembles the managed worker's status from the SCM, the status file,
/// and (when running) the worker API.
pub async fn status(app: &AppHandle) -> Result<LocalWorkerStatus> {
    #[cfg(not(windows))]
    {
        let _ = app;
        Ok(LocalWorkerStatus::unsupported())
    }
    #[cfg(windows)]
    {
        let service_state = service_state()?;
        let binding = installed_binding();
        let run = read_run_report();

        let mut worker = None;
        let mut worker_error = None;
        if service_state == ServiceState::Running
            && let Some(binding) = &binding
        {
            // The token lives in the active profile's keyring; when this
            // profile isn't the bound one the lookup may legitimately fail.
            let secrets = ServerSecrets::for_profile(app.lock_manager().active_profile().id)?;
            let token = secrets
                .resolve(ServerSecret::WorkerToken, "")
                .unwrap_or_default();
            if token.is_empty() {
                worker_error = Some("this profile has no stored worker token".to_owned());
            } else {
                let mut settings = RemoteServerSettings::default();
                settings.worker.address = binding.address.clone();
                match worker_client(&secrets, &settings, &token) {
                    Ok(client) => match client.status(false).await {
                        Ok(live) => worker = Some(live),
                        Err(err) => worker_error = Some(format!("{err:#}")),
                    },
                    Err(err) => worker_error = Some(format!("{err:#}")),
                }
            }
        }

        Ok(LocalWorkerStatus {
            supported: true,
            service: service_state,
            binding,
            stopped_for_shutdown: matches!(
                run.as_ref().map(|report| report.phase),
                Some(WorkerRunPhase::Shutdown)
            ),
            run,
            worker,
            worker_error,
            update_available: worker_update_available(),
            warnings: Vec::new(),
        })
    }
}

// ---------- provisioning ----------

/// Provisions and installs the managed worker for the active profile:
/// an independent sync credential, staged config + secrets, one elevated
/// install step, then the profile's worker settings point at the loopback
/// address.
pub async fn provision(
    app: &AppHandle,
    password: &str,
    dat_host_password: &str,
) -> Result<LocalWorkerStatus> {
    #[cfg(not(windows))]
    {
        let _ = (app, password, dat_host_password);
        bail!("the managed worker is only supported on Windows")
    }
    #[cfg(windows)]
    {
        provision_windows(app, password, dat_host_password).await
    }
}

#[cfg(windows)]
async fn provision_windows(
    app: &AppHandle,
    password: &str,
    dat_host_password: &str,
) -> Result<LocalWorkerStatus> {
    let target = sync_target(app)?;
    let sync_id = target
        .sync_id
        .clone()
        .ok_or_eyre("publish this profile before hosting a worker on this PC")?;
    ensure!(
        auth::user_info(app).is_some(),
        "sign in to Gale sync first — the worker gets its own login"
    );

    // The embedded remote settings describe the transport; the worker
    // block inside them is meaningless to the worker itself, so the copy
    // it runs with is normalized to Local to keep validation honest.
    let mut worker_remote = target.settings.clone();
    worker_remote.sync_mode = SyncMode::Local;
    ensure!(
        worker_remote.authentication != RemoteAuthentication::Agent,
        "SSH agent authentication cannot run unattended in a service; use a password or a private key"
    );

    let port = pick_port(installed_port());
    let listen = format!("127.0.0.1:{port}");
    let root = local::root_dir();
    let private = local::private_dir();

    // A key under %USERPROFILE% is unreachable to the LocalSystem
    // service, so the staged payload carries a copy and the worker's
    // config points at the installed path in the locked-down dir.
    let key_source = if worker_remote.authentication == RemoteAuthentication::PrivateKey {
        let source = PathBuf::from(&worker_remote.private_key_path);
        ensure!(source.is_file(), "SSH private key file does not exist");
        worker_remote.private_key_path = private
            .join(local::SSH_KEY_FILE)
            .to_string_lossy()
            .into_owned();
        Some(source)
    } else {
        None
    };
    worker_remote.validate()?;

    let secrets = ServerSecrets::for_profile(target.profile_id)?;
    let remote_password = remote_credential(&secrets, &target.settings, password)?;
    let dat_host_password = secrets.resolve(ServerSecret::DatHostPassword, dat_host_password)?;
    if target.settings.host_control.provider == HostProvider::DatHost {
        ensure!(
            !dat_host_password.is_empty(),
            "the DatHost account password is required for the worker to restart the server"
        );
    }

    // A second OAuth login gives the worker an independent refresh-token
    // chain. Sharing the desktop's token is not an option: every grant
    // rotates it, so whichever side refreshed second would be dead.
    let creds = auth::oauth_credentials(app).await?;

    let config = WorkerConfig {
        worker_id: local::WORKER_ID.to_owned(),
        listen: listen.clone(),
        profile_id: sync_id,
        game: target.game.clone(),
        sync_url: None,
        remote: worker_remote,
        host_control: target.settings.host_control.clone(),
        auto_sync: target.settings.worker.auto_sync,
        auto_mods: target.settings.worker.auto_mods,
        restart_policy: target.settings.restart_policy,
        poll_interval_secs: 300,
        state_dir: private.clone(),
        secrets_file: Some(private.join(local::SECRETS_FILE)),
        status_file: Some(root.join(local::STATUS_FILE)),
    };

    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let worker_secrets = Secrets {
        token: Some(token.clone()),
        remote_password: (!remote_password.is_empty()).then_some(remote_password),
        refresh_token: Some(creds.refresh_token().to_owned()),
        dat_host_password: (!dat_host_password.is_empty()).then_some(dat_host_password),
    };

    // Stage the payload where the elevated helper can read it, then run
    // the one privileged step.
    let staging = Staging::create(&config, &worker_secrets, key_source.as_deref())?;
    let log = staging.path().join("install.log");
    let elevated_args = format!(
        "service install --from \"{}\" --log \"{}\"",
        staging.path().display(),
        log.display()
    );
    let install_result = {
        // The elevated wait blocks; keep it off the async executor.
        let exe = worker_exe()?;
        tokio::task::spawn_blocking(move || run_elevated(&exe, &elevated_args))
            .await
            .context("elevated install task failed")?
    };
    let log_text = read_log(&log).unwrap_or_default();
    match install_result {
        Err(err) => {
            staging.cleanup();
            return Err(err.wrap_err(
                "failed to launch the elevated worker installer (the UAC prompt may have been declined)",
            ));
        }
        Ok(exit) if exit != 0 => {
            staging.cleanup();
            bail!(
                "worker installation failed (exit {exit}): {}",
                log_text.trim()
            );
        }
        Ok(_) if service_state()? != ServiceState::Running => {
            staging.cleanup();
            bail!("the worker service did not come up: {}", log_text.trim());
        }
        Ok(_) => {}
    }
    // The staged payload was consumed by the install; never leave copies
    // of the secrets lying in temp regardless of what happens next.
    staging.cleanup();

    // The service is installed and running. Point the profile at it.
    let mut remote = target.settings.clone();
    remote.sync_mode = SyncMode::Worker;
    remote.worker = WorkerSettings {
        address: format!("http://{listen}"),
        hosted: true,
        auto_sync: remote.worker.auto_sync,
        auto_mods: remote.worker.auto_mods,
    };
    save_remote_settings_for(app, target.profile_id, remote)?;
    persist_credential(&secrets, Some(ServerSecret::WorkerToken), &token, true)?;

    let mut worker_status = status(app).await?;
    // If the service enforces one session per user, the worker login may
    // have invalidated the desktop's chain; a forced grant finds out.
    if auth::verify_session(app).await.is_err() {
        worker_status.warnings.push(
            "the worker sign-in invalidated Gale's own session — sign in to sync again".to_owned(),
        );
    }
    Ok(worker_status)
}

// ---------- control ----------

/// Starts, stops, or restarts the installed service through the SCM.
/// These calls are unelevated — install grants interactive users control.
pub async fn control(app: &AppHandle, action: LocalWorkerAction) -> Result<LocalWorkerStatus> {
    #[cfg(not(windows))]
    {
        let _ = (app, action);
        bail!("the managed worker is only supported on Windows")
    }
    #[cfg(windows)]
    {
        use windows_service::service::ServiceState as ScmState;
        match action {
            LocalWorkerAction::Start => {
                let service = open_service(Access::Start)?;
                service
                    .start(&[] as &[&std::ffi::OsStr])
                    .context("failed to start the service")?;
                wait_for_state(&service, ScmState::Running)?;
            }
            LocalWorkerAction::Stop => {
                let service = open_service(Access::Stop)?;
                stop_and_wait(&service)?;
            }
            LocalWorkerAction::Restart => {
                let service = open_service(Access::StartStop)?;
                stop_and_wait(&service)?;
                service
                    .start(&[] as &[&std::ffi::OsStr])
                    .context("failed to start the service")?;
                wait_for_state(&service, ScmState::Running)?;
            }
        }
        status(app).await
    }
}

/// Swaps in the bundled `gale-worker.exe` and re-registers the service,
/// keeping the installed config, credentials, and journal. Used when the
/// app update shipped a newer worker binary than the service runs.
pub async fn update(app: &AppHandle) -> Result<LocalWorkerStatus> {
    #[cfg(not(windows))]
    {
        let _ = app;
        bail!("the managed worker is only supported on Windows")
    }
    #[cfg(windows)]
    {
        let log = std::env::temp_dir().join(format!("gale-worker-update-{}.log", Uuid::new_v4()));
        let args = format!("service reinstall --log \"{}\"", log.display());
        let exit = {
            let exe = worker_exe()?;
            tokio::task::spawn_blocking(move || run_elevated(&exe, &args))
                .await
                .context("elevated update task failed")?
        }?;
        if exit != 0 {
            let detail = read_log(&log).unwrap_or_default();
            bail!("worker update failed (exit {exit}): {}", detail.trim());
        }
        let _ = std::fs::remove_file(&log);
        status(app).await
    }
}

/// Stops and removes the service and its state directory through one
/// elevated step, then clears this profile's hosted-worker settings so it
/// falls back to Local mode.
pub async fn uninstall(app: &AppHandle) -> Result<LocalWorkerStatus> {
    #[cfg(not(windows))]
    {
        let _ = app;
        bail!("the managed worker is only supported on Windows")
    }
    #[cfg(windows)]
    {
        let log =
            std::env::temp_dir().join(format!("gale-worker-uninstall-{}.log", Uuid::new_v4()));
        let args = format!("service uninstall --log \"{}\"", log.display());
        let exit = {
            let exe = worker_exe()?;
            tokio::task::spawn_blocking(move || run_elevated(&exe, &args))
                .await
                .context("elevated uninstall task failed")?
        }?;
        if exit != 0 {
            let detail = read_log(&log).unwrap_or_default();
            bail!("worker uninstall failed (exit {exit}): {}", detail.trim());
        }
        let _ = std::fs::remove_file(&log);

        // Revert the profile's settings only if they still point at the
        // managed worker — a user may have switched modes already.
        let profile_id = app.lock_manager().active_profile().id;
        let mut remote = {
            let manager = app.lock_manager();
            let (_, profile) = manager.profile_by_id(profile_id)?;
            profile.server_settings.clone().unwrap_or_default().remote
        };
        if remote.sync_mode == SyncMode::Worker && remote.worker.hosted {
            remote.sync_mode = SyncMode::Local;
            remote.worker = WorkerSettings::default();
            save_remote_settings_for(app, profile_id, remote)?;
            let secrets = ServerSecrets::for_profile(profile_id)?;
            persist_credential(&secrets, Some(ServerSecret::WorkerToken), "", false)?;
        }

        status(app).await
    }
}

// ---------- Windows service plumbing ----------

#[cfg(windows)]
enum Access {
    Start,
    Stop,
    StartStop,
}

#[cfg(windows)]
fn service_manager() -> Result<windows_service::service_manager::ServiceManager> {
    windows_service::service_manager::ServiceManager::local_computer(
        None::<&str>,
        windows_service::service_manager::ServiceManagerAccess::CONNECT,
    )
    .context("failed to open the service control manager")
}

/// `None` when the service does not exist; errors stay errors.
#[cfg(windows)]
fn try_open_service(
    access: windows_service::service::ServiceAccess,
) -> Result<Option<windows_service::service::Service>> {
    match service_manager()?.open_service(local::SERVICE_NAME, access) {
        Ok(service) => Ok(Some(service)),
        Err(windows_service::Error::Winapi(err)) if err.raw_os_error() == Some(1060) => Ok(None),
        Err(err) => Err(err).context("failed to open the worker service"),
    }
}

#[cfg(windows)]
fn open_service(access: Access) -> Result<windows_service::service::Service> {
    use windows_service::service::ServiceAccess;
    let access = match access {
        Access::Start => ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        Access::Stop => ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        Access::StartStop => {
            ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS
        }
    };
    try_open_service(access)?.ok_or_eyre("the managed worker is not installed")
}

#[cfg(windows)]
fn service_state() -> Result<ServiceState> {
    use windows_service::service::{ServiceAccess, ServiceState as ScmState};
    let Some(service) = try_open_service(ServiceAccess::QUERY_STATUS)? else {
        return Ok(ServiceState::NotInstalled);
    };
    let state = service
        .query_status()
        .context("failed to query the worker service")?
        .current_state;
    Ok(match state {
        ScmState::Stopped => ServiceState::Stopped,
        ScmState::StartPending => ServiceState::StartPending,
        ScmState::Running => ServiceState::Running,
        ScmState::StopPending => ServiceState::StopPending,
        _ => ServiceState::Other,
    })
}

#[cfg(windows)]
fn wait_for_state(
    service: &windows_service::service::Service,
    target: windows_service::service::ServiceState,
) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    loop {
        let state = service.query_status()?.current_state;
        if state == target {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("the worker service did not reach {target:?} (state: {state:?})");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[cfg(windows)]
fn stop_and_wait(service: &windows_service::service::Service) -> Result<()> {
    use windows_service::service::ServiceState as ScmState;
    if service.query_status()?.current_state == ScmState::Stopped {
        return Ok(());
    }
    if let Err(err) = service.stop()
        && service.query_status()?.current_state != ScmState::Stopped
    {
        return Err(err).context("failed to stop the service");
    }
    wait_for_state(service, ScmState::Stopped)
}

/// Runs `gale-worker <args>` elevated through the UAC prompt and waits
/// for it. This is the one privileged step in the lifecycle; everything
/// else goes through the service's unelevated control rights.
#[cfg(windows)]
fn run_elevated(exe: &std::path::Path, params: &str) -> Result<u32> {
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
        "the elevation prompt was cancelled or failed — installing the worker needs it once",
    )?;

    ensure!(
        !info.hProcess.is_invalid(),
        "the elevated worker installer produced no process handle"
    );

    let code = unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code
    };
    Ok(code)
}

// ---------- shared helpers ----------

#[cfg(windows)]
fn read_log(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The directory the staged config + secrets are written to. Temp works
/// because the elevated helper can read it regardless of ACLs elsewhere.
#[cfg(windows)]
struct Staging {
    path: PathBuf,
}

#[cfg(windows)]
impl Staging {
    fn create(
        config: &WorkerConfig,
        secrets: &Secrets,
        ssh_key: Option<&std::path::Path>,
    ) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("gale-worker-provision-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).context("failed to create the worker staging directory")?;
        config.save(&path.join(local::CONFIG_FILE))?;
        std::fs::write(path.join(local::SECRETS_FILE), secrets.render())
            .context("failed to write staged secrets")?;
        if let Some(source) = ssh_key {
            std::fs::copy(source, path.join(local::SSH_KEY_FILE))
                .context("failed to stage the SSH private key")?;
        }
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The installed worker's binding, read leniently — status must degrade
/// gracefully when the config was written by a different version.
#[cfg(windows)]
fn installed_binding() -> Option<WorkerBinding> {
    let path = local::root_dir().join(local::CONFIG_FILE);
    let bytes = std::fs::read(&path).ok()?;
    let config: WorkerConfig = serde_json::from_slice(&bytes).ok()?;
    Some(WorkerBinding {
        worker_id: config.worker_id,
        profile_id: config.profile_id,
        listen: config.listen.clone(),
        address: format!("http://{}", config.listen),
    })
}

/// The port the existing install listens on, so reprovisioning keeps the
/// profile's worker address stable.
#[cfg(windows)]
fn installed_port() -> Option<u16> {
    installed_binding().and_then(|binding| {
        binding
            .listen
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
    })
}

/// The last run report the worker wrote, if any.
#[cfg(windows)]
fn read_run_report() -> Option<WorkerRunReport> {
    let path = local::root_dir().join(local::STATUS_FILE);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// First free port in the managed range; `preferred` wins when free so a
/// reprovisioned worker keeps the same address.
#[cfg(windows)]
fn pick_port(preferred: Option<u16>) -> u16 {
    let free = |port: u16| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
    if let Some(port) = preferred
        && free(port)
    {
        return port;
    }
    for port in local::PORT_RANGE {
        if free(port) {
            return port;
        }
    }
    local::DEFAULT_PORT
}

/// True when the installed service binary differs from the bundled one —
/// i.e. a Gale update shipped a newer worker that has not been rolled
/// out. Byte comparison is deliberate: dev builds carry no version.
#[cfg(windows)]
fn worker_update_available() -> bool {
    let Ok(bundled) = worker_exe() else {
        return false;
    };
    let installed = local::root_dir().join(local::WORKER_EXE);
    match (std::fs::read(&bundled), std::fs::read(&installed)) {
        (Ok(bundled), Ok(installed)) => bundled != installed,
        _ => false,
    }
}

/// `gale-worker.exe` next to the app binary (packaged installs), falling
/// back to cargo build outputs for development.
#[cfg(windows)]
fn worker_exe() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(local::WORKER_EXE);
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for profile in ["release", "debug"] {
        let candidate = manifest
            .join("target")
            .join(profile)
            .join(local::WORKER_EXE);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "{} was not found next to Gale or in target/ — package it with the app or run `cargo build --features worker --bin gale-worker`",
        local::WORKER_EXE
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn pick_port_prefers_the_existing_install_and_skips_taken_ports() {
        // A free preferred port always wins, so reprovisioning keeps the
        // profile's worker address stable.
        assert_eq!(pick_port(Some(local::DEFAULT_PORT)), local::DEFAULT_PORT);

        // With the preferred port taken, the probe moves down the range.
        let blocker = std::net::TcpListener::bind(("127.0.0.1", local::DEFAULT_PORT)).unwrap();
        let port = pick_port(Some(local::DEFAULT_PORT));
        assert_ne!(port, local::DEFAULT_PORT);
        assert!(local::PORT_RANGE.contains(&port));
        drop(blocker);
    }
}
