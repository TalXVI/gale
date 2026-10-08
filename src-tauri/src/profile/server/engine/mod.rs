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

mod execute;
mod layout;
mod restart;
mod session;
mod snapshot;
#[cfg(test)]
mod tests;

use std::{thread, time::Duration};

use chrono::{DateTime, Utc};
use eyre::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub(super) use self::restart::apply_restart_policy_reporting;
pub use self::session::{Session, open_session};
use self::{
    execute::execute,
    session::{persist_state, refresh_state},
    snapshot::take_snapshot,
};
use super::{
    host::HostControl,
    lease::{self, Lease, Ownership},
    paths::RemotePathBuf,
    plan::{self, DeploySelection, DeploymentPlan, DesiredDeployment, PlanContext},
    progress::{ProgressReporter, SyncOperation, SyncPhase},
    remote::RemoteOps,
    settings::RestartPolicy,
    spec::DeploymentSpec,
    state::{
        ExecutorKind, OperationKind, OperationRecord, OperationStatus, OperationSummary,
        RestartOutcome, ServerDeploymentState,
    },
};
use crate::profile::{
    export::{ConfigPath, ContentHash},
    sync::{ConfigUpdatePolicy, FetchedPublication},
};

/// Ownership re-checks are cheap (a stat and a directory probe), so a
/// check that cannot complete is retried a few times before the
/// operation aborts with an honest reason rather than a false takeover.
const OWNERSHIP_CHECK_ATTEMPTS: usize = 3;
const OWNERSHIP_RETRY_DELAY: Duration = Duration::from_millis(400);

