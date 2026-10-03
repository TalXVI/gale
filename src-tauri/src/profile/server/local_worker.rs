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

use crate::worker::{
    ManagedServiceState, ServiceControlAction,
    api::{StatusResponse, WorkerRunReport},
};

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
                persist_credential, remote_credential, sync_id_for, sync_target,
                update_settings_for, worker_client,
            },
            secrets::{ServerSecret, ServerSecrets},
            settings::{
                HostProvider, RemoteAuthentication, RemoteServerSettings, SyncMode, WorkerSettings,
            },
        },
        sync::auth,
    },
    state::ManagerExt,
    worker::{config::WorkerConfig, local, secrets::Secrets, tray},
};

/// The installed worker's ownership relative to the profile being viewed.
/// There is exactly one `GaleWorker` per machine, bound to one sync
/// profile at install; it must never be silently rebound or controlled
/// by another profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkerOwnership {
    /// No worker is installed.
    None,
    /// Installed for this profile and fully bound (settings + token).
    Owned,
    /// Installed for this profile's sync id, but the desktop binding
    /// never completed. An earlier provisioning failed after the
    /// service was already running. Running setup again finishes it.
    Incomplete,
    /// Installed for a different profile. All control is refused; the
    /// owning profile manages (or uninstalls) it.
    Foreign,
}

/// Resolves who the installed binding belongs to. `bound` is whether the
/// profile's settings + keyring actually point at the installed worker.
/// A binding whose profile id does not match is foreign even when the
/// profile currently has no sync id. Never treat "unknown" as "mine".
#[cfg(windows)]
fn ownership_of(
    installed_profile: Option<&str>,
    sync_id: Option<&str>,
    bound: bool,
) -> WorkerOwnership {
    let Some(installed_profile) = installed_profile else {
        return WorkerOwnership::None;
    };
    if sync_id != Some(installed_profile) {
        return WorkerOwnership::Foreign;
    }
    if bound {
        WorkerOwnership::Owned
    } else {
        WorkerOwnership::Incomplete
    }
}

/// Managed Worker service and API status for the Server page.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalWorkerStatus {
    /// `false` off Windows. Provisioning is unavailable there.
    pub supported: bool,
    pub service: ManagedServiceState,
    /// The installed worker's loopback API address, when its config is readable.
    pub address: Option<String>,
    /// Whether the installed worker belongs to this profile. Controls
    /// must only be offered for `Owned` (or `Incomplete`, to finish
    /// setup), never for `Foreign`.
    pub ownership: WorkerOwnership,
    /// The worker's last reported run phase; a `running` report while the
    /// service is stopped means the process died unexpectedly.
    pub run: Option<WorkerRunReport>,
    /// Live API status, present when the service is running and the
    /// profile holds the worker token.
    pub worker: Option<StatusResponse>,
    /// Why `worker` is absent (unreachable, bad token, ...).
    pub worker_error: Option<String>,
    /// True when the bundled `gale-worker.exe` differs from the copy the
    /// installed service runs. An app update left a newer worker behind.
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
            service: ManagedServiceState::NotInstalled,
            address: None,
            ownership: WorkerOwnership::None,
            run: None,
            worker: None,
            worker_error: None,
            update_available: false,
            warnings: Vec::new(),
        }
    }
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
        let service_state = local::service_state()?;
        let installed = installed_config();
        let address = installed
            .as_ref()
            .map(|config| format!("http://{}", config.listen));
        let run = read_run_report();

        let profile_id = app.lock_manager().active_profile().id;
        let ownership = ownership_of(
            installed.as_ref().map(|config| config.profile_id.as_str()),
            sync_id_for(app, profile_id).as_deref(),
            address
                .as_ref()
                .is_some_and(|address| profile_bound_to(app, profile_id, address)),
        );

        let mut worker = None;
        let mut worker_error = None;
        match ownership {
            // Another profile owns the service. Never contact it with
            // this profile's credentials.
            WorkerOwnership::Foreign => {
                worker_error =
                    Some("the installed worker belongs to a different profile".to_owned());
            }
            WorkerOwnership::Incomplete => {
                worker_error =
                    Some("Setup did not finish. Run 'Set up worker' to complete it".to_owned());
            }
            WorkerOwnership::Owned if service_state == ManagedServiceState::Running => {
                let secrets = ServerSecrets::for_profile(profile_id)?;
                let token = secrets
                    .resolve(ServerSecret::WorkerToken, "")
                    .unwrap_or_default();
                if token.is_empty() {
                    worker_error = Some("this profile has no stored worker token".to_owned());
                } else {
                    let mut settings = RemoteServerSettings::default();
                    settings.worker.address = address
                        .as_ref()
                        .expect("owned implies an installed address")
                        .clone();
                    match worker_client(&secrets, &settings, &token) {
                        Ok(client) => match client.status(false).await {
                            Ok(live) => worker = Some(live),
                            Err(err) => worker_error = Some(format!("{err:#}")),
                        },
                        Err(err) => worker_error = Some(format!("{err:#}")),
                    }
                }
            }
            _ => {}
        }

        Ok(LocalWorkerStatus {
            supported: true,
            service: service_state,
            address,
            ownership,
            run,
            worker,
            worker_error,
            update_available: worker_exe()
                .is_ok_and(|bundled| local::worker_update_available(&bundled)),
            warnings: Vec::new(),
        })
    }
}

