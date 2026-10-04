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
use sha1::{Digest, Sha1};
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
use super::local::{self, TRANSITION_TIMEOUT, await_listen_ready, stop_and_wait, wait_for_state};
use super::named_mutex::NamedMutex;
use super::secrets::Secrets;
use super::server;

const DISPLAY_NAME: &str = "Gale sync worker";
const DESCRIPTION: &str =
    "Deploys Gale profile publications to a dedicated server. Managed by the Gale app.";
/// Windows ERROR_SERVICE_MARKED_FOR_DELETE, returned while any handle
/// to the service remains open after `delete()`.
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
/// Serializes elevated install, reinstall, and uninstall. Taken inside
/// those processes so the lock outlives the unelevated caller that showed
/// UAC. Session-local: the consenting user's elevated process can open it.
/// Callers must not hold this name across the launch, or the child waits
/// on its parent until the timeout.
const UPDATE_MUTEX: &str = r"Local\GaleWorkerUpdate";
/// Covers one install or reinstall that stops, replaces, and starts the
/// service. A waiter past this fails closed instead of overlapping.
const UPDATE_LOCK_TIMEOUT: Duration = Duration::from_secs(3 * 60);

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

    run_service(&args, shutdown, os_shutdown, |status| {
        status_handle.set_service_status(status)
    })
}

