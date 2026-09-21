//! Windows Service Control Manager integration for `gale-worker`.
//!
//! Three entry points, all subcommands of the worker binary:
//!
//! - `gale-worker service --config X --log-file Y` is what the SCM
//!   launches. It connects to the service dispatcher, reports status, and
//!   maps STOP/SHUTDOWN controls onto the worker's cancellation token so
//!   the HTTP server drains and the poll loop exits promptly.
//! - `gale-worker service install --from <dir> --log <file>` is the
//!   elevated provisioning step Gale runs once. It lays out
//!   `%ProgramData%\Gale\worker`, locks down ACLs, registers the service
//!   (auto-start, restart-on-crash), and starts it.
//! - `gale-worker service uninstall --log <file>` stops and deletes the
//!   service and removes the state directory.
//!
//! The install/uninstall steps write progress to `--log` because they run
//! elevated and headless; the file is how the unelevated caller learns
//! what went wrong.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eyre::{Context, Result, bail, ensure};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::api::WorkerRunPhase;
use super::config::WorkerConfig;
use super::journal;
use super::local;
use super::secrets::Secrets;
use super::server;

const DISPLAY_NAME: &str = "Gale sync worker";
const DESCRIPTION: &str =
    "Deploys Gale profile publications to a dedicated server. Managed by the Gale app.";
/// Windows ERROR_SERVICE_DOES_NOT_EXIST.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
/// Windows ERROR_SERVICE_MARKED_FOR_DELETE — returned while any handle
/// to the service remains open after `delete()`.
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
/// How long to wait for service state transitions.
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(45);

/// The SDDL DACL applied to the service. It is the Windows default plus
/// start/stop rights for interactive users, so Gale can control the
/// worker without elevation. Configuration changes still need admin.
const SERVICE_SDDL: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)\
(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)\
(A;;CCLCSWLOCRRCRPWP;;;IU)\
(A;;CCLCSWLOCRRC;;;SU)";

// ---------- service entry ----------

#[derive(Clone)]
struct ServiceArgs {
    config: PathBuf,
    log_file: PathBuf,
}

static SERVICE_ARGS: std::sync::OnceLock<ServiceArgs> = std::sync::OnceLock::new();

windows_service::define_windows_service!(ffi_service_main, service_main);

/// The `gale-worker service` entrypoint. The SCM launches the binary with
/// these arguments; `service_dispatcher::start` then hands the process
/// over to the control manager and blocks until the service stops.
pub fn run_as_service(config: PathBuf, log_file: PathBuf) -> Result<()> {
    SERVICE_ARGS
        .set(ServiceArgs { config, log_file })
        .map_err(|_| eyre::eyre!("service arguments already set"))?;
    service_dispatcher::start(local::SERVICE_NAME, ffi_service_main).context(
        "failed to connect to the service control manager; this command must run as the GaleWorker service",
    )
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(err) = service_main_inner() {
        // The dispatcher reports the failure to the SCM; the log file is
        // where the detail lives.
        error!(error = format!("{err:#}"), "service failed");
    }
}

fn service_main_inner() -> Result<()> {
    let args = SERVICE_ARGS
        .get()
        .ok_or_else(|| eyre::eyre!("service arguments missing"))?
        .clone();
    init_file_log(&args.log_file);

    let shutdown = CancellationToken::new();
    let os_shutdown = Arc::new(AtomicBool::new(false));

    let (token, flag) = (shutdown.clone(), os_shutdown.clone());
    let status_handle = service_control_handler::register(local::SERVICE_NAME, move |event| {
        match event {
            ServiceControl::Stop => {
                token.cancel();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Shutdown => {
                flag.store(true, Ordering::SeqCst);
                token.cancel();
                ServiceControlHandlerResult::NoError
            }
            // The SCM expects every service to answer Interrogate.
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })?;

    let set_status = |state: ServiceState,
                      accepted: ServiceControlAccept,
                      code: u32,
                      checkpoint: u32,
                      wait_hint: Duration| {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(code),
            checkpoint,
            wait_hint,
            process_id: None,
        })
    };

    // StartPending covers all of initialization: config and secrets
    // loading, the instance lock, the journal, and the API bind. The SCM
    // only sees Running once `server::run` signals readiness, so a failed
    // start is reported as Stopped with a nonzero code — which the
    // configured failure actions count like any other failure.
    set_status(
        ServiceState::StartPending,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
        1,
        Duration::from_secs(15),
    )?;

    let loaded = WorkerConfig::load(&args.config)
        .and_then(|config| Secrets::resolve(&config).map(|secrets| (config, secrets)));

    let (config, result) = match loaded {
        Ok((config, secrets)) => {
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build worker runtime")?
                .block_on(async {
                    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
                    let fut =
                        server::run(config.clone(), secrets, shutdown.clone(), Some(ready_tx));
                    tokio::pin!(fut);
                    tokio::select! {
                        // The run future completing before the ready
                        // signal means initialization failed; propagate
                        // its error rather than reporting Running.
                        res = &mut fut => res,
                        msg = ready_rx => {
                            if msg.is_ok() {
                                set_status(
                                    ServiceState::Running,
                                    ServiceControlAccept::STOP
                                        | ServiceControlAccept::SHUTDOWN,
                                    0,
                                    0,
                                    Duration::ZERO,
                                )?;
                            }
                            fut.await
                        }
                    }
                });
            (Some(config), result)
        }
        Err(err) => (None, Err(err)),
    };

    if let Some(config) = &config {
        // `server::run` writes Stopped on its own exit path; write the
        // terminal report here too so init failures don't leave a stale
        // `running` from a previous crash, and so an OS shutdown is
        // distinguishable from a requested stop.
        let phase = if os_shutdown.load(Ordering::SeqCst) {
            WorkerRunPhase::Shutdown
        } else {
            WorkerRunPhase::Stopped
        };
        server::report_run_state(config, phase);
    }

    let code = if result.is_ok() { 0 } else { 1 };
    if let Err(err) = set_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        code,
        0,
        Duration::ZERO,
    ) {
        warn!(%err, "failed to report stopped status");
    }
    result
}

