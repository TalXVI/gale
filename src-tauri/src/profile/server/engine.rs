//! The deployment engine shared by Local mode and the worker.
//!
//! One deployment is three phases:
//!
//! 1. **Plan.** Snapshot the remote and run the pure planner ([`preview_with_progress`],
//!    or the re-plan inside [`deploy_with_progress`]). The plan hash binds user approval
//!    to exactly the actions that will run.
//! 2. **Files.** Under the remote lease, apply removals, uploads, and
//!    config writes; record every success in the deployment state and
//!    persist it before touching the game process.
//! 3. **Restart.** The async [`apply_restart_policy_reporting`] consults the host
//!    provider, then [`finish`] records the operation and releases the
//!    lease.
//!
//! Keeping the lease across restart prevents a second executor from
//! starting a deployment while the server is still coming back up.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use eyre::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use ssh2::ErrorCode;
use suppaftp::{FtpError, Status};
use tracing::{debug, info, warn};

use super::{
    host::{HostControl, HostStatus},
    lease::{self, Lease, LeaseRecord, Ownership},
    paths::{DeployPath, DeployPathBuf, RemotePath, RemotePathBuf},
    plan::{
        self, ConfigAction, DeploySelection, DeploymentPlan, DesiredDeployment, FileSource,
        PlanContext, Publication, RemoteLayout, RemoteSnapshot, UploadKind,
    },
    progress::{ProgressReporter, SyncPhase},
    remote::{ReadOnly, RemoteOps, RemoteReader},
    settings::RestartPolicy,
    spec::DeploymentSpec,
    state::{
        self, ExecutorKind, LoadedState, OperationKind, OperationRecord, OperationStatus,
        OperationSummary, OwnedFile, RestartOutcome, ServerDeploymentState,
    },
};
use crate::profile::{
    export::{ConfigPath, ContentHash},
    sync::ConfigUpdatePolicy,
};

const TRANSFER_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Ownership re-checks are cheap (a stat and a directory probe), so a
/// check that cannot complete is retried a few times before the
/// operation aborts with an honest reason rather than a false takeover.
const OWNERSHIP_CHECK_ATTEMPTS: usize = 3;
const OWNERSHIP_RETRY_DELAY: Duration = Duration::from_millis(400);
/// Config files are small; remote reads for decision-making are bounded.
const MAX_CONFIG_READ: u64 = 1024 * 1024;
const RESTART_VERIFY_ATTEMPTS: usize = 12;
const RESTART_VERIFY_DELAY: Duration = Duration::from_secs(5);
/// Payload scanning and verification are round-trip bound (each LIST/RETR is
/// several control and data-channel round trips carrying little data), so
/// helper connections scale throughput almost linearly. Four keeps a Deploy
/// at six concurrent control connections (session + heartbeat + four
/// helpers), each still subject to proactive renewal.
const MAX_SNAPSHOT_READERS: usize = 4;
/// A helper login costs roughly as many round trips as verifying one or two
/// files; only open one per this many desired payload files.
const FILES_PER_SNAPSHOT_READER: usize = 8;

/// Identifies an operation across the lease record, operation history, and
/// progress reporting.
#[derive(Debug, Clone)]
pub struct OperationMeta {
    pub id: String,
    pub executor: ExecutorKind,
    pub kind: OperationKind,
    /// Lease owner label, e.g. `local:default` or `worker:my-vps`. Shown to
    /// the user when a competing deployment holds the lease.
    pub owner: String,
    pub worker_id: Option<String>,
    pub started_at: DateTime<Utc>,
}

impl OperationMeta {
    pub fn local(profile_id: &str, kind: OperationKind) -> Self {
        Self::new(
            ExecutorKind::Local,
            kind,
            format!("local:{profile_id}"),
            None,
        )
    }

    #[cfg(feature = "worker")]
    pub fn worker(worker_id: &str, kind: OperationKind) -> Self {
        Self::new(
            ExecutorKind::Worker,
            kind,
            format!("worker:{worker_id}"),
            Some(worker_id.to_owned()),
        )
    }

    fn new(
        executor: ExecutorKind,
        kind: OperationKind,
        owner: String,
        worker_id: Option<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            executor,
            kind,
            owner,
            worker_id,
            started_at: Utc::now(),
        }
    }
}

/// Per-file progress reported while executing a plan.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineProgress {
    pub completed: usize,
    pub total: usize,
    pub path: DeployPathBuf,
    pub operation: ProgressOp,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProgressOp {
    Remove,
    Upload,
    WriteConfig,
}

/// Maps deploy paths to absolute remote paths for the detected layout.
struct RemoteMapper {
    spec: DeploymentSpec,
    base: RemotePathBuf,
    layout: RemoteLayout,
}

impl RemoteMapper {
    fn remote_path(&self, deploy: &DeployPath) -> RemotePathBuf {
        match self.layout {
            RemoteLayout::Standard => self.base.join(deploy),
            RemoteLayout::MirrorRoot => self
                .spec
                .strip_mirror_root(deploy)
                .map(|relative| self.base.join(&relative))
                .unwrap_or_else(|| self.base.join(deploy)),
        }
    }
}

/// An open remote session: connection, path mapping, layout, and the
/// authoritative deployment state.
pub struct Session {
    pub ops: Box<dyn RemoteOps>,
    mapper: RemoteMapper,
    pub layout: RemoteLayout,
    pub host_managed: bool,
    pub state: ServerDeploymentState,
    /// The `operation_seq` the in-memory `state` was loaded with. Writing
    /// is refused when the remote sequence has moved past it, because a
    /// stale writer must never overwrite newer deployment state.
    base_seq: u64,
    /// A live lease held by another executor, if one was observed.
    pub lease: Option<LeaseRecord>,
    pub warnings: Vec<String>,
}

/// Everything [`open_session`] needs besides a connection.
pub fn open_session(
    mut ops: Box<dyn RemoteOps>,
    spec: &DeploymentSpec,
    base: RemotePathBuf,
) -> Result<Session> {
    ensure!(
        ops.is_dir(&base)
            .context("failed to check remote directory")?,
        "remote server directory '{base}' does not exist or is not a directory"
    );

    let layout = detect_layout(ops.as_mut(), spec, &base)?;
    let mapper = RemoteMapper {
        spec: spec.clone(),
        base,
        layout,
    };

    let LoadedState { state, warnings } = read_authoritative_state(ops.as_mut(), &mapper)?;

    let host_managed = detect_host_managed(ops.as_mut(), &mapper, &state)?;
    let lease_file = spec.lease_dir.join(state::LEASE_FILE_NAME)?;
    let lease = lease::read_lease(ops.as_mut(), &mapper.remote_path(&lease_file))?;

    info!(
        ?layout,
        host_managed,
        state_files = state.files.len(),
        "opened remote deployment session"
    );

    let base_seq = state.operation_seq;

    Ok(Session {
        ops,
        mapper,
        layout,
        host_managed,
        state,
        base_seq,
        lease,
        warnings,
    })
}

/// Re-reads the authoritative deployment state from the remote. Called
/// under the deployment lease so planning, policy updates, and the
/// persistence sequence guard all work on fresh state rather than what
/// was loaded when the session opened.
fn refresh_state(session: &mut Session) -> Result<()> {
    let LoadedState { state, warnings } =
        read_authoritative_state(session.ops.as_mut(), &session.mapper)?;
    session.base_seq = state.operation_seq;
    session.state = state;
    for warning in warnings {
        if !session.warnings.contains(&warning) {
            session.warnings.push(warning);
        }
    }
    Ok(())
}

/// A state read is read-only, so an interrupted MLST/RETR can be repeated
/// on a new control connection. The sequence guard still runs on the bytes
/// returned by the successful read before any state write.
fn read_authoritative_state(ops: &mut dyn RemoteOps, mapper: &RemoteMapper) -> Result<LoadedState> {
    let read = |ops: &mut dyn RemoteOps| {
        state::read_state(
            ops,
            &mapper.spec,
            &mapper.remote_path(&mapper.spec.state_path),
        )
    };
    match read(ops) {
        Ok(loaded) => Ok(loaded),
        Err(error) if error.downcast_ref::<state::StateReadError>().is_none() => Err(error),
        Err(first) => {
            warn!(error = %first, "authoritative state read failed; reconnecting before retry");
            ops.reconnect().with_context(|| {
                format!("failed to reconnect after authoritative state read: {first}")
            })?;
            read(ops).with_context(|| {
                format!("authoritative state read failed after reconnect; first error: {first}")
            })
        }
    }
}

/// A preview: the plan plus the context the UI needs to explain it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub plan: DeploymentPlan,
    /// The live lease holder, so the UI can warn before Deploy Now fails
    /// and offer a takeover when the holder is stale.
    pub busy: Option<lease::LeaseBusy>,
    pub warnings: Vec<String>,
}

/// Snapshots the remote and computes the plan. The snapshot is taken under
/// the deployment lease so the observed state cannot shift mid-read; when
/// another executor holds the lease the plan is still computed (unlocked,
/// advisory only) and the holder is reported through `busy`.
pub fn preview_with_progress(
    session: &mut Session,
    publication: &Publication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    context: &PlanContext,
    meta: &OperationMeta,
    progress: &mut ProgressReporter,
) -> Result<Preview> {
    progress.phase(SyncPhase::CheckingLease);
    let held = match lease::acquire(
        session.ops.as_mut(),
        &session.mapper.remote_path(&session.mapper.spec.lease_dir),
        &meta.owner,
        meta.executor,
        &meta.id,
        false,
    ) {
        Ok(lease) => Some(lease),
        Err(err) => {
            let busy = err.downcast::<lease::LeaseBusy>()?;
            info!(phase = "preview.busy", owner = %busy.record.owner, stale = busy.stale, "preview found an existing deployment lease; taking a read-only snapshot");
            let snapshot = take_snapshot(session, publication, desired, selection, progress)?;
            progress.phase(SyncPhase::BuildingPlan);
            let plan = plan::build_plan(
                publication,
                desired,
                &snapshot,
                selection,
                context,
                &session.mapper.spec,
            )?;
            progress.phase(SyncPhase::FinalizingPreview);
            return Ok(Preview {
                plan,
                busy: Some(busy),
                warnings: session.warnings.clone(),
            });
        }
    };

    let snapshot = take_snapshot(session, publication, desired, selection, progress);
    if let Some(lease) = held {
        lease.release(session.ops.as_mut());
    }

    let snapshot = snapshot?;
    progress.phase(SyncPhase::BuildingPlan);
    let plan = plan::build_plan(
        publication,
        desired,
        &snapshot,
        selection,
        context,
        &session.mapper.spec,
    )?;

    progress.phase(SyncPhase::FinalizingPreview);
    Ok(Preview {
        plan,
        busy: None,
        warnings: session.warnings.clone(),
    })
}

/// The file phase of a deployment, completed with the lease still held.
/// The caller decides the restart, then calls [`complete_with_progress`].
pub struct Deployment {
    pub plan: DeploymentPlan,
    pub summary: OperationSummary,
    pub warnings: Vec<String>,
    /// Selected config files whose write failed; their records were not
    /// advanced.
    pub failed_config_writes: Vec<ConfigPath>,
    lease: Lease,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentResult {
    pub plan: DeploymentPlan,
    pub summary: OperationSummary,
    pub warnings: Vec<String>,
    pub failed_config_writes: Vec<ConfigPath>,
    pub restart: RestartOutcome,
    pub state: ServerDeploymentState,
}

/// Completes the restart and records the result before releasing the lease.
pub async fn complete_with_progress(
    session: Session,
    deployment: Deployment,
    host: &dyn HostControl,
    policy: RestartPolicy,
    meta: OperationMeta,
    progress: &mut ProgressReporter,
) -> Result<DeploymentResult> {
    complete_impl(session, deployment, host, policy, meta, Some(progress)).await
}

async fn complete_impl(
    mut session: Session,
    deployment: Deployment,
    host: &dyn HostControl,
    policy: RestartPolicy,
    meta: OperationMeta,
    mut progress: Option<&mut ProgressReporter>,
) -> Result<DeploymentResult> {
    if let Some(progress) = progress.as_deref_mut() {
        progress.phase(SyncPhase::ApplyingRestart);
    }
    let restart = apply_restart_policy_reporting(
        host,
        policy,
        session.state.restart_required,
        progress.as_deref_mut(),
    )
    .await;
    let plan = deployment.plan.clone();
    let summary = deployment.summary.clone();
    let warnings = deployment.warnings.clone();
    let failed_config_writes = deployment.failed_config_writes.clone();
    if let Some(progress) = progress.as_deref_mut() {
        progress.phase(SyncPhase::ReleasingLease);
    }
    let state =
        tokio::task::spawn_blocking(move || finish(&mut session, deployment, restart, &meta))
            .await
            .context("deployment finalization task failed")??;

    if let Some(progress) = progress {
        progress.succeeded();
    }

    Ok(DeploymentResult {
        plan,
        summary,
        warnings,
        failed_config_writes,
        restart,
        state,
    })
}

/// Executes an approved plan under the deployment lease.
///
/// This function takes the lease *before* reading the authoritative
/// snapshot, so the state the plan runs against cannot be swapped out
/// after approval. It then computes the plan from that fresh snapshot,
/// and the plan must hash identically to `expected_plan_hash`. An
/// approval for a different plan is never reused. If the deployment fails
/// partway through, the state still describes exactly which files
/// succeeded, the operation is recorded as failed, and the lease is
/// released.
///
/// `force` breaks a *stale foreign* lease, the documented recovery once
/// the old executor is confirmed stopped. Live foreign leases always win.
#[allow(clippy::too_many_arguments)]
pub fn deploy_with_progress(
    session: &mut Session,
    connect: impl FnMut() -> Result<Box<dyn RemoteOps>> + Send + 'static,
    publication: &Publication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    context: &PlanContext,
    meta: &OperationMeta,
    expected_plan_hash: Option<&str>,
    force: bool,
    progress: &mut ProgressReporter,
    mut report: impl FnMut(EngineProgress),
) -> Result<Deployment> {
    progress.phase(SyncPhase::CheckingLease);
    let mut lease = lease::acquire(
        session.ops.as_mut(),
        &session.mapper.remote_path(&session.mapper.spec.lease_dir),
        &meta.owner,
        meta.executor,
        &meta.id,
        force,
    )?;
    lease::start_heartbeat(&mut lease, connect);

    // Under the lease, re-read the authoritative state and re-plan. The
    // approval must match what the remote looks like now.
    let plan = (|| -> Result<DeploymentPlan> {
        let snapshot = take_snapshot(session, publication, desired, selection, progress)?;
        progress.phase(SyncPhase::BuildingPlan);
        let plan = plan::build_plan(
            publication,
            desired,
            &snapshot,
            selection,
            context,
            &session.mapper.spec,
        )?;

        if let Some(expected) = expected_plan_hash
            && plan.hash != expected
        {
            return Err(plan::stale_plan_error());
        }

        ensure_ownership(&lease, session)?;
        Ok(plan)
    })();

    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            lease.release(session.ops.as_mut());
            return Err(error);
        }
    };

    let result = execute(
        session,
        &plan,
        desired,
        publication,
        &lease,
        progress,
        &mut report,
    );

    match result {
        Ok((summary, mut warnings, failed_config_writes)) => {
            if lease.is_lost() {
                warnings.push(
                    "the deployment lease was lost during execution; another executor may have modified the server"
                        .to_owned(),
                );
            }
            // Persist the deployment state after the file phase, before
            // the caller decides on a restart. A crash here still leaves
            // correct ownership and revision records. Both guards fail
            // closed: losing the lease or seeing a newer remote state
            // aborts the operation.
            progress.phase(SyncPhase::PersistingState);
            if let Err(err) = ensure_ownership(&lease, session).and_then(|_| persist_state(session))
            {
                let recorded = fail_operation(session, &lease, meta, &plan, &err);
                lease.release(session.ops.as_mut());
                return Err(err.wrap_err(format!(
                    "failed to persist remote deployment state after file changes; remote filesystem may need reconciliation{}",
                    recorded
                        .err()
                        .map(|error| format!("; failure record could not be persisted: {error:#}"))
                        .unwrap_or_default()
                )));
            }

            Ok(Deployment {
                plan,
                summary,
                warnings,
                failed_config_writes,
                lease,
            })
        }
        Err(error) => {
            warn!(%error, "deployment failed; recording accurate state");
            let recorded = fail_operation(session, &lease, meta, &plan, &error);
            lease.release(session.ops.as_mut());
            Err(error.wrap_err(format!(
                "deployment failed during the file phase; remote filesystem may need reconciliation{}",
                recorded
                    .err()
                    .map(|state_err| format!("; failure record could not be persisted: {state_err:#}"))
                    .unwrap_or_default()
            )))
        }
    }
}

