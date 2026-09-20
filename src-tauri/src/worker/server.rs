//! The worker's HTTP server and automatic-sync poll loop.
//!
//! Authentication is a single bearer token (`GALE_WORKER_TOKEN`). The API
//! deliberately has no profile/server parameters: the worker is bound to
//! one profile and one remote at startup, so a remote caller can trigger a
//! deployment but can never redirect it at another target.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use eyre::{Context, Result, bail, ensure};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::{
    api::{
        ConfigureRequest, DeployRequest, DeployResponse, ErrorResponse, PolicyRequest,
        PreviewRequest, PreviewResponse, StatusResponse,
    },
    config::WorkerConfig,
    journal::{Journal, busy_marker},
    sync_client::{PublicationProbe, SyncClient},
};
use crate::{
    game,
    profile::{
        server::{
            engine::{self, OperationMeta, Preview, Session},
            host,
            plan::{self, DeploySelection, Publication},
            remote::{ConnectionAttempt, RemoteConnection, RemoteOps},
            settings::RestartPolicy,
            spec::DeploymentSpec,
            stage::{self, CachePayloadSource},
            state::{OperationKind, ServerDeploymentState},
        },
        sync::FetchedPublication,
    },
};

/// Everything a request handler or the poll loop needs.
pub struct WorkerContext {
    config: WorkerConfig,
    journal: Journal,
    sync: SyncClient,
    /// Leaked so `Session<'static>` can move between blocking tasks.
    spec: &'static DeploymentSpec,
    /// Serializes operations inside this process. Cross-executor safety is
    /// the remote lease's job; this keeps a manual request and an automatic
    /// poll from racing in the same process.
    operation_lock: Mutex<()>,
    token: String,
    cache_dir: std::path::PathBuf,
}

impl WorkerContext {
    fn new(config: WorkerConfig, journal: Journal) -> Result<Self> {
        let game = game::from_slug(&config.game)
            .ok_or_else(|| eyre::eyre!("unknown game slug '{}'", config.game))?;
        ensure!(
            game.server,
            "game '{}' has no dedicated-server support",
            config.game
        );
        let spec = DeploymentSpec::for_loader(&game.mod_loader)
            .context("this game's mod loader has no deployment spec")?;

        Ok(Self {
            cache_dir: config.state_dir.join("cache"),
            token: WorkerConfig::token()?,
            sync: SyncClient::new(config.clone()),
            config,
            journal,
            spec: Box::leak(Box::new(spec)),
            operation_lock: Mutex::new(()),
        })
    }

    /// Opens an authenticated remote session. Runs inside `spawn_blocking`.
    fn connect(&self) -> Result<Box<dyn RemoteOps>> {
        let password = WorkerConfig::remote_password();
        match RemoteConnection::connect(&self.config.remote, &password)? {
            ConnectionAttempt::Connected(connection) => Ok(Box::new(connection)),
            ConnectionAttempt::HostKeyUntrusted { fingerprint } => bail!(
                "SFTP host key is not trusted ({fingerprint}); pin it as trustedHostKey in the worker config"
            ),
            ConnectionAttempt::CertificateUntrusted { fingerprint } => bail!(
                "FTPS certificate is not trusted ({fingerprint}); pin it as trustedHostKey in the worker config"
            ),
        }
    }

    fn open_session(&self) -> Result<Session<'static>> {
        let ops = self.connect()?;
        engine::open_session(ops, self.spec, self.config.remote.server_directory()?)
    }

    fn meta(&self, kind: OperationKind) -> OperationMeta {
        OperationMeta::worker(&self.config.worker_id, kind)
    }

    fn host(&self) -> Box<dyn host::HostControl> {
        host::from_settings(
            &self.config.host_control,
            WorkerConfig::dat_host_password().as_deref(),
        )
    }

    /// Fetches the latest canonical publication and stages its mod payload
    /// into `DesiredDeployment`. Runs inside `spawn_blocking`-adjacent code
    /// — the fetch is async, staging is blocking.
    async fn publication(&self) -> Result<(FetchedPublication, plan::DesiredDeployment)> {
        let publication = match self.sync.poll(&self.journal, None).await? {
            PublicationProbe::New(publication) => publication,
            PublicationProbe::Unchanged(metadata) => {
                // `since = None` should never produce Unchanged, but a
                // defensive fetch keeps behavior sane if it ever does.
                let _ = metadata;
                bail!("sync service reported no new publication")
            }
            PublicationProbe::None => {
                bail!("profile has no published revision yet")
            }
        };

        let source = CachePayloadSource {
            root: self.cache_dir.clone(),
            client: reqwest::Client::new(),
            mod_loader: &game::from_slug(&self.config.game)
                .ok_or_else(|| eyre::eyre!("unknown game slug"))?
                .mod_loader,
        };

        let desired = stage::stage_publication(
            &Publication::from_fetched(&publication),
            &source,
            self.spec,
            true,
            |_, _, _| {},
        )
        .await?;

        Ok((publication, desired))
    }
}