/// Whether the profile's persisted settings and keyring actually point at
/// the installed worker. This is the desktop half of the binding. Provisioning
/// can leave this incomplete if the install succeeded but saving settings
/// or the bearer token failed.
#[cfg(windows)]
fn profile_bound_to(app: &AppHandle, profile_id: i64, address: &str) -> bool {
    let bound = {
        let manager = app.lock_manager();
        manager
            .profile_by_id(profile_id)
            .ok()
            .and_then(|(_, profile)| profile.server_settings.as_ref())
            .is_some_and(|settings| {
                settings.remote.sync_mode == SyncMode::Worker
                    && settings.remote.worker.hosted
                    && settings.remote.worker.address == address
            })
    };
    let token = ServerSecrets::for_profile(profile_id)
        .and_then(|secrets| secrets.resolve(ServerSecret::WorkerToken, ""))
        .unwrap_or_default();
    bound && !token.is_empty()
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
        "sign in to Gale sync first. The worker needs its own login"
    );

    // Preflight everything that would strand a half-provisioned worker:
    // without the helper there is nothing to install, so check it before
    // OAuth or credentials are staged.
    let exe = worker_exe()?;
    // The installed worker belongs to one profile forever; refuse to
    // replace another profile's, and check again after OAuth. The prompt
    // takes minutes and the dialog could have installed one meanwhile.
    ensure!(
        installed_config().is_none_or(|config| config.profile_id == sync_id),
        "the installed worker belongs to a different profile. Manage it from that profile"
    );
    Staging::sweep_stale();

    // The embedded remote settings describe the transport; the worker
    // block inside them is meaningless to the worker itself, so the copy
    // it runs with is normalized to Local to keep validation honest.
    let mut worker_remote = target.settings.clone();
    worker_remote.sync_mode = SyncMode::Local;
    ensure!(
        worker_remote.authentication != RemoteAuthentication::Agent,
        "SSH agent authentication cannot run unattended in a service; use a password or a private key"
    );

    let port = pick_port(installed_port())?;
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
    ensure!(
        installed_config().is_none_or(|config| config.profile_id == sync_id),
        "a worker for a different profile was installed during setup. Manage it from that profile"
    );

    let config = WorkerConfig {
        worker_id: local::WORKER_ID.to_owned(),
        listen: listen.clone(),
        profile_id: sync_id,
        game: target.game.clone(),
        sync_url: None,
        remote: worker_remote,
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
        let exe = exe.clone();
        tokio::task::spawn_blocking(move || local::run_elevated_worker(&exe, &elevated_args))
            .await
            .context("elevated install task failed")?
    };
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    match install_result {
        Err(err) => {
            return Err(err.wrap_err(
                "failed to launch the elevated worker installer (the UAC prompt may have been declined)",
            ));
        }
        Ok(exit) if exit != 0 => {
            bail!(
                "worker installation failed (exit {exit}): {}",
                log_text.trim()
            );
        }
        Ok(_) if local::service_state()? != ManagedServiceState::Running => {
            bail!("the worker service did not come up: {}", log_text.trim());
        }
        Ok(_) => {}
    }
    install_tray_companion().context(
        "the Worker is running, but its notification-area companion could not be installed",
    )?;
    // The install consumed the staged payload; don't leave secret copies
    // in temp for the rest of setup.
    drop(staging);

    // The service is installed and running. Point the profile at it.
    let mut remote = target.settings.clone();
    remote.sync_mode = SyncMode::Worker;
    remote.worker.address = format!("http://{listen}");
    remote.worker.hosted = true;
    // Persisting the desktop half of the binding can still fail, leaving
    // a running worker whose profile can't reach it. Status reports it
    // as `incomplete`. Retrying setup reinstalls and relinks the worker,
    // preserving its pending work for the same profile and server.
    update_settings_for(app, target.profile_id, |settings| settings.remote = remote).context(
        "the worker is installed and running, but saving its settings failed. Run 'Set up worker' again to finish setup",
    )?;
    persist_credential(&secrets, Some(ServerSecret::WorkerToken), &token, true).context(
        "the worker is installed and running, but storing its token failed. Run 'Set up worker' again to finish setup",
    )?;

    let mut worker_status = status(app).await?;
    // A reprovisioned worker keeps its journal. The config written above
    // only seeds a first run, so the journal's automation setting is
    // authoritative even when it differs from what was just installed.
    // Mirror the worker's actual setting into the profile's settings so the
    // page and future provisioning see the same truth.
    if let Some(live) = worker_status.worker.as_ref() {
        update_settings_for(app, target.profile_id, |settings| {
            settings.remote.worker.auto_deploy_mods = live.auto_deploy_mods;
            settings.remote.restart_policy = live.restart_policy;
        })
        .context("failed to mirror the worker's automation settings")?;
    }
    // If the service enforces one session per user, the worker login may
    // have invalidated the desktop's chain; a forced grant finds out.
    if auth::verify_session(app).await.is_err() {
        worker_status.warnings.push(
            "the worker sign-in invalidated Gale's own session. Sign in to sync again".to_owned(),
        );
    }
    Ok(worker_status)
}