/// Fails the operation when the lease verifiably no longer belongs to
/// this executor. A foreign owner means another deployment may be
/// mutating the server, so this one must stop before its next phase
/// rather than interleave its writes with the new owner's. A check that
/// cannot complete is retried and then aborts with the actual reason —
/// an unreadable lease record is not proof of a takeover.
fn ensure_ownership(lease: &Lease, session: &mut Session) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..OWNERSHIP_CHECK_ATTEMPTS {
        if lease.is_lost() {
            bail!(
                "the deployment lease was taken over by another executor; aborting this operation"
            );
        }
        match lease.check_ownership(session.ops.as_mut()) {
            Ownership::Held => return Ok(()),
            Ownership::HeldViaMarker => {
                // Ownership is proven by the marker directory because the
                // host will not return the lease record on reads. Surface
                // it once per session so support can tell this apart from
                // a normal deployment.
                const WARNING: &str = "this host does not return the lease record on reads; \
                    lease ownership is being verified through its marker directory";
                if !session.warnings.iter().any(|w| w == WARNING) {
                    session.warnings.push(WARNING.to_owned());
                }
                return Ok(());
            }
            Ownership::Lost { holder } => match holder {
                Some(owner) => {
                    bail!("the deployment lease was taken over by {owner}; aborting this operation")
                }
                None => bail!(
                    "the deployment lease was removed by another executor; aborting this operation"
                ),
            },
            Ownership::Unverifiable(err) => {
                warn!(attempt, %err, "could not verify deployment lease ownership; retrying");
                last_error = Some(err);
                thread::sleep(OWNERSHIP_RETRY_DELAY);
                if let Err(err) = session.ops.reconnect() {
                    warn!(%err, "lease ownership re-check could not reconnect");
                }
            }
        }
    }
    Err(last_error
        .expect("at least one ownership check ran")
        .wrap_err("could not verify the deployment lease still belongs to this executor; aborting rather than risk conflicting writes"))
}

/// Consults the host provider for the configured restart policy.
///
/// `NotRequired`/`Awaiting*` outcomes are recorded in the deployment state
/// by [`finish`]; an unknown player count is never treated as empty.
pub(super) async fn apply_restart_policy_reporting(
    host: &dyn HostControl,
    policy: RestartPolicy,
    requires_restart: bool,
    mut progress: Option<&mut ProgressReporter>,
) -> RestartOutcome {
    if !requires_restart {
        if let Some(progress) = progress.as_deref_mut() {
            progress.item("No restart needed");
        }
        return RestartOutcome::NotRequired;
    }

    if !host.can_restart() {
        if let Some(progress) = progress.as_deref_mut() {
            progress.item("Waiting for a manual restart");
        }
        return RestartOutcome::AwaitingManual;
    }

    match policy {
        RestartPolicy::Manual => {
            if let Some(progress) = progress.as_deref_mut() {
                progress.item("Waiting for a manual restart");
            }
            RestartOutcome::AwaitingManual
        }
        RestartPolicy::Immediate => {
            if let Some(progress) = progress.as_deref_mut() {
                progress.item("Checking server status");
            }
            do_restart(host, host.status().await.ok(), progress).await
        }
        RestartPolicy::WhenEmpty => match host.status().await {
            Ok(status) if status.players == Some(0) => {
                do_restart(host, Some(status), progress).await
            }
            _ => {
                if let Some(progress) = progress {
                    progress.item("Waiting until the server is empty");
                }
                RestartOutcome::AwaitingEmpty
            }
        },
    }
}

fn observed_restart(saw_stopped: &mut bool, saw_booting: &mut bool, status: &HostStatus) -> bool {
    if status.running == Some(false) {
        *saw_stopped = true;
    }
    if status.booting == Some(true) {
        *saw_booting = true;
    }
    status.running == Some(true)
        && status.booting != Some(true)
        && (*saw_stopped || (*saw_booting && status.booting == Some(false)))
}

async fn do_restart(
    host: &dyn HostControl,
    before: Option<HostStatus>,
    mut progress: Option<&mut ProgressReporter>,
) -> RestartOutcome {
    info!(host = host.name(), "restarting dedicated server");
    if let Some(progress) = progress.as_deref_mut() {
        progress.item("Requesting server restart");
    }
    if host.restart().await.is_err() {
        return RestartOutcome::Failed;
    }

    // A running response alone may still describe the old process. Observe
    // either a stop/start or a post-request booting/completed transition.
    let mut saw_stopped = before
        .as_ref()
        .is_some_and(|status| status.running == Some(false));
    let mut saw_booting = false;
    for attempt in 0..RESTART_VERIFY_ATTEMPTS {
        if let Some(progress) = progress.as_deref_mut() {
            let waiting_for = if before
                .as_ref()
                .is_some_and(|status| status.booting.is_some())
            {
                "server to finish booting"
            } else {
                "server to stop and start"
            };
            progress.item(format!(
                "Waiting for {waiting_for} (check {} of {})",
                attempt + 1,
                RESTART_VERIFY_ATTEMPTS
            ));
        }
        tokio::time::sleep(RESTART_VERIFY_DELAY).await;
        match host.status().await {
            Ok(status) if observed_restart(&mut saw_stopped, &mut saw_booting, &status) => {
                return RestartOutcome::Restarted;
            }
            Ok(_) => {}
            Err(_) => return RestartOutcome::StartupUnverified,
        }
    }
    RestartOutcome::StartupUnverified
}

/// Records the operation, persists the final state, and releases the lease.
/// Returns the state so callers can refresh status without reconnecting.
pub fn finish(
    session: &mut Session,
    deployment: Deployment,
    restart: RestartOutcome,
    meta: &OperationMeta,
) -> Result<ServerDeploymentState> {
    session.state.restart_required = match restart {
        RestartOutcome::Restarted => false,
        RestartOutcome::NotRequired => session.state.restart_required,
        _ => true,
    };

    session.state.record_operation(OperationRecord {
        id: meta.id.clone(),
        executor: meta.executor,
        kind: meta.kind,
        worker_id: meta.worker_id.clone(),
        publication_revision: Some(deployment.plan.publication_revision),
        mods_revision: deployment
            .plan
            .mods_phase
            .then(|| deployment.plan.mods_revision.clone()),
        // A deployment with failed writes is not a success: it is Partial,
        // with the per-file records describing exactly what landed.
        status: if deployment.failed_config_writes.is_empty() {
            OperationStatus::Succeeded
        } else {
            OperationStatus::Partial
        },
        summary: deployment.summary.clone(),
        restart,
        external_restart_acknowledged: false,
        error: (!deployment.failed_config_writes.is_empty()).then(|| {
            format!(
                "{} selected config file(s) could not be written",
                deployment.failed_config_writes.len()
            )
        }),
        started_at: meta.started_at,
        finished_at: Utc::now(),
    });

    let persist = ensure_ownership(&deployment.lease, session).and_then(|_| persist_state(session));
    deployment.lease.release(session.ops.as_mut());
    persist?;

    Ok(session.state.clone())
}

/// Records the user's explicit confirmation that the server was restarted
/// outside Gale. A running status alone cannot prove a restart occurred.
pub fn acknowledge_external_restart(
    session: &mut Session,
    meta: &OperationMeta,
) -> Result<ServerDeploymentState> {
    let lease = lease::acquire(
        session.ops.as_mut(),
        &session.mapper.remote_path(&session.mapper.spec.lease_dir),
        &meta.owner,
        meta.executor,
        &meta.id,
        false,
    )?;
    let result = (|| {
        refresh_state(session)?;
        ensure!(
            session.state.restart_required,
            "there is no outstanding restart to acknowledge"
        );
        ensure_ownership(&lease, session)?;
        session.state.restart_required = false;
        session.state.record_operation(OperationRecord {
            id: meta.id.clone(),
            executor: meta.executor,
            kind: meta.kind,
            worker_id: meta.worker_id.clone(),
            publication_revision: None,
            mods_revision: session.state.mods_revision.clone(),
            status: OperationStatus::Succeeded,
            summary: OperationSummary::default(),
            restart: RestartOutcome::Restarted,
            external_restart_acknowledged: true,
            error: None,
            started_at: meta.started_at,
            finished_at: Utc::now(),
        });
        persist_state(session)?;
        Ok(session.state.clone())
    })();
    lease.release(session.ops.as_mut());
    result
}

/// Records a failed operation and persists whatever state is accurate.
fn fail_operation(
    session: &mut Session,
    lease: &Lease,
    meta: &OperationMeta,
    plan: &DeploymentPlan,
    error: &eyre::Report,
) -> Result<()> {
    ensure_ownership(lease, session)?;
    session.state.restart_required = true;
    session.state.record_operation(OperationRecord {
        id: meta.id.clone(),
        executor: meta.executor,
        kind: meta.kind,
        worker_id: meta.worker_id.clone(),
        publication_revision: Some(plan.publication_revision),
        mods_revision: plan.mods_phase.then(|| plan.mods_revision.clone()),
        status: OperationStatus::Failed,
        summary: OperationSummary::default(),
        restart: RestartOutcome::NotRequired,
        external_restart_acknowledged: false,
        error: Some(error.to_string()),
        started_at: meta.started_at,
        finished_at: Utc::now(),
    });
    persist_state(session)
}

/// Gathers everything the planner needs to know about the remote: a fresh
/// authoritative state read, the payload listing, the content hashes of
/// owned managed payloads, and the hashes of every config path a decision
/// may touch.
fn take_snapshot(
    session: &mut Session,
    publication: &Publication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    progress: &mut ProgressReporter,
) -> Result<RemoteSnapshot> {
    progress.phase(SyncPhase::RefreshingState);
    refresh_state(session)?;
    info!(
        phase = "snapshot",
        owned_files = session.state.files.len(),
        desired_files = desired.payload.len(),
        "starting remote snapshot"
    );

    let spec = &session.mapper.spec;
    let mut payload_files = BTreeMap::new();
    let mut payload_dirs = BTreeSet::new();
    let mut payload_hashes = BTreeMap::new();

    if selection.include_mods {
        progress.phase(SyncPhase::ScanningPayload);
        let mut scanned = 0;
        let mut directories_listed = 0usize;

        // Payload scanning and verification are round-trip bound, so
        // extra read-only connections run the LIST/RETR work in parallel.
        // State, lease, and mutation traffic stays on the authoritative
        // session connection.
        let requested =
            (desired.payload.len() / FILES_PER_SNAPSHOT_READER).min(MAX_SNAPSHOT_READERS);
        let mut readers: Vec<Box<dyn RemoteReader + '_>> =
            open_snapshot_readers(session.ops.as_ref(), requested);
        if readers.is_empty() {
            readers.push(Box::new(ReadOnly(session.ops.as_mut())));
        }

        // List the payload tree level by level: every directory in a
        // level is listed in parallel, then its children form the next
        // level. Equivalent to the old recursive walk — the files/dirs
        // maps are order-independent — while keeping each connection
        // strictly serial.
        let scan_started = Instant::now();
        let mut frontier: Vec<DeployPathBuf> = spec.payload_dirs.clone();
        let mapper = &session.mapper;
        while !frontier.is_empty() {
            directories_listed += frontier.len();
            let listings = run_parallel(
                &mut readers,
                &frontier,
                |reader, dir| {
                    let remote_dir = mapper.remote_path(dir);
                    reader
                        .list(&remote_dir)
                        .with_context(|| format!("failed to inspect remote directory {remote_dir}"))
                },
                progress,
                |progress, dir| progress.item(dir.to_string()),
                |_, _, _| {},
            )?;
            let mut next = Vec::new();
            for (dir, entries) in frontier.iter().zip(listings) {
                for entry in entries {
                    let relative = dir.join(&entry.name).with_context(|| {
                        format!("remote directory contains an unsafe name: {}", entry.name)
                    })?;
                    if spec.is_gale_internal(&relative) {
                        continue;
                    }
                    if entry.is_directory {
                        payload_dirs.insert(relative.clone());
                        next.push(relative);
                    } else {
                        payload_files.insert(relative.clone(), entry.size.unwrap_or(0));
                        progress.item(relative.to_string());
                    }
                    scanned += 1;
                    progress.advance(scanned, None);
                }
            }
            frontier = next;
        }
        info!(
            phase = "snapshot.scan",
            readers = readers.len(),
            directories_listed,
            entries = scanned,
            duration_ms = scan_started.elapsed().as_millis(),
            "payload scan complete"
        );

        // Verify the actual remote bytes of every Gale-owned file the
        // publication still wants. Two different contents can have the
        // same size, so size equality alone cannot detect a remote edit.
        // Only files that are both owned and desired are read: hashing
        // foreign files gains nothing, and an owned file that is no longer
        // desired is being removed anyway.
        progress.phase(SyncPhase::VerifyingPayload);
        let to_hash: Vec<_> = desired
            .payload
            .iter()
            .filter(|(path, staged)| {
                session.state.files.contains_key(*path)
                    && payload_files.get(*path) == Some(&staged.size)
            })
            .collect();
        progress.work(to_hash.len(), None);
        let verify_started = Instant::now();
        let mut completed = 0usize;
        let mut files_verified = 0usize;
        let mut files_missing = 0usize;
        let mut bytes_verified = 0u64;
        let mut retries = 0usize;
        let outcomes = run_parallel(
            &mut readers,
            &to_hash,
            |reader, &(path, staged)| {
                let remote = mapper.remote_path(path);
                debug!(phase = "snapshot.payload", path = %remote, "reading owned payload");
                verify_snapshot_payload(reader, &remote, staged.size)
            },
            progress,
            |progress, (path, _)| progress.item(path.to_string()),
            |progress, _, outcome| {
                completed += 1;
                progress.advance(completed, None);
                bytes_verified += outcome.bytes;
                retries += usize::from(outcome.retried);
                if outcome.hash.is_some() {
                    files_verified += 1;
                } else {
                    files_missing += 1;
                }
            },
        )?;
        for ((path, _), outcome) in to_hash.iter().zip(outcomes) {
            if let Some(hash) = outcome.hash {
                payload_hashes.insert((*path).clone(), hash);
            }
        }
        info!(
            phase = "snapshot.payload",
            readers = readers.len(),
            files_verified,
            files_missing,
            bytes_verified,
            retries,
            duration_ms = verify_started.elapsed().as_millis(),
            "completed owned payload hashes"
        );

        // Helpers are done before the config phase — and before any
        // mutation — so the authoritative connection is free again.
        drop(readers);
    }

    // Hash every config path a decision could touch: published files,
    // recorded ones, and explicitly selected paths. A mods-only operation
    // never reads config files — the server owns them.
    let mut config_remote = BTreeMap::new();
    if selection.include_configs {
        let mut paths: BTreeSet<ConfigPath> = BTreeSet::new();
        paths.extend(
            publication
                .config
                .keys()
                .filter(|path| spec.is_managed_config(path))
                .cloned(),
        );
        paths.extend(session.state.config.keys().cloned());
        paths.extend(selection.apply_configs.iter().cloned());

        progress.phase(SyncPhase::CheckingConfigs);
        progress.work(paths.len(), None);
        for (index, path) in paths.into_iter().enumerate() {
            progress.item(path.to_string());
            let deploy = DeployPathBuf::new(path.as_str())
                .map_err(|_| eyre::eyre!("config path is not deployable: {path}"))?;
            let remote = session.mapper.remote_path(&deploy);
            let hash = read_snapshot_file(
                session.ops.as_mut(),
                &remote,
                MAX_CONFIG_READ,
                "snapshot.config",
            )?
            .map(|bytes| ContentHash::from_hash(blake3::hash(&bytes)));
            config_remote.insert(path, hash);
            progress.advance(index + 1, None);
        }
    }

    Ok(RemoteSnapshot {
        state: session.state.clone(),
        payload_files,
        payload_dirs,
        payload_hashes,
        config_remote,
        host_managed: session.host_managed,
        layout: session.layout,
    })
}