// ---------- auth ----------

fn authorized(ctx: &WorkerContext, headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == ctx.token)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            error: "missing or invalid bearer token".to_owned(),
        }),
    )
        .into_response()
}

fn error_response(error: &eyre::Report) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: format!("{error:#}"),
        }),
    )
        .into_response()
}

fn busy_response(owner: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse {
            error: format!("a deployment is already running ({owner})"),
        }),
    )
        .into_response()
}

// ---------- handlers ----------

#[derive(Deserialize)]
struct StatusQuery {
    refresh: Option<bool>,
}

async fn status(
    State(ctx): State<Arc<WorkerContext>>,
    headers: HeaderMap,
    Query(query): Query<StatusQuery>,
) -> Response {
    if !authorized(&ctx, &headers) {
        return unauthorized();
    }

    let journal = ctx.journal.state.lock().await.clone();

    let server = if query.refresh.unwrap_or(false) {
        let ctx2 = ctx.clone();
        match tokio::task::spawn_blocking(move || ctx2.open_session()).await {
            Ok(Ok(session)) => Some(super::api::ServerStateSummary {
                mods_revision: session.state.mods_revision.clone(),
                restart_required: session.state.restart_required,
                pending_configs: session.state.pending.len(),
                last_operation: session.state.last_operation.clone(),
                lease: session.lease,
            }),
            Ok(Err(err)) => {
                return error_response(&err);
            }
            Err(err) => {
                return error_response(&eyre::eyre!(err));
            }
        }
    } else {
        None
    };

    Json(StatusResponse {
        worker_id: ctx.config.worker_id.clone(),
        profile_id: ctx.config.profile_id.clone(),
        auto_sync: journal.auto_sync,
        auto_mods: journal.auto_mods,
        restart_policy: journal.restart_policy,
        last_seen_revision: journal.last_seen_revision,
        busy: journal.interrupted_operation.clone(),
        last_operation: journal.last_operation.clone(),
        last_error: journal.last_error.clone(),
        server,
    })
    .into_response()
}

async fn preview(
    State(ctx): State<Arc<WorkerContext>>,
    headers: HeaderMap,
    Json(request): Json<PreviewRequest>,
) -> Response {
    if !authorized(&ctx, &headers) {
        return unauthorized();
    }

    let (publication, desired) = match ctx.publication().await {
        Ok(pair) => pair,
        Err(err) => return error_response(&err),
    };

    let ctx2 = ctx.clone();
    let selection = request.selection;
    match tokio::task::spawn_blocking(move || {
        let mut session = ctx2.open_session()?;
        engine::preview(
            &mut session,
            &Publication::from_fetched(&publication),
            &desired,
            &selection,
        )
    })
    .await
    {
        Ok(Ok(Preview {
            plan,
            busy,
            warnings,
        })) => Json(PreviewResponse {
            plan,
            busy,
            warnings,
        })
        .into_response(),
        Ok(Err(err)) => error_response(&err),
        Err(err) => error_response(&eyre::eyre!(err)),
    }
}

async fn deploy(
    State(ctx): State<Arc<WorkerContext>>,
    headers: HeaderMap,
    Json(request): Json<DeployRequest>,
) -> Response {
    if !authorized(&ctx, &headers) {
        return unauthorized();
    }

    // Serializes against an in-flight automatic deployment. The remote
    // lease is the real cross-process lock.
    let Ok(guard) = ctx.operation_lock.try_lock() else {
        return busy_response("this worker");
    };

    let response = match run_deployment(
        &ctx,
        request.selection,
        Some(request.plan_hash),
        request.restart_policy,
        OperationKind::Manual,
    )
    .await
    {
        Ok(response) => Json(response).into_response(),
        Err(err) => error_response(&err),
    };
    drop(guard);
    response
}

