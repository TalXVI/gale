//! Dedicated-server commands.
//!
//! Two concerns are deliberately separate:
//!
//! - **Setup** saves settings and credentials explicitly. Connection tests
//!   only verify the supplied connection details.
//! - **Routine synchronization** (`get_server_sync_status`,
//!   `preview_server_sync`, `deploy_server_sync`, `set_server_config_policy`,
//!   `configure_worker`) uses the stored settings plus stored credentials,
//!   so no settings dialog is needed on every deploy.
//!
//! Both execution modes share the wire shapes in `worker::api`, so the
//! frontend handles Local and Worker results identically.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, OptionExt, bail, ensure};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, command};
use tokio::sync::Mutex;

use crate::{
    game::mod_loader::ModLoader,
    profile::{
        export::ConfigPath,
        server::{
            args,
            engine::{self, OperationMeta},
            host, local, local_worker,
            plan::{self, DeploySelection, Publication},
            progress::{ProgressReporter, SyncOperation, SyncPhase, SyncProgress},
            remote::{self, ConnectionAttempt, ConnectionTestResult, RemoteConnection, RemoteOps},
            runtime::{self, ServerStatus, SharedChild},
            secrets::{ServerSecret, ServerSecrets},
            settings::{
                ProfileServerSettings, RemoteServerSettings, RestartPolicy, SyncDialogPreferences,
                SyncMode,
            },
            spec::DeploymentSpec,
            stage::{self, CachePayloadSource},
            worker_client::WorkerClient,
        },
        sync::{self, ConfigUpdatePolicy, FetchedPublication},
    },
    state::ManagerExt,
    util::cmd::Result,
    worker::api::{DeployResponse, PreviewResponse, ServerStateSummary, StatusResponse},
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchDedicatedServerRequest {
    /// Settings to launch with. Falls back to the profile's stored settings,
    /// so an already-configured server can launch immediately.
    #[serde(default)]
    pub settings: Option<ProfileServerSettings>,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub remember_password: bool,
}

/// Settings + optional credentials, saved together from the settings
/// page. Empty credential fields leave stored credentials untouched;
/// each `remember*` flag controls only its own credential — `false`
/// clears that one and nothing else.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveServerSettingsRequest {
    pub settings: ProfileServerSettings,
    #[serde(default)]
    pub game_password: String,
    #[serde(default)]
    pub remember_game_password: bool,
    #[serde(default)]
    pub remote_password: String,
    #[serde(default)]
    pub remember_remote_password: bool,
    #[serde(default)]
    pub worker_token: String,
    #[serde(default)]
    pub remember_worker_token: bool,
    #[serde(default)]
    pub dat_host_password: String,
    #[serde(default)]
    pub remember_dat_host_password: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteServerRequest {
    pub settings: RemoteServerSettings,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub worker_token: String,
}

/// Routine remote requests use stored settings; the credential fields only
/// override what's in the keyring.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSyncRequest {
    #[serde(default)]
    pub run_id: String,
    pub selection: DeploySelection,
    /// The restart policy the deploy will use. Preview binds it into the
    /// plan hash so changing it afterwards invalidates the approval.
    /// `None` uses the stored policy.
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub worker_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSyncDeployRequest {
    #[serde(default)]
    pub run_id: String,
    pub selection: DeploySelection,
    /// The hash of the approved preview; deployment is rejected when the
    /// recomputed plan differs.
    pub plan_hash: String,
    /// Overrides the stored restart policy for this operation.
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
    /// Take over a stale foreign lease, the recovery path after the old
    /// executor is confirmed stopped. Live leases always win.
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub worker_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSyncStatusRequest {
    /// Whether to open a live remote session (Local) or ask the worker to
    /// refresh its view (Worker). `false` returns cheap cached state.
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub worker_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigPolicyRequest {
    pub path: ConfigPath,
    pub policy: ConfigUpdatePolicy,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub worker_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureWorkerRequest {
    pub auto_deploy_mods: bool,
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub worker_token: String,
}

/// What the UI needs to render the server-sync surface regardless of
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
    let mut manager = app.lock_manager();
    let profile = manager.active_profile();
    let profile_id = profile.id;
    let (_, profile) = manager.profile_by_id_mut(profile_id)?;
    let mut settings = profile.server_settings.clone().unwrap_or_default();
    settings.sync_dialog = preferences;
    profile.server_settings = Some(settings);
    Ok(profile.save(&app, true)?)
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
    // A settings dialog opened earlier must not overwrite newer Sync-dialog
    // defaults. That dedicated command owns this client-local field.
    settings.sync_dialog = stored
        .as_ref()
        .map(|settings| settings.sync_dialog.clone())
        .unwrap_or_default();
    if settings.remote.sync_mode == SyncMode::Worker
        && worker_config_differs(stored.as_ref(), &settings)
    {
        // The running worker is authoritative for automation — the
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
        settings.remote.worker.automation.auto_deploy_mods = confirmed.auto_deploy_mods;
        settings.remote.restart_policy = confirmed.restart_policy;
    }

    persist_credential(
        &secrets,
        Some(ServerSecret::GamePassword),
        &request.game_password,
        request.remember_game_password,
    )?;

    save_settings_for(&app, profile_id, settings.clone())?;

    persist_remote_credentials(&secrets, &settings.remote, &request)?;

    Ok(())
}

/// Persists each remote credential independently: a provided value is
/// stored per its own `remember` flag, an empty one leaves the stored
/// value alone unless `remember` is off — which clears just that
/// credential and nothing else.
fn persist_remote_credentials(
    secrets: &ServerSecrets,
    remote: &RemoteServerSettings,
    request: &SaveServerSettingsRequest,
) -> eyre::Result<()> {
    persist_credential(
        secrets,
        remote_secret(remote),
        &request.remote_password,
        request.remember_remote_password,
    )?;
    persist_credential(
        secrets,
        Some(ServerSecret::WorkerToken),
        &request.worker_token,
        request.remember_worker_token,
    )?;
    persist_credential(
        secrets,
        Some(ServerSecret::DatHostPassword),
        &request.dat_host_password,
        request.remember_dat_host_password,
    )
}

/// The transport credential the remote settings require, if any.
pub(crate) fn remote_secret(settings: &RemoteServerSettings) -> Option<ServerSecret> {
    ServerSecret::required_by(settings)
}

pub(crate) fn persist_credential(
    secrets: &ServerSecrets,
    secret: Option<ServerSecret>,
    value: &str,
    remember: bool,
) -> eyre::Result<()> {
    let Some(secret) = secret else {
        return Ok(());
    };
    if !value.is_empty() || !remember {
        secrets.persist(secret, value, remember)?;
    }
    Ok(())
}

/// Whether the requested settings change what the bound worker runs —
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
    stored.remote.sync_mode != SyncMode::Worker
        || stored.remote.worker.address.trim() != new.remote.worker.address.trim()
        || stored.remote.worker.automation.auto_deploy_mods
            != new.remote.worker.automation.auto_deploy_mods
        || stored.remote.restart_policy != new.remote.restart_policy
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
    let client = worker_client(secrets, remote, worker_token)?;
    let status = client
        .status(false)
        .await
        .context("the worker did not answer — its automation settings were not changed")?;
    // A worker is always bound to a sync profile id. Without this
    // profile's own id the binding cannot be verified — reconfiguring
    // anyway could silently change a worker owned by another profile.
    let Some(expected) = expected_profile else {
        bail!(
            "this profile is not published — publish it before Gale can verify \
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
            remote.worker.automation.auto_deploy_mods,
            remote.restart_policy,
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
    let (game, stored_settings) = {
        let manager = app.lock_manager();
        let (game, profile) = manager.profile_by_id(profile_id)?;
        (game, profile.server_settings.clone())
    };

    let settings = match request.settings {
        Some(settings) => {
            args::validate_game_args(game, &settings.local, &password)?;
            save_settings_for(&app, profile_id, settings.clone())?;
            settings
        }
        None => stored_settings.ok_or_eyre("the dedicated server has not been configured yet")?,
    };

    args::validate_game_args(game, &settings.local, &password)?;
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
    request.settings.validate_connection()?;

    let secrets = ServerSecrets::for_profile(profile_id)?;
    let credential = remote_credential(&secrets, &request.settings, &request.password)?;

    let settings = request.settings.clone();
    let password = credential.clone();

    let result = tokio::task::spawn_blocking(move || remote::test_connection(&settings, &password))
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
    let client = worker_client(&secrets, &request.settings, &request.worker_token)?;
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
pub struct LocalWorkerProvisionRequest {
    /// Remote transport credential override when the keyring lacks one.
    #[serde(default)]
    pub password: String,
    /// DatHost account password override when the keyring lacks one.
    #[serde(default)]
    pub dat_host_password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalWorkerControlRequest {
    pub action: local_worker::LocalWorkerAction,
}

/// SCM state + status file + live API status for the managed worker.
#[command]
pub async fn get_local_worker_status(app: AppHandle) -> Result<local_worker::LocalWorkerStatus> {
    Ok(local_worker::status(&app).await?)
}

/// Provisions and installs the managed worker: a second OAuth login gives
/// it an independent credential chain, then one elevated step registers
/// and starts the Windows service.
#[command]
pub async fn provision_local_worker(
    request: LocalWorkerProvisionRequest,
    app: AppHandle,
) -> Result<local_worker::LocalWorkerStatus> {
    Ok(local_worker::provision(&app, &request.password, &request.dat_host_password).await?)
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
        Some(id) => match sync::fetch_publication_meta(id, &app).await {
            Ok(Some(meta)) => Some(meta.updated_at()),
            Ok(None) => None,
            Err(err) => {
                warnings.push(format!("could not check for a new publication: {err:#}"));
                None
            }
        },
        None => None,
    };

    let mut status = ServerSyncStatus {
        mode: target.settings.sync_mode,
        server: None,
        publication_revision,
        worker: None,
        credential_required: false,
        warnings,
    };

    if !request.refresh && target.settings.sync_mode == SyncMode::Local {
        return Ok(status);
    }

    let refreshed = match resolve_executor(&target, &request.password, &request.worker_token) {
        Ok(Executor::Worker(client)) => client.status(request.refresh).await.map(|worker| {
            status.server = worker.server.clone();
            status.publication_revision = worker.observed_revision.or(status.publication_revision);
            status.worker = Some(worker);
        }),
        Ok(Executor::Local(credential)) => {
            open_remote_session(&target.settings, &credential, target.mod_loader)
                .await
                .map(|session| {
                    if session.migrated {
                        status.warnings.push(
                            "adopted the previous deployment manifest into the new state format"
                                .to_owned(),
                        );
                    }
                    status.server = Some(ServerStateSummary {
                        mods_revision: session.state.mods_revision.clone(),
                        restart_required: session.state.restart_required,
                        last_operation: session.state.last_operation.clone(),
                        lease: session.lease.clone(),
                    });
                    status.warnings.extend(session.warnings);
                })
        }
        Err(err) => Err(err),
    };
    if let Err(err) = refreshed {
        status.credential_required = ServerSecrets::for_profile(target.profile_id)
            .map(|secrets| {
                credential_missing(
                    &secrets,
                    &target.settings,
                    &request.password,
                    &request.worker_token,
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
/// and the credential store — the only refresh failure that entering a
/// credential can fix. Other failures surface as plain warnings.
fn credential_missing(
    secrets: &ServerSecrets,
    settings: &RemoteServerSettings,
    password: &str,
    worker_token: &str,
) -> bool {
    let (secret, provided) = match settings.sync_mode {
        SyncMode::Local => match ServerSecret::required_by(settings) {
            Some(secret) => (secret, password),
            // SSH agent auth has no storable credential.
            None => return false,
        },
        SyncMode::Worker => (ServerSecret::WorkerToken, worker_token),
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
    let target = sync_target(&app)?;

    match resolve_executor(&target, &request.password, &request.worker_token)? {
        Executor::Worker(client) => Ok(client
            .preview(&request.selection, request.restart_policy, &request.run_id)
            .await?),
        Executor::Local(credential) => Ok(local_preview(
            &app,
            &target,
            &request.selection,
            request.restart_policy,
            &credential,
            operation_run_id(&request.run_id),
        )
        .await?),
    }
}

#[command]
pub async fn deploy_server_sync(
    request: ServerSyncDeployRequest,
    app: AppHandle,
) -> Result<DeployResponse> {
    ensure_no_pending_installs(&app)?;
    let target = sync_target(&app)?;

    match resolve_executor(&target, &request.password, &request.worker_token)? {
        Executor::Worker(client) => Ok(client
            .deploy(
                &request.selection,
                &request.plan_hash,
                request.restart_policy,
                request.force,
                &request.run_id,
            )
            .await?),
        Executor::Local(credential) => Ok(local_deploy(&app, &target, request, &credential).await?),
    }
}

/// A cheap authenticated read of the worker's single bounded progress
/// snapshot. The dialog matches `run_id` before displaying it.
#[command]
pub async fn get_server_sync_progress(
    request: ServerSyncStatusRequest,
    app: AppHandle,
) -> Result<Option<SyncProgress>> {
    let target = sync_target(&app)?;
    let Executor::Worker(client) =
        resolve_executor(&target, &request.password, &request.worker_token)?
    else {
        return Ok(None);
    };
    Ok(client.progress().await?)
}

fn operation_run_id(requested: &str) -> String {
    if requested.is_empty() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        requested.to_owned()
    }
}

#[command]
pub async fn set_server_config_policy(
    request: SetConfigPolicyRequest,
    app: AppHandle,
) -> Result<()> {
    let target = sync_target(&app)?;

    match resolve_executor(&target, &request.password, &request.worker_token)? {
        Executor::Worker(client) => {
            client.set_policy(&request.path, request.policy).await?;
        }
        Executor::Local(credential) => {
            // The policy pin comes from the canonical publication on the
            // trusted side, because a client-supplied value could pin the
            // policy at the wrong revision.
            let pinned_at = match &target.sync_id {
                Some(id) => sync::fetch_publication(id, &app)
                    .await
                    .context("failed to fetch the canonical publication for the policy pin")?
                    .config
                    .get(&request.path)
                    .map(|file| file.hash.clone()),
                None => None,
            };
            let (settings, password, mod_loader, meta) = (
                target.settings.clone(),
                credential,
                target.mod_loader,
                OperationMeta::local(
                    &target.profile_id.to_string(),
                    crate::profile::server::state::OperationKind::Manual,
                ),
            );
            tokio::task::spawn_blocking(move || {
                let mut session = open_session_blocking(&settings, &password, mod_loader)?;
                engine::set_config_policy(
                    &mut session,
                    &request.path,
                    request.policy,
                    pinned_at.as_ref(),
                    &meta,
                )
            })
            .await
            .map_err(|err| eyre::eyre!("policy update failed: {err}"))??;
        }
    }

    Ok(())
}

/// Explicitly records that the user verified a restart outside Gale.
/// Reading a running status cannot establish that a restart happened.
#[command]
pub async fn acknowledge_external_server_restart(
    request: ServerSyncStatusRequest,
    app: AppHandle,
) -> Result<()> {
    let target = sync_target(&app)?;
    match resolve_executor(&target, &request.password, &request.worker_token)? {
        Executor::Worker(client) => client.acknowledge_external_restart().await?,
        Executor::Local(credential) => {
            let settings = target.settings.clone();
            let mod_loader = target.mod_loader;
            let meta = OperationMeta::local(
                &target.profile_id.to_string(),
                crate::profile::server::state::OperationKind::Manual,
            );
            tokio::task::spawn_blocking(move || {
                let mut session = open_session_blocking(&settings, &credential, mod_loader)?;
                engine::acknowledge_external_restart(&mut session, &meta).map(|_| ())
            })
            .await
            .map_err(|err| eyre::eyre!("restart acknowledgment failed: {err}"))??;
        }
    }
    Ok(())
}

/// Updates the worker's automation toggles and mirrors the state it
/// confirmed into the stored settings, so both dialogs reflect the
/// worker's actual behavior. Returns the worker's confirmed status.
#[command]
pub async fn configure_worker(
    request: ConfigureWorkerRequest,
    app: AppHandle,
) -> Result<StatusResponse> {
    let target = sync_target(&app)?;
    if target.settings.sync_mode != SyncMode::Worker {
        return Err(
            eyre::eyre!("automatic mod deployment is only available in Worker mode").into(),
        );
    }

    let secrets = ServerSecrets::for_profile(target.profile_id)?;
    let mut remote = target.settings.clone();
    remote.worker.automation.auto_deploy_mods = request.auto_deploy_mods;
    remote.restart_policy = request.restart_policy;
    let confirmed = configure_running_worker(
        &secrets,
        &remote,
        sync_id_for(&app, target.profile_id).as_deref(),
        &request.worker_token,
    )
    .await?;

    // Persist what the worker confirmed — by profile id, so an
    // active-profile switch during the push cannot redirect the write.
    let mut manager = app.lock_manager();
    let (_, profile) = manager.profile_by_id_mut(target.profile_id)?;
    let mut settings = profile.server_settings.clone().unwrap_or_default();
    settings.remote.worker.automation.auto_deploy_mods = confirmed.auto_deploy_mods;
    settings.remote.restart_policy = confirmed.restart_policy;
    profile.server_settings = Some(settings);
    profile.save(&app, true)?;

    Ok(confirmed)
}

// ---------- Local-mode orchestration ----------

/// The pinned profile/server context for one operation, captured before
/// any await so an active-profile switch cannot redirect it (R03).
pub(crate) struct SyncTarget {
    pub(crate) profile_id: i64,
    /// The profile's game slug, captured so plan approvals stay bound to
    /// the originating game across an active-profile switch.
    pub(crate) game: String,
    pub(crate) sync_id: Option<String>,
    pub(crate) mod_loader: &'static ModLoader<'static>,
    pub(crate) settings: RemoteServerSettings,
    pub(crate) cache_dir: PathBuf,
}

pub(crate) fn sync_target(app: &AppHandle) -> eyre::Result<SyncTarget> {
    let manager = app.lock_manager();
    let profile = manager.active_profile();
    let settings = profile
        .server_settings
        .as_ref()
        .map(|settings| settings.remote.clone())
        .ok_or_eyre("the dedicated server has not been configured yet")?;

    Ok(SyncTarget {
        profile_id: profile.id,
        game: manager.active_game().game.slug.to_string(),
        sync_id: profile.sync.as_ref().map(|sync| sync.id().to_owned()),
        mod_loader: manager.active_mod_loader(),
        settings,
        cache_dir: app.lock_prefs().cache_dir(),
    })
}

/// The sync id of a specific profile, looked up by id rather than through
/// the active profile, so an active-profile switch cannot redirect it.
pub(crate) fn sync_id_for(app: &AppHandle, profile_id: i64) -> Option<String> {
    app.lock_manager()
        .profile_by_id(profile_id)
        .ok()
        .and_then(|(_, profile)| profile.sync.as_ref().map(|sync| sync.id().to_owned()))
}

enum Executor {
    Local(String),
    Worker(WorkerClient),
}

fn resolve_executor(
    target: &SyncTarget,
    password: &str,
    worker_token: &str,
) -> eyre::Result<Executor> {
    match target.settings.sync_mode {
        SyncMode::Local => Ok(Executor::Local(remote_credential(
            &ServerSecrets::for_profile(target.profile_id)?,
            &target.settings,
            password,
        )?)),
        SyncMode::Worker => Ok(Executor::Worker(worker_client(
            &ServerSecrets::for_profile(target.profile_id)?,
            &target.settings,
            worker_token,
        )?)),
    }
}

pub(crate) fn worker_client(
    secrets: &ServerSecrets,
    settings: &RemoteServerSettings,
    provided: &str,
) -> eyre::Result<WorkerClient> {
    let token = secrets.resolve(ServerSecret::WorkerToken, provided)?;
    WorkerClient::new(settings, token)
}

pub(crate) fn remote_credential(
    secrets: &ServerSecrets,
    settings: &RemoteServerSettings,
    provided: &str,
) -> eyre::Result<String> {
    match remote_secret(settings) {
        Some(secret) => secrets.resolve(secret, provided),
        None => Ok(String::new()),
    }
}

/// Connects to the remote server, failing on untrusted keys rather than
/// silently sending credentials.
fn connect_remote(
    settings: &RemoteServerSettings,
    password: &str,
) -> eyre::Result<Box<dyn RemoteOps>> {
    match RemoteConnection::connect(settings, password)? {
        ConnectionAttempt::Connected(connection) => Ok(Box::new(connection)),
        ConnectionAttempt::HostKeyUntrusted { fingerprint } => bail!(
            "SFTP host key is not trusted ({fingerprint}); trust it from the server settings first"
        ),
        ConnectionAttempt::CertificateUntrusted { fingerprint } => bail!(
            "FTPS certificate is not trusted ({fingerprint}); pin it from the server settings first"
        ),
    }
}

fn open_session_blocking(
    settings: &RemoteServerSettings,
    password: &str,
    mod_loader: &'static ModLoader<'static>,
) -> eyre::Result<engine::Session> {
    let spec = DeploymentSpec::for_loader(mod_loader)?;
    engine::open_session(
        connect_remote(settings, password)?,
        &spec,
        settings.server_directory()?,
    )
}

async fn open_remote_session(
    settings: &RemoteServerSettings,
    password: &str,
    mod_loader: &'static ModLoader<'static>,
) -> eyre::Result<engine::Session> {
    let (settings, password) = (settings.clone(), password.to_owned());
    tokio::task::spawn_blocking(move || open_session_blocking(&settings, &password, mod_loader))
        .await
        .map_err(|err| eyre::eyre!("remote session worker failed: {err}"))?
}

/// Fetches the canonical publication and stages its payload, the same two
/// steps the worker performs, so both executors see identical content.
async fn fetch_and_stage(
    app: &AppHandle,
    target: &SyncTarget,
    spec: &DeploymentSpec,
    include_mods: bool,
    progress: &mut ProgressReporter,
) -> eyre::Result<(FetchedPublication, plan::DesiredDeployment)> {
    let sync_id = target
        .sync_id
        .clone()
        .ok_or_eyre("this profile is not published; publish it before syncing the server")?;

    let publication = sync::fetch_publication(&sync_id, app)
        .await
        .context("failed to fetch the canonical publication")?;
    if include_mods {
        progress.phase(SyncPhase::StagingPayload);
    }

    let source = CachePayloadSource {
        root: target.cache_dir.clone(),
        client: reqwest::Client::new(),
        mod_loader: target.mod_loader,
    };

    let mut staging_total = None;
    let desired = stage::stage_publication(
        &Publication::from_fetched(&publication),
        &source,
        spec,
        include_mods,
        |completed, total, name| {
            if staging_total.is_none() {
                progress.work(total, None);
                staging_total = Some(total);
            }
            if !name.is_empty() {
                progress.item(name);
            }
            progress.advance(completed, None);
        },
    )
    .await
    .context("failed to stage the published mod set")?;

    Ok((publication, desired))
}

/// The context the plan hash binds an approval to: the profile, game,
/// remote identity, and the restart policy the operation will apply.
fn plan_context(target: &SyncTarget, restart_policy: Option<RestartPolicy>) -> plan::PlanContext {
    plan::PlanContext {
        profile_id: target.profile_id.to_string(),
        game: target.game.clone(),
        target: target.settings.describe_target(),
        restart_policy: restart_policy.unwrap_or(target.settings.restart_policy),
    }
}

async fn local_preview(
    app: &AppHandle,
    target: &SyncTarget,
    selection: &DeploySelection,
    restart_policy: Option<RestartPolicy>,
    credential: &str,
    run_id: String,
) -> eyre::Result<PreviewResponse> {
    let event_app = app.clone();
    let mut progress =
        ProgressReporter::new(run_id, SyncOperation::Preview, selection, move |snapshot| {
            let _ = event_app.emit("server_sync_operation_progress", snapshot);
        });
    let result = local_preview_inner(
        app,
        target,
        selection,
        restart_policy,
        credential,
        &mut progress,
    )
    .await;
    if result.is_err() {
        progress.failed();
    } else {
        progress.succeeded();
    }
    result
}

async fn local_preview_inner(
    app: &AppHandle,
    target: &SyncTarget,
    selection: &DeploySelection,
    restart_policy: Option<RestartPolicy>,
    credential: &str,
    progress: &mut ProgressReporter,
) -> eyre::Result<PreviewResponse> {
    let spec = DeploymentSpec::for_loader(target.mod_loader)?;
    let (publication, desired) =
        fetch_and_stage(app, target, &spec, selection.include_mods, progress).await?;

    let (settings, password, selection, mod_loader, context, meta) = (
        target.settings.clone(),
        credential.to_owned(),
        selection.clone(),
        target.mod_loader,
        plan_context(target, restart_policy),
        OperationMeta::local(
            &target.profile_id.to_string(),
            crate::profile::server::state::OperationKind::Manual,
        ),
    );
    let mut blocking_progress = std::mem::replace(
        progress,
        ProgressReporter::silent(SyncOperation::Preview, &selection),
    );
    let (result, returned) = tokio::task::spawn_blocking(move || {
        let result = (|| {
            blocking_progress.phase(SyncPhase::Connecting);
            let spec = DeploymentSpec::for_loader(mod_loader)?;
            let ops = connect_remote(&settings, &password)?;
            blocking_progress.phase(SyncPhase::ReadingState);
            let mut session = engine::open_session(ops, &spec, settings.server_directory()?)?;
            engine::preview_with_progress(
                &mut session,
                &Publication::from_fetched(&publication),
                &desired,
                &selection,
                &context,
                &meta,
                &mut blocking_progress,
            )
        })();
        (result, blocking_progress)
    })
    .await
    .map_err(|err| eyre::eyre!("preview worker failed: {err}"))?;
    *progress = returned;
    result
}

async fn local_deploy(
    app: &AppHandle,
    target: &SyncTarget,
    request: ServerSyncDeployRequest,
    credential: &str,
) -> eyre::Result<DeployResponse> {
    let event_app = app.clone();
    let mut progress = ProgressReporter::new(
        operation_run_id(&request.run_id),
        SyncOperation::Deploy,
        &request.selection,
        move |snapshot| {
            let _ = event_app.emit("server_sync_operation_progress", snapshot);
        },
    );
    let result = local_deploy_inner(app, target, request, credential, &mut progress).await;
    if result.is_err() {
        progress.failed();
    }
    result
}

async fn local_deploy_inner(
    app: &AppHandle,
    target: &SyncTarget,
    request: ServerSyncDeployRequest,
    credential: &str,
    progress: &mut ProgressReporter,
) -> eyre::Result<DeployResponse> {
    let spec = DeploymentSpec::for_loader(target.mod_loader)?;
    let (publication, desired) =
        fetch_and_stage(app, target, &spec, request.selection.include_mods, progress).await?;

    let meta = OperationMeta::local(
        &target.profile_id.to_string(),
        crate::profile::server::state::OperationKind::Manual,
    );

    let (settings, password, selection, plan_hash, mod_loader, context, force) = (
        target.settings.clone(),
        credential.to_owned(),
        request.selection.clone(),
        request.plan_hash.clone(),
        target.mod_loader,
        plan_context(target, request.restart_policy),
        request.force,
    );
    let connect = {
        let (settings, password) = (settings.clone(), password.clone());
        move || connect_remote(&settings, &password)
    };

    let meta2 = meta.clone();
    let mut blocking_progress = std::mem::replace(
        progress,
        ProgressReporter::silent(SyncOperation::Deploy, &selection),
    );
    let (result, returned) = tokio::task::spawn_blocking(move || {
        let result = (|| {
            blocking_progress.phase(SyncPhase::Connecting);
            let spec = DeploymentSpec::for_loader(mod_loader)?;
            let ops = connect_remote(&settings, &password)?;
            blocking_progress.phase(SyncPhase::ReadingState);
            let mut session = engine::open_session(ops, &spec, settings.server_directory()?)?;
            let deployment = engine::deploy_with_progress(
                &mut session,
                connect,
                &Publication::from_fetched(&publication),
                &desired,
                &selection,
                &context,
                &meta2,
                Some(plan_hash.as_str()),
                force,
                &mut blocking_progress,
                |_| {},
            )?;
            Ok::<_, eyre::Report>((session, deployment))
        })();
        (result, blocking_progress)
    })
    .await
    .map_err(|err| eyre::eyre!("deployment worker failed: {err}"))?;
    *progress = returned;
    let (session, deployment) = result?;

    // Restart policy through the configured host provider; unknown player
    // presence is never treated as empty.
    let secrets = ServerSecrets::for_profile(target.profile_id)?;
    let dat_host_password = secrets.get(ServerSecret::DatHostPassword)?;
    let host = host::from_settings(&target.settings.host_control, dat_host_password.as_deref());
    let policy = request
        .restart_policy
        .unwrap_or(target.settings.restart_policy);
    engine::complete_with_progress(session, deployment, host.as_ref(), policy, meta, progress).await
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

/// Saves into the profile captured before any asynchronous work.
fn save_settings_for(
    app: &AppHandle,
    profile_id: i64,
    settings: ProfileServerSettings,
) -> eyre::Result<()> {
    let mut manager = app.lock_manager();
    let (_, profile) = manager.profile_by_id_mut(profile_id)?;
    profile.server_settings = Some(settings);
    profile.save(app, true)
}

pub(crate) fn save_remote_settings_for(
    app: &AppHandle,
    profile_id: i64,
    remote: RemoteServerSettings,
) -> eyre::Result<()> {
    let mut manager = app.lock_manager();
    let (_, profile) = manager.profile_by_id_mut(profile_id)?;
    let mut settings = profile.server_settings.clone().unwrap_or_default();
    settings.remote = remote;
    profile.server_settings = Some(settings);
    profile.save(app, true)
}

#[cfg(test)]
mod tests {
    use super::super::settings::ServerLocation;
    use super::*;

    /// Remote settings in worker mode bound to a worker address.
    fn worker_settings() -> ProfileServerSettings {
        let mut settings = ProfileServerSettings {
            location: ServerLocation::Remote,
            ..ProfileServerSettings::default()
        };
        settings.remote.sync_mode = SyncMode::Worker;
        settings.remote.worker.address = "http://127.0.0.1:8472".to_owned();
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
        new.remote.worker.automation.auto_deploy_mods = true;
        assert!(worker_config_differs(Some(&stored), &new));

        // The restart policy is worker-run configuration too.
        let mut new = stored.clone();
        new.remote.restart_policy = RestartPolicy::Immediate;
        assert!(worker_config_differs(Some(&stored), &new));

        // Retargeting the worker address pushes the flags to the new
        // worker.
        let mut new = stored.clone();
        new.remote.worker.address = "http://127.0.0.1:9999".to_owned();
        assert!(worker_config_differs(Some(&stored), &new));

        // Unrelated fields never trigger a worker call.
        let mut new = stored.clone();
        new.remote.host = "example.com".to_owned();
        new.local.server_name = "Other".to_owned();
        new.sync_dialog.restart_policy = RestartPolicy::WhenEmpty;
        assert!(!worker_config_differs(Some(&stored), &new));

        // A stored binding that never pointed at a worker differs, as
        // does having no stored settings at all.
        let mut local = stored.clone();
        local.remote.sync_mode = SyncMode::Local;
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
        remote.authentication = super::super::settings::RemoteAuthentication::Agent;
        secrets.remove(ServerSecret::SftpPassword).unwrap();
        assert!(!credential_missing(&secrets, &remote, "", ""));

        // Worker mode keys off the worker token instead.
        remote.sync_mode = SyncMode::Worker;
        assert!(credential_missing(&secrets, &remote, "typed", ""));
        assert!(!credential_missing(&secrets, &remote, "", "token"));
        secrets
            .set(ServerSecret::WorkerToken, "saved-token")
            .unwrap();
        assert!(!credential_missing(&secrets, &remote, "", ""));
    }

    /// Every credential stored, in the order the assertion tuples use.
    fn fully_stocked_secrets() -> ServerSecrets {
        use super::super::secrets::in_memory_secrets;

        let secrets = in_memory_secrets();
        for secret in [
            ServerSecret::GamePassword,
            ServerSecret::SftpPassword,
            ServerSecret::FtpPassword,
            ServerSecret::SshKeyPassphrase,
            ServerSecret::DatHostPassword,
            ServerSecret::WorkerToken,
        ] {
            secrets.set(secret, "stored").unwrap();
        }
        secrets
    }

    /// Presence of every secret, in `fully_stocked_secrets` order.
    fn stored(secrets: &ServerSecrets) -> [bool; 6] {
        [
            secrets.has(ServerSecret::GamePassword).unwrap(),
            secrets.has(ServerSecret::SftpPassword).unwrap(),
            secrets.has(ServerSecret::FtpPassword).unwrap(),
            secrets.has(ServerSecret::SshKeyPassphrase).unwrap(),
            secrets.has(ServerSecret::DatHostPassword).unwrap(),
            secrets.has(ServerSecret::WorkerToken).unwrap(),
        ]
    }

    fn save_request(remote: RemoteServerSettings) -> SaveServerSettingsRequest {
        SaveServerSettingsRequest {
            settings: ProfileServerSettings {
                remote,
                ..ProfileServerSettings::default()
            },
            game_password: String::new(),
            remember_game_password: true,
            remote_password: String::new(),
            remember_remote_password: true,
            worker_token: String::new(),
            remember_worker_token: true,
            dat_host_password: String::new(),
            remember_dat_host_password: true,
        }
    }

    const ALL: [bool; 6] = [true; 6];
    const SFTP: usize = 1;
    const FTP: usize = 2;
    const KEY: usize = 3;
    const DATHOST: usize = 4;
    const TOKEN: usize = 5;

    /// Only the flag's own credential is dropped — index `cleared` is the
    /// one slot that flips to false.
    fn assert_only_cleared(cleared: usize, stored: [bool; 6]) {
        let mut expected = ALL;
        expected[cleared] = false;
        assert_eq!(stored, expected, "only slot {cleared} should be cleared");
    }

    #[test]
    fn forgetting_one_remote_credential_leaves_all_others() {
        use super::super::settings::{RemoteAuthentication, RemoteProtocol};

        // SFTP password auth clears only the SFTP password.
        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remember_remote_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(SFTP, stored(&secrets));

        // Private-key auth clears only the key passphrase.
        let secrets = fully_stocked_secrets();
        let mut remote = RemoteServerSettings::default();
        remote.authentication = RemoteAuthentication::PrivateKey;
        let mut request = save_request(remote);
        request.remember_remote_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(KEY, stored(&secrets));

        // FTP/FTPS clears only the FTP password.
        for protocol in [RemoteProtocol::Ftp, RemoteProtocol::Ftps] {
            let secrets = fully_stocked_secrets();
            let mut remote = RemoteServerSettings::default();
            remote.protocol = protocol;
            let mut request = save_request(remote);
            request.remember_remote_password = false;
            persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
            assert_only_cleared(FTP, stored(&secrets));
        }

        // Agent auth has no transport credential: nothing is cleared.
        let secrets = fully_stocked_secrets();
        let mut remote = RemoteServerSettings::default();
        remote.authentication = RemoteAuthentication::Agent;
        let mut request = save_request(remote);
        request.remember_remote_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_eq!(stored(&secrets), ALL);

        // Worker token and DatHost password are independent flags.
        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remember_worker_token = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(TOKEN, stored(&secrets));

        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remember_dat_host_password = false;
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_only_cleared(DATHOST, stored(&secrets));
    }

    #[test]
    fn remembered_credentials_stay_untouched_or_replace_only_themselves() {
        // All remember flags on with empty values: nothing changes.
        let secrets = fully_stocked_secrets();
        let request = save_request(RemoteServerSettings::default());
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_eq!(stored(&secrets), ALL);

        // A typed value replaces only its own secret.
        let secrets = fully_stocked_secrets();
        let mut request = save_request(RemoteServerSettings::default());
        request.remote_password = "new-remote-password".to_owned();
        persist_remote_credentials(&secrets, &request.settings.remote, &request).unwrap();
        assert_eq!(stored(&secrets), ALL);
        assert_eq!(
            secrets.get(ServerSecret::SftpPassword).unwrap().as_deref(),
            Some("new-remote-password")
        );
        assert_eq!(
            secrets.get(ServerSecret::WorkerToken).unwrap().as_deref(),
            Some("stored")
        );
    }
}