/// Service logs go to a file — there is no console or stderr to see.
fn init_file_log(path: &Path) {
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(move || file.try_clone().expect("log handle"))
        .init();
}

// ---------- install / uninstall ----------

/// Logs a line to the provisioning log *and* the process output, so both
/// the unelevated caller and a manual run see the same record.
fn provision_log(log: &Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(file, "{line}");
    }
    eprintln!("{line}");
}

fn provision_fail(log: &Path, err: &eyre::Report) -> eyre::Report {
    provision_log(log, &format!("error: {err:#}"));
    eyre::eyre!("{err:#}")
}

/// `gale-worker service install --from <staging> --log <file>`.
///
/// `staging` holds the `gale-worker.json` and `secrets.env` the desktop
/// prepared. Everything after this point runs elevated, so it must not
/// read user-specific state beyond those staged files.
pub fn install(staging: &Path, log: &Path) -> Result<()> {
    if let Err(err) = install_inner(staging, log) {
        return Err(provision_fail(log, &err));
    }
    provision_log(log, "installed and started");
    Ok(())
}

fn install_inner(staging: &Path, log: &Path) -> Result<()> {
    // Validate the staged payload before touching anything on disk.
    let config = WorkerConfig::load(&staging.join(local::CONFIG_FILE))
        .context("staged worker config is invalid")?;
    let staged_secrets = Secrets::load_file(&staging.join(local::SECRETS_FILE))
        .context("staged secrets are unreadable")?;
    ensure!(
        staged_secrets
            .token
            .as_deref()
            .is_some_and(|token| !token.is_empty()),
        "staged secrets.env is missing {}",
        super::secrets::ENV_TOKEN
    );

    let root = local::root_dir();
    let private = local::private_dir();
    std::fs::create_dir_all(&private)
        .with_context(|| format!("failed to create {}", private.display()))?;

    apply_directory_acls(&root, &private).context("failed to set directory permissions")?;

    // A different binding makes the old journal meaningless — its pending
    // work and rotated token belong to another profile/remote. Read the
    // old binding before stopping the old service so nothing rewrites
    // the journal mid-check.
    let installed = root.join(local::CONFIG_FILE);
    let binding_changed = std::fs::read(&installed)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<WorkerConfig>(&bytes).ok())
        .is_some_and(|old| {
            old.profile_id != config.profile_id
                || old.game != config.game
                || old.remote.describe_target() != config.remote.describe_target()
        });

    let manager = open_manager_for_create()?;
    remove_existing_service(&manager, log)?;

    if binding_changed {
        provision_log(log, "binding changed; clearing prior worker state");
        let _ = std::fs::remove_file(private.join(journal::JOURNAL_FILE));
        let _ = std::fs::remove_file(root.join(local::STATUS_FILE));
    }

    // Files copied after the ACLs are set inherit the locked-down set.
    config
        .save(&installed)
        .context("failed to install worker config")?;
    std::fs::copy(
        staging.join(local::SECRETS_FILE),
        private.join(local::SECRETS_FILE),
    )
    .context("failed to install worker secrets")?;
    // A staged SSH key accompanies private-key configs — the service
    // cannot reach user-profile paths as LocalSystem.
    let staged_key = staging.join(local::SSH_KEY_FILE);
    if staged_key.is_file() {
        std::fs::copy(&staged_key, private.join(local::SSH_KEY_FILE))
            .context("failed to install the SSH private key")?;
    }

    // The service runs a copy in ProgramData, not the bundled binary:
    // Gale updates can then replace the bundled exe without fighting a
    // file lock on the running service.
    let exe = install_worker_binary(&root)?;
    let service = create_service(&manager, &exe, &root)?;
    configure_recovery(&service).context("failed to configure crash recovery")?;
    grant_user_control().context("failed to grant user control")?;

    provision_log(log, "starting service");
    service
        .start(&[] as &[&OsStr])
        .context("failed to start the service")?;
    wait_for_state(&service, ServiceState::Running)?;
    // The service reports Running only after the API is bound, but a
    // crash between the report and now would leave SCM's view stale —
    // confirm the port actually answers before declaring success.
    await_listen_ready(&config.listen)?;
    Ok(())
}