// ---------- control ----------

/// Starts, stops, or restarts the installed service through the SCM.
/// These calls are unelevated because installation grants interactive users control.
pub async fn control(app: &AppHandle, action: ServiceControlAction) -> Result<LocalWorkerStatus> {
    #[cfg(not(windows))]
    {
        let _ = (app, action);
        bail!("the managed worker is only supported on Windows")
    }
    #[cfg(windows)]
    {
        require_owned(app)?;
        tokio::task::spawn_blocking(move || local::control_service(action))
            .await
            .context("worker control task failed")??;
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
        require_owned(app)?;
        {
            let exe = worker_exe()?;
            let helper = tray_exe()?;
            tokio::task::spawn_blocking(move || {
                tray::UpdatePlan::new(exe, local::root_dir().join(local::WORKER_EXE), helper)?
                    .perform()
            })
            .await
            .context("elevated update task failed")?
        }?;
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
        // Capture the profile before the elevated wait. The settings
        // revert below must land on the profile that owned the worker
        // even if the user switches profiles while UAC is open.
        let profile_id = app.lock_manager().active_profile().id;
        require_owned(app)?;
        let log =
            std::env::temp_dir().join(format!("gale-worker-uninstall-{}.log", Uuid::new_v4()));
        let args = format!("service uninstall --log \"{}\"", log.display());
        let exit = {
            let exe = worker_exe()?;
            tokio::task::spawn_blocking(move || local::run_elevated_worker(&exe, &args))
                .await
                .context("elevated uninstall task failed")?
        }?;
        if exit != 0 {
            let detail = std::fs::read_to_string(&log).unwrap_or_default();
            bail!("worker uninstall failed (exit {exit}): {}", detail.trim());
        }
        let _ = std::fs::remove_file(&log);

        // Revert the profile's settings only if they still point at the
        // managed worker. A user may have switched modes already.
        let hosted = app
            .lock_manager()
            .profile_by_id(profile_id)?
            .1
            .server_settings
            .as_ref()
            .is_some_and(|settings| {
                settings.remote.sync_mode == SyncMode::Worker && settings.remote.worker.hosted
            });
        if hosted {
            update_settings_for(app, profile_id, |settings| {
                settings.remote.sync_mode = SyncMode::Local;
                settings.remote.worker = WorkerSettings::default();
            })?;
            let secrets = ServerSecrets::for_profile(profile_id)?;
            persist_credential(&secrets, Some(ServerSecret::WorkerToken), "", false)?;
        }

        tray::uninstall_for_current_user().context(
            "the Worker was removed, but its tray startup entry could not be cleaned up",
        )?;

        status(app).await
    }
}