/// Shared deployment path for manual API requests and the automatic poll
/// loop: identical fetch → plan → lease → execute → restart → persist
/// semantics in both cases.
async fn run_deployment(
    ctx: &Arc<WorkerContext>,
    selection: DeploySelection,
    plan_hash: Option<String>,
    restart_policy: Option<RestartPolicy>,
    kind: OperationKind,
) -> Result<DeployResponse> {
    let (publication, desired) = ctx.publication().await?;

    let meta = ctx.meta(kind);
    {
        // Marks the in-flight operation so a crash mid-deployment is
        // visible through /v1/status after restart.
        let mut state = ctx.journal.state.lock().await;
        state.interrupted_operation = Some(busy_marker(meta.id.clone(), kind));
        if let Err(err) = ctx.journal.save(&state) {
            warn!(%err, "failed to persist in-flight marker");
        }
    }

    let result = execute_deployment(
        ctx,
        publication,
        desired,
        selection,
        plan_hash,
        restart_policy,
        &meta,
    )
    .await;

    if result.is_err() {
        // `record_operation` clears the marker on success; on failure it
        // must be cleared explicitly so status doesn't report a stale
        // in-flight operation and the next startup doesn't warn about an
        // interruption that was actually a handled error.
        let mut state = ctx.journal.state.lock().await;
        state.interrupted_operation = None;
        if let Err(err) = ctx.journal.save(&state) {
            warn!(%err, "failed to clear in-flight marker");
        }
    }

    result
}

/// The blocking half of [`run_deployment`]: deploy under the lease, decide
/// the restart, then persist and record. Separated so the caller can clear
/// the in-flight marker on every exit path.
#[allow(clippy::too_many_arguments)]
async fn execute_deployment(
    ctx: &Arc<WorkerContext>,
    publication: FetchedPublication,
    desired: plan::DesiredDeployment,
    selection: DeploySelection,
    plan_hash: Option<String>,
    restart_policy: Option<RestartPolicy>,
    meta: &OperationMeta,
) -> Result<DeployResponse> {
    let ctx2 = ctx.clone();
    let meta2 = meta.clone();
    let connect = {
        let ctx3 = ctx.clone();
        move || ctx3.connect()
    };

    let (mut session, deployment) = tokio::task::spawn_blocking(move || {
        let mut session = ctx2.open_session()?;
        let deployment = engine::deploy(
            &mut session,
            connect,
            &Publication::from_fetched(&publication),
            &desired,
            &selection,
            &meta2,
            plan_hash.as_deref(),
            |_| {},
        )?;
        Ok::<_, eyre::Report>((session, deployment))
    })
    .await
    .context("deployment task panicked")??;

    let policy = match restart_policy {
        Some(policy) => policy,
        // The journal's current policy wins over the config file so a
        // runtime change via /v1/config persists across restarts.
        None => ctx.journal.state.lock().await.restart_policy,
    };
    let restart = engine::apply_restart_policy(
        ctx.host().as_ref(),
        policy,
        deployment.plan.requires_restart,
    )
    .await;

    let mut response = DeployResponse {
        plan: deployment.plan.clone(),
        summary: deployment.summary.clone(),
        warnings: deployment.warnings.clone(),
        failed_config_writes: deployment.failed_config_writes.clone(),
        restart,
        state: ServerDeploymentState::default(),
    };

    let meta3 = meta.clone();
    let state = tokio::task::spawn_blocking(move || {
        engine::finish(&mut session, deployment, restart, &meta3)
    })
    .await
    .context("deployment task panicked")??;

    if let Some(record) = state.last_operation.clone() {
        if let Err(err) = ctx.journal.record_operation(record).await {
            warn!(%err, "failed to record operation in journal");
        }
    }

    response.state = state;
    Ok(response)
}

async fn set_policy(
    State(ctx): State<Arc<WorkerContext>>,
    headers: HeaderMap,
    Json(request): Json<PolicyRequest>,
) -> Response {
    if !authorized(&ctx, &headers) {
        return unauthorized();
    }

    let Ok(guard) = ctx.operation_lock.try_lock() else {
        return busy_response("this worker");
    };

    let worker = Arc::clone(&ctx);
    let result = tokio::task::spawn_blocking(move || {
        let mut session = worker.open_session()?;
        engine::set_config_policy(
            &mut session,
            &request.path,
            request.policy,
            request.pinned_at.as_ref(),
        )
    })
    .await;

    drop(guard);

    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(err)) => error_response(&err),
        Err(err) => error_response(&eyre::eyre!(err)),
    }
}

async fn configure(
    State(ctx): State<Arc<WorkerContext>>,
    headers: HeaderMap,
    Json(request): Json<ConfigureRequest>,
) -> Response {
    if !authorized(&ctx, &headers) {
        return unauthorized();
    }

    let mut state = ctx.journal.state.lock().await;
    state.auto_sync = request.auto_sync;
    state.auto_mods = request.auto_mods;
    state.restart_policy = request.restart_policy;
    match ctx.journal.save(&state) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(&err),
    }
}

