//! Dedicated-server commands.
//!
//! Two concerns are deliberately separate:
//!
//! - **Setup** saves settings and credentials explicitly. Connection tests
//!   only verify the supplied connection details.
//! - **Routine synchronization** (`get_server_sync_status`,
//!   `preview_server_sync`, `deploy_server_sync`, `set_server_config_policy`,
//!   `configure_worker`) uses the stored settings plus stored credentials.
//!
//! Both execution modes share the wire shapes in `worker::api`, so the
//! frontend handles Local and Worker results identically.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, OptionExt, bail, ensure};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, command};
use tokio::sync::Mutex;

use super::settings_store::SaveServerSettingsRequest;
use crate::{
    profile::{
        server::{
            args,
            executor::{
                CredentialOverrides, ExecutorStatus, executor, remote_credential, sync_id_for,
                sync_target, worker_client,
            },
            local, local_worker,
            progress::SyncProgress,
            remote::{self, ConnectionTestResult},
            runtime::{self, ServerStatus, SharedChild},
            secrets::{ServerSecret, ServerSecrets},
            settings::{
                AutomationSettings, ProfileServerSettings, RemoteServerSettings, RestartPolicy,
                SyncDialogPreferences, SyncMode,
            },
            settings_store::{persist_settings, update_settings_for},
        },
        sync,
    },
    state::ManagerExt,
    util::cmd::Result,
    worker::api::{
        DeployRequest, DeployResponse, PolicyRequest, PreviewRequest, PreviewResponse,
        ServerStateSummary, StatusResponse,
    },
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchDedicatedServerRequest {
    /// Settings to launch with. They are saved to the profile on launch.
    pub settings: ProfileServerSettings,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub remember_password: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteServerRequest {
    pub settings: RemoteServerSettings,
    #[serde(flatten)]
    pub credentials: CredentialOverrides,
}

/// Routine remote requests use stored settings; the credentials only
/// override what's in the keyring.
#[derive(Debug, Deserialize)]
pub struct ServerSyncRequest {
    #[serde(flatten)]
    pub operation: PreviewRequest,
    #[serde(flatten)]
    pub credentials: CredentialOverrides,
}

#[derive(Debug, Deserialize)]
pub struct ServerSyncDeployRequest {
    #[serde(flatten)]
    pub operation: DeployRequest,
    #[serde(flatten)]
    pub credentials: CredentialOverrides,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSyncStatusRequest {
    /// Whether to open a live remote session (Local) or ask the worker to
    /// refresh its view (Worker). `false` returns cheap cached state.
    #[serde(default)]
    pub refresh: bool,
    #[serde(flatten)]
    pub credentials: CredentialOverrides,
}

#[derive(Debug, Deserialize)]
pub struct SetConfigPolicyRequest {
    #[serde(flatten)]
    pub operation: PolicyRequest,
    #[serde(flatten)]
    pub credentials: CredentialOverrides,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureWorkerRequest {
    pub auto_deploy_mods: bool,
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub worker_token: String,
}

/// What the UI needs to render the server sync panel regardless of
/// execution mode.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSyncStatus {
    pub mode: SyncMode,
    /// Live remote deployment state, when fetched.
    pub server: Option<ServerStateSummary>,
    /// The newest publication revision known to the executor.
    pub publication_revision: Option<DateTime<Utc>>,
    /// The worker's full status (Worker mode only).
    pub worker: Option<StatusResponse>,
    /// Stored credentials were insufficient for the live refresh.
    pub credential_required: bool,
    pub warnings: Vec<String>,
}

impl ServerSyncStatus {
    /// Combines the executor's state with independently fetched publication metadata.
    fn observe_worker(&mut self, worker: StatusResponse) {
        self.server = worker.server.clone();
        self.publication_revision = self.publication_revision.max(worker.observed_revision);
        self.worker = Some(worker);
    }
}

// ---------- settings ----------

#[command]
pub fn get_dedicated_server_settings(app: AppHandle) -> Option<ProfileServerSettings> {
    app.lock_manager().active_profile().server_settings.clone()
}

/// Which of the profile's credentials have a stored value, so the UI can
/// show "saved" markers. Values are never returned.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedServerCredentials {
    pub game_password: bool,
    pub sftp_password: bool,
    pub ftp_password: bool,
    pub ssh_key_passphrase: bool,
    pub dat_host_password: bool,
    pub worker_token: bool,
}

#[command]
pub fn get_saved_server_credentials(app: AppHandle) -> Result<SavedServerCredentials> {
    let secrets = ServerSecrets::for_profile(active_profile_id(&app))?;
    Ok(SavedServerCredentials {
        game_password: secrets.has(ServerSecret::GamePassword)?,
        sftp_password: secrets.has(ServerSecret::SftpPassword)?,
        ftp_password: secrets.has(ServerSecret::FtpPassword)?,
        ssh_key_passphrase: secrets.has(ServerSecret::SshKeyPassphrase)?,
        dat_host_password: secrets.has(ServerSecret::DatHostPassword)?,
        worker_token: secrets.has(ServerSecret::WorkerToken)?,
    })
}

#[command]
pub fn get_sync_dialog_preferences(app: AppHandle) -> SyncDialogPreferences {
    app.lock_manager()
        .active_profile()
        .server_settings
        .as_ref()
        .map(|settings| settings.sync_dialog.clone())
        .unwrap_or_default()
}

#[command]
pub fn set_sync_dialog_preferences(
    preferences: SyncDialogPreferences,
    app: AppHandle,
) -> Result<()> {
    Ok(update_settings_for(
        &app,
        active_profile_id(&app),
        |settings| settings.sync_dialog = preferences,
    )?)
}

#[command]
pub async fn set_dedicated_server_settings(
    request: SaveServerSettingsRequest,
    app: AppHandle,
) -> Result<()> {
    request.settings.validate()?;

    // The stored settings are captured with the profile id: the worker
    // push below awaits, and an active-profile switch must not redirect
    // either it or the save.
    let (profile_id, stored) = {
        let manager = app.lock_manager();
        let profile = manager.active_profile();
        (profile.id, profile.server_settings.clone())
    };
    let secrets = ServerSecrets::for_profile(profile_id)?;

    let mut settings = request.settings.clone();
    if settings.remote.executor.mode.uses_worker()
        && worker_config_differs(stored.as_ref(), &settings)
    {
        // The running worker is authoritative for automation. The
        // stored copy only seeds new workers. Push the requested
        // configuration first and persist the values the worker
        // confirmed: a worker that cannot be reached must not look
        // configured.
        let confirmed = configure_running_worker(
            &secrets,
            &settings.remote,
            sync_id_for(&app, profile_id).as_deref(),
            &request.worker_token,
        )
        .await?;
        settings.remote.automation.auto_deploy_mods = confirmed.auto_deploy_mods;
        settings.remote.automation.restart_policy = confirmed.restart_policy;
    }

    persist_settings(&app, profile_id, &secrets, settings, &request)?;

    Ok(())
}

/// Whether the requested settings change what the bound worker runs,
/// the automation toggles, the restart policy, or which worker is
/// addressed. Skipping an unchanged configuration keeps a settings save
/// from depending on the worker being reachable.
fn worker_config_differs(
    stored: Option<&ProfileServerSettings>,
    new: &ProfileServerSettings,
) -> bool {
    let Some(stored) = stored else {
        return true;
    };
    !stored.remote.executor.mode.uses_worker()
        || stored.remote.executor.worker_address.trim() != new.remote.executor.worker_address.trim()
        || stored.remote.automation != new.remote.automation
}

/// Pushes the automation configuration in `remote` to the worker it
/// addresses and returns the state the worker confirmed. The worker is
/// authoritative: when the push fails nothing was applied, and callers
/// must not persist values it never accepted. A worker bound to a
/// different sync profile is refused rather than reconfigured.
async fn configure_running_worker(
    secrets: &ServerSecrets,
    remote: &RemoteServerSettings,
    expected_profile: Option<&str>,
    worker_token: &str,
) -> eyre::Result<StatusResponse> {
    let client = worker_client(secrets, &remote.executor.worker_address, worker_token)?;
    let status = client
        .status(false)
        .await
        .context("the worker did not answer. Its automation settings were not changed")?;
    // A worker is always bound to a sync profile id. Without this
    // profile's own id the binding cannot be verified. Reconfiguring
    // anyway could silently change a worker owned by another profile.
    let Some(expected) = expected_profile else {
        bail!(
            "this profile is not published. Publish it before Gale can verify \
             and configure a worker (the worker is bound to '{}')",
            status.profile_id,
        );
    };
    if status.profile_id != expected {
        bail!(
            "the worker is bound to sync profile '{}', but this profile is '{expected}'",
            status.profile_id,
        );
    }
    client
        .configure(
            remote.automation.auto_deploy_mods,
            remote.automation.restart_policy,
        )
        .await
        .context("the worker did not accept the automation update")?;
    client
        .status(false)
        .await
        .context("the worker did not confirm the automation update")
}

// ---------- local launch ----------

#[command]
pub async fn launch_dedicated_server(
    request: LaunchDedicatedServerRequest,
    app: AppHandle,
) -> Result<ServerStatus> {
    let profile_id = active_profile_id(&app);

    if app.lock_server_runtime().is_running() {
        return Err(eyre::eyre!("a Gale-managed dedicated server is already running").into());
    }

    if app.lock_prefs().pull_before_launch {
        sync::pull_profile(false, profile_id, &app).await?;
    }

    ensure_no_pending_installs(&app)?;

    let secrets = ServerSecrets::for_profile(profile_id)?;
    let password = secrets.resolve(ServerSecret::GamePassword, &request.password)?;

    // Resolve by the captured id. The `pull_profile` call awaited, so the
    // active profile may have changed since then. Never combine profile
    // A's credentials with profile B's settings or launch context.
    let game = app.lock_manager().profile_by_id(profile_id)?.0;
    let settings = request.settings;
    args::validate_game_args(game, &settings.local, &password)?;
    let saved = settings.clone();
    update_settings_for(&app, profile_id, |stored| *stored = saved)?;
    secrets.persist(
        ServerSecret::GamePassword,
        &password,
        request.remember_password,
    )?;

    let (status, child, pid) = {
        let prefs = app.lock_prefs();
        let manager = app.lock_manager();
        let (game, profile) = manager.profile_by_id(profile_id)?;
        let managed = manager
            .games
            .get(&game)
            .ok_or_eyre("the profile's game is not installed")?;

        // Installs write under the manager lock held here and refuse a
        // locked profile, so one that has not started yet fails cleanly. A
        // batch already underway must finish first: refusing it midway
        // would roll back files the server is about to load.
        if app.install_queue().lock().has_any_for_profile(profile_id) {
            return Err(eyre::eyre!(
                "please wait for mod installations to finish before starting the server"
            )
            .into());
        }

        // Keep the runtime locked from the final check through registration
        // so concurrent launches cannot leave an untracked process running.
        let mut runtime = app.lock_server_runtime();
        if runtime.is_running() {
            return Err(eyre::eyre!("a Gale-managed dedicated server is already running").into());
        }
        let process = local::launch(managed, profile, &settings.local, &password, &prefs)?;
        let pid = process
            .child
            .id()
            .ok_or_eyre("dedicated server process has no pid")?;
        let child: SharedChild = Arc::new(Mutex::new(process.child));
        let status = runtime.register(
            process.profile_id,
            process.game,
            process.server_dir,
            pid,
            child.clone(),
        )?;
        (status, child, pid)
    };

    runtime::emit_status(&app, &status);
    runtime::watch(app.clone(), child, pid);

    Ok(status)
}

#[command]
pub fn get_dedicated_server_status(app: AppHandle) -> Result<ServerStatus> {
    Ok(app.lock_server_runtime().status())
}

#[command]
pub fn open_dedicated_server_dir(app: AppHandle) -> Result<()> {
    let running_dir = { app.lock_server_runtime().server_dir() };

    let path = match running_dir {
        Some(path) => path,

        None => {
            let prefs = app.lock_prefs();
            let manager = app.lock_manager();
            let game = manager.active_game();

            local::locate_server_dir(game, &prefs)?.0
        }
    };

    open::that(&path).wrap_err_with(|| {
        format!(
            "failed to open dedicated server directory {}",
            path.display()
        )
    })?;

    Ok(())
}

#[command]
pub async fn force_stop_dedicated_server(app: AppHandle) -> Result<()> {
    let (pid, child) = {
        let mut runtime = app.lock_server_runtime();
        match runtime.begin_stop() {
            Some(pair) => pair,
            // Already stopped, or a stop is already in flight. Still
            // report the status so the UI stays in sync.
            None => {
                runtime::emit_status(&app, &runtime.status());
                return Ok(());
            }
        }
    };

    runtime::kill(app, pid, child).await?;
    Ok(())
}

// ---------- connection tests (setup) ----------

#[command]
pub async fn test_remote_server_connection(
    request: RemoteServerRequest,
    app: AppHandle,
) -> Result<ConnectionTestResult> {
    let profile_id = active_profile_id(&app);
    // Only the file transfer is exercised, so worker and host-provider
    // fields must not gate it: a worker may not be provisioned or fully
    // configured yet.
    request.settings.transport.validate()?;

    let secrets = ServerSecrets::for_profile(profile_id)?;
    let credential = remote_credential(
        &secrets,
        &request.settings.transport,
        &request.credentials.password,
    )?;

    let result = tokio::task::spawn_blocking(move || {
        remote::test_connection(&request.settings.transport, &credential)
    })
    .await
    .map_err(|err| eyre::eyre!("remote connection worker failed: {err}"))??;

    Ok(result)
}

/// Validates the worker's reachability, bearer token, and profile binding.
#[command]
pub async fn test_worker_connection(
    request: RemoteServerRequest,
    app: AppHandle,
) -> Result<StatusResponse> {
    let profile_id = app.lock_manager().active_profile().id;
    request.settings.validate()?;

    let secrets = ServerSecrets::for_profile(profile_id)?;
    // Capture the profile identity before awaiting the connection test.
    let sync_id = sync_id_for(&app, profile_id);
    let client = worker_client(
        &secrets,
        &request.settings.executor.worker_address,
        &request.credentials.worker_token,
    )?;
    let status = client.status(false).await?;

    // The worker is bound to one profile; a mismatch means the address
    // points at a worker managing a different profile.
    if let Some(sync_id) = sync_id
        && status.profile_id != sync_id
    {
        return Err(eyre::eyre!(
            "the worker is bound to sync profile '{}', but this profile is '{sync_id}'",
            status.profile_id,
        )
        .into());
    }

    Ok(status)
}

// ---------- managed local worker ("host worker on this PC") ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalWorkerControlRequest {
    pub action: crate::worker::ServiceControlAction,
}

/// SCM state + status file + live API status for the managed worker.
#[command]
pub async fn get_local_worker_status(app: AppHandle) -> Result<local_worker::LocalWorkerStatus> {
    Ok(local_worker::status(&app).await?)
}

/// Provisions and installs the managed worker: a second OAuth login gives
/// it an independent credential chain, then one elevated step registers
/// and starts the Windows service.
///
/// The request carries the page's settings and credentials, saved or not.
/// They are stored only once the service runs, so a failed setup leaves
/// the profile's saved settings as they were.
#[command]
pub async fn provision_local_worker(
    request: SaveServerSettingsRequest,
    app: AppHandle,
) -> Result<local_worker::LocalWorkerStatus> {
    Ok(local_worker::provision(&app, request).await?)
}

#[command]
pub async fn control_local_worker(
    request: LocalWorkerControlRequest,
    app: AppHandle,
) -> Result<local_worker::LocalWorkerStatus> {
    Ok(local_worker::control(&app, request.action).await?)
}

/// Reinstalls the service with the bundled worker binary, keeping the
/// installed config, credentials, and journal. Used after a Gale update
/// shipped a newer worker than the installed service runs.
#[command]
pub async fn update_local_worker(app: AppHandle) -> Result<local_worker::LocalWorkerStatus> {
    Ok(local_worker::update(&app).await?)
}

/// Stops and removes the service and its state, and reverts the profile
/// to Local sync mode when it still points at the managed worker.
#[command]
pub async fn uninstall_local_worker(app: AppHandle) -> Result<local_worker::LocalWorkerStatus> {
    Ok(local_worker::uninstall(&app).await?)
}

// ---------- routine synchronization ----------

#[command]
pub async fn get_server_sync_status(
    request: ServerSyncStatusRequest,
    app: AppHandle,
) -> Result<ServerSyncStatus> {
    let target = sync_target(&app)?;
    let mut warnings = Vec::new();

    // The latest publication revision the desktop knows about. This is
    // cheap metadata, and a failure is non-fatal when sync isn't
    // configured or unreachable.
    let publication_revision = match &target.sync_id {
        Some(id) => match sync::get_profile_meta(id, &app).await {
            Ok(Some(meta)) => Some(meta.updated_at),
            Ok(None) => None,
            Err(err) => {
                warnings.push(format!("could not check for a new publication: {err:#}"));
                None
            }
        },
        None => None,
    };

    let mut status = ServerSyncStatus {
        mode: target.settings.executor.mode,
        server: None,
        publication_revision,
        worker: None,
        credential_required: false,
        warnings,
    };

    if !request.refresh && !target.settings.executor.mode.uses_worker() {
        return Ok(status);
    }

    let (profile_id, settings) = (target.profile_id, target.settings.clone());
    let refreshed = async {
        let executor = executor(&app, target, &request.credentials)?;
        match executor.read_status(request.refresh).await? {
            Some(ExecutorStatus::Worker(worker)) => status.observe_worker(worker),
            Some(ExecutorStatus::Session {
                server,
                mut warnings,
            }) => {
                status.warnings.append(&mut warnings);
                status.server = Some(server);
            }
            None => {}
        }
        eyre::Ok(())
    }
    .await;
    if let Err(err) = refreshed {
        status.credential_required = ServerSecrets::for_profile(profile_id)
            .map(|secrets| {
                credential_missing(
                    &secrets,
                    &settings,
                    &request.credentials.password,
                    &request.credentials.worker_token,
                )
            })
            .unwrap_or(false);
        status
            .warnings
            .push(format!("could not read server status: {err:#}"));
    }

    Ok(status)
}

/// Whether the executor's required secret is absent from both the request
/// and the credential store. The only refresh failure that entering a
/// credential can fix. Other failures appear as plain warnings.
fn credential_missing(
    secrets: &ServerSecrets,
    settings: &RemoteServerSettings,
    password: &str,
    worker_token: &str,
) -> bool {
    let (secret, provided) = match settings.executor.mode {
        SyncMode::Local => match ServerSecret::required_by(&settings.transport) {
            Some(secret) => (secret, password),
            // SSH agent auth has no storable credential.
            None => return false,
        },
        SyncMode::HostedWorker | SyncMode::Worker => (ServerSecret::WorkerToken, worker_token),
    };
    provided.is_empty()
        && secrets
            .get(secret)
            .map(|value| value.is_none())
            .unwrap_or(false)
}

#[command]
pub async fn preview_server_sync(
    request: ServerSyncRequest,
    app: AppHandle,
) -> Result<PreviewResponse> {
    ensure_no_pending_installs(&app)?;
    let executor = executor(&app, sync_target(&app)?, &request.credentials)?;
    Ok(executor.preview(request.operation).await?)
}

#[command]
pub async fn deploy_server_sync(
    request: ServerSyncDeployRequest,
    app: AppHandle,
) -> Result<DeployResponse> {
    ensure_no_pending_installs(&app)?;
    let executor = executor(&app, sync_target(&app)?, &request.credentials)?;
    Ok(executor.deploy(request.operation).await?)
}

/// A cheap authenticated read of the worker's single bounded progress
/// snapshot. The dialog matches `run_id` before displaying it.
#[command]
pub async fn get_server_sync_progress(
    request: ServerSyncStatusRequest,
    app: AppHandle,
) -> Result<Option<SyncProgress>> {
    let executor = executor(&app, sync_target(&app)?, &request.credentials)?;
    Ok(executor.progress().await?)
}

#[command]
pub async fn set_server_config_policy(
    request: SetConfigPolicyRequest,
    app: AppHandle,
) -> Result<()> {
    let executor = executor(&app, sync_target(&app)?, &request.credentials)?;
    Ok(executor.set_policy(request.operation).await?)
}

/// Explicitly records that the user verified a restart outside Gale.
/// Reading a running status cannot establish that a restart happened.
#[command]
pub async fn acknowledge_external_server_restart(
    request: ServerSyncStatusRequest,
    app: AppHandle,
) -> Result<()> {
    let executor = executor(&app, sync_target(&app)?, &request.credentials)?;
    Ok(executor.acknowledge_external_restart().await?)
}

/// Updates the worker's automation toggles and mirrors the state it
/// confirmed into the stored settings, so the page reflects the worker's
/// actual behavior. Returns the worker's confirmed status.
#[command]
pub async fn configure_worker(
    request: ConfigureWorkerRequest,
    app: AppHandle,
) -> Result<StatusResponse> {
    let target = sync_target(&app)?;
    if !target.settings.executor.mode.uses_worker() {
        return Err(
            eyre::eyre!("automatic mod deployment is only available in Worker mode").into(),
        );
    }

    let secrets = ServerSecrets::for_profile(target.profile_id)?;
    let mut remote = target.settings.clone();
    remote.automation = AutomationSettings {
        auto_deploy_mods: request.auto_deploy_mods,
        restart_policy: request.restart_policy,
    };
    let confirmed = configure_running_worker(
        &secrets,
        &remote,
        sync_id_for(&app, target.profile_id).as_deref(),
        &request.worker_token,
    )
    .await?;

    update_settings_for(&app, target.profile_id, |settings| {
        settings.remote.automation.auto_deploy_mods = confirmed.auto_deploy_mods;
        settings.remote.automation.restart_policy = confirmed.restart_policy;
    })?;

    Ok(confirmed)
}

// ---------- misc ----------

fn ensure_no_pending_installs(app: &AppHandle) -> eyre::Result<()> {
    ensure!(
        !app.install_queue().lock().is_processing(),
        "please wait for mod installations to finish before syncing the server"
    );

    Ok(())
}

fn active_profile_id(app: &AppHandle) -> i64 {
    app.lock_manager().active_profile().id
}

#[cfg(test)]
mod tests {
    use super::super::settings::ServerLocation;
    use super::*;

    #[test]
    fn stale_worker_observation_cannot_mask_canonical_publication() {
        let old = DateTime::parse_from_rfc3339("2026-09-22T00:00:00Z")
            .unwrap()
            .to_utc();
        let new = old + chrono::Duration::seconds(1);
        for (canonical, observed, expected) in [
            (Some(new), Some(old), Some(new)),
            (Some(old), Some(new), Some(new)),
            (Some(new), None, Some(new)),
            (None, Some(new), Some(new)),
        ] {
            let mut status = ServerSyncStatus {
                mode: SyncMode::Worker,
                server: None,
                publication_revision: canonical,
                worker: None,
                credential_required: false,
                warnings: Vec::new(),
            };
            let worker = serde_json::from_value(serde_json::json!({
                "workerId": "worker", "profileId": "sync-profile-1", "autoDeployMods": false,
                "restartPolicy": "manual", "observedRevision": observed, "pendingRevision": null,
                "nextAttemptAt": null, "lastDeployedRevision": old, "busy": false,
                "lastOperation": null, "lastError": null, "pollError": null, "server": null,
            }))
            .unwrap();
            status.observe_worker(worker);
            assert_eq!(
                status.publication_revision, expected,
                "keep the newest trustworthy publication"
            );
            assert!(status.worker.is_some());
        }
    }

    /// Remote settings in worker mode bound to a worker address.
    fn worker_settings() -> ProfileServerSettings {
        let mut settings = ProfileServerSettings {
            location: ServerLocation::Remote,
            ..ProfileServerSettings::default()
        };
        settings.remote.executor.mode = SyncMode::Worker;
        settings.remote.executor.worker_address = "http://127.0.0.1:8472".to_owned();
        settings
    }

    #[test]
    fn worker_config_differs_only_for_worker_relevant_fields() {
        let stored = worker_settings();

        // Identical settings skip the worker round-trip entirely, so a
        // plain save never depends on the worker being reachable.
        assert!(!worker_config_differs(Some(&stored), &stored.clone()));

        // Toggling automation differs.
        let mut new = stored.clone();
        new.remote.automation.auto_deploy_mods = true;
        assert!(worker_config_differs(Some(&stored), &new));

        // The restart policy is worker-run configuration too.
        let mut new = stored.clone();
        new.remote.automation.restart_policy = RestartPolicy::Immediate;
        assert!(worker_config_differs(Some(&stored), &new));

        // Retargeting the worker address pushes the flags to the new
        // worker.
        let mut new = stored.clone();
        new.remote.executor.worker_address = "http://127.0.0.1:9999".to_owned();
        assert!(worker_config_differs(Some(&stored), &new));

        // Unrelated fields never trigger a worker call.
        let mut new = stored.clone();
        new.remote.transport.host = "example.com".to_owned();
        new.local.server_name = "Other".to_owned();
        new.sync_dialog.restart_policy = RestartPolicy::WhenEmpty;
        assert!(!worker_config_differs(Some(&stored), &new));

        // A stored binding that never pointed at a worker differs, as
        // does having no stored settings at all.
        let mut local = stored.clone();
        local.remote.executor.mode = SyncMode::Local;
        assert!(worker_config_differs(Some(&local), &stored));
        assert!(worker_config_differs(None, &stored));
    }

    #[test]
    fn credential_missing_only_when_the_required_secret_is_absent() {
        use super::super::secrets::in_memory_secrets;

        let secrets = in_memory_secrets();

        // Local mode with password auth needs the SFTP credential.
        let mut remote = RemoteServerSettings::default();
        assert!(credential_missing(&secrets, &remote, "", ""));

        // A provided value or a stored credential both cover it.
        assert!(!credential_missing(&secrets, &remote, "typed", ""));
        secrets.set(ServerSecret::SftpPassword, "saved").unwrap();
        assert!(!credential_missing(&secrets, &remote, "", ""));

        // Agent auth needs no credential at all.
        remote.transport.authentication = super::super::settings::RemoteAuthentication::Agent;
        secrets.remove(ServerSecret::SftpPassword).unwrap();
        assert!(!credential_missing(&secrets, &remote, "", ""));

        // Worker mode keys off the worker token instead.
        remote.executor.mode = SyncMode::Worker;
        assert!(credential_missing(&secrets, &remote, "typed", ""));
        assert!(!credential_missing(&secrets, &remote, "", "token"));
        secrets
            .set(ServerSecret::WorkerToken, "saved-token")
            .unwrap();
        assert!(!credential_missing(&secrets, &remote, "", ""));
    }

    /// The page sends each routine request as one flat object, the shape
    /// `src/lib/api/profile/server.ts` builds. Typed credentials must reach
    /// the executor rather than silently fall back to the stored ones.
    #[test]
    fn sync_requests_accept_the_flat_payloads_the_page_sends() {
        use crate::profile::{
            export::ConfigPath, server::plan::ConfigDecision, sync::ConfigUpdatePolicy,
        };

        let config = ConfigPath::try_from("BepInEx/config/a.cfg".to_owned()).unwrap();
        let deploy: ServerSyncDeployRequest = serde_json::from_value(serde_json::json!({
            "selection": {
                "includeMods": false, "includeConfigs": true,
                "applyConfigs": [config.as_str()], "restoreConfigs": [], "declineConfigs": [],
            },
            "planHash": "approved-hash", "restartPolicy": null, "force": true,
            "password": "typed-password", "workerToken": "typed-token", "runId": "run-1",
        }))
        .unwrap();
        assert_eq!(deploy.operation.plan_hash, "approved-hash");
        assert_eq!(
            deploy.operation.selection.configs,
            Some([(config.clone(), ConfigDecision::Apply)].into())
        );
        assert_eq!(deploy.operation.restart_policy, None);
        assert!(deploy.operation.force);
        assert_eq!(deploy.operation.run_id, "run-1");
        assert_eq!(deploy.credentials.password, "typed-password");
        assert_eq!(deploy.credentials.worker_token, "typed-token");

        let preview: ServerSyncRequest = serde_json::from_value(serde_json::json!({
            "selection": { "includeMods": true, "includeConfigs": false },
            "restartPolicy": "whenEmpty", "password": "", "workerToken": "typed-token",
            "runId": "run-2",
        }))
        .unwrap();
        assert_eq!(
            preview.operation.restart_policy,
            Some(RestartPolicy::WhenEmpty)
        );
        assert!(preview.operation.selection.include_mods);
        assert_eq!(preview.operation.run_id, "run-2");
        assert_eq!(preview.credentials.worker_token, "typed-token");

        let policy: SetConfigPolicyRequest = serde_json::from_value(serde_json::json!({
            "path": config.as_str(), "policy": "alwaysKeep",
            "password": "typed-password", "workerToken": "",
        }))
        .unwrap();
        assert_eq!(policy.operation.path, config);
        assert_eq!(policy.operation.policy, ConfigUpdatePolicy::AlwaysKeep);
        assert_eq!(policy.credentials.password, "typed-password");

        // The progress poll sends only the token; everything else defaults.
        let progress: ServerSyncStatusRequest =
            serde_json::from_value(serde_json::json!({ "workerToken": "typed-token" })).unwrap();
        assert!(!progress.refresh);
        assert_eq!(progress.credentials.password, "");
        assert_eq!(progress.credentials.worker_token, "typed-token");

        let test: RemoteServerRequest = serde_json::from_value(serde_json::json!({
            "settings": RemoteServerSettings::default(),
            "password": "typed-password", "workerToken": "",
        }))
        .unwrap();
        assert_eq!(test.credentials.password, "typed-password");
    }
}