// ---------- Windows service plumbing ----------

/// Backend ownership gate for every operation that touches the globally
/// installed `GaleWorker` service: the active profile's sync id must
/// match the installed binding. A worker bound to another profile is
/// that profile's to manage. Never controllable from here.
#[cfg(windows)]
fn require_owned(app: &AppHandle) -> Result<()> {
    let config = installed_config().ok_or_eyre("the managed worker is not installed")?;
    let profile_id = app.lock_manager().active_profile().id;
    let sync_id = sync_id_for(app, profile_id);
    ensure!(
        sync_id.as_deref() == Some(config.profile_id.as_str()),
        "the installed worker belongs to a different profile. Switch to that profile to manage it"
    );
    Ok(())
}

/// The directory the staged config + secrets are written to. Temp works
/// because the elevated helper can read it regardless of ACLs elsewhere.
///
/// Cleanup is `Drop`-based so every early return removes the secrets;
/// dirs abandoned by a killed process are reclaimed by `sweep_stale` on
/// the next provisioning attempt.
#[cfg(windows)]
struct Staging {
    path: PathBuf,
}

#[cfg(windows)]
impl Staging {
    /// How old an abandoned staging dir must be before `sweep_stale`
    /// reclaims it, long enough that a legitimately in-flight provision
    /// (OAuth + UAC round trips) is never touched.
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

    fn create(
        config: &WorkerConfig,
        secrets: &Secrets,
        ssh_key: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::create_in(&std::env::temp_dir(), config, secrets, ssh_key)
    }