// ---------- poll loop ----------

/// Periodically checks for new publications and deploys them when the
/// journal's automation flags allow. `last_seen` advances on observation,
/// not on success — a failed automatic deploy surfaces in `last_error` and
/// waits for the next revision rather than retry-looping.
async fn poll_loop(ctx: Arc<WorkerContext>) {
    let interval = Duration::from_secs(ctx.config.poll_interval_secs);

    // Poll once immediately — a publication pushed while the worker was
    // down shouldn't wait a full interval — then on the fixed cadence.
    loop {
        poll_once(&ctx).await;
        tokio::time::sleep(interval).await;
    }
}

/// One poll cycle: revision check, then an automatic deployment when the
/// journal's flags allow.
async fn poll_once(ctx: &Arc<WorkerContext>) {
    let (auto_sync, auto_mods, last_seen) = {
        let state = ctx.journal.state.lock().await;
        (state.auto_sync, state.auto_mods, state.last_seen_revision)
    };

    let publication = match ctx.sync.poll(&ctx.journal, last_seen).await {
        Ok(PublicationProbe::New(publication)) => publication,
        Ok(PublicationProbe::Unchanged(_)) | Ok(PublicationProbe::None) => return,
        Err(err) => {
            warn!(error = %err, "publication poll failed");
            if let Err(save_err) = ctx
                .journal
                .record_error(Some(format!("poll failed: {err:#}")))
                .await
            {
                warn!(%save_err, "failed to record poll error in journal");
            }
            return;
        }
    };

    info!(revision = %publication.revision, "observed new publication");
    {
        let mut state = ctx.journal.state.lock().await;
        state.last_seen_revision = Some(publication.revision);
        if let Err(err) = ctx.journal.save(&state) {
            warn!(%err, "failed to persist observed revision");
        }
    }

    if !auto_sync {
        return;
    }

    // Automatic selection: mods follow auto_mods; configs are always
    // evaluated so per-file persistent policies apply. `Ask`-policy
    // conflicts become pending rather than overwritten.
    let selection = DeploySelection {
        include_mods: auto_mods,
        include_configs: true,
        apply_configs: Vec::new(),
        restore_configs: Vec::new(),
        decline_configs: Vec::new(),
    };

    let Ok(guard) = ctx.operation_lock.try_lock() else {
        info!("automatic deploy skipped: another operation is running");
        return;
    };

    let result = run_deployment(ctx, selection, None, None, OperationKind::Automatic).await;
    drop(guard);

    match result {
        Ok(response) => {
            info!(
                revision = %response.plan.publication_revision,
                "automatic deployment completed"
            );
            if let Err(err) = ctx.journal.record_error(None).await {
                warn!(%err, "failed to clear journal error");
            }
        }
        Err(err) => {
            error!(error = %err, "automatic deployment failed");
            if let Err(save_err) = ctx
                .journal
                .record_error(Some(format!("automatic deployment failed: {err:#}")))
                .await
            {
                warn!(%save_err, "failed to record deploy error in journal");
            }
        }
    }
}

// ---------- entry point ----------

pub async fn run(config: WorkerConfig) -> Result<()> {
    let journal = Journal::load(&config.state_dir)?;
    {
        let mut state = journal.state.lock().await;
        if !state.automation_seeded {
            // The config file seeds automation flags on first run; after
            // that the journal is the source of truth so /v1/config
            // changes persist across restarts.
            state.auto_sync = config.auto_sync;
            state.auto_mods = config.auto_mods;
            state.restart_policy = config.restart_policy;
            state.automation_seeded = true;
        }
        if let Some(op) = state.interrupted_operation.take() {
            warn!(operation = %op.id, "last run was interrupted mid-operation; the remote lease governs recovery");
        }
        journal.save(&state)?;
    }

    let ctx = Arc::new(WorkerContext::new(config, journal)?);

    let app = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/preview", post(preview))
        .route("/v1/deploy", post(deploy))
        .route("/v1/policy", post(set_policy))
        .route("/v1/config", post(configure))
        .route("/v1/health", get(|| async { StatusCode::NO_CONTENT }))
        .with_state(ctx.clone());

    let listener = tokio::net::TcpListener::bind(&ctx.config.listen)
        .await
        .with_context(|| format!("failed to bind worker API on {}", ctx.config.listen))?;

    info!(listen = %ctx.config.listen, profile = %ctx.config.profile_id, "gale-worker listening");

    tokio::spawn(poll_loop(ctx.clone()));

    axum::serve(listener, app)
        .await
        .context("worker API server failed")
}
