//! Routine sync operations, dispatched to whichever executor the profile
//! uses. Worker mode forwards each operation to the worker's HTTP API.
//! Local mode runs the same [`DeployService`] the worker's handlers run,
//! in this process, so both modes share one orchestration.

use std::path::PathBuf;

use eyre::{Context, OptionExt, Result};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use tauri::{AppHandle, Emitter};

use super::{
    engine::OperationMeta,
    host,
    plan::DeploySelection,
    progress::{ProgressReporter, SyncOperation, SyncProgress},
    secrets::{ServerSecret, ServerSecrets},
    service::{Connector, DeployService, PinAdvice},
    settings::{RemoteServerSettings, RestartPolicy, SyncMode, TransportSettings},
    spec::DeploymentSpec,
    state::OperationKind,
    worker_client::WorkerClient,
};
use crate::{
    game::mod_loader::ModLoader,
    profile::{ModManager, sync},
    state::ManagerExt,
    worker::api::{
        DeployRequest, DeployResponse, PolicyRequest, PreviewRequest, PreviewResponse,
        ServerStateSummary, StatusResponse,
    },
};

/// Credentials typed for one request. Empty fields fall back to the
/// stored ones.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialOverrides {
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub worker_token: String,
}

/// The pinned profile/server context for one operation, captured before
/// any await so an active-profile switch cannot redirect it.
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

impl SyncTarget {
    fn meta(&self) -> OperationMeta {
        OperationMeta::local(&self.profile_id.to_string(), OperationKind::Manual)
    }

    /// Emits progress to the page, which matches the run id before showing it.
    fn reporter(
        &self,
        app: &AppHandle,
        run_id: String,
        operation: SyncOperation,
        selection: &DeploySelection,
    ) -> ProgressReporter {
        let app = app.clone();
        ProgressReporter::new(run_id, operation, selection, move |snapshot| {
            let _ = app.emit("server_sync_operation_progress", snapshot);
        })
    }

    /// The restart policy an operation applies: the requested one, or
    /// the stored one.
    fn restart_policy(&self, requested: Option<RestartPolicy>) -> RestartPolicy {
        requested.unwrap_or(self.settings.automation.restart_policy)
    }
}

pub(crate) fn sync_target(app: &AppHandle) -> Result<SyncTarget> {
    let manager = app.lock_manager();
    let settings = manager
        .active_profile()
        .server_settings
        .as_ref()
        .map(|settings| settings.remote.clone())
        .ok_or_eyre("the dedicated server has not been configured yet")?;

    Ok(active_target(app, &manager, settings))
}

/// The active profile's target acting on `settings` instead of the stored
/// ones, for setup that must not depend on an earlier save.
pub(crate) fn sync_target_with(app: &AppHandle, settings: RemoteServerSettings) -> SyncTarget {
    active_target(app, &app.lock_manager(), settings)
}

fn active_target(
    app: &AppHandle,
    manager: &ModManager,
    settings: RemoteServerSettings,
) -> SyncTarget {
    let profile = manager.active_profile();

    SyncTarget {
        profile_id: profile.id,
        game: manager.active_game().game.slug.to_string(),
        sync_id: profile.sync.as_ref().map(|sync| sync.id().to_owned()),
        mod_loader: manager.active_mod_loader(),
        settings,
        cache_dir: app.lock_prefs().cache_dir(),
    }
}

/// The sync id of a specific profile, looked up by id rather than through
/// the active profile, so an active-profile switch cannot redirect it.
pub(crate) fn sync_id_for(app: &AppHandle, profile_id: i64) -> Option<String> {
    app.lock_manager()
        .profile_by_id(profile_id)
        .ok()
        .and_then(|(_, profile)| profile.sync.as_ref().map(|sync| sync.id().to_owned()))
}

pub(crate) fn worker_client(
    secrets: &ServerSecrets,
    address: &str,
    provided: &str,
) -> Result<WorkerClient> {
    let token = secrets.resolve(ServerSecret::WorkerToken, provided)?;
    WorkerClient::new(address, token)
}

pub(crate) fn remote_credential(
    secrets: &ServerSecrets,
    settings: &TransportSettings,
    provided: &str,
) -> Result<String> {
    match ServerSecret::required_by(settings) {
        Some(secret) => secrets.resolve(secret, provided),
        None => Ok(String::new()),
    }
}

/// What an executor reports about the server.
// Returned once per status read and consumed immediately, so the variants'
// size difference costs nothing.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ExecutorStatus {
    /// The worker's journal and, when refreshed, its live server read.
    Worker(StatusResponse),
    /// A live session this process read.
    Session {
        server: ServerStateSummary,
        warnings: Vec<String>,
    },
}

/// The routine sync operations, in the worker API's shape.
pub(crate) trait SyncExecutor: Send + Sync {
    /// The executor's view of the server. `refresh` asks for a live read;
    /// `None` means there is nothing to report without one.
    fn read_status(&self, refresh: bool) -> BoxFuture<'_, Result<Option<ExecutorStatus>>>;

    /// The latest progress snapshot, for executors that run elsewhere.
    fn progress(&self) -> BoxFuture<'_, Result<Option<SyncProgress>>>;

    fn preview(&self, request: PreviewRequest) -> BoxFuture<'_, Result<PreviewResponse>>;

    fn deploy(&self, request: DeployRequest) -> BoxFuture<'_, Result<DeployResponse>>;

    /// Sets a persistent per-file config update policy. The executor pins
    /// it at the canonical publication, so callers never supply the pin.
    fn set_policy(&self, request: PolicyRequest) -> BoxFuture<'_, Result<()>>;

