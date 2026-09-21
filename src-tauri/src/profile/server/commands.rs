//! Dedicated-server commands.
//!
//! Two concerns are deliberately separate:
//!
//! - **Setup** (`set_dedicated_server_settings`, `test_*_connection`)
//!   persists settings and credentials.
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
            engine::{self, EngineProgress, OperationMeta},
            host, local, local_worker,
            plan::{self, DeploySelection, Publication},
            remote::{self, ConnectionAttempt, ConnectionTestResult, RemoteConnection, RemoteOps},
            runtime::{self, ServerStatus, SharedChild},
            secrets::{ServerSecret, ServerSecrets},
            settings::{ProfileServerSettings, RemoteServerSettings, RestartPolicy, SyncMode},
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
/// dialog. Empty credential fields leave stored credentials untouched;
/// `rememberCredentials = false` clears them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveServerSettingsRequest {
    pub settings: ProfileServerSettings,
    #[serde(default)]
    pub remote_password: String,
    #[serde(default)]
    pub worker_token: String,
    #[serde(default)]
    pub dat_host_password: String,
    #[serde(default)]
    pub remember_credentials: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteServerRequest {
    pub settings: RemoteServerSettings,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub worker_token: String,
    #[serde(default)]
    pub dat_host_password: String,
    #[serde(default)]
    pub remember_password: bool,
}

/// Routine remote requests use stored settings; the credential fields only
/// override what's in the keyring.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSyncRequest {
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
    pub auto_sync: bool,
    pub auto_mods: bool,
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