    fn create_in(
        root: &std::path::Path,
        config: &WorkerConfig,
        secrets: &Secrets,
        ssh_key: Option<&std::path::Path>,
    ) -> Result<Self> {
        let path = root.join(format!("gale-worker-provision-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).context("failed to create the worker staging directory")?;
        // Arm the cleanup guard before any fallible step. A failed ACL,
        // config write, secrets write, or key copy must not strand a
        // partially staged directory.
        let staging = Self { path };
        // A bearer token and possibly an SSH private key are about to
        // land here. Restrict the dir to this user (plus SYSTEM/Admins,
        // who must read it elevated) before writing anything sensitive.
        restrict_dir(&staging.path)?;
        config.save(&staging.path.join(local::CONFIG_FILE))?;
        std::fs::write(staging.path.join(local::SECRETS_FILE), secrets.render()?)
            .context("failed to write staged secrets")?;
        if let Some(source) = ssh_key {
            std::fs::copy(source, staging.path.join(local::SSH_KEY_FILE))
                .context("failed to stage the SSH private key")?;
        }
        Ok(staging)
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Removes staging dirs left behind when provisioning died mid-write
    /// (Drop cannot run on a killed process). Narrowly scoped: only
    /// `gale-worker-provision-*` dirs in this user's temp dir, only ones
    /// untouched for `STALE_AFTER`, so a concurrent or in-flight attempt
    /// is never swept.
    fn sweep_stale() {
        Self::sweep_stale_in(&std::env::temp_dir());
    }

    fn sweep_stale_in(temp: &std::path::Path) {
        let Ok(entries) = temp.read_dir() else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("gale-worker-provision-") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let fresh = meta
                .modified()
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age < Self::STALE_AFTER);
            if meta.is_dir() && !fresh {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

#[cfg(windows)]
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Restricts a directory to the current user, SYSTEM, and Administrators
/// before credentials are written inside it.
#[cfg(windows)]
fn restrict_dir(path: &std::path::Path) -> Result<()> {
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_else(|_| String::new()),
        std::env::var("USERNAME").context("cannot determine the current user for staging ACLs")?,
    );
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            &format!("{user}:(OI)(CI)F"),
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ])
        .output()
        .context("failed to restrict the worker staging directory")?;
    ensure!(
        status.status.success(),
        "failed to restrict the worker staging directory: {}",
        String::from_utf8_lossy(&status.stdout).trim()
    );
    Ok(())
}

/// Reads the installed worker's binding leniently. Status must degrade
/// gracefully when the config was written by a different version.
#[cfg(windows)]
fn installed_config() -> Option<WorkerConfig> {
    let path = local::root_dir().join(local::CONFIG_FILE);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The port the existing install listens on, so reprovisioning keeps the
/// profile's worker address stable.
#[cfg(windows)]
fn installed_port() -> Option<u16> {
    installed_config().and_then(|config| {
        config
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
fn pick_port(preferred: Option<u16>) -> Result<u16> {
    preferred
        .into_iter()
        .chain(local::PORT_RANGE)
        .find(|&port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok())
        .ok_or_eyre("no free port is available for the managed worker")
}

/// `gale-worker.exe` next to the app binary (packaged installs), falling
/// back to cargo build outputs for development.
#[cfg(windows)]
fn worker_exe() -> Result<PathBuf> {
    bundled_exe(local::WORKER_EXE)
}

#[cfg(windows)]
fn tray_exe() -> Result<PathBuf> {
    bundled_exe(local::TRAY_EXE)
}

#[cfg(windows)]
fn bundled_exe(name: &str) -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(name);
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for profile in ["release", "debug"] {
        let candidate = manifest.join("target").join(profile).join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "{} was not found next to Gale or in target/. Package it with the app or run `cargo build --features worker --bin gale-worker`",
        name
    )
}

#[cfg(windows)]
fn install_tray_companion() -> Result<()> {
    tray::install_for_current_user(&tray_exe()?, &worker_exe()?)
}

/// Refreshes the LocalAppData tray copy after a Gale app update. This is
/// intentionally best-effort at app startup; provisioning still treats a
/// failed tray install as an actionable setup error.
#[cfg(windows)]
pub(crate) fn refresh_tray_companion() -> Result<()> {
    if local::service_state()? == ManagedServiceState::NotInstalled {
        return Ok(());
    }
    install_tray_companion()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// The single installed `GaleWorker` must only answer to the profile
    /// it was provisioned for. "Bound" covers the desktop half: settings
    /// pointing at the address and a stored bearer token.
    #[test]
    fn ownership_tracks_the_installed_binding() {
        // No service installed → nothing to own.
        assert_eq!(
            ownership_of(None, Some("sync-A"), false),
            WorkerOwnership::None
        );

        // Bound to this profile's sync id, fully linked.
        assert_eq!(
            ownership_of(Some("sync-A"), Some("sync-A"), true),
            WorkerOwnership::Owned
        );

        // Installed for this profile but the desktop binding never
        // completed. This needs setup, and it is not foreign.
        assert_eq!(
            ownership_of(Some("sync-A"), Some("sync-A"), false),
            WorkerOwnership::Incomplete
        );

        // A different sync id owns the service. Every operation must be
        // refused for this profile, whether or not it has synced before.
        assert_eq!(
            ownership_of(Some("sync-A"), Some("sync-B"), true),
            WorkerOwnership::Foreign
        );
        assert_eq!(
            ownership_of(Some("sync-A"), None, false),
            WorkerOwnership::Foreign,
            "an unknown local sync id must never count as ownership"
        );
    }

    #[test]
    fn pick_port_prefers_the_existing_install_and_skips_taken_ports() {
        // A free preferred port always wins, so reprovisioning keeps the
        // profile's worker address stable.
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let preferred = probe.local_addr().unwrap().port();
        drop(probe);
        assert_eq!(pick_port(Some(preferred)).unwrap(), preferred);

        // With the preferred port taken, the probe moves down the range.
        let blocker = std::net::TcpListener::bind(("127.0.0.1", preferred)).unwrap();
        let port = pick_port(Some(preferred)).unwrap();
        assert_ne!(port, preferred);
        assert!(local::PORT_RANGE.contains(&port));
        drop(blocker);
    }

    /// Ages a directory's modified time so `sweep_stale` sees it as
    /// abandoned. Needs `FILE_FLAG_BACKUP_SEMANTICS` to open a dir handle.
    #[cfg(windows)]
    fn age_dir(path: &std::path::Path) {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
    }

    #[test]
    fn staging_cleans_up_on_drop_and_sweeps_only_abandoned_dirs() {
        let root = tempfile::tempdir().unwrap();
        let secrets = Secrets {
            token: Some("token".to_owned()),
            ..Secrets::default()
        };

        // Dropping the guard removes the dir. Every early return path
        // in provisioning gets this for free.
        let path;
        {
            let staging =
                Staging::create_in(root.path(), &WorkerConfig::default(), &secrets, None).unwrap();
            path = staging.path().to_path_buf();
            assert!(path.join(local::SECRETS_FILE).is_file());
            assert!(path.join(local::CONFIG_FILE).is_file());
        }
        assert!(!path.exists());

        let stale = root.path().join("gale-worker-provision-stale");
        std::fs::create_dir(&stale).unwrap();
        age_dir(&stale);
        let fresh = root.path().join("gale-worker-provision-fresh");
        std::fs::create_dir(&fresh).unwrap();
        let unrelated = root.path().join("unrelated-temp-entry");
        std::fs::create_dir(&unrelated).unwrap();

        Staging::sweep_stale_in(root.path());
        assert!(!stale.exists(), "abandoned staging dir should be swept");
        assert!(fresh.exists(), "an in-flight provision must be untouched");
        assert!(unrelated.exists(), "unrelated temp entries are off-limits");
    }

    /// A failure partway through staging must remove the directory. This test
    /// uses a missing SSH key after config and secrets have been written.
    /// The cleanup guard is armed right after
    /// `create_dir_all`, so no partial payload survives an early return.
    #[test]
    fn a_failed_ssh_key_copy_removes_the_partial_staging_dir() {
        let root = tempfile::tempdir().unwrap();
        let secrets = Secrets {
            token: Some("Bearer SECRET-TOKEN-9f2c".to_owned()),
            ..Secrets::default()
        };
        let missing_key = root.path().join("no-such-key.pem");

        let result = Staging::create_in(
            root.path(),
            &WorkerConfig::default(),
            &secrets,
            Some(&missing_key),
        );
        let err = match result {
            Ok(_) => panic!("staging a missing SSH key must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("private key"),
            "unexpected error: {err:#}"
        );
        // The failure message must never carry a credential value.
        assert!(
            !format!("{err:#}").contains("SECRET-TOKEN-9f2c"),
            "error leaked the token: {err:#}"
        );
        assert!(
            root.path().read_dir().unwrap().next().is_none(),
            "partial staging dir was left behind"
        );
    }

    #[test]
    fn restrict_dir_removes_inherited_access() {
        fn powershell(path: &std::path::Path, script: &str) {
            let output = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("GALE_ACL_TEST_DIR", path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("icacls")
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .output()
            .unwrap();
        assert!(status.status.success());
        let path = dir.path().join("restricted");
        std::fs::create_dir(&path).unwrap();
        powershell(
            &path,
            r#"
            $ErrorActionPreference = 'Stop'
            $acl = [System.IO.Directory]::GetAccessControl($env:GALE_ACL_TEST_DIR)
            $rules = $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])
            if (-not ($rules | Where-Object { $_.IdentityReference.Value -eq 'S-1-1-0' -and $_.IsInherited })) {
                throw 'fixture must inherit Everyone access'
            }
        "#,
        );
        restrict_dir(&path).unwrap();
        powershell(
            &path,
            r#"
            $ErrorActionPreference = 'Stop'
            $acl = [System.IO.Directory]::GetAccessControl($env:GALE_ACL_TEST_DIR)
            if (-not $acl.AreAccessRulesProtected) { throw 'inheritance must be disabled' }
            $rules = $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])
            $userSid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
            $allowed = @($userSid, 'S-1-5-18', 'S-1-5-32-544')
            foreach ($rule in $rules) {
                if ($rule.IsInherited -or $rule.IdentityReference.Value -notin $allowed) { throw 'unexpected inherited or broad access' }
            }
            foreach ($sid in $allowed) {
                if (-not ($rules | Where-Object {
                    $_.IdentityReference.Value -eq $sid -and $_.AccessControlType -eq 'Allow' -and
                    ($_.FileSystemRights -band [System.Security.AccessControl.FileSystemRights]::FullControl) -eq
                        [System.Security.AccessControl.FileSystemRights]::FullControl
                })) { throw ('required FullControl grant missing: ' + $sid) }
            }
        "#,
        );
    }
}
