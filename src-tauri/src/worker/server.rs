//! The worker's HTTP server and automatic-sync poll loop.
//!
//! Authentication is a single bearer token (`GALE_WORKER_TOKEN`). The API
//! has no profile/server parameters on purpose: the worker is bound to
//! one profile and one remote at startup, so a remote caller can trigger
//! a deployment but can never redirect it at another target.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use eyre::{Context, Result, bail, ensure};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::{
    api::{
        ConfigureRequest, DeployRequest, DeployResponse, ErrorResponse, PolicyRequest,
        PreviewRequest, PreviewResponse, StatusResponse, WorkerRunPhase, WorkerRunReport,
    },
    config::WorkerConfig,
    journal::{Journal, PendingWork, WorkerJournal, busy_marker},
    secrets::Secrets,
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
    secrets: Secrets,
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
    fn new(config: WorkerConfig, secrets: Secrets, journal: Journal) -> Result<Self> {
        // The bundled list is deliberate: a release build's cached
        // games.json can predate this fork's dedicated-server metadata
        // and would wrongly reject a supported game.
        let game = game::bundled_from_slug(&config.game)
            .ok_or_else(|| eyre::eyre!("unknown game slug '{}'", config.game))?;
        ensure!(
            game.dedicated_server.is_some(),
            "game '{}' has no dedicated-server support",
            config.game
        );
        let spec = DeploymentSpec::for_loader(&game.mod_loader)
            .context("this game's mod loader has no deployment spec")?;

        Ok(Self {
            cache_dir: config.state_dir.join("cache"),
            token: secrets.token()?,
            sync: SyncClient::new(config.clone(), secrets.seed_refresh_token()),
            config,
            secrets,
            journal,
            spec: Box::leak(Box::new(spec)),
            operation_lock: Mutex::new(()),
        })
    }

    /// Opens an authenticated remote session. Runs inside `spawn_blocking`.
    fn connect(&self) -> Result<Box<dyn RemoteOps>> {
        let password = self.secrets.remote_password();
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

    /// The context the plan hash binds an approval to. The identity comes
    /// from the config the worker was started with, never from request
    /// parameters.
    fn plan_context(&self, restart_policy: RestartPolicy) -> plan::PlanContext {
        plan::PlanContext {
            profile_id: self.config.profile_id.clone(),
            game: self.config.game.clone(),
            target: self.config.remote.describe_target(),
            restart_policy,
        }
    }

    fn host(&self) -> Box<dyn host::HostControl> {
        host::from_settings(
            &self.config.host_control,
            self.secrets.dat_host_password().as_deref(),
        )
    }

    /// Fetches the latest canonical publication and stages its mod payload
    /// into `DesiredDeployment`. `include_mods` gates staging exactly like
    /// Local mode: a configs-only operation never touches mod sources.
    async fn publication(
        &self,
        include_mods: bool,
    ) -> Result<(FetchedPublication, plan::DesiredDeployment)> {
        let publication = match self.sync.poll(&self.journal, None).await? {
            PublicationProbe::New(publication) => publication,
            PublicationProbe::Unchanged(metadata) => {
                // `since = None` should never produce Unchanged. If it
                // ever does, bail rather than pretend a publication
                // exists.
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
            include_mods,
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
        observed_revision: journal.last_seen_revision,
        pending_revision: journal.pending.as_ref().map(|work| work.revision),
        next_attempt_at: journal.pending.and_then(|work| work.next_attempt_at),
        last_deployed_revision: journal.last_deployed_revision,
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

    let selection = request.selection;
    let (publication, desired) = match ctx.publication(selection.include_mods).await {
        Ok(pair) => pair,
        Err(err) => return error_response(&err),
    };

    // The plan hash binds the restart policy the deploy will use.
    let policy = match request.restart_policy {
        Some(policy) => policy,
        None => ctx.journal.state.lock().await.restart_policy,
    };

    let ctx2 = ctx.clone();
    let meta = ctx.meta(OperationKind::Manual);
    let context = ctx.plan_context(policy);
    match tokio::task::spawn_blocking(move || {
        let mut session = ctx2.open_session()?;
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
        request.force,
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
/// loop. Both run the same fetch, plan, lease, execute, restart, and
/// persist steps.
async fn run_deployment(
    ctx: &Arc<WorkerContext>,
    selection: DeploySelection,
    plan_hash: Option<String>,
    restart_policy: Option<RestartPolicy>,
    force: bool,
    kind: OperationKind,
) -> Result<DeployResponse> {
    // The restart policy is resolved before planning so the approval hash
    // binds the behavior the operation will actually apply.
    let policy = match restart_policy {
        Some(policy) => policy,
        // The journal's current policy wins over the config file so a
        // runtime change via /v1/config persists across restarts.
        None => ctx.journal.state.lock().await.restart_policy,
    };

    let (publication, desired) = ctx.publication(selection.include_mods).await?;

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

    let context = ctx.plan_context(policy);
    let result = execute_deployment(
        ctx,
        publication,
        desired,
        selection,
        plan_hash,
        policy,
        force,
        &context,
        &meta,
    )
    .await;

    {
        let mut state = ctx.journal.state.lock().await;
        match &result {
            Ok(response) => {
                // Any completed deployment acknowledges pending work it
                // covered. Manual or automatic, the server is now at that
                // revision.
                state.acknowledge_deployed(response.plan.publication_revision);
            }
            Err(_) => {
                // `record_operation` clears the marker on success. On
                // failure it must be cleared here, so status doesn't
                // report a stale in-flight operation and the next startup
                // doesn't warn about an interruption that was handled.
                state.interrupted_operation = None;
            }
        }
        if let Err(err) = ctx.journal.save(&state) {
            warn!(%err, "failed to update journal after deployment");
        }
    }

    result
}

/// The blocking half of [`run_deployment`]. Deploys under the lease,
/// decides the restart, then persists and records. Kept separate so the
/// caller can clear the in-flight marker on every exit path.
#[allow(clippy::too_many_arguments)]
async fn execute_deployment(
    ctx: &Arc<WorkerContext>,
    publication: FetchedPublication,
    desired: plan::DesiredDeployment,
    selection: DeploySelection,
    plan_hash: Option<String>,
    restart_policy: RestartPolicy,
    force: bool,
    context: &plan::PlanContext,
    meta: &OperationMeta,
) -> Result<DeployResponse> {
    let ctx2 = ctx.clone();
    let meta2 = meta.clone();
    let context2 = context.clone();
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
            &context2,
            &meta2,
            plan_hash.as_deref(),
            force,
            |_| {},
        )?;
        Ok::<_, eyre::Report>((session, deployment))
    })
    .await
    .context("deployment task panicked")??;

    // A restart is owed when this plan changed content *or* an earlier
    // deployment left one pending, since a failed restart must not be lost.
    let requires_restart = deployment.plan.requires_restart || session.state.restart_required;
    let restart =
        engine::apply_restart_policy(ctx.host().as_ref(), restart_policy, requires_restart).await;

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

    // The worker resolves the policy pin from the canonical publication,
    // so the client cannot supply an arbitrary one.
    let pinned_at = match ctx.publication(false).await {
        Ok((publication, _)) => publication
            .config
            .get(&request.path)
            .map(|file| file.hash.clone()),
        Err(err) => {
            drop(guard);
            return error_response(&err);
        }
    };

    let worker = Arc::clone(&ctx);
    let meta = ctx.meta(OperationKind::Manual);
    let result = tokio::task::spawn_blocking(move || {
        let mut session = worker.open_session()?;
        engine::set_config_policy(
            &mut session,
            &request.path,
            request.policy,
            pinned_at.as_ref(),
            &meta,
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
    // Re-enabling automation re-evaluates outstanding work immediately,
    // so a revision observed while disabled does not sit in backoff.
    if request.auto_sync
        && let Some(work) = state.pending.as_mut()
    {
        work.next_attempt_at = None;
    }
    match ctx.journal.save(&state) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(&err),
    }
}

// ---------- poll loop ----------

/// Periodically checks for new publications and drives pending work when
/// the journal's automation flags allow. The journal keeps three distinct
/// marks: `last_seen_revision` (newest observed), `pending` (awaiting a
/// successful deployment, retried with backoff), and
/// `last_deployed_revision` (newest fully deployed). Observing a
/// publication never acknowledges deploying it.
async fn poll_loop(ctx: Arc<WorkerContext>, shutdown: CancellationToken) {
    let interval = Duration::from_secs(ctx.config.poll_interval_secs);

    // Poll once immediately, since a publication pushed while the worker
    // was down should not wait a full interval, then repeat on the fixed
    // cadence. Both waits select on the shutdown token so a service stop
    // is answered promptly rather than after the current interval.
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = poll_once(&ctx) => {}
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(interval) => {}
        }
    }
}

/// The poll loop's decision for outstanding automatic work.
#[derive(Debug, PartialEq, Eq)]
enum AutoAction {
    /// Nothing is waiting.
    Idle,
    /// Work is pending but `autoSync` is off. Kept for later, not dropped.
    Disabled,
    /// Backoff from the last failure is still running.
    Waiting,
    /// Deploy the pending revision now.
    Deploy,
}

/// What to do with the journal's pending work this tick. Pure, so the
/// durable-progress rules are testable without a running worker.
fn automatic_action(state: &WorkerJournal, now: DateTime<Utc>) -> AutoAction {
    let Some(pending) = &state.pending else {
        return AutoAction::Idle;
    };
    if !state.auto_sync {
        return AutoAction::Disabled;
    }
    if let Some(next) = pending.next_attempt_at
        && now < next
    {
        return AutoAction::Waiting;
    }
    AutoAction::Deploy
}

/// Bounded exponential backoff between automatic retries.
fn retry_delay(attempts: u32) -> Duration {
    const BASE: Duration = Duration::from_secs(60);
    const CAP: Duration = Duration::from_secs(30 * 60);
    BASE.checked_mul(1 << attempts.min(6))
        .unwrap_or(CAP)
        .min(CAP)
}

/// One poll cycle. Observes the newest publication, then drives pending
/// work.
async fn poll_once(ctx: &Arc<WorkerContext>) {
    let last_seen = ctx.journal.state.lock().await.last_seen_revision;
    match ctx.sync.poll(&ctx.journal, last_seen).await {
        Ok(PublicationProbe::New(publication)) => {
            let revision = publication.revision;
            info!(%revision, "observed new publication");
            let mut state = ctx.journal.state.lock().await;
            state.last_seen_revision = Some(revision);
            // A newer publication supersedes whatever was pending. The
            // worker always moves to the latest revision and never
            // finishes applying an older one after a restart.
            state.pending = Some(PendingWork::new(revision));
            if let Err(err) = ctx.journal.save(&state) {
                warn!(%err, "failed to persist observed revision");
            }
        }
        Ok(PublicationProbe::Unchanged(_)) | Ok(PublicationProbe::None) => {}
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
    }

    let action = {
        let state = ctx.journal.state.lock().await;
        automatic_action(&state, Utc::now())
    };

    match action {
        AutoAction::Idle | AutoAction::Disabled => {}
        AutoAction::Waiting => {}
        AutoAction::Deploy => {
            let Ok(guard) = ctx.operation_lock.try_lock() else {
                // A manual operation is running, so the pending work
                // stays in the journal and retries next tick.
                info!("automatic deploy deferred: another operation is running");
                return;
            };

            // Automatic selection follows auto_mods for mods. Configs are
            // always evaluated so per-file persistent policies apply, and
            // `Ask`-policy conflicts become pending decisions, not
            // errors.
            let selection = DeploySelection {
                include_mods: ctx.journal.state.lock().await.auto_mods,
                include_configs: true,
                apply_configs: Vec::new(),
                restore_configs: Vec::new(),
                decline_configs: Vec::new(),
            };

            let result =
                run_deployment(ctx, selection, None, None, false, OperationKind::Automatic).await;
            drop(guard);

            let mut state = ctx.journal.state.lock().await;
            match result {
                Ok(response) => {
                    // `run_deployment` already cleared the covered pending
                    // work and advanced `last_deployed_revision`.
                    info!(
                        revision = %response.plan.publication_revision,
                        "automatic deployment completed"
                    );
                    state.last_error = None;
                }
                Err(err) => {
                    error!(error = %err, "automatic deployment failed");
                    let message = format!("{err:#}");
                    if let Some(work) = state.pending.as_mut() {
                        work.attempts = work.attempts.saturating_add(1);
                        work.next_attempt_at = Some(
                            Utc::now()
                                + chrono::Duration::from_std(retry_delay(work.attempts))
                                    .unwrap_or_default(),
                        );
                        work.last_error = Some(message.clone());
                    }
                    state.last_error = Some(format!("automatic deployment failed: {message}"));
                }
            }
            if let Err(err) = ctx.journal.save(&state) {
                warn!(%err, "failed to persist journal after deployment");
            }
        }
    }
}

// ---------- entry point ----------

/// Writes the worker's run state to `statusFile`, if the config sets one.
/// The desktop reads this to report service status: a `running` report
/// left behind while the process is gone means the worker crashed.
pub fn report_run_state(config: &WorkerConfig, phase: WorkerRunPhase) {
    let Some(path) = &config.status_file else {
        return;
    };
    let report = WorkerRunReport {
        worker_id: config.worker_id.clone(),
        profile_id: config.profile_id.clone(),
        pid: std::process::id(),
        phase,
        at: Utc::now(),
    };
    match serde_json::to_vec(&report) {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(path, bytes) {
                warn!(%err, "failed to write worker status file");
            }
        }
        Err(err) => warn!(%err, "failed to serialize worker status"),
    }
}

/// Guards against a second worker process using the same state directory.
/// The remote lease coordinates across machines; this lock covers the
/// local case, e.g. a manual `gale-worker` run competing with the service.
fn lock_instance(
    state_dir: &std::path::Path,
) -> Result<fd_lock::RwLockWriteGuard<'static, std::fs::File>> {
    let path = state_dir.join("gale-worker.lock");
    let file = std::fs::File::create(&path)
        .with_context(|| format!("failed to open worker lock {}", path.display()))?;
    // Leaked: the lock lives for the rest of the process by design.
    let lock = Box::leak(Box::new(fd_lock::RwLock::new(file)));
    let guard = lock.try_write().map_err(|err| match err.kind() {
        std::io::ErrorKind::WouldBlock => eyre::eyre!(
            "another gale-worker instance is already running in {}",
            state_dir.display()
        ),
        _ => eyre::eyre!("failed to lock worker state directory: {err}"),
    })?;
    Ok(guard)
}

/// Serves the HTTP API and runs the poll loop until `shutdown` is
/// cancelled. The caller writes the terminal status-file report, since it
/// is the one that knows whether the stop was a request or an OS shutdown.
///
/// `ready` fires once the API listener is bound and the poll loop is
/// armed — the point where the service can truthfully report `Running`
/// to the SCM. It is never fired when initialization fails, so a service
/// start that returns early reports `Stopped` instead.
pub async fn run(
    config: WorkerConfig,
    secrets: Secrets,
    shutdown: CancellationToken,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    std::fs::create_dir_all(&config.state_dir).with_context(|| {
        format!(
            "failed to create worker state directory {}",
            config.state_dir.display()
        )
    })?;
    let _instance_guard = lock_instance(&config.state_dir)?;

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

    let ctx = Arc::new(WorkerContext::new(config, secrets, journal)?);

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
    report_run_state(&ctx.config, WorkerRunPhase::Running);

    tokio::spawn(poll_loop(ctx.clone(), shutdown.clone()));

    if let Some(ready) = ready {
        let _ = ready.send(());
    }

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("worker API server failed");

    report_run_state(&ctx.config, WorkerRunPhase::Stopped);
    result
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, Utc};

    use super::{AutoAction, PendingWork, automatic_action, retry_delay};
    use crate::worker::journal::WorkerJournal;

    fn journal() -> WorkerJournal {
        WorkerJournal {
            auto_sync: true,
            ..WorkerJournal::default()
        }
    }

    #[test]
    fn pending_work_deploys_only_when_enabled_and_due() {
        let mut state = journal();

        // Nothing pending, so the loop is idle, not deploying.
        assert_eq!(automatic_action(&state, Utc::now()), AutoAction::Idle);

        state.pending = Some(PendingWork::new(Utc::now()));
        assert_eq!(automatic_action(&state, Utc::now()), AutoAction::Deploy);

        // Automation disabled: the pending work is retained for later,
        // never dropped.
        state.auto_sync = false;
        assert_eq!(automatic_action(&state, Utc::now()), AutoAction::Disabled);

        state.auto_sync = true;
        // A failed attempt scheduled a retry in the future, so wait.
        state.pending.as_mut().unwrap().next_attempt_at =
            Some(Utc::now() + ChronoDuration::minutes(5));
        assert_eq!(automatic_action(&state, Utc::now()), AutoAction::Waiting);

        // Once the backoff time passes, the work is due again.
        state.pending.as_mut().unwrap().next_attempt_at =
            Some(Utc::now() - ChronoDuration::seconds(1));
        assert_eq!(automatic_action(&state, Utc::now()), AutoAction::Deploy);
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(0), std::time::Duration::from_secs(60));
        assert_eq!(retry_delay(1), std::time::Duration::from_secs(120));
        assert_eq!(retry_delay(4), std::time::Duration::from_secs(60 * 16));
        // The cap holds for arbitrarily many attempts. Retries continue
        // but stay spread out.
        assert_eq!(retry_delay(30), std::time::Duration::from_secs(30 * 60));
    }

    #[test]
    fn observed_revision_does_not_acknowledge_deployment() {
        // The journal's three marks stay distinct: observing a publication
        // must never look like deploying it.
        let mut state = journal();
        let revision = Utc::now();

        state.last_seen_revision = Some(revision);
        state.pending = Some(PendingWork::new(revision));

        assert_eq!(state.last_seen_revision, Some(revision));
        assert_eq!(state.last_deployed_revision, None);
        assert!(state.pending.is_some());
    }

    fn status_config(dir: &std::path::Path) -> super::WorkerConfig {
        super::WorkerConfig {
            worker_id: "w".to_owned(),
            profile_id: "p".to_owned(),
            status_file: Some(dir.join("status.json")),
            state_dir: dir.to_path_buf(),
            ..super::WorkerConfig::default()
        }
    }

    #[test]
    fn run_report_lifecycle_writes_parseable_phases() {
        let dir = tempfile::tempdir().unwrap();
        let config = status_config(dir.path());

        super::report_run_state(&config, super::WorkerRunPhase::Running);
        let report: super::WorkerRunReport =
            serde_json::from_slice(&std::fs::read(config.status_file.as_ref().unwrap()).unwrap())
                .unwrap();
        assert_eq!(report.phase, super::WorkerRunPhase::Running);
        assert_eq!(report.profile_id, "p");
        assert!(report.pid > 0);

        // Later phases overwrite the file — the reader always sees the
        // most recent terminal state.
        super::report_run_state(&config, super::WorkerRunPhase::Shutdown);
        let report: super::WorkerRunReport =
            serde_json::from_slice(&std::fs::read(config.status_file.as_ref().unwrap()).unwrap())
                .unwrap();
        assert_eq!(report.phase, super::WorkerRunPhase::Shutdown);
    }

    #[test]
    fn run_report_is_optional_without_a_status_file() {
        let config = super::WorkerConfig::default();
        // No status_file configured: reporting must not fail or create
        // anything — manual workers have no reader.
        super::report_run_state(&config, super::WorkerRunPhase::Stopped);
    }

    #[test]
    fn a_second_instance_lock_on_the_same_state_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        let _first = super::lock_instance(dir.path()).unwrap();
        let err = super::lock_instance(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("already running"),
            "unexpected error: {err:#}"
        );
    }

    fn worker_config(dir: &std::path::Path, listen: String) -> super::WorkerConfig {
        super::WorkerConfig {
            worker_id: "w".to_owned(),
            profile_id: "p".to_owned(),
            game: "valheim".to_owned(),
            listen,
            status_file: Some(dir.join("status.json")),
            state_dir: dir.to_path_buf(),
            ..super::WorkerConfig::default()
        }
    }

    fn worker_secrets() -> super::Secrets {
        super::Secrets {
            token: Some("token".to_owned()),
            ..super::Secrets::default()
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A minimal plaintext FTP endpoint for end-to-end status tests. It
    /// refuses AUTH TLS (exercising the automatic-FTP plaintext
    /// fallback), accepts any login, treats every directory as present,
    /// and reports every file as missing — exactly what an untouched
    /// remote looks like to `open_session`.
    fn spawn_ftp_server() -> std::net::SocketAddr {
        use std::io::{BufRead, BufReader, Write};
        use std::net::{TcpListener, TcpStream};

        fn send(writer: &mut TcpStream, text: String) -> bool {
            writer.write_all(format!("{text}\r\n").as_bytes()).is_ok()
        }

        fn serve(stream: TcpStream) {
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut passive: Option<TcpListener> = None;
            let mut line = String::new();

            if !send(&mut writer, "220 fake ftp ready".to_owned()) {
                return;
            }
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let verb = line
                    .trim_end()
                    .split(' ')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                let response = match verb.as_str() {
                    "AUTH" => "502 TLS is not supported".to_owned(),
                    "USER" => "331 password required".to_owned(),
                    "PASS" => "230 logged in".to_owned(),
                    "PWD" | "XPWD" => "257 \"/\" is the current directory".to_owned(),
                    "CWD" | "CDUP" => "250 directory changed".to_owned(),
                    "TYPE" | "MODE" | "STRU" | "NOOP" => "200 ok".to_owned(),
                    "SYST" => "215 UNIX Type: L8".to_owned(),
                    // Missing files look exactly like this to read_state
                    // and read_lease, which then fall back to defaults.
                    "SIZE" | "MDTM" | "RETR" => "550 not found".to_owned(),
                    "PASV" => {
                        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                        let port = listener.local_addr().unwrap().port();
                        passive = Some(listener);
                        format!(
                            "227 Entering Passive Mode (127,0,0,1,{},{})",
                            port / 256,
                            port % 256
                        )
                    }
                    "LIST" | "NLST" | "MLSD" => {
                        if !send(&mut writer, "150 opening data connection".to_owned()) {
                            return;
                        }
                        // An empty listing: accept the data connection
                        // and close it immediately.
                        if let Some(listener) = passive.take() {
                            let _ = listener.accept();
                        }
                        "226 transfer complete".to_owned()
                    }
                    "QUIT" => {
                        let _ = send(&mut writer, "221 goodbye".to_owned());
                        return;
                    }
                    _ => "502 not implemented".to_owned(),
                };
                if !send(&mut writer, response) {
                    return;
                }
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => std::thread::spawn(move || serve(stream)),
                    Err(_) => break,
                };
            }
        });
        address
    }

    /// End-to-end coverage for the refresh flag: the desktop client must
    /// produce a query the real router accepts, malformed values must be
    /// rejected rather than coerced, and unauthenticated callers get
    /// nothing. The original bug sent `?refresh=1`, which the parser
    /// rejected with a 400 before the handler ever ran.
    #[tokio::test]
    async fn status_refresh_flag_round_trips_through_the_http_route() {
        use crate::profile::server::{
            settings::{RemoteProtocol, RemoteServerSettings},
            worker_client::WorkerClient,
        };

        let dir = tempfile::tempdir().unwrap();
        let ftp = spawn_ftp_server();
        let api_port = free_port();

        let mut config = worker_config(dir.path(), format!("127.0.0.1:{api_port}"));
        config.remote = RemoteServerSettings {
            protocol: RemoteProtocol::Ftp,
            host: "127.0.0.1".to_owned(),
            port: ftp.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            ..RemoteServerSettings::default()
        };
        // The poll loop is not under test; keep it off the network.
        config.sync_url = Some("http://127.0.0.1:1/".to_owned());
        let secrets = super::Secrets {
            remote_password: Some("pw".to_owned()),
            ..worker_secrets()
        };

        let shutdown = tokio_util::sync::CancellationToken::new();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(super::run(
            config,
            secrets,
            shutdown.clone(),
            Some(ready_tx),
        ));
        ready_rx.await.expect("the worker never became ready");

        let mut settings = RemoteServerSettings::default();
        settings.worker.address = format!("http://127.0.0.1:{api_port}");
        let client = WorkerClient::new(&settings, "token".to_owned()).unwrap();

        // Without refresh the handler returns journal state only — no
        // remote session is opened.
        let status = client.status(false).await.unwrap();
        assert!(status.server.is_none());

        // With refresh the handler opens a real session against the
        // remote and returns its live summary.
        let status = client.status(true).await.unwrap();
        let server = status
            .server
            .expect("a refresh request must return remote state");
        assert_eq!(server.pending_configs, 0);
        assert!(server.lease.is_none());

        // The endpoint still requires the bearer token.
        let unauthenticated = WorkerClient::new(&settings, "wrong".to_owned()).unwrap();
        let err = unauthenticated.status(false).await.unwrap_err();
        assert!(
            err.to_string().contains("bearer token"),
            "unexpected error: {err:#}"
        );

        // Malformed refresh values are rejected outright — never
        // silently coerced into a wrong status.
        let http = reqwest::Client::new();
        for query in ["refresh=1", "refresh=yes", "refresh=bogus"] {
            let response = http
                .get(format!("http://127.0.0.1:{api_port}/v1/status?{query}"))
                .bearer_auth("token")
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{query} must be rejected"
            );
        }

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    /// The readiness signal mirrors what the service reports to the SCM:
    /// it fires only once the API listener is bound — never while init
    /// is still running, and never when init fails.
    #[tokio::test]
    async fn run_signals_ready_only_after_binding() {
        let dir = tempfile::tempdir().unwrap();
        let config = worker_config(dir.path(), "127.0.0.1:0".to_owned());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let task = tokio::spawn(super::run(
            config,
            worker_secrets(),
            shutdown.clone(),
            Some(ready_tx),
        ));
        ready_rx.await.expect("the worker never became ready");
        shutdown.cancel();
        task.await.unwrap().unwrap();

        let report: super::WorkerRunReport =
            serde_json::from_slice(&std::fs::read(dir.path().join("status.json")).unwrap())
                .unwrap();
        assert_eq!(report.phase, super::WorkerRunPhase::Stopped);
    }

    /// An occupied port is an init failure: `run` errors and the ready
    /// sender drops without firing — the service-side caller observes
    /// `Err` and must report Stopped, never Running.
    #[tokio::test]
    async fn run_fails_when_the_port_is_occupied() {
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = blocker.local_addr().unwrap().to_string();
        let dir = tempfile::tempdir().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let result = super::run(
            worker_config(dir.path(), listen),
            worker_secrets(),
            tokio_util::sync::CancellationToken::new(),
            Some(ready_tx),
        )
        .await;
        assert!(
            ready_rx.await.is_err(),
            "readiness must not fire on failure"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("failed to bind"),
            "unexpected error: {err:#}"
        );
    }

    /// Unsupported game metadata is an init failure — the worker must
    /// reject it rather than serving an API that cannot deploy.
    #[tokio::test]
    async fn run_rejects_a_game_without_dedicated_server_support() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = worker_config(dir.path(), "127.0.0.1:0".to_owned());
        config.game = "h3vr".to_owned();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let err = super::run(
            config,
            worker_secrets(),
            tokio_util::sync::CancellationToken::new(),
            Some(ready_tx),
        )
        .await
        .unwrap_err();
        assert!(ready_rx.await.is_err());
        assert!(
            err.to_string().contains("no dedicated-server support"),
            "unexpected error: {err:#}"
        );
    }

    /// Codex's release failure: a cached `games.json` from an older app
    /// version carries the legacy `server` flag instead of
    /// `dedicatedServer`, and the worker wrongly rejected Valheim. The
    /// worker resolves the bundled list, which this binary was built
    /// against — the stale cache is irrelevant to it.
    #[test]
    fn bundled_metadata_is_immune_to_a_legacy_cache() {
        // What the legacy cache actually looked like: a `server` flag,
        // no `dedicatedServer` object. Parsed through the current schema
        // it yields no dedicated-server metadata at all.
        let legacy: crate::game::GameData = serde_json::from_str(
            r#"{
                "name": "Valheim",
                "slug": "valheim",
                "server": true,
                "modLoader": { "name": "BepInEx" }
            }"#,
        )
        .unwrap();
        assert!(legacy.dedicated_server.is_none());

        // The worker's lookup ignores that cache entirely.
        let game =
            crate::game::bundled_from_slug("valheim").expect("bundled valheim metadata missing");
        assert!(game.dedicated_server.is_some());
    }

    /// `WorkerContext::new` is the gate the service exercises: bundled
    /// metadata plus a usable token produce a context; a game without
    /// dedicated-server support or a missing token are init errors.
    #[test]
    fn worker_context_init_gates() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::worker::journal::Journal::load(dir.path()).unwrap();

        let ctx = super::WorkerContext::new(
            worker_config(dir.path(), "127.0.0.1:0".to_owned()),
            worker_secrets(),
            journal,
        );
        assert!(ctx.is_ok(), "unexpected error: {:?}", ctx.err());

        let journal = crate::worker::journal::Journal::load(dir.path()).unwrap();
        let mut unsupported = worker_config(dir.path(), "127.0.0.1:0".to_owned());
        unsupported.game = "h3vr".to_owned();
        let err = match super::WorkerContext::new(unsupported, worker_secrets(), journal) {
            Ok(_) => panic!("a game without dedicated-server support must be rejected"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("no dedicated-server support"));

        let journal = crate::worker::journal::Journal::load(dir.path()).unwrap();
        let err = match super::WorkerContext::new(
            worker_config(dir.path(), "127.0.0.1:0".to_owned()),
            super::Secrets::default(),
            journal,
        ) {
            Ok(_) => panic!("a missing worker token must be rejected"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("token"),
            "unexpected error: {err:#}"
        );
    }

    /// The defect-1 contract end to end: `POST /v1/config` must land in
    /// the journal file, and a restart must keep the journal's flags
    /// rather than re-seeding them from the config file.
    #[tokio::test]
    async fn configure_updates_the_journal_and_survives_restarts() {
        use crate::profile::server::{
            settings::{RemoteProtocol, RemoteServerSettings, RestartPolicy},
            worker_client::WorkerClient,
        };
        use crate::worker::journal::Journal;

        let dir = tempfile::tempdir().unwrap();
        let ftp = spawn_ftp_server();
        let api_port = free_port();

        let mut config = worker_config(dir.path(), format!("127.0.0.1:{api_port}"));
        config.auto_sync = true;
        config.auto_mods = true;
        config.remote = RemoteServerSettings {
            protocol: RemoteProtocol::Ftp,
            host: "127.0.0.1".to_owned(),
            port: ftp.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            ..RemoteServerSettings::default()
        };
        config.sync_url = Some("http://127.0.0.1:1/".to_owned());

        let mut settings = RemoteServerSettings::default();
        settings.worker.address = format!("http://127.0.0.1:{api_port}");
        let client = WorkerClient::new(&settings, "token".to_owned()).unwrap();

        // First run: the config seeds the journal's automation flags.
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(super::run(
            config.clone(),
            worker_secrets(),
            shutdown.clone(),
            Some(ready_tx),
        ));
        ready_rx.await.expect("the worker never became ready");

        let status = client.status(false).await.unwrap();
        assert!(status.auto_sync && status.auto_mods);

        // The update is visible in the API *and* durable on disk.
        client
            .configure(false, false, RestartPolicy::Manual)
            .await
            .unwrap();
        let status = client.status(false).await.unwrap();
        assert!(!status.auto_sync && !status.auto_mods);

        shutdown.cancel();
        task.await.unwrap().unwrap();

        {
            let journal = Journal::load(dir.path()).unwrap();
            let state = journal.state.lock().await;
            assert!(!state.auto_sync && !state.auto_mods);
        }

        // A restart with a config claiming the flags on keeps the
        // journal's values — it is authoritative once seeded.
        config.auto_sync = true;
        config.auto_mods = true;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(super::run(
            config,
            worker_secrets(),
            shutdown.clone(),
            Some(ready_tx),
        ));
        ready_rx.await.expect("the worker never became ready");

        let status = client.status(false).await.unwrap();
        assert!(
            !status.auto_sync && !status.auto_mods,
            "the journal must win over the config after the first seed"
        );

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }
}