    /// Records that the user verified a restart outside Gale.
    fn acknowledge_external_restart(&self) -> BoxFuture<'_, Result<()>>;
}

/// The executor `target` is configured for, authenticated with the
/// request's credentials or the stored ones.
pub(crate) fn executor(
    app: &AppHandle,
    target: SyncTarget,
    credentials: &CredentialOverrides,
) -> Result<Box<dyn SyncExecutor>> {
    let secrets = ServerSecrets::for_profile(target.profile_id)?;
    Ok(match target.settings.executor.mode {
        SyncMode::Local => Box::new(LocalExecutor {
            credential: remote_credential(
                &secrets,
                &target.settings.transport,
                &credentials.password,
            )?,
            app: app.clone(),
            target,
        }),
        SyncMode::HostedWorker | SyncMode::Worker => Box::new(worker_client(
            &secrets,
            &target.settings.executor.worker_address,
            &credentials.worker_token,
        )?),
    })
}

/// Runs the deployment service in this process against the profile's
/// stored server settings.
struct LocalExecutor {
    app: AppHandle,
    target: SyncTarget,
    credential: String,
}

impl LocalExecutor {
    fn service(&self) -> Result<DeployService> {
        Ok(DeployService {
            connector: Connector {
                settings: self.target.settings.transport.clone(),
                password: self.credential.clone(),
                pin_advice: PinAdvice {
                    host_key: "trust it from the server settings first",
                    certificate: "pin it from the server settings first",
                },
            },
            spec: DeploymentSpec::for_loader(self.target.mod_loader)?,
            profile_id: self.target.profile_id.to_string(),
            game: self.target.game.clone(),
            mod_loader: self.target.mod_loader,
            cache_dir: self.target.cache_dir.clone(),
            http: self.app.http().clone(),
        })
    }

    /// Fetches the canonical publication, the same source the worker
    /// polls, so both executors see identical content.
    async fn publication(&self) -> Result<sync::FetchedPublication> {
        let sync_id =
            self.target.sync_id.as_deref().ok_or_eyre(
                "this profile is not published; publish it before syncing the server",
            )?;

        sync::fetch_publication(sync_id, &self.app)
            .await
            .context("failed to fetch the canonical publication")
    }
}

impl SyncExecutor for LocalExecutor {
    fn read_status(&self, refresh: bool) -> BoxFuture<'_, Result<Option<ExecutorStatus>>> {
        Box::pin(async move {
            if !refresh {
                return Ok(None);
            }
            let mut session = self.service()?.read_session().await?;
            Ok(Some(ExecutorStatus::Session {
                warnings: std::mem::take(&mut session.warnings),
                server: session.into(),
            }))
        })
    }

    fn progress(&self) -> BoxFuture<'_, Result<Option<SyncProgress>>> {
        // Local progress streams to the page as events instead.
        Box::pin(async { Ok(None) })
    }

    fn preview(&self, request: PreviewRequest) -> BoxFuture<'_, Result<PreviewResponse>> {
        Box::pin(async move {
            let PreviewRequest {
                run_id,
                selection,
                restart_policy,
            } = request;
            let mut progress =
                self.target
                    .reporter(&self.app, run_id, SyncOperation::Preview, &selection);
            let result = async {
                let service = self.service()?;
                let publication = self.publication().await?;
                service
                    .preview(
                        publication,
                        selection,
                        self.target.restart_policy(restart_policy),
                        self.target.meta(),
                        &mut progress,
                    )
                    .await
            }
            .await;
            if result.is_err() {
                progress.failed();
            } else {
                progress.succeeded();
            }
            result
        })
    }

    fn deploy(&self, request: DeployRequest) -> BoxFuture<'_, Result<DeployResponse>> {
        Box::pin(async move {
            let DeployRequest {
                run_id,
                selection,
                plan_hash,
                restart_policy,
                force,
            } = request;
            let mut progress =
                self.target
                    .reporter(&self.app, run_id, SyncOperation::Deploy, &selection);
            let result = async {
                let service = self.service()?;
                let publication = self.publication().await?;
                let dat_host_password = ServerSecrets::for_profile(self.target.profile_id)?
                    .get(ServerSecret::DatHostPassword)?;
                let host = host::from_settings(
                    &self.target.settings.host_control,
                    dat_host_password.as_deref(),
                );
                service
                    .deploy(
                        publication,
                        selection,
                        Some(plan_hash),
                        force,
                        self.target.restart_policy(restart_policy),
                        host.as_ref(),
                        self.target.meta(),
                        &mut progress,
                    )
                    .await
            }
            .await;
            if result.is_err() {
                progress.failed();
            }
            result
        })
    }

    fn set_policy(&self, request: PolicyRequest) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // The policy pin comes from the canonical publication on the
            // trusted side, because a client-supplied value could pin the
            // policy at the wrong revision.
            let pinned_at = match &self.target.sync_id {
                Some(id) => sync::fetch_publication(id, &self.app)
                    .await
                    .context("failed to fetch the canonical publication for the policy pin")?
                    .config
                    .get(&request.path)
                    .map(|file| file.hash.clone()),
                None => None,
            };
            self.service()?
                .set_config_policy(request.path, request.policy, pinned_at, self.target.meta())
                .await
        })
    }

    fn acknowledge_external_restart(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.service()?
                .acknowledge_external_restart(self.target.meta())
                .await
                .map(|_| ())
        })
    }
}