/// Runs the lifecycle after SCM registration, reporting every transition
/// through the supplied SCM status sink.
fn run_service(
    args: &ServiceArgs,
    shutdown: CancellationToken,
    os_shutdown: Arc<AtomicBool>,
    report: impl Fn(ServiceStatus) -> windows_service::Result<()>,
) -> Result<()> {
    let set_status = |state: ServiceState,
                      accepted: ServiceControlAccept,
                      code: u32,
                      checkpoint: u32,
                      wait_hint: Duration| {
        report(ServiceStatus {
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
    // start is reported as Stopped with a nonzero code, which the
    // configured failure actions count like any other failure.
    set_status(
        ServiceState::StartPending,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
        1,
        Duration::from_secs(15),
    )?;

    // Everything fallible that remains is inside `load_and_build`, so no
    // `?` can bypass the Stopped/nonzero finalizer below, including
    // Tokio runtime construction.
    let (config, result) = match load_and_build(args) {
        Ok(init) => {
            let config = init.config.clone();
            let result = init.runtime.block_on(async {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
                let fut = server::run(
                    config.clone(),
                    init.secrets,
                    shutdown.clone(),
                    Some(ready_tx),
                );
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

/// Everything the worker needs to start serving, collected as one
/// fallible unit. `service_main_inner` calls this after reporting
/// StartPending so that failures in config, secrets, or Tokio runtime
/// construction reach the common
/// Stopped/nonzero finalizer rather than escaping through `?`.
struct WorkerInit {
    config: WorkerConfig,
    secrets: Secrets,
    runtime: tokio::runtime::Runtime,
}

fn load_and_build(args: &ServiceArgs) -> Result<WorkerInit> {
    let config = WorkerConfig::load(&args.config)?;
    let secrets = Secrets::resolve(&config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build worker runtime")?;
    Ok(WorkerInit {
        config,
        secrets,
        runtime,
    })
}

/// Service logs go to a file because the service has no visible console or stderr.
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

/// The desktop reads the provisioning log, not the elevated process's
/// stderr, so a lock failure has to be written there before it returns.
fn lock_service_change(log: &Path) -> Result<NamedMutex> {
    let acquire = || {
        let lock =
            NamedMutex::open(UPDATE_MUTEX).context("failed to open the worker update lock")?;
        let wait = |timeout| {
            lock.wait(timeout)
                .context("failed to acquire the worker update lock")
        };
        if !wait(Duration::ZERO)? {
            provision_log(
                log,
                "waiting for another Worker install, update, or uninstall to finish",
            );
            ensure!(
                wait(UPDATE_LOCK_TIMEOUT)?,
                "another Worker install, update, or uninstall is still running. Wait for it to finish and try again"
            );
        }
        Ok(lock)
    };
    acquire().map_err(|err| provision_fail(log, &err))
}

/// `gale-worker service install --from <staging> --log <file>`.
///
/// `staging` holds the `gale-worker.json` and `secrets.env` the desktop
/// prepared. Everything after this point runs elevated, so it must not
/// read user-specific state beyond those staged files.
pub fn install(staging: &Path, log: &Path) -> Result<()> {
    let _lock = lock_service_change(log)?;
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

    // A different binding makes the old journal meaningless. Its pending
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
                || old.remote.transport.describe_target()
                    != config.remote.transport.describe_target()
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
    install_secrets(staged_secrets, &private, &config.state_dir)?;
    // A staged SSH key accompanies private-key configs. The service's
    // virtual account cannot read user-profile paths.
    let staged_key = staging.join(local::SSH_KEY_FILE);
    if staged_key.is_file() {
        std::fs::copy(&staged_key, private.join(local::SSH_KEY_FILE))
            .context("failed to install the SSH private key")?;
    }

    start_installed_worker(&manager, &root, &config.listen, log)
}

/// The managed worker's sync credential lives only in the journal: the
/// journal rotates it on every grant, so a copy left in secrets.env would
/// go stale and be mistaken for a new sign-in after a rejection latch.
fn install_secrets(mut staged: Secrets, private: &Path, state_dir: &Path) -> Result<()> {
    let refresh_token = staged.refresh_token.take();
    std::fs::write(private.join(local::SECRETS_FILE), staged.render()?)
        .context("failed to install worker secrets")?;
    if let Some(token) = refresh_token {
        // Setup obtains a new login. An old rotated token must not override
        // it when the retained journal is loaded on the next start.
        let journal = journal::Journal::load(state_dir)?;
        let mut state = journal.state.blocking_lock();
        state.replace_sync_credential(token);
        journal.save(&state)?;
    }
    Ok(())
}

fn start_installed_worker(
    manager: &ServiceManager,
    root: &Path,
    listen: &str,
    log: &Path,
) -> Result<()> {
    let exe = install_worker_binary(root)?;
    let service = create_service(manager, &exe, root)?;
    configure_recovery(&service).context("failed to configure crash recovery")?;
    grant_user_control().context("failed to grant user control")?;
    provision_log(log, "starting service");
    service
        .start(&[] as &[&OsStr])
        .context("failed to start the service")?;
    wait_for_state(&service, ServiceState::Running)?;
    await_listen_ready(listen)
}

/// `gale-worker service uninstall --log <file>`. Stops and deletes the
/// service, then removes `%ProgramData%\Gale\worker`. This deletes durable
/// state, so migration docs tell users to copy it first.
pub fn uninstall(log: &Path) -> Result<()> {
    let _lock = lock_service_change(log)?;
    if let Err(err) = uninstall_inner(log) {
        return Err(provision_fail(log, &err));
    }
    provision_log(log, "uninstalled");
    Ok(())
}

fn uninstall_inner(log: &Path) -> Result<()> {
    let manager = local::service_manager()?;

    remove_existing_service(&manager, log)?;

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

fn wait_until_deleted(manager: &ServiceManager) -> Result<()> {
    let deadline = Instant::now() + TRANSITION_TIMEOUT;
    loop {
        match manager.open_service(local::SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Err(err) if local::is_not_installed(&err) => return Ok(()),
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
        Err(err) if local::is_not_installed(&err) => Ok(()),
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
        // The service's own virtual account. It holds no rights beyond the
        // worker directory, so a compromised worker cannot take over the
        // machine the way it could as LocalSystem.
        account_name: Some(OsString::from(format!(
            r"NT SERVICE\{}",
            local::SERVICE_NAME
        ))),
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
    let _lock = lock_service_change(log)?;
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

    start_installed_worker(&manager, &root, &config.listen, log)
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

/// The SID of the service's virtual account, `NT SERVICE\<name>`, derived
/// the way Windows derives service SIDs: `S-1-5-80` followed by the SHA-1
/// of the upper-cased UTF-16 service name, read as five little-endian
/// 32-bit values. Granting by SID works before the service exists.
fn service_sid(name: &str) -> String {
    let utf16: Vec<u8> = name
        .to_uppercase()
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let hash = Sha1::digest(&utf16);
    let parts = hash
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk).to_string());

    std::iter::once("S-1-5-80".to_owned())
        .chain(parts)
        .collect::<Vec<_>>()
        .join("-")
}

/// Locks down the worker directory. Well-known SIDs (`*S-1-...`) are used
/// instead of group names because icacls localizes names on non-English
/// Windows.
fn apply_directory_acls(root: &Path, private: &Path) -> Result<()> {
    let service = format!("*{}:(OI)(CI)M", service_sid(local::SERVICE_NAME));
    // Root: SYSTEM + Administrators full control, all users read, and the
    // service writes its status and log. Gale reads status.json and
    // worker.log unelevated.
    icacls(
        root,
        &[
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-32-545:(OI)(CI)RX",
            &service,
        ],
    )?;
    // Private: SYSTEM + Administrators + the service only. secrets.env and
    // the journal carry the worker's bearer token and sync refresh token.
    icacls(
        private,
        &["*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", &service],
    )
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
        "icacls failed for {} ({}): {}{}",
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ServiceArgs, await_listen_ready, service_sid};

    #[test]
    fn service_sid_matches_the_sid_windows_assigns() {
        // Windows' own values: TrustedInstaller's documented SID, and
        // `sc showsid GaleWorker`, which works without the service.
        assert_eq!(
            service_sid("TrustedInstaller"),
            "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
        );
        assert_eq!(
            service_sid("GaleWorker"),
            "S-1-5-80-2769169290-2553713292-796222524-4265435681-1965319269"
        );
    }

    #[test]
    fn initialization_failure_reports_stopped_with_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let args = ServiceArgs {
            config: dir.path().join("worker.json"),
            log_file: dir.path().join("worker.log"),
        };
        for malformed in [false, true] {
            if malformed {
                std::fs::write(&args.config, b"{invalid").unwrap();
            }
            let statuses = std::sync::Mutex::new(Vec::new());
            let result = super::run_service(
                &args,
                tokio_util::sync::CancellationToken::new(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                |status| {
                    statuses.lock().unwrap().push(status);
                    Ok(())
                },
            );
            assert!(result.is_err());
            let statuses = statuses.into_inner().unwrap();
            assert_eq!(statuses.len(), 2);
            assert_eq!(statuses[0].current_state, super::ServiceState::StartPending);
            assert_eq!(statuses[1].current_state, super::ServiceState::Stopped);
            assert_eq!(statuses[1].exit_code, super::ServiceExitCode::Win32(1));
            assert!(statuses[1].controls_accepted.is_empty());
        }
    }

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

    #[test]
    fn update_lock_is_exclusive_until_the_holder_releases() {
        let name = r"Local\GaleWorkerUpdateTest";
        let held = super::NamedMutex::open(name).unwrap();
        assert!(
            held.wait(Duration::from_secs(5)).unwrap(),
            "the free lock should be acquired immediately"
        );

        // A Windows mutex lets the owning thread wait again without
        // blocking, so the contender has to be a different thread.
        // The handle is not `Send`, so the contender opens its own.
        let acquired = std::thread::spawn(move || {
            let contender = super::NamedMutex::open(name).unwrap();
            contender.wait(Duration::from_millis(200)).unwrap()
        })
        .join()
        .unwrap();
        assert!(
            !acquired,
            "a held lock should time out instead of proceeding"
        );

        drop(held);
        let next = super::NamedMutex::open(name).unwrap();
        assert!(
            next.wait(Duration::from_secs(5)).unwrap(),
            "releasing the holder should let the next waiter acquire"
        );
    }

    /// A same-binding re-install recovers the rejected credential: the issued
    /// sync token lands in the journal, never in secrets.env where it
    /// would go stale. The rejection clears and all operational state survives.
    #[test]
    fn same_binding_reauthorization_clears_only_the_credential_rejection() {
        use crate::profile::export::ModRevision;
        use crate::profile::server::settings::RestartPolicy;
        use crate::profile::server::state::{
            ExecutorKind, OperationKind, OperationRecord, OperationStatus, OperationSummary,
            RestartOutcome,
        };
        use crate::worker::journal::{Journal, PendingWork};
        use chrono::Utc;

        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();

        let mod_rev = |byte: char| ModRevision::try_from(byte.to_string().repeat(64)).unwrap();
        let journal = Journal::load(&state_dir).unwrap();
        let before = {
            let mut state = journal.state.blocking_lock();
            state.pending = Some(PendingWork::new(Utc::now(), mod_rev('b')));
            state.pending.as_mut().unwrap().attempts = 3;
            state.pending.as_mut().unwrap().next_attempt_at =
                Some(Utc::now() + chrono::Duration::minutes(5));
            state.last_deployed_revision = Some(Utc::now() - chrono::Duration::hours(1));
            state.deployed_mods_revision = Some(mod_rev('a'));
            state.auto_deploy_mods = true;
            state.restart_policy = RestartPolicy::WhenEmpty;
            state.automation_seeded = true;
            state.last_error = Some("automatic deployment failed: upload failed".to_owned());
            state.poll_error =
                Some("Publication check failed: sync token request failed".to_owned());
            state.last_operation = Some(OperationRecord {
                id: "op-1".to_owned(),
                executor: ExecutorKind::Worker,
                kind: OperationKind::Automatic,
                worker_id: Some("test".to_owned()),
                publication_revision: None,
                mods_revision: None,
                status: OperationStatus::Succeeded,
                summary: OperationSummary::default(),
                restart: RestartOutcome::NotRequired,
                error: None,
                started_at: Utc::now(),
                finished_at: Utc::now(),
            });
            state.refresh_token = Some("rt-9".to_owned());
            state.sync_reauthorization_required = true;
            journal.save(&state).unwrap();
            serde_json::to_value(&*state).unwrap()
        };
        drop(journal);

        super::install_secrets(
            super::Secrets {
                token: Some("api-token".to_owned()),
                remote_password: Some("remote-pw".to_owned()),
                refresh_token: Some("fresh".to_owned()),
                dat_host_password: Some("dathost-pw".to_owned()),
            },
            &private,
            &state_dir,
        )
        .unwrap();

        let installed =
            super::Secrets::load_file(&private.join(super::local::SECRETS_FILE)).unwrap();
        assert_eq!(installed.token.as_deref(), Some("api-token"));
        assert_eq!(installed.remote_password.as_deref(), Some("remote-pw"));
        assert_eq!(installed.dat_host_password.as_deref(), Some("dathost-pw"));
        // The sync credential never reaches the file, so a worker built
        // from it gets no seed that could override the journal.
        assert_eq!(installed.seed_refresh_token(), None);

        let journal = Journal::load(&state_dir).unwrap();
        let state = journal.state.blocking_lock();
        assert_eq!(state.refresh_token.as_deref(), Some("fresh"));
        assert!(!state.sync_reauthorization_required);
        assert!(state.poll_error.is_none());
        let mut after = serde_json::to_value(&*state).unwrap();
        let mut expected = before;
        expected["pollError"] = serde_json::Value::Null;
        for value in [&mut after, &mut expected] {
            let object = value.as_object_mut().unwrap();
            object.remove("refreshToken");
            object.remove("syncReauthorizationRequired");
        }
        assert_eq!(
            after, expected,
            "only the credential and its rejection state changed"
        );
    }
}