/// A failed read can leave an FTP control channel waiting for a transfer
/// completion reply. Replace the connection before retrying, and never turn
/// an unresolved transport error into apparent content divergence.
fn read_snapshot_file(
    ops: &mut dyn RemoteOps,
    path: &RemotePath,
    max: u64,
    phase: &'static str,
) -> Result<Option<Vec<u8>>> {
    match ops.read(path, max) {
        Ok(bytes) => Ok(bytes),
        Err(first) => {
            warn!(phase, path = %path, error = %first, "remote read failed; reconnecting before retry");
            ops.reconnect().with_context(|| {
                format!("{phase}: failed to reconnect after reading {path}: {first}")
            })?;
            ops.read(path, max).with_context(|| {
                format!("{phase}: read failed after reconnect for {path}; first error: {first}")
            })
        }
    }
}

/// Opens up to `requested` read-only helper connections for the payload
/// scan and verify phases. Connections that fail to open are logged and
/// skipped — the snapshot still runs over however many opened, and an
/// empty result means the caller falls back to the authoritative
/// connection entirely.
fn open_snapshot_readers(ops: &dyn RemoteOps, requested: usize) -> Vec<Box<dyn RemoteReader>> {
    if requested == 0 {
        return Vec::new();
    }
    let Some(connect) = ops.reader_connector() else {
        return Vec::new();
    };
    let started_at = Instant::now();
    let (readers, failed) = thread::scope(|scope| {
        let handles: Vec<_> = (0..requested).map(|_| scope.spawn(|| connect())).collect();
        let mut readers = Vec::with_capacity(handles.len());
        let mut failed = 0usize;
        for handle in handles {
            match handle.join() {
                Ok(Ok(reader)) => readers.push(reader),
                Ok(Err(error)) => {
                    failed += 1;
                    warn!(phase = "snapshot", error = %error, "snapshot helper connection failed");
                }
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
        (readers, failed)
    });
    info!(
        phase = "snapshot",
        requested,
        opened = readers.len(),
        failed,
        duration_ms = started_at.elapsed().as_millis(),
        "opened snapshot reader connections"
    );
    readers
}

/// Runs `work` on every item, spread over `readers` — one scoped thread
/// per reader claiming items in order — or inline when only the
/// authoritative connection is available. `started` and `done` run on the
/// calling thread for progress reporting. The first failure stops new
/// work; the error returned is the one with the lowest item index, and
/// results come back in input order, so outcomes are deterministic
/// regardless of completion order.
fn run_parallel<T: Sync, R: Send>(
    readers: &mut [Box<dyn RemoteReader + '_>],
    items: &[T],
    work: impl Fn(&mut dyn RemoteReader, &T) -> Result<R> + Sync,
    progress: &mut ProgressReporter,
    mut started: impl FnMut(&mut ProgressReporter, &T),
    mut done: impl FnMut(&mut ProgressReporter, &T, &R),
) -> Result<Vec<R>> {
    if readers.len() <= 1 {
        let Some(reader) = readers.first_mut() else {
            bail!("snapshot has no reader connection");
        };
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            started(progress, item);
            let result = work(reader.as_mut(), item)?;
            done(progress, item, &result);
            results.push(result);
        }
        return Ok(results);
    }

    enum Message<R> {
        Started(usize),
        Finished(usize, Result<R>),
    }

    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Message<R>>();
    thread::scope(|scope| {
        for reader in readers.iter_mut() {
            let tx = tx.clone();
            let next = &next;
            let stop = &stop;
            let work = &work;
            scope.spawn(move || {
                let reader = reader.as_mut();
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    if tx.send(Message::Started(index)).is_err() {
                        break;
                    }
                    let result = work(reader, item);
                    let failed = result.is_err();
                    if tx.send(Message::Finished(index, result)).is_err() {
                        break;
                    }
                    if failed {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let mut results: Vec<Option<R>> = items.iter().map(|_| None).collect();
        let mut errors: Vec<(usize, eyre::Report)> = Vec::new();
        for message in rx {
            match message {
                Message::Started(index) => started(progress, &items[index]),
                Message::Finished(index, Ok(result)) => {
                    done(progress, &items[index], &result);
                    results[index] = Some(result);
                }
                Message::Finished(index, Err(error)) => {
                    stop.store(true, Ordering::Relaxed);
                    errors.push((index, error));
                }
            }
        }
        if let Some((_, error)) = errors.into_iter().min_by_key(|(index, _)| *index) {
            return Err(error);
        }
        results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.ok_or_else(|| eyre::eyre!("parallel work item {index} has no result"))
            })
            .collect()
    })
}

/// What verifying one owned payload file against its remote bytes found.
struct PayloadVerification {
    /// Remote content hash; `None` when the file vanished after listing.
    hash: Option<ContentHash>,
    bytes: u64,
    /// The read needed a reconnect-and-retry.
    retried: bool,
}

/// Streams `remote` through a fresh hasher — one reconnect-and-retry on
/// transport failure, mirroring `read_snapshot_file` for the read-only
/// helper connections.
fn verify_snapshot_payload(
    reader: &mut dyn RemoteReader,
    remote: &RemotePath,
    listed_size: u64,
) -> Result<PayloadVerification> {
    match hash_snapshot_payload(reader, remote, listed_size) {
        Ok(outcome) => Ok(outcome),
        Err(first) => {
            warn!(phase = "snapshot.payload", path = %remote, error = %first, "remote read failed; reconnecting before retry");
            reader.reconnect().with_context(|| {
                format!("snapshot.payload: failed to reconnect after reading {remote}: {first}")
            })?;
            hash_snapshot_payload(reader, remote, listed_size)
                .map(|outcome| PayloadVerification {
                    retried: true,
                    ..outcome
                })
                .with_context(|| {
                    format!(
                        "snapshot.payload: read failed after reconnect for {remote}; first error: {first}"
                    )
                })
        }
    }
}

fn hash_snapshot_payload(
    reader: &mut dyn RemoteReader,
    remote: &RemotePath,
    listed_size: u64,
) -> Result<PayloadVerification> {
    let mut sink = HashingSink::default();
    // A file removed after listing remains divergent. Transport failures
    // are retried by the caller and must not become uploads.
    let present = reader.read_listed(remote, listed_size, &mut sink)?;
    Ok(PayloadVerification {
        hash: present.then(|| ContentHash::from_hash(sink.hasher.finalize())),
        bytes: sink.bytes,
        retried: false,
    })
}

/// Streams payload bytes into a BLAKE3 hasher while counting them, so a
/// verification read never materializes the file.
#[derive(Default)]
struct HashingSink {
    hasher: blake3::Hasher,
    bytes: u64,
}

impl std::io::Write for HashingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.bytes += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Applies the plan's file operations and updates `session.state` to
/// describe exactly what succeeded. Lease ownership is verified between
/// phases: an executor that loses the lease stops before its next mutation
/// rather than interleaving writes with the new holder.
fn execute(
    session: &mut Session,
    plan: &DeploymentPlan,
    desired: &DesiredDeployment,
    publication: &Publication,
    lease: &Lease,
    progress: &mut ProgressReporter,
    report: &mut impl FnMut(EngineProgress),
) -> Result<(OperationSummary, Vec<String>, Vec<ConfigPath>)> {
    let total = plan.removals.len() + plan.directory_removals.len() + plan.uploads.len();
    let mut completed = 0;
    let mut summary = OperationSummary {
        unchanged_files: plan.unchanged_files,
        ..Default::default()
    };
    let mut warnings = Vec::new();
    let mut failed_config_writes = Vec::new();
    let removals_total = plan.removals.len() + plan.directory_removals.len();
    if removals_total > 0 {
        progress.phase(SyncPhase::RemovingFiles);
        progress.work(removals_total, None);
    }
    let mut removed = 0;

    // ---- Removals (payload only). Failures keep the ownership record so
    // the next deployment retries instead of forgetting the file.
    for path in &plan.removals {
        progress.item(path.to_string());
        let remote = session.mapper.remote_path(path);
        if session
            .ops
            .delete_file(&remote)
            .with_context(|| format!("failed to remove {path}"))?
        {
            summary.removed_files += 1;
            session.state.restart_required = true;
        }
        session.state.files.remove(path);
        completed += 1;
        removed += 1;
        progress.advance(removed, None);
        report(EngineProgress {
            completed,
            total,
            path: path.clone(),
            operation: ProgressOp::Remove,
        });
    }

    for dir in &plan.directory_removals {
        progress.item(format!("{dir}/"));
        let remote = session.mapper.remote_path(dir);
        if let Err(error) = session.ops.delete_dir(&remote) {
            warnings.push(format!("could not remove {dir}/: {error}"));
        }
        completed += 1;
        removed += 1;
        progress.advance(removed, None);
        report(EngineProgress {
            completed,
            total,
            path: dir.clone(),
            operation: ProgressOp::Remove,
        });
    }

    // ---- Uploads: payload first, then published config writes.
    ensure_ownership(lease, session)?;
    let mut ensured = BTreeSet::new();
    let payload_uploads: Vec<_> = plan
        .uploads
        .iter()
        .filter(|upload| upload.kind != UploadKind::Config)
        .collect();
    let payload_bytes: u64 = payload_uploads.iter().map(|upload| upload.size).sum();
    let config_total = plan
        .uploads
        .iter()
        .filter(|upload| upload.kind == UploadKind::Config)
        .count();
    if !payload_uploads.is_empty() {
        progress.phase(SyncPhase::UploadingPayload);
        progress.work(payload_uploads.len(), Some(payload_bytes));
    }
    let mut uploaded = 0;
    let mut uploaded_bytes = 0;
    let mut written_configs = 0;
    let mut writing_configs = false;

    for upload in &plan.uploads {
        if upload.kind == UploadKind::Config && !writing_configs {
            progress.phase(SyncPhase::WritingConfigs);
            progress.work(config_total, None);
            writing_configs = true;
        }
        progress.item(upload.path.to_string());
        let result = match upload.kind {
            UploadKind::Payload => {
                let staged = desired.payload.get(&upload.path).ok_or_else(|| {
                    eyre::eyre!("planned upload missing staged content: {}", upload.path)
                });

                staged.and_then(|staged| {
                    upload_file(session, &upload.path, &staged.source, &mut ensured).map(|_| {
                        record_staged(session, upload, staged);
                    })
                })
            }
            UploadKind::Config => {
                let config_path = ConfigPath::try_from(upload.path.as_str().to_owned())
                    .map_err(|_| eyre::eyre!("config path is not deployable: {}", upload.path));
                config_path.and_then(|config_path| {
                    let file = publication.config.get(&config_path).ok_or_else(|| {
                        eyre::eyre!("planned config write is not published: {config_path}")
                    })?;
                    upload_file(
                        session,
                        &upload.path,
                        &FileSource::Bytes(Arc::new(file.bytes.clone())),
                        &mut ensured,
                    )?;
                    session.state.record_applied(&config_path, &file.hash);
                    summary.config_writes += 1;
                    Ok(())
                })
            }
        };

        match result {
            Ok(()) => {
                session.state.restart_required = true;
                if matches!(upload.kind, UploadKind::Payload) {
                    summary.uploaded_files += 1;
                    summary.uploaded_bytes += upload.size;
                }
            }
            Err(error) if upload.kind == UploadKind::Config => {
                // A failed config write must not advance the file's record;
                // it stays pending/reviewable for the next attempt.
                if let Ok(path) = ConfigPath::try_from(upload.path.as_str().to_owned()) {
                    failed_config_writes.push(path.clone());
                }
                warnings.push(format!("could not write config {}: {error}", upload.path));
            }
            Err(error) => {
                // A payload failure means the published mod set is not fully
                // deployed; remaining uploads are abandoned rather than
                // half-applying them silently.
                return Err(error.wrap_err(format!("failed to upload {}", upload.path)));
            }
        }

        completed += 1;
        if upload.kind == UploadKind::Config {
            written_configs += 1;
            progress.advance(written_configs, None);
        } else {
            uploaded += 1;
            uploaded_bytes += upload.size;
            progress.advance(uploaded, Some(uploaded_bytes));
        }
        report(EngineProgress {
            completed,
            total,
            path: upload.path.clone(),
            operation: match upload.kind {
                UploadKind::Config => ProgressOp::WriteConfig,
                _ => ProgressOp::Upload,
            },
        });
    }

    // ---- Config decision records (non-write outcomes).
    if plan.configs_phase {
        let published: BTreeMap<ConfigPath, ContentHash> = publication
            .config
            .iter()
            .filter(|(path, _)| session.mapper.spec.is_managed_config(path))
            .map(|(path, file)| (path.clone(), file.hash.clone()))
            .collect();

        for entry in &plan.config_entries {
            let hash = &published[&entry.path];
            match &entry.action {
                ConfigAction::MarkApplied => session.state.record_applied(&entry.path, hash),
                ConfigAction::Decline => session.state.record_declined(&entry.path, hash),
                ConfigAction::Pending { .. } => session.state.record_conflict(&entry.path, hash),
                ConfigAction::Keep | ConfigAction::Write | ConfigAction::Unapplied => {}
            }
        }
    }

    // ---- Advance the recorded revision: reaching this point means the
    // whole payload phase succeeded, since a failure returns early above.
    if plan.mods_phase {
        session.state.mods_revision = Some(plan.mods_revision.clone());
        session.state.deployed_mods = plan.deployed_mods.clone();
    }

    Ok((summary, warnings, failed_config_writes))
}

/// Records a successful payload upload in the deployment state.
fn record_staged(session: &mut Session, upload: &plan::PlanUpload, staged: &plan::StagedFile) {
    if upload.kind == UploadKind::Payload {
        session.state.files.insert(
            upload.path.clone(),
            OwnedFile {
                hash: staged.hash.clone(),
                size: staged.size,
            },
        );
    }
}

/// Streams one staged file to its remote path through a temporary file and
/// an atomic-where-possible rename.
fn upload_file(
    session: &mut Session,
    path: &DeployPath,
    source: &FileSource,
    ensured: &mut BTreeSet<RemotePathBuf>,
) -> Result<()> {
    ensure_remote_parents(session, path, ensured)?;

    let target = session.mapper.remote_path(path);
    let temporary = target.with_suffix(".gale-upload");

    // Re-hash staged disk content right before upload: a file that changed
    // since staging must not silently deploy different bytes than planned.
    with_retry(session, |session| match source {
        FileSource::Path(local) => session.ops.upload(local, &temporary),
        FileSource::Bytes(bytes) => session.ops.write(&temporary, bytes),
    })?;
    replace_remote(session.ops.as_mut(), &temporary, &target)
        .with_context(|| format!("failed to upload remote file {path}"))
}

/// Retries `work` after reconnecting, for transient transport failures.
fn with_retry<T>(
    session: &mut Session,
    mut work: impl FnMut(&mut Session) -> Result<T>,
) -> Result<T> {
    let mut attempt = 1;
    loop {
        match work(session) {
            Ok(value) => return Ok(value),
            Err(error) if attempt == TRANSFER_ATTEMPTS => return Err(error),
            Err(error) => {
                warn!(attempt, %error, "transfer failed; reconnecting");
                thread::sleep(RETRY_DELAY * attempt as u32);
                session.ops.reconnect()?;
                attempt += 1;
            }
        }
    }
}

fn ensure_remote_parents(
    session: &mut Session,
    path: &DeployPath,
    ensured: &mut BTreeSet<RemotePathBuf>,
) -> Result<()> {
    let mut ancestors = path.self_and_ancestors().collect::<Vec<_>>();
    ancestors.pop();

    for ancestor in ancestors {
        let remote = session.mapper.remote_path(&ancestor);
        if ensured.contains(&remote) {
            continue;
        }
        session
            .ops
            .ensure_dir(&remote)
            .with_context(|| format!("failed to create remote directory {remote}"))?;
        ensured.insert(remote);
    }

    Ok(())
}

/// Moves an uploaded temporary file over its target as safely as the
/// protocol allows.
fn replace_remote(
    ops: &mut dyn RemoteOps,
    temporary: &RemotePath,
    target: &RemotePath,
) -> Result<()> {
    match ops.rename(temporary, target) {
        Ok(()) => return Ok(()),
        Err(error) if is_replace_conflict(&error) => {}
        Err(error) => {
            return Err(error).context(
                "remote rename did not confirm whether it completed; remote filesystem may need reconciliation",
            );
        }
    }

    // FTP and older SFTP servers cannot rename over an existing file. FTP
    // also uses 550 when the source is missing, so verify both files before
    // moving the existing target aside.
    let backup = target.with_suffix(".gale-backup");
    let temporary_exists = ops.is_file(temporary)?;
    ensure!(
        temporary_exists,
        "remote rename was refused while {temporary} was absent; remote filesystem may need reconciliation"
    );
    let target_exists = ops.is_file(target)?;
    ensure!(
        target_exists,
        "remote rename was refused while {target} was absent; remote filesystem may need reconciliation"
    );

    ensure!(
        !ops.is_file(&backup)?,
        "remote backup {backup} already exists; remote filesystem may need reconciliation"
    );
    ops.rename(target, &backup).context(
        "could not confirm backing up the existing remote file; remote filesystem may need reconciliation",
    )?;

    ops.rename(temporary, target).context(
        "could not confirm moving the uploaded file into place; remote filesystem may need reconciliation",
    )?;

    if let Err(error) = ops.delete_file(&backup) {
        warn!(%error, path = %backup, "could not remove the previous remote file backup");
    }

    Ok(())
}

/// Only a definite file-exists response permits the backup-and-replace
/// sequence. A lost reply may mean the first rename already happened.
fn is_replace_conflict(error: &eyre::Report) -> bool {
    matches!(
        error.downcast_ref::<FtpError>(),
        Some(FtpError::UnexpectedResponse(response)) if response.status == Status::FileUnavailable
    ) || matches!(
        error.downcast_ref::<ssh2::Error>(),
        Some(error) if error.code() == ErrorCode::SFTP(11)
    )
}

/// Writes the deployment state through a temporary file + rename.
///
/// Before writing, this re-reads the remote state's `operation_seq`: it
/// must still equal the sequence this session loaded under the lease. A
/// newer or diverged remote sequence means another writer slipped past
/// the lease. This session's stale view must never overwrite it, so the
/// write fails closed instead.
///
/// The temporary file is verified byte-for-byte on a fresh connection
/// before it may replace the target, and the target is verified again
/// on a fresh connection after replacement — a truncated or refused
/// transfer can never silently become the authoritative state.
pub fn persist_state(session: &mut Session) -> Result<()> {
    let target = session.mapper.remote_path(&session.mapper.spec.state_path);
    let remote = read_authoritative_state(session.ops.as_mut(), &session.mapper)
        .context("failed to verify remote deployment state before writing")?;
    ensure!(
        remote.state.operation_seq == session.base_seq,
        "the remote deployment state changed during this operation; refusing to overwrite newer state"
    );

    let bytes = state::serialize(&session.state)?;

    if let Some(parent) = session.mapper.spec.state_path.parent() {
        let remote = session.mapper.remote_path(&parent);
        session
            .ops
            .ensure_dir(&remote)
            .context("failed to create remote state directory")?;
    }

    let temporary = target.with_suffix(".tmp");
    with_retry(session, |session| {
        session.ops.write(&temporary, &bytes)?;
        session
            .ops
            .reconnect()
            .context("failed to reconnect before verifying temporary deployment state")?;
        match session.ops.read(&temporary, state::MAX_STATE_BYTES)? {
            Some(actual) if actual == bytes => {}
            Some(_) => bail!(
                "temporary remote deployment state at {temporary} does not read back as written"
            ),
            None => bail!(
                "temporary remote deployment state was written to {temporary} but cannot be read back"
            ),
        }
        Ok(())
    })
    .context("failed to write remote deployment state")?;
    replace_remote(session.ops.as_mut(), &temporary, &target).context(
        "failed to replace remote deployment state; remote filesystem may need reconciliation",
    )?;

    // The state file is authoritative for every later operation, so a
    // deployment only succeeds once its persistence is verified: the
    // temporary file must read back byte-identical on a fresh connection
    // before it may replace the target, and the target is verified again
    // on a fresh connection after replacement. A host that cannot return
    // it would silently lose ownership records, config policies, and the
    // operation sequence on the next session — that failure must surface
    // here, not after a subsequent deploy has already trusted it.
    session
        .ops
        .reconnect()
        .context("failed to reconnect before verifying remote deployment state")?;
    let verified = with_retry(session, |session| {
        session.ops.read(&target, state::MAX_STATE_BYTES)
    })
    .context("could not verify the written deployment state")?;
    match verified {
        Some(actual) if actual == bytes => {}
        Some(_) => bail!(
            "remote deployment state at {target} does not read back as written; \
             refusing to treat the deployment state as durable"
        ),
        None => bail!(
            "remote deployment state was written to {target} but cannot be read back; \
             refusing to treat the deployment state as durable"
        ),
    }

    session.base_seq = session.state.operation_seq;
    Ok(())
}

/// Sets a persistent per-file update policy in the remote deployment
/// state. A policy write mutates the authoritative state, so it takes the
/// same deployment lease as a sync and re-reads the state under it. That
/// way a policy write can never race or overwrite a concurrent
/// deployment. `pinned_at` is the published hash the policy was set
/// against, matching the client's `policy_set_at` behavior.
pub fn set_config_policy(
    session: &mut Session,
    path: &ConfigPath,
    policy: ConfigUpdatePolicy,
    pinned_at: Option<&ContentHash>,
    meta: &OperationMeta,
) -> Result<()> {
    ensure!(
        session.mapper.spec.is_managed_config(path),
        "unsupported server config path: {path}"
    );
    let lease = lease::acquire(
        session.ops.as_mut(),
        &session.mapper.remote_path(&session.mapper.spec.lease_dir),
        &meta.owner,
        meta.executor,
        &meta.id,
        false,
    )?;

    let result = (|| {
        refresh_state(session)?;
        let record = session.state.config.entry(path.clone()).or_default();
        record.policy = policy;
        record.policy_set_at = pinned_at.cloned();
        ensure_ownership(&lease, session)?;
        persist_state(session)
    })();

    lease.release(session.ops.as_mut());
    result
}

/// Detects the remote layout. A host that exposes the loader's contents
/// directly shows stripped mirror dirs (`plugins`, `config`, ...) at the
/// remote root; that presence is authoritative over a stray `BepInEx`
/// directory, which earlier deployments could have created accidentally
/// and which must not flip a restricted host back to Standard.
fn detect_layout(
    ops: &mut dyn RemoteOps,
    spec: &DeploymentSpec,
    base: &RemotePath,
) -> Result<RemoteLayout> {
    for dir in spec.payload_dirs.iter().chain(spec.config_dirs.iter()) {
        let Some(at_root) = spec.strip_mirror_root(dir) else {
            continue;
        };
        if ops
            .is_dir(&base.join(&at_root))
            .context("failed to check for a restricted server layout")?
        {
            return Ok(RemoteLayout::MirrorRoot);
        }
    }

    Ok(RemoteLayout::Standard)
}

/// Whether the host manages the mod loader itself. Restricted hosts always
/// do. On standard layouts the loader marker files decide, unless Gale
/// recorded a loader deployment, because files Gale uploaded stay Gale's.
fn detect_host_managed(
    ops: &mut dyn RemoteOps,
    mapper: &RemoteMapper,
    state: &ServerDeploymentState,
) -> Result<bool> {
    if mapper.layout == RemoteLayout::MirrorRoot {
        return Ok(true);
    }

    let owns_loader = mapper
        .spec
        .owns_loader(state.files.keys().map(DeployPathBuf::as_path));
    if owns_loader {
        return Ok(false);
    }

    for marker in &mapper.spec.loader_markers {
        if ops
            .is_dir(&mapper.remote_path(marker))
            .context("failed to check for a server-managed mod loader installation")?
        {
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        game::mod_loader::{ModLoader, ModLoaderKind},
        profile::{
            export::{ConfigPath, ModRevision, R2Mod},
            server::{
                host,
                lease::LeaseRecord,
                paths::RemotePathBuf,
                plan::{Publication, StagedFile},
                progress::SyncOperation,
                remote::{self, ConnectionAttempt, RemoteConnection, memory::MemoryRemote},
                settings::{RemoteAuthentication, RemoteProtocol, RemoteServerSettings},
            },
            sync::{PendingConfigReason, archive::ValidatedConfigFile},
        },
        thunderstore::{Backend, PackageIdent},
    };

    const BASE: &str = "/srv";
    const STATE_REMOTE: &str = "/srv/BepInEx/config/.gale-server-state.json";
    const LEASE_DIR_REMOTE: &str = "/srv/BepInEx/config/.gale-deploy.lock";
    const LEASE_FILE_REMOTE: &str = "/srv/BepInEx/config/.gale-deploy.lock/lease.json";
    const MOD_DLL_REMOTE: &str = "/srv/BepInEx/plugins/Author-ModA/ModA.dll";

    fn spec() -> DeploymentSpec {
        DeploymentSpec::for_loader(&ModLoader {
            package_name: None,
            file_target: None,
            kind: ModLoaderKind::BepInEx {
                extra_subdirs: Vec::new(),
            },
        })
        .unwrap()
    }

    fn deploy_path(value: &str) -> DeployPathBuf {
        DeployPathBuf::new(value).unwrap()
    }

    fn config_path(value: &str) -> ConfigPath {
        ConfigPath::try_from(value.to_owned()).unwrap()
    }

    fn staged(bytes: &[u8]) -> StagedFile {
        StagedFile {
            source: FileSource::Bytes(Arc::new(bytes.to_vec())),
            hash: blake3::hash(bytes).to_hex().to_string(),
            size: bytes.len() as u64,
        }
    }

    fn config_file(bytes: &[u8]) -> ValidatedConfigFile {
        ValidatedConfigFile {
            hash: ContentHash::from_hash(blake3::hash(bytes)),
            bytes: bytes.to_vec(),
        }
    }

    /// Owns the borrowed halves of a `Publication`.
    struct Fixture {
        mods: Vec<R2Mod>,
        config: BTreeMap<ConfigPath, ValidatedConfigFile>,
    }

    impl Fixture {
        fn publication(&self) -> Publication<'_> {
            Publication {
                revision: DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                    .unwrap()
                    .to_utc(),
                mods_revision: ModRevision::from_hash(blake3::hash(b"rev-1")),
                mods: &self.mods,
                config: &self.config,
            }
        }
    }

    fn mod_fixture() -> Fixture {
        Fixture {
            mods: vec![R2Mod {
                ident: PackageIdent::from(("Author", "ModA")),
                version: semver::Version::new(1, 0, 0).into(),
                enabled: true,
                source: Backend::Thunderstore,
            }],
            config: BTreeMap::new(),
        }
    }

    fn desired_payload() -> DesiredDeployment {
        let mut payload = BTreeMap::new();
        payload.insert(
            deploy_path("BepInEx/plugins/Author-ModA/ModA.dll"),
            staged(b"dll-bytes"),
        );
        DesiredDeployment { payload }
    }

    /// The multi-mod payload the FTPS reader-pool tests and the latency
    /// benchmark share: `mod_dirs` directories under `BepInEx/plugins`,
    /// each with Mod.dll, manifest.json and docs.txt (README.md would be
    /// an excluded package-metadata file); the first eight also carry
    /// en/de translations. 50 dirs make 166 desired payload files across
    /// 58 payload subdirectories.
    fn large_mods_payload(mod_dirs: usize) -> DesiredDeployment {
        let mut payload = BTreeMap::new();
        for index in 0..mod_dirs {
            let dir = format!("BepInEx/plugins/Author-Mod{index:02}");
            for file in ["Mod.dll", "manifest.json", "docs.txt"] {
                let path = format!("{dir}/{file}");
                payload.insert(deploy_path(&path), staged(path.as_bytes()));
            }
            if index < 8 {
                for file in ["translations/en.json", "translations/de.json"] {
                    let path = format!("{dir}/{file}");
                    payload.insert(deploy_path(&path), staged(path.as_bytes()));
                }
            }
        }
        DesiredDeployment { payload }
    }

    fn selection(mods: bool, configs: bool) -> DeploySelection {
        DeploySelection {
            include_mods: mods,
            include_configs: configs,
            apply_configs: Vec::new(),
            restore_configs: Vec::new(),
            decline_configs: Vec::new(),
        }
    }

    /// A remote with the server root present.
    /// A remote handle the test can keep observing after the session boxes
    /// a clone of it.
    type Shared = Arc<Mutex<MemoryRemote>>;

    fn remote() -> Shared {
        let mut remote = MemoryRemote::new();
        remote.dirs.insert(BASE.to_owned());
        Arc::new(Mutex::new(remote))
    }

    fn open(remote: Shared) -> Result<Session> {
        let spec = spec();
        open_session(Box::new(remote), &spec, RemotePathBuf::new(BASE).unwrap())
    }

    fn context() -> PlanContext {
        PlanContext {
            profile_id: "1".to_owned(),
            game: "valheim".to_owned(),
            target: "sftp://host:22/srv".to_owned(),
            restart_policy: RestartPolicy::Manual,
        }
    }

    /// The heartbeat would only connect after the 60s interval; deployments
    /// in these tests finish first, so the factory is never exercised.
    fn no_connect() -> Result<Box<dyn RemoteOps>> {
        eyre::bail!("heartbeat connection not expected in tests")
    }

    fn meta() -> OperationMeta {
        OperationMeta::local("1", OperationKind::Manual)
    }

    fn preview(
        session: &mut Session,
        publication: &Publication,
        desired: &DesiredDeployment,
        selection: &DeploySelection,
        context: &PlanContext,
        meta: &OperationMeta,
    ) -> Result<Preview> {
        let mut progress = ProgressReporter::silent(SyncOperation::Preview, selection);
        preview_with_progress(
            session,
            publication,
            desired,
            selection,
            context,
            meta,
            &mut progress,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn deploy(
        session: &mut Session,
        connect: impl FnMut() -> Result<Box<dyn RemoteOps>> + Send + 'static,
        publication: &Publication,
        desired: &DesiredDeployment,
        selection: &DeploySelection,
        context: &PlanContext,
        meta: &OperationMeta,
        expected_plan_hash: Option<&str>,
        force: bool,
        mut report: impl FnMut(EngineProgress),
    ) -> Result<Deployment> {
        let mut progress = ProgressReporter::silent(SyncOperation::Deploy, selection);
        deploy_with_progress(
            session,
            connect,
            publication,
            desired,
            selection,
            context,
            meta,
            expected_plan_hash,
            force,
            &mut progress,
            &mut report,
        )
    }

    fn remote_state(remote: &Shared) -> ServerDeploymentState {
        serde_json::from_slice(
            remote
                .lock()
                .unwrap()
                .contents(STATE_REMOTE)
                .expect("state file missing"),
        )
        .expect("remote state file is not valid deployment state")
    }

    fn plant_live_lease(remote: &mut MemoryRemote) {
        remote.dirs.insert(LEASE_DIR_REMOTE.to_owned());
        let record = LeaseRecord {
            owner: "worker:vps".to_owned(),
            executor: ExecutorKind::Worker,
            operation_id: "op-x".to_owned(),
            acquired_at: Utc::now(),
            heartbeat_at: Utc::now(),
            ttl_secs: lease::LEASE_TTL.as_secs(),
        };
        remote.put_file(LEASE_FILE_REMOTE, &serde_json::to_vec(&record).unwrap());
    }

    #[test]
    fn missing_server_directory_is_rejected() {
        assert!(open(Arc::new(Mutex::new(MemoryRemote::new()))).is_err());
    }

    #[test]
    fn ambiguous_rename_does_not_start_a_second_mutation_sequence() {
        let mut remote = MemoryRemote::new();
        remote.ftp_rename_semantics = true;
        let target = RemotePathBuf::new("/srv/BepInEx/config/mod.cfg").unwrap();
        let temporary = target.with_suffix(".gale-upload");
        remote.put_file(target.as_str(), b"original");
        // A missing temporary file can mean an earlier rename completed
        // but its reply was lost. The old target and any backup stay put.
        let error = replace_remote(&mut remote, &temporary, &target).unwrap_err();
        assert!(format!("{error:#}").contains("may need reconciliation"));
        assert!(format!("{error:#}").contains("was absent"));
        assert_eq!(
            remote.contents(target.as_str()),
            Some(b"original".as_slice())
        );
        assert!(
            remote
                .contents(target.with_suffix(".gale-backup").as_str())
                .is_none()
        );
    }

    #[test]
    fn definite_ftp_rename_conflict_replaces_via_backup() {
        let mut remote = MemoryRemote::new();
        remote.ftp_rename_semantics = true;
        let target = RemotePathBuf::new("/srv/BepInEx/config/mod.cfg").unwrap();
        let temporary = target.with_suffix(".gale-upload");
        remote.put_file(target.as_str(), b"original");
        remote.put_file(temporary.as_str(), b"updated");

        replace_remote(&mut remote, &temporary, &target).unwrap();
        assert_eq!(
            remote.contents(target.as_str()),
            Some(b"updated".as_slice())
        );
        assert!(remote.contents(temporary.as_str()).is_none());
        assert!(
            remote
                .contents(target.with_suffix(".gale-backup").as_str())
                .is_none()
        );
    }

    #[test]
    fn payload_deploy_uploads_and_commits_state() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();
        let mut progress = Vec::new();

        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |p| progress.push(p),
        )
        .unwrap();

        assert!(
            remote_state(&memory).restart_required,
            "restart must survive a crash before finalization"
        );
        assert_eq!(deployment.summary.uploaded_files, 1);
        assert!(!progress.is_empty());

        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        // The payload landed, ownership and revision were recorded, the
        // state file was persisted remotely, and the lease was released.
        assert_eq!(
            remote_contents(&memory, MOD_DLL_REMOTE),
            Some(b"dll-bytes".to_vec())
        );
        assert_eq!(state.mods_revision, Some(publication.mods_revision.clone()));
        assert_eq!(
            state
                .files
                .get(&deploy_path("BepInEx/plugins/Author-ModA/ModA.dll")),
            Some(&OwnedFile {
                hash: blake3::hash(b"dll-bytes").to_hex().to_string(),
                size: 9,
            })
        );
        assert!(state.restart_required);
        let record = state.last_operation.unwrap();
        assert_eq!(record.status, OperationStatus::Succeeded);
        assert_eq!(record.executor, ExecutorKind::Local);

        let persisted = remote_state(&memory);
        assert_eq!(persisted.mods_revision, state.mods_revision);
        assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
    }

    #[test]
    fn deploy_rejects_a_stale_plan_hash() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();

        let result = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            Some("bogus-hash"),
            false,
            |_| {},
        );

        assert!(result.is_err());
        // Nothing was written: no upload, no state, no lease.
        assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
        assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
        assert!(remote_contents(&memory, STATE_REMOTE).is_none());
    }

    #[test]
    fn live_lease_blocks_deploy_and_is_reported_by_preview() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
        plant_live_lease(&mut memory.lock().unwrap());
        let mut session = open(memory.clone()).unwrap();

        let preview = preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert_eq!(preview.busy.unwrap().record.owner, "worker:vps");

        let result = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        );
        match result {
            Err(err) => assert!(err.downcast_ref::<lease::LeaseBusy>().is_some()),
            Ok(_) => panic!("expected LeaseBusy"),
        }
        assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
    }

    #[test]
    fn failed_upload_records_failure_and_releases_lease() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
        memory
            .lock()
            .unwrap()
            .fail_always
            .insert(format!("{MOD_DLL_REMOTE}.gale-upload"));
        let mut session = open(memory.clone()).unwrap();

        let result = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        );
        assert!(result.is_err());

        // The accurate state was still committed remotely: the operation is
        // recorded as failed, restart is flagged, and the failed file was
        // never claimed as owned.
        let persisted = remote_state(&memory);
        let record = persisted.last_operation.unwrap();
        assert_eq!(record.status, OperationStatus::Failed);
        assert!(persisted.restart_required);
        assert!(persisted.files.is_empty());
        assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
    }

    #[test]
    fn failed_removal_keeps_ownership_for_retry() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        // Desired payload does not include the stale owned file.
        let desired = DesiredDeployment {
            payload: BTreeMap::new(),
        };

        let memory = remote();
        let stale = "BepInEx/plugins/Old/Old.dll";
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file(&format!("{BASE}/{stale}"), b"old");
        }
        let mut state = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        state.files.insert(
            deploy_path(stale),
            OwnedFile {
                hash: "old-hash".to_owned(),
                size: 3,
            },
        );
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
            remote.fail_always.insert(format!("{BASE}/{stale}"));
        }

        let mut session = open(memory.clone()).unwrap();
        let result = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .err()
        .expect("failed removal must fail the deployment");
        assert!(format!("{result:#}").contains("failed to remove"));
        assert!(remote_contents(&memory, &format!("{BASE}/{stale}")).is_some());
        let state = remote_state(&memory);
        assert!(state.files.contains_key(&deploy_path(stale)));
        assert_eq!(state.mods_revision, None);
        assert_eq!(
            state.last_operation.unwrap().status,
            OperationStatus::Failed
        );
    }

    #[test]
    fn selected_config_is_written_and_applied() {
        let mut fixture = mod_fixture();
        fixture
            .config
            .insert(config_path("BepInEx/config/mod.cfg"), config_file(b"v2"));
        let publication = fixture.publication();

        let memory = remote();
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file("/srv/BepInEx/config/mod.cfg", b"custom");
            remote.put_file("/srv/BepInEx/config/other.cfg", b"server-only");
        }
        let mut session = open(memory.clone()).unwrap();

        let mut sel = selection(false, true);
        sel.apply_configs = vec![config_path("BepInEx/config/mod.cfg")];

        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &DesiredDeployment::default(),
            &sel,
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        // Selected config applied; the unselected server file is untouched.
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
            Some(b"v2".to_vec())
        );
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/other.cfg"),
            Some(b"server-only".to_vec())
        );
        assert_eq!(
            state.config[&config_path("BepInEx/config/mod.cfg")].applied,
            Some(ContentHash::from_hash(blake3::hash(b"v2")))
        );
        // A config write also requires a restart; NotRequired cannot clear it.
        assert!(state.restart_required);
    }

    #[test]
    fn deleted_published_config_is_restored_only_by_explicit_selection() {
        let path = config_path("BepInEx/config/mod.cfg");
        let file = config_file(b"published");
        let mut fixture = mod_fixture();
        fixture.config.insert(path.clone(), file.clone());
        let publication = fixture.publication();
        let memory = remote();
        let mut previous = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        previous.record_applied(&path, &file.hash);
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file(STATE_REMOTE, &state::serialize(&previous).unwrap());
            remote.put_file("/srv/BepInEx/config/server-only.cfg", b"server-only");
        }

        let mut session = open(memory.clone()).unwrap();
        let pending = preview(
            &mut session,
            &publication,
            &DesiredDeployment::default(),
            &selection(false, true),
            &context(),
            &meta(),
        )
        .unwrap();
        assert_eq!(
            pending.plan.conflicts[0].reason,
            PendingConfigReason::DeletedLocally
        );
        assert!(pending.plan.uploads.is_empty());

        let mut selected = selection(false, true);
        selected.apply_configs.push(path.clone());
        let still_pending = preview(
            &mut session,
            &publication,
            &DesiredDeployment::default(),
            &selected,
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(still_pending.plan.uploads.is_empty());

        selected.restore_configs.push(path.clone());
        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &DesiredDeployment::default(),
            &selected,
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        finish(
            &mut session,
            deployment,
            RestartOutcome::AwaitingManual,
            &meta(),
        )
        .unwrap();
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
            Some(b"published".to_vec())
        );
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/server-only.cfg"),
            Some(b"server-only".to_vec())
        );
        assert_eq!(
            open(memory).unwrap().state.config[&path].applied,
            Some(file.hash)
        );
    }

    #[test]
    fn out_of_scope_published_text_does_not_write_or_recreate_config_records() {
        let ordinary = config_path("BepInEx/config/mod.cfg");
        let translation = config_path("BepInEx/plugins/Mod/translations/en.json");
        let loader = config_path("doorstop_config.ini");
        let mut fixture = mod_fixture();
        for path in [&ordinary, &translation, &loader] {
            fixture
                .config
                .insert(path.clone(), config_file(b"publication"));
        }
        let publication = fixture.publication();
        let memory = remote();
        let mut previous = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        for path in [&translation, &loader] {
            previous.config.insert(path.clone(), Default::default());
        }
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file(STATE_REMOTE, &state::serialize(&previous).unwrap());
            remote.put_file(
                "/srv/BepInEx/plugins/Mod/translations/en.json",
                b"server translation",
            );
            remote.put_file("/srv/doorstop_config.ini", b"host loader");
        }

        let mut session = open(memory.clone()).unwrap();
        assert_eq!(session.warnings.len(), 1);
        assert!(session.warnings[0].contains("ignored 2 out-of-scope"));
        let mut selected = selection(false, true);
        selected.apply_configs.push(ordinary.clone());
        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &DesiredDeployment::default(),
            &selected,
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        finish(
            &mut session,
            deployment,
            RestartOutcome::AwaitingManual,
            &meta(),
        )
        .unwrap();
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
            Some(b"publication".to_vec())
        );
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/plugins/Mod/translations/en.json"),
            Some(b"server translation".to_vec())
        );
        assert_eq!(
            remote_contents(&memory, "/srv/doorstop_config.ini"),
            Some(b"host loader".to_vec())
        );
        let reopened = open(memory).unwrap();
        assert!(reopened.warnings.is_empty());
        assert_eq!(reopened.state.config.len(), 1);
        assert!(reopened.state.config.contains_key(&ordinary));
    }

    #[test]
    fn modified_unselected_config_stays_pending_and_untouched() {
        let mut fixture = mod_fixture();
        fixture
            .config
            .insert(config_path("BepInEx/config/mod.cfg"), config_file(b"v2"));
        let publication = fixture.publication();

        let memory = remote();
        memory
            .lock()
            .unwrap()
            .put_file("/srv/BepInEx/config/mod.cfg", b"custom");
        let mut session = open(memory.clone()).unwrap();

        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &DesiredDeployment::default(),
            &selection(false, true),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
            Some(b"custom".to_vec())
        );
        // The conflict is recorded for the next preview: nothing applied,
        // nothing declined, the file is still undecided.
        let record = &state.config[&config_path("BepInEx/config/mod.cfg")];
        assert!(record.applied.is_none() && record.written.is_none());
        assert!(record.declined.is_none());
    }

    #[test]
    fn mods_only_never_reads_writes_or_records_configs() {
        // The hard boundary: includeMods without includeConfigs performs
        // literally zero config interaction — no reconciliation reads, no
        // writes, no entries, no conflicts, no config state mutation.
        let mut fixture = mod_fixture();
        fixture.config.insert(
            config_path("BepInEx/config/published.cfg"),
            config_file(b"v2"),
        );
        let publication = fixture.publication();

        let memory = remote();
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file("/srv/BepInEx/config/server.cfg", b"server-owned");
            remote.put_file("/srv/BepInEx/config/published.cfg", b"old");
            // Any config reconciliation read must fail the operation.
            remote
                .fail_read_always
                .insert("/srv/BepInEx/config/server.cfg".to_owned());
            remote
                .fail_read_always
                .insert("/srv/BepInEx/config/published.cfg".to_owned());
        }
        let mut session = open(memory.clone()).unwrap();
        let preview = preview(
            &mut session,
            &publication,
            &desired_payload(),
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(preview.plan.config_entries.is_empty());
        assert!(preview.plan.conflicts.is_empty());
        assert!(
            preview
                .plan
                .uploads
                .iter()
                .all(|upload| upload.kind == UploadKind::Payload)
        );

        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired_payload(),
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        // Nothing read, nothing written, nothing recorded.
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/server.cfg"),
            Some(b"server-owned".to_vec())
        );
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/published.cfg"),
            Some(b"old".to_vec())
        );
        assert!(state.config.is_empty());
    }

    /// Deploys `desired` onto the memory remote and finishes, leaving an
    /// unchanged remote — the shared setup for the reader-pool tests.
    fn deploy_to_memory(memory: &Shared, publication: &Publication, desired: &DesiredDeployment) {
        let mut session = open(memory.clone()).unwrap();
        let deployment = deploy(
            &mut session,
            no_connect,
            publication,
            desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
    }

    /// The pooled snapshot must match the serial plan with bounded helpers.
    #[test]
    fn memory_readers_match_the_serial_snapshot() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = large_mods_payload(8);
        let memory = remote();
        deploy_to_memory(&memory, &publication, &desired);

        let serial = {
            let mut session = open(memory.clone()).unwrap();
            preview(
                &mut session,
                &publication,
                &desired,
                &selection(true, false),
                &context(),
                &meta(),
            )
            .unwrap()
        };
        assert!(serial.plan.uploads.is_empty());

        memory.lock().unwrap().allow_readers = true;
        memory.lock().unwrap().readers_opened = 0;
        let mut session = open(memory.clone()).unwrap();
        let pooled = preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert_eq!(pooled.plan.hash, serial.plan.hash);
        assert_eq!(pooled.plan.uploads, serial.plan.uploads);
        let opened = memory.lock().unwrap().readers_opened;
        assert!(
            (1..=MAX_SNAPSHOT_READERS).contains(&opened),
            "expected bounded helper readers, opened {opened}"
        );
    }

    /// When helper connections cannot be opened the snapshot runs over
    /// the authoritative connection and produces the serial plan.
    #[test]
    fn memory_reader_connect_failure_falls_back_to_serial() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = large_mods_payload(8);
        let memory = remote();
        deploy_to_memory(&memory, &publication, &desired);

        let serial = {
            let mut session = open(memory.clone()).unwrap();
            preview(
                &mut session,
                &publication,
                &desired,
                &selection(true, false),
                &context(),
                &meta(),
            )
            .unwrap()
        };

        {
            let mut remote = memory.lock().unwrap();
            remote.allow_readers = true;
            remote.fail_reader_connect = true;
        }
        let mut session = open(memory.clone()).unwrap();
        let fallback = preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert_eq!(fallback.plan.hash, serial.plan.hash);
        assert_eq!(memory.lock().unwrap().readers_opened, 0);
    }

    /// The zero-config boundary holds with the pool active: a mods-only
    /// preview opens reader helpers but performs no config reads — any
    /// config read is armed to fail the operation.
    #[test]
    fn mods_only_preview_with_readers_never_reads_configs() {
        let mut fixture = mod_fixture();
        fixture.config.insert(
            config_path("BepInEx/config/published.cfg"),
            config_file(b"v2"),
        );
        let publication = fixture.publication();
        let desired = large_mods_payload(8);
        let memory = remote();
        deploy_to_memory(&memory, &publication, &desired);

        {
            let mut remote = memory.lock().unwrap();
            remote.allow_readers = true;
            remote.put_file(
                &format!("{BASE}/BepInEx/config/server.cfg"),
                b"server-owned",
            );
            remote
                .fail_read_always
                .insert(format!("{BASE}/BepInEx/config/server.cfg"));
            remote
                .fail_read_always
                .insert(format!("{BASE}/BepInEx/config/published.cfg"));
        }
        let mut session = open(memory.clone()).unwrap();
        let previewed = preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(
            memory.lock().unwrap().readers_opened > 0,
            "the reader pool must be active for this test to cover it"
        );
        assert!(previewed.plan.config_entries.is_empty());
        assert!(previewed.plan.conflicts.is_empty());
        assert!(previewed.plan.uploads.is_empty());
    }

    #[test]
    fn config_preview_reads_the_remote_configs_it_evaluates() {
        // The same boundary from the other side: an explicit config
        // preview does reconcile remote config content, so a refused
        // read fails the operation rather than silently skipping it.
        let mut fixture = mod_fixture();
        fixture.config.insert(
            config_path("BepInEx/config/published.cfg"),
            config_file(b"v2"),
        );
        let publication = fixture.publication();

        let memory = remote();
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file("/srv/BepInEx/config/published.cfg", b"old");
            remote
                .fail_read_always
                .insert("/srv/BepInEx/config/published.cfg".to_owned());
        }
        let mut session = open(memory.clone()).unwrap();
        assert!(
            preview(
                &mut session,
                &publication,
                &desired_payload(),
                &selection(false, true),
                &context(),
                &meta(),
            )
            .is_err(),
            "an explicit config preview must read the configs it evaluates"
        );
    }

    #[test]
    fn removal_is_bounded_to_owned_files_and_strays_survive() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();

        let memory = remote();
        let stale = "BepInEx/plugins/Old/Old.dll";
        {
            let mut remote = memory.lock().unwrap();
            // A manually installed, never-owned plugin inside the payload
            // dir: removal authority comes from records, not directory
            // membership, so it must survive and be surfaced as unmanaged.
            remote.put_file("/srv/BepInEx/plugins/ServerOnly.dll", b"stray");
            remote.put_file(&format!("{BASE}/{stale}"), b"old");
            // Outside the managed scope nothing is ever touched: config
            // files and unrelated server content survive every deploy.
            remote.put_file("/srv/BepInEx/config/user.cfg", b"user");
            remote.put_file("/srv/saves/world.db", b"save");
        }
        let mut state = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        state.files.insert(
            deploy_path(stale),
            OwnedFile {
                hash: "old".to_owned(),
                size: 3,
            },
        );
        memory
            .lock()
            .unwrap()
            .put_file(STATE_REMOTE, &state::serialize(&state).unwrap());

        let mut session = open(memory.clone()).unwrap();
        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        // The never-owned server-only plugin is reported as unmanaged.
        let unmanaged = deployment.plan.unmanaged.clone();

        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        // The obsolete Gale-owned plugin was removed; the never-owned
        // server-only plugin survived.
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/plugins/ServerOnly.dll"),
            Some(b"stray".to_vec())
        );
        assert_eq!(
            unmanaged,
            vec![deploy_path("BepInEx/plugins/ServerOnly.dll")]
        );
        assert!(remote_contents(&memory, &format!("{BASE}/{stale}")).is_none());
        assert!(!state.files.contains_key(&deploy_path(stale)));
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/user.cfg"),
            Some(b"user".to_vec())
        );
        assert_eq!(
            remote_contents(&memory, "/srv/saves/world.db"),
            Some(b"save".to_vec())
        );
    }

    #[test]
    fn same_size_remote_edit_of_owned_payload_is_repaired() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();

        // First deployment lands ModA.
        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        // The remote file is then edited to different bytes of the same
        // length. A size comparison alone cannot see the change.
        memory
            .lock()
            .unwrap()
            .put_file(MOD_DLL_REMOTE, b"edited-dd");

        let mut session = open(memory.clone()).unwrap();
        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();

        // Content hashing detected the divergence and re-uploaded.
        assert_eq!(deployment.summary.uploaded_files, 1);
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        assert_eq!(
            remote_contents(&memory, MOD_DLL_REMOTE),
            Some(b"dll-bytes".to_vec())
        );
        assert_eq!(state.mods_revision, Some(publication.mods_revision.clone()));
    }

    #[test]
    fn deploy_rejects_approval_when_remote_config_changed() {
        let mut fixture = mod_fixture();
        fixture
            .config
            .insert(config_path("BepInEx/config/mod.cfg"), config_file(b"v2"));
        let publication = fixture.publication();

        let memory = remote();
        memory
            .lock()
            .unwrap()
            .put_file("/srv/BepInEx/config/mod.cfg", b"server-v1");
        let mut session = open(memory.clone()).unwrap();

        let mut sel = selection(false, true);
        sel.apply_configs = vec![config_path("BepInEx/config/mod.cfg")];

        let preview = preview(
            &mut session,
            &publication,
            &DesiredDeployment::default(),
            &sel,
            &context(),
            &meta(),
        )
        .unwrap();
        let approved = preview.plan.hash;

        // The remote file changes after the approval was given. The planned action is still Write,
        // but the approved content no longer matches what is actually there.
        memory
            .lock()
            .unwrap()
            .put_file("/srv/BepInEx/config/mod.cfg", b"server-v2-edited");

        let result = deploy(
            &mut session,
            no_connect,
            &publication,
            &DesiredDeployment::default(),
            &sel,
            &context(),
            &meta(),
            Some(&approved),
            false,
            |_| {},
        );
        match result {
            Err(err) => assert!(err.to_string().contains("preview")),
            Ok(_) => panic!("a stale approval must be rejected"),
        }
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
            Some(b"server-v2-edited".to_vec())
        );
    }

    #[test]
    fn deploy_rejects_a_changed_restart_policy() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();

        let preview = preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();

        // The approval was given for Manual restarts; deploying with
        // Immediate must not reuse it.
        let mut changed = context();
        changed.restart_policy = RestartPolicy::Immediate;
        let result = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &changed,
            &meta(),
            Some(&preview.plan.hash),
            false,
            |_| {},
        );
        match result {
            Err(err) => assert!(err.to_string().contains("preview")),
            Ok(_) => panic!("a restart-policy change must invalidate the approval"),
        }
        assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
    }

    #[test]
    fn set_config_policy_is_serialized_under_the_lease() {
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();
        let path = config_path("BepInEx/config/mod.cfg");
        let hash = ContentHash::from_hash(blake3::hash(b"v1"));

        // While another executor holds the lease, the policy write is busy.
        plant_live_lease(&mut memory.lock().unwrap());
        let blocked = set_config_policy(
            &mut session,
            &path,
            ConfigUpdatePolicy::AlwaysKeep,
            Some(&hash),
            &meta(),
        );
        match blocked {
            Err(err) => assert!(err.downcast_ref::<lease::LeaseBusy>().is_some()),
            Ok(()) => panic!("a policy update must not bypass the deployment lease"),
        }

        // Once the lease frees up, the policy write goes through. The
        // record file is deleted first because a directory can't be
        // removed while it still holds the file.
        memory
            .lock()
            .unwrap()
            .delete_file(&RemotePathBuf::new(LEASE_FILE_REMOTE).unwrap())
            .unwrap();
        memory
            .lock()
            .unwrap()
            .delete_dir(&RemotePathBuf::new(LEASE_DIR_REMOTE).unwrap())
            .unwrap();
        set_config_policy(
            &mut session,
            &path,
            ConfigUpdatePolicy::AlwaysKeep,
            Some(&hash),
            &meta(),
        )
        .unwrap();
        let record = remote_state(&memory).config[&path].clone();
        assert_eq!(record.policy, ConfigUpdatePolicy::AlwaysKeep);
        assert_eq!(record.policy_set_at, Some(hash));
    }

    #[test]
    fn failed_config_write_marks_the_operation_partial() {
        let mut fixture = mod_fixture();
        fixture
            .config
            .insert(config_path("BepInEx/config/one.cfg"), config_file(b"one"));
        fixture
            .config
            .insert(config_path("BepInEx/config/two.cfg"), config_file(b"two"));
        let publication = fixture.publication();

        let memory = remote();
        memory
            .lock()
            .unwrap()
            .fail_always
            .insert("/srv/BepInEx/config/two.cfg.gale-upload".to_owned());
        let mut session = open(memory.clone()).unwrap();

        let mut sel = selection(false, true);
        sel.apply_configs = vec![
            config_path("BepInEx/config/one.cfg"),
            config_path("BepInEx/config/two.cfg"),
        ];

        let deployment = deploy(
            &mut session,
            no_connect,
            &publication,
            &DesiredDeployment::default(),
            &sel,
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        assert_eq!(
            deployment.failed_config_writes,
            vec![config_path("BepInEx/config/two.cfg")]
        );

        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        // One config was written and recorded as applied. The other stays
        // retryable, and the operation is Partial, not Succeeded.
        let record = state.last_operation.unwrap();
        assert_eq!(record.status, OperationStatus::Partial);
        assert_eq!(
            state.config[&config_path("BepInEx/config/one.cfg")].applied,
            Some(ContentHash::from_hash(blake3::hash(b"one")))
        );
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/one.cfg"),
            Some(b"one".to_vec())
        );
        assert!(
            state
                .config
                .get(&config_path("BepInEx/config/two.cfg"))
                .map_or(true, |record| record.applied.is_none())
        );
    }

    #[test]
    fn external_restart_requires_explicit_acknowledgment_under_the_lease() {
        let memory = remote();
        let state = ServerDeploymentState {
            version: state::VERSION,
            restart_required: true,
            ..Default::default()
        };
        memory
            .lock()
            .unwrap()
            .put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
        let mut session = open(memory.clone()).unwrap();
        assert!(session.state.restart_required);

        let acknowledged = acknowledge_external_restart(&mut session, &meta()).unwrap();
        assert!(!acknowledged.restart_required);
        assert_eq!(acknowledged.operation_seq, 1);
        let record = acknowledged.last_operation.unwrap();
        assert_eq!(record.restart, RestartOutcome::Restarted);
        assert!(record.external_restart_acknowledged);
        assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
        assert!(!open(memory.clone()).unwrap().state.restart_required);
        assert!(acknowledge_external_restart(&mut session, &meta()).is_err());

        // A later no-change deployment cannot resurrect the old reminder.
        let fixture = mod_fixture();
        let deployment = deploy(
            &mut session,
            no_connect,
            &fixture.publication(),
            &DesiredDeployment::default(),
            &selection(false, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        assert!(!state.restart_required);
    }

    #[test]
    fn only_a_confirmed_gale_restart_clears_an_outstanding_restart() {
        for (outcome, required) in [
            (RestartOutcome::Restarted, false),
            (RestartOutcome::StartupUnverified, true),
            (RestartOutcome::Failed, true),
            (RestartOutcome::NotRequired, true),
            (RestartOutcome::AwaitingManual, true),
        ] {
            let fixture = mod_fixture();
            let memory = remote();
            let mut session = open(memory.clone()).unwrap();
            let deployment = deploy(
                &mut session,
                no_connect,
                &fixture.publication(),
                &desired_payload(),
                &selection(true, false),
                &context(),
                &meta(),
                None,
                false,
                |_| {},
            )
            .unwrap();
            let state = finish(&mut session, deployment, outcome, &meta()).unwrap();
            assert_eq!(state.restart_required, required, "{outcome:?}");
            let persisted = open(memory).unwrap().state;
            assert_eq!(
                persisted.restart_required, required,
                "persisted {outcome:?}"
            );
            assert_eq!(persisted.last_operation.unwrap().restart, outcome);
        }
    }

    #[test]
    fn deploy_does_not_confuse_a_dead_transport_with_a_takeover() {
        // The transport dies after the session opens. Whatever fails
        // first must report the transport problem — never a phantom
        // competing executor — and must not touch payload files.
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();
        memory.lock().unwrap().connection_dead = true;

        let err = deploy(
            &mut session,
            no_connect,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .err()
        .expect("a dead transport must fail the deployment");

        let chain = format!("{err:#}");
        assert!(
            !chain.contains("taken over"),
            "a transport failure must not blame another executor: {chain}"
        );
        assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
    }

    // ---------- end-to-end over the in-memory FTP server ----------
    //
    // These run the real `RemoteConnection` against `remote::fake_ftp`,
    // so the transport's command choices (TYPE I, MLST, RETR) are part
    // of what is verified.

    fn ftp_ops(addr: std::net::SocketAddr) -> Result<Box<dyn RemoteOps>> {
        let settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftp,
            host: "127.0.0.1".to_owned(),
            port: addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            ..Default::default()
        };
        match RemoteConnection::connect(&settings, "pw")? {
            ConnectionAttempt::Connected(conn) => Ok(conn),
            _ => eyre::bail!("unexpected trust prompt from the fake FTP server"),
        }
    }

    fn open_ftp(addr: std::net::SocketAddr) -> Result<Session> {
        let spec = spec();
        open_session(ftp_ops(addr)?, &spec, RemotePathBuf::new("/").unwrap())
    }

    /// Same connection path as `ftp_ops`, but over explicit FTPS pinned
    /// to the fake's self-signed certificate.
    fn ftps_connection(server: &remote::fake_ftp::FakeFtp) -> Result<RemoteConnection> {
        let settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftps,
            host: "127.0.0.1".to_owned(),
            port: server.addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            trusted_certificate: server.trusted_certificate(),
            ..Default::default()
        };
        match RemoteConnection::connect(&settings, "pw")? {
            ConnectionAttempt::Connected(conn) => Ok(*conn),
            _ => eyre::bail!("pinned fake FTPS certificate was not accepted"),
        }
    }

    fn open_ftps(server: &remote::fake_ftp::FakeFtp) -> Result<Session> {
        let spec = spec();
        open_session(
            Box::new(ftps_connection(server)?),
            &spec,
            RemotePathBuf::new("/").unwrap(),
        )
    }

    fn ftps_ops(addr: std::net::SocketAddr, certificate: &str) -> Result<Box<dyn RemoteOps>> {
        let settings = RemoteServerSettings {
            protocol: RemoteProtocol::Ftps,
            host: "127.0.0.1".to_owned(),
            port: addr.port(),
            username: "u".to_owned(),
            server_directory: "/".to_owned(),
            authentication: RemoteAuthentication::Password,
            trusted_certificate: Some(certificate.to_owned()),
            ..Default::default()
        };
        match RemoteConnection::connect(&settings, "pw")? {
            ConnectionAttempt::Connected(conn) => Ok(conn),
            _ => eyre::bail!("pinned fake FTPS certificate was not accepted"),
        }
    }

    /// Deploys `desired` onto the fake host and finishes the operation,
    /// leaving an unchanged remote for the phase under test.
    fn deploy_initial(
        server: &remote::fake_ftp::FakeFtp,
        publication: &Publication,
        desired: &DesiredDeployment,
    ) {
        let addr = server.addr;
        let certificate = server.trusted_certificate().unwrap();
        let mut session = open_ftps(server).unwrap();
        let deployment = deploy(
            &mut session,
            move || ftps_ops(addr, &certificate),
            publication,
            desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
    }

    #[test]
    fn ftps_preview_recovers_after_reset_without_reupload_or_stranded_lease() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            tls: true,
            size_requires_binary: true,
            ..Default::default()
        });
        let mut desired = DesiredDeployment {
            payload: BTreeMap::new(),
        };
        for index in 0..164 {
            let path = deploy_path(&format!("BepInEx/plugins/Author-ModA/mod-{index:03}.dll"));
            desired
                .payload
                .insert(path, staged(format!("mod-{index:03}").as_bytes()));
        }
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let addr = server.addr;
        let certificate = server.trusted_certificate().unwrap();
        let mut deployed = open_ftps(&server).unwrap();
        let deployment = deploy(
            &mut deployed,
            {
                let certificate = certificate.clone();
                move || ftps_ops(addr, &certificate)
            },
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        assert_eq!(deployment.summary.uploaded_files, 164);
        finish(
            &mut deployed,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        drop(deployed);

        let mut post_deploy = open_ftps(&server).unwrap();
        assert_eq!(post_deploy.state.files.len(), 164);
        // Drop the control connection before the 80th RETR completion.
        // The incomplete hash must be retried on a new FTPS connection.
        server.reset_on_retr(81, false); // state refresh is the first RETR
        let recovered = preview(
            &mut post_deploy,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(recovered.plan.uploads.is_empty());
        assert!(recovered.busy.is_none());
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

        // If the first read of a payload file and its reconnect retry both
        // fail, Preview must return the transport error instead of
        // manufacturing an upload plan. RETRs run on parallel readers now,
        // so the reset targets one path rather than a global ordinal.
        server.reset_on_retr_of("/BepInEx/plugins/Author-ModA/mod-000.dll", 2, false);
        let error = preview(
            &mut post_deploy,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("read failed after reconnect"));
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

        // The in-snapshot state refresh is the authoritative connection's
        // last read before it idles until release: its data and 226 arrive,
        // then the server closes the connection. Release must reconnect,
        // verify the owner, and remove the claim through that fresh
        // connection.
        server.reset_on_retr_of("/BepInEx/config/.gale-server-state.json", 1, true);
        let logins_before = server
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|command| command.starts_with("USER "))
            .count();
        let completed = preview(
            &mut post_deploy,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(completed.plan.uploads.is_empty());
        assert!(completed.busy.is_none());
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
        let logins_after = server
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|command| command.starts_with("USER "))
            .count();
        assert!(
            logins_after > logins_before,
            "lease release must reconnect after the reset"
        );
        drop(post_deploy);

        let mut subsequent = open_ftps(&server).unwrap();
        let next = preview(
            &mut subsequent,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(next.plan.uploads.is_empty());
        assert!(
            next.busy.is_none(),
            "a successful preview must leave no in-progress warning"
        );
        let unchanged = deploy(
            &mut subsequent,
            {
                let certificate = certificate.clone();
                move || ftps_ops(addr, &certificate)
            },
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            Some(&next.plan.hash),
            false,
            |_| {},
        )
        .unwrap();
        assert_eq!(unchanged.summary.uploaded_files, 0);
        finish(
            &mut subsequent,
            unchanged,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
    }

    /// Counts received `verb` commands by their path argument, e.g.
    /// `RETR /BepInEx/plugins/Mod.dll`.
    fn command_targets(server: &remote::fake_ftp::FakeFtp, verb: &str) -> BTreeMap<String, usize> {
        let mut targets = BTreeMap::new();
        for command in server.commands.lock().unwrap().iter() {
            if let Some(path) = command.strip_prefix(&format!("{verb} ")) {
                *targets.entry(path.to_owned()).or_default() += 1;
            }
        }
        targets
    }

    /// An unchanged mods-only preview over FTPS: scanning and hashing run
    /// on read-only helper connections, so every payload file and
    /// directory is touched exactly once and no payload path pays an MLST
    /// probe — the listing already supplied its size.
    #[test]
    fn ftps_preview_verifies_each_file_and_directory_exactly_once() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            tls: true,
            size_requires_binary: true,
            ..Default::default()
        });
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = large_mods_payload(50);
        deploy_initial(&server, &publication, &desired);

        let mut session = open_ftps(&server).unwrap();
        server.clear_commands();
        let previewed = preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(previewed.plan.uploads.is_empty());
        assert!(previewed.busy.is_none());
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

        assert_eq!(
            server
                .commands
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.starts_with("MLST /BepInEx/plugins"))
                .count(),
            0,
            "verification reads must not probe payload metadata"
        );

        let retrs = command_targets(&server, "RETR");
        for path in desired.payload.keys() {
            let remote = format!("/{path}");
            assert_eq!(
                retrs.get(&remote),
                Some(&1),
                "expected exactly one verification read of {remote}"
            );
        }
        // 3 payload roots (plugins, patchers, monomod) + 50 mod dirs +
        // 8 translations dirs.
        let lists = command_targets(&server, "LIST");
        assert_eq!(lists.len(), 61);
        for (dir, count) in &lists {
            assert_eq!(*count, 1, "directory {dir} listed {count} times");
        }

        // Authoritative connection plus at most MAX_SNAPSHOT_READERS
        // helpers; preview runs no heartbeat.
        assert!(
            server.peak_connections() > 1,
            "reader helpers must be opened for this fixture"
        );
        assert!(server.peak_connections() <= 1 + MAX_SNAPSHOT_READERS);
    }

    /// Remote drift between approval and execution — a same-size edit or
    /// a deletion — must fail the deploy as stale rather than overwrite.
    #[test]
    fn ftps_deploy_rejects_payload_drift_after_the_approved_preview() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            tls: true,
            size_requires_binary: true,
            ..Default::default()
        });
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = large_mods_payload(50);
        deploy_initial(&server, &publication, &desired);
        let addr = server.addr;
        let certificate = server.trusted_certificate().unwrap();

        let drifted = "BepInEx/plugins/Author-Mod00/Mod.dll";
        let drifted_remote = format!("/{drifted}");
        let staged_size = desired.payload[&deploy_path(drifted)].size;

        fn approve(
            session: &mut Session,
            publication: &Publication,
            desired: &DesiredDeployment,
        ) -> String {
            preview(
                session,
                publication,
                desired,
                &selection(true, false),
                &context(),
                &meta(),
            )
            .unwrap()
            .plan
            .hash
        }
        fn attempt(
            session: &mut Session,
            publication: &Publication,
            desired: &DesiredDeployment,
            addr: std::net::SocketAddr,
            certificate: &str,
            hash: &str,
        ) -> Result<Deployment> {
            deploy(
                session,
                {
                    let certificate = certificate.to_owned();
                    move || ftps_ops(addr, &certificate)
                },
                publication,
                desired,
                &selection(true, false),
                &context(),
                &meta(),
                Some(hash),
                false,
                |_| {},
            )
        }

        let mut session = open_ftps(&server).unwrap();

        // A same-size remote edit: only hashing the bytes detects it.
        let approved = approve(&mut session, &publication, &desired);
        let replacement = vec![b'!'; staged_size as usize];
        server.seed_file(&drifted_remote, &replacement);
        let error = match attempt(
            &mut session,
            &publication,
            &desired,
            addr,
            &certificate,
            &approved,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a stale approval must be rejected"),
        };
        assert!(
            format!("{error:#}").contains("server state changed"),
            "expected the stale-plan error, got: {error:#}"
        );
        assert_eq!(
            server.file(&drifted_remote),
            Some(replacement),
            "a stale deploy must not overwrite the remote file"
        );
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

        // A deletion between approval and execution is stale too.
        let approved = approve(&mut session, &publication, &desired);
        server.remove_file(&drifted_remote);
        let error = match attempt(
            &mut session,
            &publication,
            &desired,
            addr,
            &certificate,
            &approved,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a stale approval must be rejected"),
        };
        assert!(
            format!("{error:#}").contains("server state changed"),
            "expected the stale-plan error, got: {error:#}"
        );
        assert!(server.file(&drifted_remote).is_none());
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
        // Authoritative + heartbeat slot + readers, across every deploy above.
        assert!(server.peak_connections() <= 2 + MAX_SNAPSHOT_READERS);
    }

    /// Snapshot helpers are read-only connections: during an unchanged
    /// preview every mutating command the fake server receives targets the
    /// deployment lease, never a payload or config path.
    #[test]
    fn ftps_preview_reader_connections_never_mutate_payload_paths() {
        use remote::fake_ftp::{FakeFtp, Options};

        const MUTATING: [&str; 7] = ["STOR", "APPE", "DELE", "MKD", "RMD", "RNFR", "RNTO"];

        let server = FakeFtp::valheim_host(Options {
            tls: true,
            size_requires_binary: true,
            ..Default::default()
        });
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = large_mods_payload(8);
        deploy_initial(&server, &publication, &desired);

        let mut session = open_ftps(&server).unwrap();
        server.clear_commands();
        let previewed = preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap();
        assert!(previewed.plan.uploads.is_empty());
        assert!(
            server.peak_connections() > 1,
            "reader helpers must be opened for this test to prove anything"
        );

        let mut saw_mutation = false;
        for command in server.commands.lock().unwrap().iter() {
            let verb = command.split(' ').next().unwrap_or_default();
            if MUTATING.contains(&verb) {
                saw_mutation = true;
                let target = command.split(' ').nth(1).unwrap_or_default();
                assert!(
                    target.starts_with("/BepInEx/config/.gale-deploy.lock"),
                    "{command} mutated a path outside the deployment lease"
                );
            }
        }
        assert!(saw_mutation, "lease acquire/release must have run");
    }
    /// dot-prefixed paths fully visible — exercised through the real FTP
    /// transport. A completed deployment must write, verify, and reload
    /// its exact recorded state through a fresh connection, and a second
    /// operation must remove an owned file while preserving unmanaged
    /// ones.
    #[test]
    fn deployment_state_round_trips_on_a_size_refusing_host() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            size_requires_binary: true,
            ..Default::default()
        });
        // An unmanaged file inside the payload tree must never be removed.
        server.seed_file("/BepInEx/plugins/hand-placed.dll", b"foreign");
        let addr = server.addr;

        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();

        let mut session = open_ftp(addr).unwrap();
        let deployment = deploy(
            &mut session,
            move || ftp_ops(addr),
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        assert_eq!(deployment.summary.uploaded_files, 1);
        let first = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        assert_eq!(first.operation_seq, 1);
        drop(session);

        // A fresh connection reloads the exact recorded state — the same
        // authoritative record a worker executor would see.
        let mut second = open_ftp(addr).unwrap();
        assert_eq!(
            state::serialize(&second.state).unwrap(),
            state::serialize(&first).unwrap(),
            "a fresh session must reload the persisted state exactly"
        );
        assert!(
            second
                .state
                .files
                .contains_key(&deploy_path("BepInEx/plugins/Author-ModA/ModA.dll"))
        );

        // Second operation over another connection: the owned mod is
        // removed, the unmanaged file survives, the sequence advances.
        let empty = Fixture {
            mods: Vec::new(),
            config: BTreeMap::new(),
        };
        let deployment = deploy(
            &mut second,
            move || ftp_ops(addr),
            &empty.publication(),
            &DesiredDeployment {
                payload: BTreeMap::new(),
            },
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .unwrap();
        assert_eq!(deployment.summary.removed_files, 1);
        let second_state = finish(
            &mut second,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        assert_eq!(second_state.operation_seq, 2);

        assert!(
            server
                .file("/BepInEx/plugins/Author-ModA/ModA.dll")
                .is_none()
        );
        assert_eq!(
            server.file("/BepInEx/plugins/hand-placed.dll"),
            Some(b"foreign".to_vec())
        );
        assert!(
            server
                .file("/BepInEx/config/.gale-server-state.json")
                .is_some()
        );
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
    }

    /// A host that refuses to return even freshly-written state cannot
    /// prove persistence — the deployment must fail closed. The holder
    /// marker still verifies the lease (its record is equally refused),
    /// and the claim is released cleanly.
    #[test]
    fn deploy_fails_when_the_state_cannot_be_verified() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            refuse_retr: true,
            ..Default::default()
        });
        let addr = server.addr;

        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();

        let mut session = open_ftp(addr).unwrap();
        let err = deploy(
            &mut session,
            move || ftp_ops(addr),
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
            |_| {},
        )
        .err()
        .expect("unverifiable state persistence must fail the deploy");

        assert!(
            format!("{err:#}").contains("failed to persist remote deployment state"),
            "expected a persistence failure, got: {err:#}"
        );
        // The marker proved ownership even though the lease record could
        // not be read back either.
        assert!(
            session
                .warnings
                .iter()
                .any(|w| w.contains("marker directory"))
        );
        // The upload landed and the lease was released. The state temp
        // was written but could never be verified, so it was never
        // renamed — an unverifiable write must not become the
        // authoritative state.
        assert!(
            server
                .file("/BepInEx/plugins/Author-ModA/ModA.dll")
                .is_some()
        );
        assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
        assert!(
            server
                .file("/BepInEx/config/.gale-server-state.json")
                .is_none()
        );
        assert!(
            server
                .file("/BepInEx/config/.gale-server-state.json.tmp")
                .is_some()
        );
    }

    /// A remote state that moved under this session is never overwritten:
    /// the sequence guard fires over the real transport too.
    #[test]
    fn persist_state_refuses_to_overwrite_a_moved_remote() {
        use remote::fake_ftp::FakeFtp;

        let server = FakeFtp::valheim_host(Default::default());
        let mut session = open_ftp(server.addr).unwrap();

        // Another writer moved the remote state after this session loaded it.
        let moved = serde_json::json!({"version": 2, "operationSeq": 7});
        server.seed_file(
            "/BepInEx/config/.gale-server-state.json",
            &serde_json::to_vec(&moved).unwrap(),
        );

        let err = persist_state(&mut session)
            .err()
            .expect("a moved remote sequence must refuse the write");
        assert!(
            format!("{err:#}").contains("changed during this operation"),
            "expected the sequence guard, got: {err:#}"
        );
        // And nothing was written over the newer state.
        assert_eq!(
            server.file("/BepInEx/config/.gale-server-state.json"),
            Some(serde_json::to_vec(&moved).unwrap())
        );
    }

    #[test]
    fn authoritative_state_read_reconnects_before_persisting_exact_bytes() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            tls: true,
            ..Default::default()
        });
        let target = "/BepInEx/config/.gale-server-state.json";
        let initial = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        server.seed_file(target, &state::serialize(&initial).unwrap());
        let mut session = open_ftps(&server).unwrap();
        session.state.restart_required = true;
        let expected = state::serialize(&session.state).unwrap();

        // The first RETR is the pre-write authoritative read. A lost
        // completion response must not prevent its safe fresh-connection retry.
        server.reset_on_retr(1, false);
        persist_state(&mut session).unwrap();
        let mut fresh = ftps_connection(&server).unwrap();
        assert_eq!(
            fresh
                .read(&RemotePathBuf::new(target).unwrap(), state::MAX_STATE_BYTES)
                .unwrap(),
            Some(expected)
        );

        // A changed sequence still defeats the guard after reconnection.
        server.seed_file(target, br#"{"version":2,"operationSeq":4}"#);
        server.reset_on_retr(1, false);
        let error = persist_state(&mut session).unwrap_err();
        assert!(format!("{error:#}").contains("changed during this operation"));
    }

    /// A store that truncates the state temp file but still answers `226`
    /// fails the read-back check — and that failure must not have already
    /// destroyed the previously valid state. A fresh FTPS connection
    /// proves the exact original bytes survive.
    #[test]
    fn persist_state_never_replaces_valid_state_with_a_truncated_temp() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            tls: true,
            truncate_stor_to: Some(512),
            ..Default::default()
        });
        let original = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        let original_bytes = state::serialize(&original).unwrap();
        let target = "/BepInEx/config/.gale-server-state.json";
        server.seed_file(target, &original_bytes);

        let mut session = open_ftps(&server).unwrap();
        for index in 0..64 {
            let path = deploy_path(&format!("BepInEx/plugins/Owned-{index}/file.dll"));
            session.state.files.insert(
                path,
                OwnedFile {
                    hash: blake3::hash(format!("owned-{index}").as_bytes())
                        .to_hex()
                        .to_string(),
                    size: index as u64,
                },
            );
        }
        assert!(state::serialize(&session.state).unwrap().len() > 512);

        let error = persist_state(&mut session).expect_err("truncated temp must fail persistence");
        assert!(
            format!("{error:#}").contains("failed to write remote deployment state"),
            "expected truncated state persistence to fail, got: {error:#}"
        );

        let mut fresh = ftps_connection(&server).unwrap();
        let actual = fresh
            .read(&RemotePathBuf::new(target).unwrap(), state::MAX_STATE_BYTES)
            .unwrap()
            .expect("the prior state must remain");
        assert_eq!(actual.len(), original_bytes.len());
        assert_eq!(blake3::hash(&actual), blake3::hash(&original_bytes));
        assert_eq!(actual, original_bytes);
    }

    /// The success path of the same contract: a verifiable state write
    /// lands byte-identical on the target, proven through an independent
    /// FTPS connection rather than the fake's filesystem shortcut.
    #[test]
    fn persist_state_round_trips_exact_bytes_across_fresh_ftps_connections() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            tls: true,
            ..Default::default()
        });
        let mut session = open_ftps(&server).unwrap();
        for index in 0..64 {
            let path = deploy_path(&format!("BepInEx/plugins/Owned-{index}/file.dll"));
            session.state.files.insert(
                path,
                OwnedFile {
                    hash: blake3::hash(format!("owned-{index}").as_bytes())
                        .to_hex()
                        .to_string(),
                    size: index as u64,
                },
            );
        }
        let expected = state::serialize(&session.state).unwrap();

        persist_state(&mut session).unwrap();
        drop(session);

        let target = RemotePathBuf::new("/BepInEx/config/.gale-server-state.json").unwrap();
        let mut fresh = ftps_connection(&server).unwrap();
        let actual = fresh
            .read(target.as_path(), state::MAX_STATE_BYTES)
            .unwrap()
            .expect("persisted state must exist");
        assert_eq!(actual.len(), expected.len());
        assert_eq!(blake3::hash(&actual), blake3::hash(&expected));
        assert_eq!(actual, expected);
        assert!(
            server
                .file("/BepInEx/config/.gale-server-state.json.tmp")
                .is_none()
        );
        let auth_tls_count = server
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.as_str() == "AUTH TLS")
            .count();
        assert!(
            auth_tls_count >= 3,
            "write, temporary verification, final verification, and test read must use fresh FTPS sessions"
        );
    }

    /// A persistent config policy set under the lease survives a full
    /// reconnect — decisions are part of the durable state.
    #[test]
    fn a_config_policy_persists_across_reconnects_over_ftp() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options::default());
        let addr = server.addr;
        let path = config_path("BepInEx/config/test.cfg");

        let mut session = open_ftp(addr).unwrap();
        set_config_policy(
            &mut session,
            &path,
            ConfigUpdatePolicy::AlwaysKeep,
            None,
            &meta(),
        )
        .unwrap();
        drop(session);

        let second = open_ftp(addr).unwrap();
        assert_eq!(
            second.state.config.get(&path).map(|r| r.policy),
            Some(ConfigUpdatePolicy::AlwaysKeep)
        );
    }

    #[test]
    fn preview_reports_multifile_hashing_and_config_reads_after_a_reconnect() {
        use crate::profile::server::progress::{ProgressStatus, SyncPhase};

        let memory = remote();
        let mut fixture = mod_fixture();
        let mut desired = DesiredDeployment::default();
        let mut state = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        for index in 0..4 {
            let path = deploy_path(&format!("BepInEx/plugins/Author-ModA/file-{index:02}.dll"));
            let bytes = format!("payload-{index:02}");
            desired
                .payload
                .insert(path.clone(), staged(bytes.as_bytes()));
            state.files.insert(
                path.clone(),
                OwnedFile {
                    hash: blake3::hash(bytes.as_bytes()).to_hex().to_string(),
                    size: bytes.len() as u64,
                },
            );
            memory
                .lock()
                .unwrap()
                .put_file(&format!("{BASE}/{path}"), bytes.as_bytes());
        }
        for index in 0..2 {
            let path = config_path(&format!("BepInEx/config/file-{index:02}.cfg"));
            let bytes = format!("config-{index:02}");
            fixture
                .config
                .insert(path.clone(), config_file(bytes.as_bytes()));
            memory
                .lock()
                .unwrap()
                .put_file(&format!("{BASE}/{path}"), bytes.as_bytes());
        }
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
            remote
                .fail_read_once
                .insert(format!("{BASE}/BepInEx/plugins/Author-ModA/file-01.dll"));
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut progress = ProgressReporter::new(
            "preview-run".to_owned(),
            SyncOperation::Preview,
            &selection(true, true),
            move |snapshot| sink.lock().unwrap().push(snapshot),
        );
        let mut session = open(memory).unwrap();
        let result = preview_with_progress(
            &mut session,
            &fixture.publication(),
            &desired,
            &selection(true, true),
            &context(),
            &meta(),
            &mut progress,
        );
        assert!(result.is_ok(), "{result:?}");
        progress.succeeded();

        let events = events.lock().unwrap();
        let first = |phase| {
            events
                .iter()
                .position(|event| event.phase == phase)
                .unwrap()
        };
        assert!(first(SyncPhase::VerifyingPayload) < first(SyncPhase::CheckingConfigs));
        for (phase, total) in [
            (SyncPhase::VerifyingPayload, 4),
            (SyncPhase::CheckingConfigs, 2),
        ] {
            let updates: Vec<_> = events
                .iter()
                .filter(|event| event.phase == phase && event.total == Some(total))
                .collect();
            assert!(
                updates
                    .windows(2)
                    .all(|pair| pair[0].completed <= pair[1].completed)
            );
            let mut completions: Vec<_> = updates.iter().map(|event| event.completed).collect();
            completions.dedup();
            assert_eq!(completions, (0..=total).collect::<Vec<_>>());
        }
        assert_eq!(events.last().unwrap().status, ProgressStatus::Succeeded);
        assert_eq!(
            events.last().unwrap().completed_phases,
            events.last().unwrap().total_phases
        );
    }

    #[test]
    fn failed_preview_keeps_the_hashing_phase_and_last_path() {
        use crate::profile::server::progress::{ProgressStatus, SyncPhase};

        let memory = remote();
        let path = deploy_path("BepInEx/plugins/Author-ModA/broken.dll");
        let bytes = b"owned payload";
        let mut desired = DesiredDeployment::default();
        desired.payload.insert(path.clone(), staged(bytes));
        let mut state = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        state.files.insert(
            path.clone(),
            OwnedFile {
                hash: blake3::hash(bytes).to_hex().to_string(),
                size: bytes.len() as u64,
            },
        );
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file(&format!("{BASE}/{path}"), bytes);
            remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
            remote.fail_read_always.insert(format!("{BASE}/{path}"));
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut progress = ProgressReporter::new(
            "failed-run".to_owned(),
            SyncOperation::Preview,
            &selection(true, false),
            move |snapshot| sink.lock().unwrap().push(snapshot),
        );
        let mut session = open(memory).unwrap();
        assert!(
            preview_with_progress(
                &mut session,
                &mod_fixture().publication(),
                &desired,
                &selection(true, false),
                &context(),
                &meta(),
                &mut progress,
            )
            .is_err()
        );
        progress.failed();
        let events = events.lock().unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.status, ProgressStatus::Failed);
        assert_eq!(last.phase, SyncPhase::VerifyingPayload);
        assert_eq!(last.item.as_deref(), Some(path.as_str()));
        assert_eq!(last.completed, 0);
        assert_eq!(last.total, Some(1));
    }

    #[tokio::test]
    async fn deploy_reports_revalidation_mutations_bytes_and_completion() {
        use crate::profile::server::progress::{ProgressStatus, SyncPhase};

        let memory = remote();
        let old = deploy_path("BepInEx/plugins/Old/old.dll");
        let mut state = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        state.files.insert(
            old.clone(),
            OwnedFile {
                hash: blake3::hash(b"old").to_hex().to_string(),
                size: 3,
            },
        );
        {
            let mut remote = memory.lock().unwrap();
            remote.put_file(&format!("{BASE}/{old}"), b"old");
            remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
        }
        let mut desired = DesiredDeployment::default();
        for index in 0..4 {
            let path = deploy_path(&format!("BepInEx/plugins/New/file-{index}.dll"));
            desired
                .payload
                .insert(path, staged(&vec![index as u8; (index + 1) * 1024]));
        }
        let config = config_path("BepInEx/config/published.cfg");
        let mut fixture = mod_fixture();
        fixture
            .config
            .insert(config.clone(), config_file(b"published"));
        let mut selected = selection(true, true);
        selected.apply_configs.push(config);
        let mut preview_session = open(memory.clone()).unwrap();
        let approved = preview(
            &mut preview_session,
            &fixture.publication(),
            &desired,
            &selected,
            &context(),
            &meta(),
        )
        .unwrap();
        let mut session = open(memory.clone()).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut progress = ProgressReporter::new(
            "deploy-run".to_owned(),
            SyncOperation::Deploy,
            &selected,
            move |snapshot| sink.lock().unwrap().push(snapshot),
        );
        let operation = meta();
        let deployment = deploy_with_progress(
            &mut session,
            no_connect,
            &fixture.publication(),
            &desired,
            &selected,
            &context(),
            &operation,
            Some(&approved.plan.hash),
            false,
            &mut progress,
            |_| {},
        )
        .unwrap();
        let host = host::from_settings(&Default::default(), None);
        complete_with_progress(
            session,
            deployment,
            host.as_ref(),
            RestartPolicy::Manual,
            operation,
            &mut progress,
        )
        .await
        .unwrap();

        let events = events.lock().unwrap();
        let position = |phase| {
            events
                .iter()
                .position(|event| event.phase == phase)
                .unwrap()
        };
        assert!(position(SyncPhase::VerifyingPayload) < position(SyncPhase::RemovingFiles));
        assert!(position(SyncPhase::CheckingConfigs) < position(SyncPhase::RemovingFiles));
        assert!(position(SyncPhase::RemovingFiles) < position(SyncPhase::UploadingPayload));
        assert!(position(SyncPhase::UploadingPayload) < position(SyncPhase::PersistingState));
        assert!(position(SyncPhase::WritingConfigs) < position(SyncPhase::PersistingState));
        assert!(position(SyncPhase::PersistingState) < position(SyncPhase::ApplyingRestart));
        let completed = |phase| {
            events
                .iter()
                .filter(|event| event.phase == phase)
                .last()
                .unwrap()
        };
        let removal = completed(SyncPhase::RemovingFiles);
        assert_eq!(removal.completed, removal.total.unwrap());
        assert!(removal.completed >= 1);
        let upload = completed(SyncPhase::UploadingPayload);
        assert_eq!(upload.completed, 4);
        assert_eq!(upload.completed_bytes, Some(10 * 1024));
        assert_eq!(upload.total_bytes, Some(10 * 1024));
        assert_eq!(completed(SyncPhase::WritingConfigs).completed, 1);
        assert_eq!(events.last().unwrap().status, ProgressStatus::Succeeded);
    }

    #[tokio::test(start_paused = true)]
    async fn restart_verification_requires_evidence_and_stops_promptly() {
        use std::collections::VecDeque;

        use crate::profile::server::host::BoxFuture;

        struct RestartHost {
            statuses: Mutex<VecDeque<Result<HostStatus>>>,
            last: HostStatus,
            probes: Mutex<usize>,
            fail_restart: bool,
        }

        impl HostControl for RestartHost {
            fn can_restart(&self) -> bool {
                true
            }

            fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    if self.fail_restart {
                        bail!("restart rejected")
                    }
                    Ok(())
                })
            }

            fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
                Box::pin(async move {
                    *self.probes.lock().unwrap() += 1;
                    self.statuses
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| Ok(self.last.clone()))
                })
            }

            fn name(&self) -> &'static str {
                "test host"
            }
        }

        let status = |running, booting| HostStatus {
            running: Some(running),
            booting,
            players: None,
        };
        let before = status(true, Some(false));
        for (name, statuses, last, fail_restart, expected, probes) in [
            (
                "dathost reboot",
                vec![
                    Ok(before.clone()),
                    Ok(status(true, Some(true))),
                    Ok(status(true, Some(false))),
                ],
                before.clone(),
                false,
                RestartOutcome::Restarted,
                3,
            ),
            (
                "generic stop/start",
                vec![
                    Ok(status(true, None)),
                    Ok(status(false, None)),
                    Ok(status(true, None)),
                ],
                status(true, None),
                false,
                RestartOutcome::Restarted,
                3,
            ),
            (
                "on alone",
                vec![Ok(before.clone())],
                before.clone(),
                false,
                RestartOutcome::StartupUnverified,
                13,
            ),
            (
                "unknown booting",
                vec![Ok(status(true, None)), Ok(status(true, Some(true)))],
                status(true, None),
                false,
                RestartOutcome::StartupUnverified,
                13,
            ),
            (
                "status failure",
                vec![Ok(before.clone()), Err(eyre::eyre!("status failed"))],
                before.clone(),
                false,
                RestartOutcome::StartupUnverified,
                2,
            ),
            (
                "stuck booting",
                vec![Ok(before.clone())],
                status(true, Some(true)),
                false,
                RestartOutcome::StartupUnverified,
                13,
            ),
            (
                "restart failure",
                vec![Ok(before.clone())],
                before.clone(),
                true,
                RestartOutcome::Failed,
                1,
            ),
        ] {
            let host = RestartHost {
                statuses: Mutex::new(VecDeque::from(statuses)),
                last,
                probes: Mutex::new(0),
                fail_restart,
            };
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            let mut progress = ProgressReporter::new(
                "restart-run".to_owned(),
                SyncOperation::Deploy,
                &selection(true, true),
                move |snapshot| sink.lock().unwrap().push(snapshot),
            );
            progress.phase(SyncPhase::ApplyingRestart);
            let result = apply_restart_policy_reporting(
                &host,
                RestartPolicy::Immediate,
                true,
                Some(&mut progress),
            )
            .await;
            assert_eq!(result, expected, "{name}");
            assert_eq!(*host.probes.lock().unwrap(), probes, "{name}");
            if name == "dathost reboot" {
                assert!(events.lock().unwrap().iter().any(|event| {
                    event.item.as_deref()
                        == Some("Waiting for server to finish booting (check 2 of 12)")
                }));
            }
        }
    }

    // --- remote accessors through the shared handle ---

    fn remote_has_dir(remote: &Shared, path: &str) -> bool {
        remote.lock().unwrap().dirs.contains(path)
    }

    fn remote_contents(remote: &Shared, path: &str) -> Option<Vec<u8>> {
        remote.lock().unwrap().files.get(path).cloned()
    }
}