/// Identifies an operation across the lease record, last operation, and
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

    /// The operation record finishing now, before operation-specific fields.
    fn record(&self, status: OperationStatus, restart: RestartOutcome) -> OperationRecord {
        OperationRecord {
            id: self.id.clone(),
            executor: self.executor,
            kind: self.kind,
            worker_id: self.worker_id.clone(),
            publication_revision: None,
            mods_revision: None,
            status,
            summary: OperationSummary::default(),
            restart,
            error: None,
            started_at: self.started_at,
            finished_at: Utc::now(),
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
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    context: &PlanContext,
    meta: &OperationMeta,
    progress: &mut ProgressReporter,
) -> Result<Preview> {
    progress.phase(SyncPhase::CheckingLease);
    let (held, busy) = match session.acquire_lease(meta, false) {
        Ok(lease) => (Some(lease), None),
        Err(err) => {
            let busy = err.downcast::<lease::LeaseBusy>()?;
            info!(phase = "preview.busy", owner = ?busy.record.as_ref().map(|record| &record.owner), stale = busy.stale, "preview found an existing deployment lease; taking a read-only snapshot");
            (None, Some(busy))
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
        busy,
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

/// Runs the blocking remote preview behind the async process boundary.
/// Both desktop and Worker supply their own authenticated connection.
#[allow(clippy::too_many_arguments)]
pub async fn preview(
    connect: impl Fn() -> Result<Box<dyn RemoteOps>> + Send + 'static,
    spec: DeploymentSpec,
    base: RemotePathBuf,
    publication: FetchedPublication,
    desired: DesiredDeployment,
    selection: DeploySelection,
    context: PlanContext,
    meta: OperationMeta,
    progress: &mut ProgressReporter,
) -> Result<Preview> {
    with_blocking_session(
        connect,
        spec,
        base,
        progress,
        move |mut session, progress| {
            preview_with_progress(
                &mut session,
                &publication,
                &desired,
                &selection,
                &context,
                &meta,
                progress,
            )
        },
    )
    .await
}

/// Opens a session and runs blocking remote work on it behind the async
/// boundary. The caller's reporter moves into the blocking task and comes
/// back afterwards, so progress keeps flowing to the same sink.
async fn with_blocking_session<T: Send + 'static>(
    connect: impl FnOnce() -> Result<Box<dyn RemoteOps>> + Send + 'static,
    spec: DeploymentSpec,
    base: RemotePathBuf,
    progress: &mut ProgressReporter,
    work: impl FnOnce(Session, &mut ProgressReporter) -> Result<T> + Send + 'static,
) -> Result<T> {
    let placeholder = ProgressReporter::silent(SyncOperation::Preview, &DeploySelection::default());
    let mut moved = std::mem::replace(progress, placeholder);
    let (result, returned) = tokio::task::spawn_blocking(move || {
        let result = (|| {
            moved.phase(SyncPhase::Connecting);
            let ops = connect()?;
            moved.phase(SyncPhase::ReadingState);
            work(open_session(ops, &spec, base)?, &mut moved)
        })();
        (result, moved)
    })
    .await
    .context("remote operation task failed")?;
    *progress = returned;
    result
}

/// Runs the blocking file phase, then the async restart, while retaining
/// the same remote lease throughout both phases.
#[allow(clippy::too_many_arguments)]
pub async fn deploy(
    connect: impl Fn() -> Result<Box<dyn RemoteOps>> + Clone + Send + 'static,
    spec: DeploymentSpec,
    base: RemotePathBuf,
    publication: FetchedPublication,
    desired: DesiredDeployment,
    selection: DeploySelection,
    context: PlanContext,
    meta: OperationMeta,
    expected_plan_hash: Option<String>,
    force: bool,
    host: &dyn HostControl,
    policy: RestartPolicy,
    progress: &mut ProgressReporter,
) -> Result<DeploymentResult> {
    let heartbeat = connect.clone();
    let final_meta = meta.clone();
    let (session, deployment) = with_blocking_session(
        connect,
        spec,
        base,
        progress,
        move |mut session, progress| {
            let deployment = deploy_with_progress(
                &mut session,
                heartbeat,
                &publication,
                &desired,
                &selection,
                &context,
                &meta,
                expected_plan_hash.as_deref(),
                force,
                progress,
            )?;
            Ok((session, deployment))
        },
    )
    .await?;
    complete_with_progress(session, deployment, host, policy, final_meta, progress).await
}

/// Completes the restart and records the result before releasing the lease.
pub async fn complete_with_progress(
    mut session: Session,
    deployment: Deployment,
    host: &dyn HostControl,
    policy: RestartPolicy,
    meta: OperationMeta,
    progress: &mut ProgressReporter,
) -> Result<DeploymentResult> {
    progress.phase(SyncPhase::ApplyingRestart);
    let report =
        apply_restart_policy_reporting(host, policy, session.state.restart_required, progress)
            .await;
    let restart = report.outcome;
    let plan = deployment.plan.clone();
    let summary = deployment.summary.clone();
    let mut warnings = deployment.warnings.clone();
    warnings.extend(report.warning);
    let failed_config_writes = deployment.failed_config_writes.clone();
    progress.phase(SyncPhase::ReleasingLease);
    let state =
        tokio::task::spawn_blocking(move || finish(&mut session, deployment, restart, &meta))
            .await
            .context("deployment finalization task failed")??;

    progress.succeeded();

    Ok(DeploymentResult {
        plan,
        summary,
        warnings,
        failed_config_writes,
        restart,
        state,
    })
}

/// Rechecks a deferred restart without staging or deploying files. The
/// operation that requested it must still be authoritative under the lease.
#[cfg(any(feature = "worker", test))]
pub async fn resume_deferred_restart(
    session: Session,
    connect: impl FnMut() -> Result<Box<dyn RemoteOps>> + Send + 'static,
    expected_operation_id: String,
    host: &dyn HostControl,
    meta: OperationMeta,
    progress: &mut ProgressReporter,
) -> Result<ServerDeploymentState> {
    progress.phase(SyncPhase::CheckingLease);
    let (mut session, lease) = tokio::task::spawn_blocking(move || {
        let mut session = session;
        let mut lease = session.acquire_lease(&meta, false)?;
        let prepared: Result<bool> = (|| {
            refresh_state(&mut session)?;
            let pending = session.state.restart_required
                && session
                    .state
                    .last_operation
                    .as_ref()
                    .is_some_and(|operation| {
                        operation.id == expected_operation_id
                            && operation.restart == RestartOutcome::AwaitingEmpty
                    });
            if pending {
                ensure_ownership(&lease, &mut session)?;
            }
            Ok(pending)
        })();
        match prepared {
            Ok(true) => {
                lease::start_heartbeat(&mut lease, connect);
                Ok((session, Some(lease)))
            }
            other => {
                lease.release(session.ops.as_mut());
                other.map(|_| (session, None))
            }
        }
    })
    .await
    .context("deferred restart preparation task failed")??;

    let Some(mut lease) = lease else {
        progress.succeeded();
        return Ok(session.state);
    };
    progress.phase(SyncPhase::ApplyingRestart);
    let report = if !host.can_restart() {
        RestartOutcome::AwaitingManual.into()
    } else if let Some(before) = restart::empty_server_status(host, progress).await {
        // Persist intent before the request. A crash or a failed final state
        // write must not leave AwaitingEmpty eligible to restart again.
        let (prepared_session, held) = tokio::task::spawn_blocking(move || {
            let prepared = (|| {
                ensure_ownership(&lease, &mut session)?;
                let mut record = session
                    .state
                    .last_operation
                    .clone()
                    .ok_or_else(|| eyre::eyre!("deferred restart operation is missing"))?;
                record.restart = RestartOutcome::StartupUnverified;
                session.state.record_operation(record);
                persist_state(&mut session)
            })();
            if let Err(error) = prepared {
                lease.release(session.ops.as_mut());
                return Err(error);
            }
            Ok((session, lease))
        })
        .await
        .context("deferred restart intent task failed")??;
        session = prepared_session;
        lease = held;
        restart::do_restart(host, Some(before), progress).await
    } else {
        RestartOutcome::AwaitingEmpty.into()
    };
    progress.phase(SyncPhase::ReleasingLease);
    let state = tokio::task::spawn_blocking(move || {
        let result: Result<ServerDeploymentState> = (|| {
            ensure_ownership(&lease, &mut session)?;
            if report.outcome != RestartOutcome::AwaitingEmpty {
                let mut record = session
                    .state
                    .last_operation
                    .take()
                    .ok_or_else(|| eyre::eyre!("deferred restart operation is missing"))?;
                record.restart = report.outcome;
                if let Some(warning) = report.warning {
                    record.error = Some(match record.error {
                        Some(error) => format!("{error}; {warning}"),
                        None => warning,
                    });
                }
                session.state.restart_required = report.outcome != RestartOutcome::Restarted;
                session.state.record_operation(record);
                persist_state(&mut session)?;
            }
            Ok(session.state.clone())
        })();
        lease.release(session.ops.as_mut());
        result
    })
    .await
    .context("deferred restart finalization task failed")??;
    progress.succeeded();
    Ok(state)
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
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    context: &PlanContext,
    meta: &OperationMeta,
    expected_plan_hash: Option<&str>,
    force: bool,
    progress: &mut ProgressReporter,
) -> Result<Deployment> {
    progress.phase(SyncPhase::CheckingLease);
    let mut lease = session.acquire_lease(meta, force)?;
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
            return Err(plan::StalePlan.into());
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

    let (summary, mut warnings, failed_config_writes) =
        match execute(session, &plan, desired, publication, &lease, progress) {
            Ok(outcome) => outcome,
            Err(error) => {
                warn!(%error, "deployment failed; recording accurate state");
                return Err(abort(
                    session,
                    lease,
                    meta,
                    &plan,
                    error,
                    "deployment failed during the file phase",
                ));
            }
        };
    if lease.is_lost() {
        warnings.push(
            "the deployment lease was lost during execution; another executor may have modified the server"
                .to_owned(),
        );
    }

    // Persist the deployment state after the file phase, before the caller
    // decides on a restart. A crash here still leaves correct ownership and
    // revision records. Both guards fail closed: losing the lease or seeing
    // a newer remote state aborts the operation.
    progress.phase(SyncPhase::PersistingState);
    if let Err(error) = ensure_ownership(&lease, session).and_then(|_| persist_state(session)) {
        return Err(abort(
            session,
            lease,
            meta,
            &plan,
            error,
            "failed to persist remote deployment state after file changes",
        ));
    }

    Ok(Deployment {
        plan,
        summary,
        warnings,
        failed_config_writes,
        lease,
    })
}

/// Records the failed operation with whatever state is accurate, releases
/// the lease, and returns `error` annotated with the recording outcome.
fn abort(
    session: &mut Session,
    lease: Lease,
    meta: &OperationMeta,
    plan: &DeploymentPlan,
    error: eyre::Report,
    context: &str,
) -> eyre::Report {
    let recorded = ensure_ownership(&lease, session).and_then(|_| {
        session.state.record_operation(OperationRecord {
            publication_revision: Some(plan.publication_revision),
            mods_revision: plan.mods_phase.then(|| plan.mods_revision.clone()),
            error: Some(error.to_string()),
            ..meta.record(OperationStatus::Failed, RestartOutcome::NotRequired)
        });
        persist_state(session)
    });
    lease.release(session.ops.as_mut());
    let unrecorded = recorded
        .err()
        .map(|err| format!("; failure record could not be persisted: {err:#}"))
        .unwrap_or_default();
    error.wrap_err(format!(
        "{context}; remote filesystem may need reconciliation{unrecorded}"
    ))
}

/// Fails the operation when the lease verifiably no longer belongs to
/// this executor. A foreign owner means another deployment may be
/// mutating the server, so this one must stop before its next phase
/// rather than interleave its writes with the new owner's. A check that
/// cannot complete is retried and then aborts with the actual reason.
/// An unreadable lease record is not proof of a takeover.
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
                // host will not return the lease record on reads. Report
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
                    bail!(
                        "the deployment lease was taken over by {owner}; aborting this operation"
                    );
                }
                None => {
                    bail!(
                        "the deployment lease was removed by another executor; aborting this operation"
                    );
                }
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

    // A deployment with failed writes is not a success: it is Partial, with
    // the per-file records describing exactly what landed.
    let failed_writes = deployment.failed_config_writes.len();
    let status = if failed_writes == 0 {
        OperationStatus::Succeeded
    } else {
        OperationStatus::Partial
    };
    session.state.record_operation(OperationRecord {
        publication_revision: Some(deployment.plan.publication_revision),
        mods_revision: deployment
            .plan
            .mods_phase
            .then(|| deployment.plan.mods_revision.clone()),
        summary: deployment.summary.clone(),
        error: (failed_writes > 0)
            .then(|| format!("{failed_writes} selected config file(s) could not be written")),
        ..meta.record(status, restart)
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
    let lease = session.acquire_lease(meta, false)?;
    let result = (|| {
        refresh_state(session)?;
        ensure!(
            session.state.restart_required,
            "there is no outstanding restart to acknowledge"
        );
        ensure_ownership(&lease, session)?;
        session.state.restart_required = false;
        session.state.record_operation(OperationRecord {
            mods_revision: session.state.mods_revision.clone(),
            ..meta.record(OperationStatus::Succeeded, RestartOutcome::Restarted)
        });
        persist_state(session)?;
        Ok(session.state.clone())
    })();
    lease.release(session.ops.as_mut());
    result
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
    let lease = session.acquire_lease(meta, false)?;

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