/// `gale-worker service uninstall --log <file>`. Stops and deletes the
/// service, then removes `%ProgramData%\Gale\worker` — durable state goes
/// with it, so migration docs tell users to copy it first.
pub fn uninstall(log: &Path) -> Result<()> {
    if let Err(err) = uninstall_inner(log) {
        return Err(provision_fail(log, &err));
    }
    provision_log(log, "uninstalled");
    Ok(())
}

fn uninstall_inner(log: &Path) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to open the service control manager")?;

    match manager.open_service(
        local::SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(service) => {
            provision_log(log, "stopping service");
            stop_and_wait(&service)?;
            service.delete().context("failed to delete the service")?;
            // The handle must close before the deletion can complete —
            // while any handle is open the service lingers in the
            // marked-for-delete state.
            drop(service);
            wait_until_deleted(&manager)?;
        }
        Err(err) if is_not_found(&err) => {}
        Err(err) => return Err(err).context("failed to open the service"),
    }

    let root = local::root_dir();
    if root.exists() {
        // The just-stopped process may still hold the log/journal briefly;
        // retry before giving up.
        let mut last_err = None;
        for _ in 0..5 {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(err) => {
                    last_err = Some(err);
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        if let Some(err) = last_err {
            return Err(err)
                .with_context(|| format!("failed to remove worker directory {}", root.display()));
        }
    }
    Ok(())
}

fn is_not_found(err: &windows_service::Error) -> bool {
    matches!(err, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST))
}