#[command]
pub fn set_dedicated_server_settings(
    request: SaveServerSettingsRequest,
    app: AppHandle,
) -> Result<()> {
    request.settings.validate()?;

    let profile_id = app.lock_manager().active_profile().id;
    let secrets = ServerSecrets::for_profile(profile_id)?;

    save_settings_for(&app, profile_id, request.settings.clone())?;

    // Credentials: a provided value is persisted per `remember`, an empty
    // one leaves the store alone unless `remember` is off (which clears).
    persist_credential(
        &secrets,
        remote_secret(&request.settings.remote),
        &request.remote_password,
        request.remember_credentials,
    )?;
    persist_credential(
        &secrets,
        Some(ServerSecret::WorkerToken),
        &request.worker_token,
        request.remember_credentials,
    )?;
    persist_credential(
        &secrets,
        Some(ServerSecret::DatHostPassword),
        &request.dat_host_password,
        request.remember_credentials,
    )?;

    Ok(())
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

// ---------- local launch ----------

#[command]
pub async fn launch_dedicated_server(
    request: LaunchDedicatedServerRequest,
    app: AppHandle,
) -> Result<ServerStatus> {
    let profile_id = active_profile_id(&app);

    if app.lock_prefs().pull_before_launch {
        sync::pull_profile(false, profile_id, &app).await?;
    }

    ensure_no_pending_installs(&app)?;

    if app.lock_server_runtime().is_running() {
        return Err(eyre::eyre!("a Gale-managed dedicated server is already running").into());
    }

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

    let process = {
        let prefs = app.lock_prefs();
        let manager = app.lock_manager();
        let (game, profile) = manager.profile_by_id(profile_id)?;
        let managed = manager
            .games
            .get(&game)
            .ok_or_eyre("the profile's game is not installed")?;

        local::launch(managed, profile, &settings.local, &password, &prefs)?
    };

    let pid = process
        .child
        .id()
        .ok_or_eyre("dedicated server process has no pid")?;
    let child: SharedChild = Arc::new(Mutex::new(process.child));

    let status = {
        let mut runtime = app.lock_server_runtime();
        runtime.register(
            process.profile_id,
            process.game,
            process.server_dir,
            pid,
            child.clone(),
        )?
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

    if matches!(result, ConnectionTestResult::Connected { .. }) {
        // Save into the profile the test was started for. The network
        // call awaited, so the active profile may have changed.
        save_tested_connection_for(&app, profile_id, &secrets, &request, &credential)?;
    }

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
    // The profile's sync identity is captured before the await, since an
    // active-profile switch must not mix identities or redirect the save.
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

    save_remote_request_for(&app, profile_id, &secrets, &request, "")?;
    persist_credential(
        &secrets,
        Some(ServerSecret::WorkerToken),
        &request.worker_token,
        request.remember_password,
    )?;

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

    if !request.refresh {
        return Ok(status);
    }

    match resolve_executor(&target, &request.password, &request.worker_token)? {
        Executor::Worker(client) => {
            let worker = client.status(true).await?;
            status.server = worker.server.clone();
            status.publication_revision = worker.observed_revision.or(status.publication_revision);
            status.worker = Some(worker);
        }
        Executor::Local(credential) => {
            let (settings, password) = (target.settings.clone(), credential);
            match open_remote_session(&settings, &password, target.mod_loader).await {
                Ok(session) => {
                    if session.migrated {
                        status.warnings.push(
                            "adopted the previous deployment manifest into the new state format"
                                .to_owned(),
                        );
                    }
                    status.server = Some(ServerStateSummary {
                        mods_revision: session.state.mods_revision.clone(),
                        restart_required: session.state.restart_required,
                        pending_configs: session.state.pending.len(),
                        last_operation: session.state.last_operation.clone(),
                        lease: session.lease.clone(),
                    });
                    status.warnings.extend(session.warnings);
                }
                Err(err) => {
                    status.credential_required = true;
                    status
                        .warnings
                        .push(format!("remote session failed: {err:#}"));
                }
            }
        }
    }

    Ok(status)
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
            .preview(&request.selection, request.restart_policy)
            .await?),
        Executor::Local(credential) => Ok(local_preview(
            &app,
            &target,
            &request.selection,
            request.restart_policy,
            &credential,
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
            )
            .await?),
        Executor::Local(credential) => Ok(local_deploy(&app, &target, request, &credential).await?),
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

/// Updates the worker's automation toggles and mirrors them into the
/// stored settings so the UI reflects the worker's actual behavior.
#[command]
pub async fn configure_worker(request: ConfigureWorkerRequest, app: AppHandle) -> Result<()> {
    let target = sync_target(&app)?;
    if target.settings.sync_mode != SyncMode::Worker {
        return Err(
            eyre::eyre!("automatic synchronization is only available in Worker mode").into(),
        );
    }

    let secrets = ServerSecrets::for_profile(target.profile_id)?;
    let client = worker_client(&secrets, &target.settings, &request.worker_token)?;
    client
        .configure(request.auto_sync, request.auto_mods, request.restart_policy)
        .await?;

    // Persist the same values into the stored settings.
    let mut manager = app.lock_manager();
    let profile = manager.active_profile_mut();
    if profile.id != target.profile_id {
        return Err(eyre::eyre!("the active profile changed while configuring the worker").into());
    }
    let mut settings = profile.server_settings.clone().unwrap_or_default();
    settings.remote.worker.auto_sync = request.auto_sync;
    settings.remote.worker.auto_mods = request.auto_mods;
    settings.remote.restart_policy = request.restart_policy;
    profile.server_settings = Some(settings);
    profile.save(&app, true)?;

    Ok(())
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
) -> eyre::Result<engine::Session<'static>> {
    let spec = Box::leak(Box::new(DeploymentSpec::for_loader(mod_loader)?));
    engine::open_session(
        connect_remote(settings, password)?,
        spec,
        settings.server_directory()?,
    )
}

async fn open_remote_session(
    settings: &RemoteServerSettings,
    password: &str,
    mod_loader: &'static ModLoader<'static>,
) -> eyre::Result<engine::Session<'static>> {
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
) -> eyre::Result<(FetchedPublication, plan::DesiredDeployment)> {
    let sync_id = target
        .sync_id
        .clone()
        .ok_or_eyre("this profile is not published; publish it before syncing the server")?;

    let publication = sync::fetch_publication(&sync_id, app)
        .await
        .context("failed to fetch the canonical publication")?;

    let source = CachePayloadSource {
        root: target.cache_dir.clone(),
        client: reqwest::Client::new(),
        mod_loader: target.mod_loader,
    };

    let progress_app = app.clone();
    let desired = stage::stage_publication(
        &Publication::from_fetched(&publication),
        &source,
        spec,
        include_mods,
        move |completed, total, name| {
            let _ = progress_app.emit(
                "server_sync_stage_progress",
                serde_json::json!({
                    "completed": completed,
                    "total": total,
                    "mod": name,
                }),
            );
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
) -> eyre::Result<PreviewResponse> {
    let spec = DeploymentSpec::for_loader(target.mod_loader)?;
    let (publication, desired) =
        fetch_and_stage(app, target, &spec, selection.include_mods).await?;

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
    tokio::task::spawn_blocking(move || {
        let mut session = open_session_blocking(&settings, &password, mod_loader)?;
        engine::preview(
            &mut session,
            &Publication::from_fetched(&publication),
            &desired,
            &selection,
            &context,
            &meta,
        )
    })
    .await
    .map_err(|err| eyre::eyre!("preview worker failed: {err}"))?
    .map(|preview| PreviewResponse {
        plan: preview.plan,
        busy: preview.busy,
        warnings: preview.warnings,
    })
}

async fn local_deploy(
    app: &AppHandle,
    target: &SyncTarget,
    request: ServerSyncDeployRequest,
    credential: &str,
) -> eyre::Result<DeployResponse> {
    let spec = DeploymentSpec::for_loader(target.mod_loader)?;
    let (publication, desired) =
        fetch_and_stage(app, target, &spec, request.selection.include_mods).await?;

    let meta = OperationMeta::local(
        &target.profile_id.to_string(),
        crate::profile::server::state::OperationKind::Manual,
    );
    let progress_app = app.clone();

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
    let (mut session, deployment) = tokio::task::spawn_blocking(move || {
        let mut session = open_session_blocking(&settings, &password, mod_loader)?;
        let deployment = engine::deploy(
            &mut session,
            connect,
            &Publication::from_fetched(&publication),
            &desired,
            &selection,
            &context,
            &meta2,
            Some(plan_hash.as_str()),
            force,
            move |progress: EngineProgress| {
                let _ = progress_app.emit("server_sync_progress", progress);
            },
        )?;
        Ok::<_, eyre::Report>((session, deployment))
    })
    .await
    .map_err(|err| eyre::eyre!("deployment worker failed: {err}"))??;

    // Restart policy through the configured host provider; unknown player
    // presence is never treated as empty.
    let secrets = ServerSecrets::for_profile(target.profile_id)?;
    let dat_host_password = secrets.get(ServerSecret::DatHostPassword)?;
    let host = host::from_settings(&target.settings.host_control, dat_host_password.as_deref());
    let policy = request
        .restart_policy
        .unwrap_or(target.settings.restart_policy);
    // A restart is owed when this plan changed content *or* an earlier
    // deployment left one pending, since a failed restart must not be lost.
    let requires_restart = deployment.plan.requires_restart || session.state.restart_required;
    let restart = engine::apply_restart_policy(host.as_ref(), policy, requires_restart).await;

    let mut response = DeployResponse {
        plan: deployment.plan.clone(),
        summary: deployment.summary.clone(),
        warnings: deployment.warnings.clone(),
        failed_config_writes: deployment.failed_config_writes.clone(),
        restart,
        state: Default::default(),
    };

    let state = tokio::task::spawn_blocking(move || {
        engine::finish(&mut session, deployment, restart, &meta)
    })
    .await
    .map_err(|err| eyre::eyre!("deployment finish failed: {err}"))??;

    response.state = state;
    Ok(response)
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

/// Persists a tested remote configuration into the profile the operation
/// started on. After an awaited network call the active profile may have
/// changed, and writing through `active_profile_mut` here would combine one
/// profile's credentials with another's settings (R03).
fn save_remote_request_for(
    app: &AppHandle,
    profile_id: i64,
    secrets: &ServerSecrets,
    request: &RemoteServerRequest,
    credential: &str,
) -> eyre::Result<()> {
    save_remote_settings_for(app, profile_id, request.settings.clone())?;
    persist_request_credentials(secrets, request, credential)
}

/// Persists a successful connection test. The proven transport settings
/// and credentials land on the profile the test started for, while the
/// stored executor (`sync_mode`/`worker`) and host-control configuration
/// stay untouched: the dialog's current sync-mode selection may name a
/// worker that does not exist yet, and a test must not activate it.
fn save_tested_connection_for(
    app: &AppHandle,
    profile_id: i64,
    secrets: &ServerSecrets,
    request: &RemoteServerRequest,
    credential: &str,
) -> eyre::Result<()> {
    let stored_remote = {
        let manager = app.lock_manager();
        let (_, profile) = manager.profile_by_id(profile_id)?;
        profile
            .server_settings
            .as_ref()
            .map(|settings| settings.remote.clone())
            .unwrap_or_default()
    };

    save_remote_settings_for(
        app,
        profile_id,
        stored_remote.with_tested_transport(&request.settings),
    )?;
    persist_request_credentials(secrets, request, credential)
}

fn persist_request_credentials(
    secrets: &ServerSecrets,
    request: &RemoteServerRequest,
    credential: &str,
) -> eyre::Result<()> {
    if let Some(secret) = remote_secret(&request.settings)
        && (!credential.is_empty() || !request.remember_password)
    {
        secrets.persist(secret, credential, request.remember_password)?;
    }
    persist_credential(
        secrets,
        Some(ServerSecret::DatHostPassword),
        &request.dat_host_password,
        request.remember_password,
    )
}

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