fn stop_and_wait(service: &windows_service::service::Service) -> Result<()> {
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

fn wait_for_state(service: &windows_service::service::Service, target: ServiceState) -> Result<()> {
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

fn wait_until_deleted(manager: &ServiceManager) -> Result<()> {
    let deadline = Instant::now() + TRANSITION_TIMEOUT;
    loop {
        match manager.open_service(local::SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Err(err) if is_not_found(&err) => return Ok(()),
            // 1072 = marked for delete: another handle (often the SCM's
            // own) is still open; the deletion completes once it closes.
            Err(windows_service::Error::Winapi(io))
                if io.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE) =>
            {
                if Instant::now() > deadline {
                    bail!("service deletion did not complete within {TRANSITION_TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return Err(err).context("failed to query service deletion"),
            Ok(service) => {
                drop(service);
                if Instant::now() > deadline {
                    bail!("service was still present after the delete timeout")
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

fn open_manager_for_create() -> Result<ServiceManager> {
    ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("failed to open the service control manager")
}

/// Stops and deletes the service when it already exists, so installs,
/// reinstalls, and upgrades always end with a fresh registration.
fn remove_existing_service(manager: &ServiceManager, log: &Path) -> Result<()> {
    match manager.open_service(
        local::SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(existing) => {
            provision_log(log, "removing existing service registration");
            stop_and_wait(&existing)?;
            existing
                .delete()
                .context("failed to delete the old service")?;
            drop(existing);
            wait_until_deleted(manager)
        }
        Err(err) if is_not_found(&err) => Ok(()),
        Err(err) => Err(err).context("failed to open the existing service"),
    }
}

/// Copies the running binary (the bundled `gale-worker.exe` Gale invoked
/// elevated) into the worker root. The copy may still be locked by a
/// service that was just stopped, so this retries briefly.
fn install_worker_binary(root: &Path) -> Result<PathBuf> {
    let source = std::env::current_exe().context("failed to resolve gale-worker.exe")?;
    let target = root.join(local::WORKER_EXE);
    if source == target {
        return Ok(target);
    }
    let mut last_err = None;
    for _ in 0..10 {
        match std::fs::copy(&source, &target) {
            Ok(_) => return Ok(target),
            Err(err) => {
                last_err = Some(err);
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
    Err(last_err.expect("copy attempted")).context("failed to install the worker binary")
}

fn create_service(
    manager: &ServiceManager,
    exe: &Path,
    root: &Path,
) -> Result<windows_service::service::Service> {
    let info = ServiceInfo {
        name: OsString::from(local::SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec![
            OsString::from("service"),
            OsString::from("--config"),
            root.join(local::CONFIG_FILE).into_os_string(),
            OsString::from("--log-file"),
            root.join(local::LOG_FILE).into_os_string(),
        ],
        dependencies: vec![],
        // LocalSystem: the service must reach its ProgramData state and
        // read the staged SSH key without a logged-in session.
        account_name: None,
        account_password: None,
    };
    let service = manager
        .create_service(
            &info,
            ServiceAccess::QUERY_STATUS
                | ServiceAccess::START
                | ServiceAccess::STOP
                | ServiceAccess::DELETE
                | ServiceAccess::CHANGE_CONFIG,
        )
        .context("failed to create the service")?;
    service
        .set_description(DESCRIPTION)
        .context("failed to set the service description")?;
    Ok(service)
}

/// `gale-worker service reinstall --log <file>`: re-registers the service
/// from the *installed* config and swaps in this binary. Gale uses it to
/// roll out an updated worker without re-running the OAuth provisioning.
pub fn reinstall(log: &Path) -> Result<()> {
    if let Err(err) = reinstall_inner(log) {
        return Err(provision_fail(log, &err));
    }
    provision_log(log, "reinstalled and started");
    Ok(())
}

fn reinstall_inner(log: &Path) -> Result<()> {
    let root = local::root_dir();
    let private = local::private_dir();
    ensure!(
        root.join(local::CONFIG_FILE).is_file(),
        "the worker is not installed"
    );
    let config = WorkerConfig::load(&root.join(local::CONFIG_FILE))
        .context("installed worker config is invalid")?;

    let manager = open_manager_for_create()?;
    remove_existing_service(&manager, log)?;
    apply_directory_acls(&root, &private).context("failed to set directory permissions")?;

    let exe = install_worker_binary(&root)?;
    let service = create_service(&manager, &exe, &root)?;
    configure_recovery(&service).context("failed to configure crash recovery")?;
    grant_user_control().context("failed to grant user control")?;

    provision_log(log, "starting service");
    service
        .start(&[] as &[&OsStr])
        .context("failed to start the service")?;
    wait_for_state(&service, ServiceState::Running)?;
    await_listen_ready(&config.listen)
}

/// Confirms the worker's listen address accepts TCP connections. SCM
/// state alone cannot prove the API survived past the Running report —
/// a crashed process can linger in Running briefly.
fn await_listen_ready(listen: &str) -> Result<()> {
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

/// SCM-managed crash recovery: restart after 5s, then 15s, then every 60s.
/// The failure counter resets after a day without crashes, and nonzero
/// exits that were reported cleanly still count as failures.
fn configure_recovery(service: &windows_service::service::Service) -> Result<()> {
    service.set_failure_actions_on_non_crash_failures(true)?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(15),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(60),
            },
        ]),
    })?;
    Ok(())
}

/// `sc.exe sdset` is the supported way to replace a service DACL; the
/// windows-service crate has no security-descriptor API.
fn grant_user_control() -> Result<()> {
    let output = std::process::Command::new("sc.exe")
        .args(["sdset", local::SERVICE_NAME, SERVICE_SDDL])
        .output()
        .context("failed to run sc.exe sdset")?;
    ensure!(
        output.status.success(),
        "sc.exe sdset failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}

/// Locks down the worker directory. Well-known SIDs (`*S-1-...`) are used
/// instead of group names because icacls localizes names on non-English
/// Windows.
fn apply_directory_acls(root: &Path, private: &Path) -> Result<()> {
    // Root: SYSTEM + Administrators full control, all users read — Gale
    // reads status.json and worker.log unelevated.
    icacls(
        root,
        &[
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-32-545:(OI)(CI)RX",
        ],
    )?;
    // Private: SYSTEM + Administrators only — secrets.env and the journal
    // carry the worker's bearer token and sync refresh token.
    icacls(private, &["*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
}

fn icacls(path: &Path, grants: &[&str]) -> Result<()> {
    let mut args: Vec<std::ffi::OsString> = vec![
        path.as_os_str().to_owned(),
        OsString::from("/inheritance:r"),
        OsString::from("/grant:r"),
    ];
    args.extend(grants.iter().map(OsString::from));
    let output = std::process::Command::new("icacls")
        .args(&args)
        .output()
        .context("failed to run icacls")?;
    ensure!(
        output.status.success(),
        "icacls failed for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::await_listen_ready;

    /// Install/start trust a real probe, not a transient SCM state: a
    /// bound port passes immediately, a dead one fails rather than
    /// reporting success.
    #[test]
    fn listen_readiness_distinguishes_bound_from_closed_ports() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ready = listener.local_addr().unwrap().to_string();
        assert!(await_listen_ready(&ready).is_ok());

        drop(listener);
        let err = await_listen_ready(&ready).unwrap_err();
        assert!(
            err.to_string().contains("not answering"),
            "unexpected error: {err:#}"
        );
    }
}
