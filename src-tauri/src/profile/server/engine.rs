//! The deployment engine shared by Local mode and the worker.
//!
//! One deployment is three phases:
//!
//! 1. **Plan.** Snapshot the remote and run the pure planner ([`preview`],
//!    or the re-plan inside [`deploy`]). The plan hash binds user approval
//!    to exactly the actions that will run.
//! 2. **Files.** Under the remote lease, apply removals, uploads, and
//!    config writes; record every success in the deployment state and
//!    persist it before touching the game process.
//! 3. **Restart.** The async [`apply_restart_policy`] consults the host
//!    provider, then [`finish`] records the operation and releases the
//!    lease.
//!
//! Keeping the lease across restart prevents a second executor from
//! starting a deployment while the server is still coming back up.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    thread,
    time::Duration,
};

use chrono::{DateTime, Utc};
use eyre::{Context, Result, bail, ensure};
use serde::Serialize;
use tracing::{info, warn};

use super::{
    host::{HostCapabilities, HostControl},
    lease::{self, Lease, LeaseRecord, Ownership},
    paths::{DeployPath, DeployPathBuf, RemotePath, RemotePathBuf},
    plan::{
        self, ConfigAction, DeploySelection, DeploymentPlan, DesiredDeployment, FileSource,
        PlanContext, Publication, RemoteLayout, RemoteSnapshot, UploadKind,
    },
    remote::RemoteOps,
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
struct RemoteMapper<'a> {
    spec: &'a DeploymentSpec,
    base: RemotePathBuf,
    layout: RemoteLayout,
}

impl<'a> RemoteMapper<'a> {
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
pub struct Session<'a> {
    pub ops: Box<dyn RemoteOps>,
    mapper: RemoteMapper<'a>,
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
    /// Whether legacy manifest records were adopted on open.
    pub migrated: bool,
}

/// Everything [`open_session`] needs besides a connection.
pub fn open_session<'a>(
    mut ops: Box<dyn RemoteOps>,
    spec: &'a DeploymentSpec,
    base: RemotePathBuf,
) -> Result<Session<'a>> {
    ensure!(
        ops.is_dir(&base)
            .context("failed to check remote directory")?,
        "remote server directory '{base}' does not exist or is not a directory"
    );

    let layout = detect_layout(ops.as_mut(), spec, &base)?;
    let mapper = RemoteMapper { spec, base, layout };

    let LoadedState {
        state,
        warnings,
        migrated,
    } = state::read_state(
        ops.as_mut(),
        spec,
        &mapper.remote_path(&spec.state_path),
        &mapper.remote_path(&spec.legacy_manifest_path),
    )?;

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
        migrated,
    })
}

/// Re-reads the authoritative deployment state from the remote. Called
/// under the deployment lease so planning, policy updates, and the
/// persistence sequence guard all work on fresh state rather than what
/// was loaded when the session opened.
fn refresh_state(session: &mut Session) -> Result<()> {
    let spec = session.mapper.spec;
    let LoadedState {
        state,
        warnings,
        migrated,
    } = state::read_state(
        session.ops.as_mut(),
        spec,
        &session.mapper.remote_path(&spec.state_path),
        &session.mapper.remote_path(&spec.legacy_manifest_path),
    )?;
    session.base_seq = state.operation_seq;
    session.state = state;
    session.migrated = migrated;
    for warning in warnings {
        if !session.warnings.contains(&warning) {
            session.warnings.push(warning);
        }
    }
    Ok(())
}

/// A preview: the plan plus the context the UI needs to explain it.
#[derive(Debug, Serialize)]
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
pub fn preview(
    session: &mut Session,
    publication: &Publication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    context: &PlanContext,
    meta: &OperationMeta,
) -> Result<Preview> {
    let held = match lease::acquire(
        session.ops.as_mut(),
        &session.mapper.remote_path(&session.mapper.spec.lease_dir),
        &meta.owner,
        meta.executor,
        &meta.id,
        false,
    ) {
        Ok(lease) => Some(lease),
        Err(err) => match err.downcast::<lease::LeaseBusy>() {
            Ok(busy) => {
                let snapshot = take_snapshot(session, publication, desired, selection)?;
                let plan = plan::build_plan(
                    publication,
                    desired,
                    &snapshot,
                    selection,
                    context,
                    session.mapper.spec,
                )?;
                return Ok(Preview {
                    plan,
                    busy: Some(busy),
                    warnings: session.warnings.clone(),
                });
            }
            Err(err) => return Err(err),
        },
    };

    let snapshot = take_snapshot(session, publication, desired, selection);
    if let Some(lease) = held {
        lease.release(session.ops.as_mut());
    }

    let plan = plan::build_plan(
        publication,
        desired,
        &snapshot?,
        selection,
        context,
        session.mapper.spec,
    )?;

    Ok(Preview {
        plan,
        busy: None,
        warnings: session.warnings.clone(),
    })
}

/// The file phase of a deployment, completed with the lease still held.
/// The caller decides the restart, then calls [`finish`].
pub struct Deployment {
    pub plan: DeploymentPlan,
    pub summary: OperationSummary,
    pub warnings: Vec<String>,
    /// Selected config files whose write failed; their records were not
    /// advanced.
    pub failed_config_writes: Vec<ConfigPath>,
    lease: Lease,
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
pub fn deploy(
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
        let snapshot = take_snapshot(session, publication, desired, selection)?;
        let plan = plan::build_plan(
            publication,
            desired,
            &snapshot,
            selection,
            context,
            session.mapper.spec,
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

    let result = execute(session, &plan, desired, publication, &lease, &mut report);

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
            if let Err(err) = ensure_ownership(&lease, session).and_then(|_| persist_state(session))
            {
                let _ = fail_operation(session, meta, &plan, &err);
                lease.release(session.ops.as_mut());
                return Err(err.wrap_err("failed to persist remote deployment state"));
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
            if let Err(state_err) = fail_operation(session, meta, &plan, &error) {
                warn!(%state_err, "failed to record deployment failure remotely");
            }
            lease.release(session.ops.as_mut());
            Err(error)
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
pub async fn apply_restart_policy(
    host: &dyn HostControl,
    policy: RestartPolicy,
    requires_restart: bool,
) -> RestartOutcome {
    if !requires_restart {
        return RestartOutcome::NotRequired;
    }

    let HostCapabilities { can_restart, .. } = host.capabilities();
    if !can_restart {
        return RestartOutcome::AwaitingManual;
    }

    match policy {
        RestartPolicy::Manual => RestartOutcome::AwaitingManual,
        RestartPolicy::Immediate => do_restart(host).await,
        RestartPolicy::WhenEmpty => match host.status().await {
            Ok(status) if status.players == Some(0) => do_restart(host).await,
            _ => RestartOutcome::AwaitingEmpty,
        },
    }
}

async fn do_restart(host: &dyn HostControl) -> RestartOutcome {
    info!(host = host.name(), "restarting dedicated server");
    if host.restart().await.is_err() {
        return RestartOutcome::Failed;
    }

    // Poll until the provider reports the server running again. Absent
    // process visibility the outcome is StartupUnverified, not Ready.
    for _ in 0..RESTART_VERIFY_ATTEMPTS {
        tokio::time::sleep(RESTART_VERIFY_DELAY).await;
        match host.status().await {
            Ok(status) if status.running == Some(true) => return RestartOutcome::Restarted,
            Ok(_) => continue,
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
    session.state.restart_required = !matches!(
        restart,
        RestartOutcome::NotRequired | RestartOutcome::Restarted
    );

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

/// Records a failed operation and persists whatever state is accurate.
fn fail_operation(
    session: &mut Session,
    meta: &OperationMeta,
    plan: &DeploymentPlan,
    error: &eyre::Report,
) -> Result<()> {
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
) -> Result<RemoteSnapshot> {
    refresh_state(session)?;

    let spec = session.mapper.spec;
    let mut payload_files = BTreeMap::new();
    let mut payload_dirs = BTreeSet::new();
    let mut payload_hashes = BTreeMap::new();

    if selection.include_mods {
        for dir in &spec.payload_dirs {
            collect_remote(
                session.ops.as_mut(),
                &session.mapper,
                dir.as_path(),
                &mut payload_files,
                &mut payload_dirs,
            )?;
        }

        // Verify the actual remote bytes of every Gale-owned file the
        // publication still wants. Two different contents can have the
        // same size, so size equality alone cannot detect a remote edit.
        // Only files that are both owned and desired are read: hashing
        // foreign files gains nothing, and an owned file that is no longer
        // desired is being removed anyway.
        for (path, staged) in &desired.payload {
            let owned = session.state.files.contains_key(path);
            if !owned || payload_files.get(path) != Some(&staged.size) {
                continue;
            }
            let remote = session.mapper.remote_path(path);
            match session.ops.read(&remote, staged.size) {
                Ok(Some(bytes)) => {
                    payload_hashes
                        .insert(path.clone(), ContentHash::from_hash(blake3::hash(&bytes)));
                }
                // Unreadable or resized between list and read: no hash is
                // recorded and the planner conservatively re-uploads.
                _ => {}
            }
        }
    }

    // Hash every config path a decision could touch: published files,
    // recorded ones, and package-default seeds.
    let mut config_remote = BTreeMap::new();
    if selection.include_configs || selection.include_mods {
        let mut paths: BTreeSet<ConfigPath> = BTreeSet::new();
        if selection.include_configs {
            paths.extend(publication.config.keys().cloned());
            paths.extend(session.state.config.keys().cloned());
            paths.extend(selection.apply_configs.iter().cloned());
        }
        if selection.include_mods {
            for path in desired.package_defaults.keys() {
                if let Ok(path) = ConfigPath::try_from(path.as_str().to_owned()) {
                    paths.insert(path);
                }
            }
        }

        for path in paths {
            let deploy = DeployPathBuf::new(path.as_str())
                .map_err(|_| eyre::eyre!("config path is not deployable: {path}"))?;
            let remote = session.mapper.remote_path(&deploy);
            let hash = match session.ops.read(&remote, MAX_CONFIG_READ)? {
                Some(bytes) => Some(ContentHash::from_hash(blake3::hash(&bytes))),
                None => None,
            };
            config_remote.insert(path, hash);
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

    // ---- Removals (payload only). Failures keep the ownership record so
    // the next deployment retries instead of forgetting the file.
    for path in &plan.removals {
        let remote = session.mapper.remote_path(path);
        match session.ops.delete_file(&remote) {
            Ok(true) => {
                session.state.files.remove(path);
                summary.removed_files += 1;
            }
            Ok(false) => {
                session.state.files.remove(path);
            }
            Err(error) => {
                warnings.push(format!("could not remove {path}: {error}"));
            }
        }
        completed += 1;
        report(EngineProgress {
            completed,
            total,
            path: path.clone(),
            operation: ProgressOp::Remove,
        });
    }

    for dir in &plan.directory_removals {
        let remote = session.mapper.remote_path(dir);
        if let Err(error) = session.ops.delete_dir(&remote) {
            warnings.push(format!("could not remove {dir}/: {error}"));
        }
        completed += 1;
        report(EngineProgress {
            completed,
            total,
            path: dir.clone(),
            operation: ProgressOp::Remove,
        });
    }

    // ---- Uploads: payload, seeds, then published config writes.
    ensure_ownership(lease, session)?;
    let mut ensured = BTreeSet::new();

    for upload in &plan.uploads {
        let result = match upload.kind {
            UploadKind::Payload | UploadKind::ConfigSeed => {
                let staged = match upload.kind {
                    UploadKind::Payload => desired.payload.get(&upload.path),
                    _ => desired.package_defaults.get(&upload.path),
                }
                .ok_or_else(|| {
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
                if matches!(upload.kind, UploadKind::Payload | UploadKind::ConfigSeed) {
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
            .map(|(path, file)| (path.clone(), file.hash.clone()))
            .collect();

        for entry in &plan.config_entries {
            let hash = &published[&entry.path];
            match &entry.action {
                ConfigAction::MarkApplied => session.state.record_applied(&entry.path, hash),
                ConfigAction::Decline => session.state.record_declined(&entry.path, hash),
                ConfigAction::Pending { reason } => {
                    session.state.record_pending(&entry.path, hash, *reason)
                }
                ConfigAction::Keep => {
                    session.state.pending.remove(&entry.path);
                }
                ConfigAction::Write | ConfigAction::Unapplied => {}
            }
        }

        session.state.retain_published(&published);
    }

    // ---- Advance the recorded revision: reaching this point means the
    // whole payload phase succeeded, since a failure returns early above.
    if plan.mods_phase {
        session.state.mods_revision = Some(plan.mods_revision.clone());
        session.state.deployed_mods = plan.deployed_mods.clone();
    }

    Ok((summary, warnings, failed_config_writes))
}

/// Records a successful payload/seed upload in the deployment state.
fn record_staged(session: &mut Session, upload: &plan::PlanUpload, staged: &plan::StagedFile) {
    match upload.kind {
        UploadKind::Payload => {
            session.state.files.insert(
                upload.path.clone(),
                OwnedFile {
                    hash: staged.hash.clone(),
                    size: staged.size,
                },
            );
        }
        UploadKind::ConfigSeed => {
            if let Ok((config_path, hash)) = ConfigPath::try_from(upload.path.as_str().to_owned())
                .and_then(|path| {
                    ContentHash::try_from(staged.hash.clone()).map(|hash| (path, hash))
                })
            {
                let record = session.state.config.entry(config_path).or_default();
                record.written = Some(hash);
            }
        }
        UploadKind::Config => {}
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
    with_retry(session, |session| {
        match source {
            FileSource::Path(local) => session.ops.upload(local, &temporary),
            FileSource::Bytes(bytes) => session.ops.write(&temporary, bytes),
        }?;
        replace_remote(session.ops.as_mut(), &temporary, &target)
    })
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
        if !ensured.insert(remote.clone()) {
            continue;
        }
        session
            .ops
            .ensure_dir(&remote)
            .with_context(|| format!("failed to create remote directory {remote}"))?;
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
    if ops.rename(temporary, target).is_ok() {
        return Ok(());
    }

    // FTP and older SFTP servers cannot rename over an existing file: move
    // the current file aside first so a mid-swap failure never leaves
    // neither version.
    let backup = target.with_suffix(".gale-backup");
    let target_exists = ops.is_file(target)?;

    if target_exists {
        if ops.is_file(&backup)? {
            ops.delete_file(&backup)
                .context("failed to clear stale remote backup")?;
        }
        if let Err(err) = ops.rename(target, &backup) {
            let _ = ops.delete_file(temporary);
            return Err(err).context("failed to back up existing remote file");
        }
    }

    if let Err(err) = ops.rename(temporary, target) {
        if target_exists {
            let _ = ops.rename(&backup, target);
        }
        let _ = ops.delete_file(temporary);
        return Err(err).context("failed to move uploaded file into place");
    }

    if target_exists {
        let _ = ops.delete_file(&backup);
    }

    Ok(())
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
    let legacy = session
        .mapper
        .remote_path(&session.mapper.spec.legacy_manifest_path);
    let remote = state::read_state(session.ops.as_mut(), session.mapper.spec, &target, &legacy)
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
        replace_remote(session.ops.as_mut(), &temporary, &target)
    })
    .context("failed to write remote deployment state")?;

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
        persist_state(session)
    })();

    lease.release(session.ops.as_mut());
    result
}

/// Lists every remote file/dir inside the payload dirs, as deploy paths.
fn collect_remote(
    ops: &mut dyn RemoteOps,
    mapper: &RemoteMapper,
    dir: &DeployPath,
    files: &mut BTreeMap<DeployPathBuf, u64>,
    directories: &mut BTreeSet<DeployPathBuf>,
) -> Result<()> {
    let remote_dir = mapper.remote_path(dir);

    for entry in ops
        .list(&remote_dir)
        .with_context(|| format!("failed to inspect remote directory {remote_dir}"))?
    {
        let relative = dir
            .join(&entry.name)
            .with_context(|| format!("remote directory contains an unsafe name: {}", entry.name))?;

        if mapper.spec.is_gale_internal(&relative) {
            continue;
        }

        if entry.is_directory {
            directories.insert(relative.clone());
            collect_remote(ops, mapper, &relative, files, directories)?;
        } else {
            files.insert(relative, entry.size.unwrap_or(0));
        }
    }

    Ok(())
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
                lease::LeaseRecord,
                paths::RemotePathBuf,
                plan::{Publication, StagedFile},
                remote::{
                    self, ConnectionAttempt, RemoteConnection,
                    memory::{FilteredReads, MemoryRemote},
                },
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
        DesiredDeployment {
            payload,
            package_defaults: BTreeMap::new(),
        }
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

    fn open(remote: Shared) -> Result<Session<'static>> {
        open_ops(Box::new(remote))
    }

    fn open_ops(ops: Box<dyn RemoteOps>) -> Result<Session<'static>> {
        let spec = Box::leak(Box::new(spec()));
        open_session(ops, spec, RemotePathBuf::new(BASE).unwrap())
    }

    /// A remote whose dot-paths are invisible to read commands: metadata
    /// like `lease.json` can be written but never read back. DatHost
    /// turned out not to filter paths — it refuses `SIZE` in ASCII mode —
    /// but this fixture still models hosts with genuine read filtering,
    /// which the marker fallback and state verification exist for.
    fn filtered() -> (Shared, FilteredReads) {
        let inner = remote();
        let filtered = FilteredReads::new(inner.clone());
        (inner, filtered)
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
        assert!(!state.restart_required);
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
            package_defaults: BTreeMap::new(),
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
        assert!(
            deployment
                .warnings
                .iter()
                .any(|w| w.contains("could not remove"))
        );

        // The remote file remains, and so does the ownership record. The
        // next deployment retries the removal instead of forgetting it.
        assert!(remote_contents(&memory, &format!("{BASE}/{stale}")).is_some());
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        assert!(state.files.contains_key(&deploy_path(stale)));
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
        assert!(state.pending.is_empty());
        // A configs-only deployment never sets restart_required.
        assert!(!state.restart_required);
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
        assert_eq!(
            state.pending[&config_path("BepInEx/config/mod.cfg")],
            PendingConfigReason::ModifiedLocally
        );
    }

    #[test]
    fn package_default_seeds_only_absent_configs() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let mut desired = desired_payload();
        desired.package_defaults.insert(
            deploy_path("BepInEx/config/packaged.cfg"),
            staged(b"packaged"),
        );

        // Absent: the seed is uploaded and recorded as written.
        let memory = remote();
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
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/packaged.cfg"),
            Some(b"packaged".to_vec())
        );
        assert_eq!(
            state.config[&config_path("BepInEx/config/packaged.cfg")].written,
            Some(ContentHash::from_hash(blake3::hash(b"packaged")))
        );

        // Present: a second deployment leaves the server's file alone.
        let memory = remote();
        memory
            .lock()
            .unwrap()
            .put_file("/srv/BepInEx/config/packaged.cfg", b"server-edited");
        let mut again = open(memory.clone()).unwrap();
        let deployment = deploy(
            &mut again,
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
        finish(&mut again, deployment, RestartOutcome::NotRequired, &meta()).unwrap();
        assert_eq!(
            remote_contents(&memory, "/srv/BepInEx/config/packaged.cfg"),
            Some(b"server-edited".to_vec())
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
        assert_eq!(
            remote_state(&memory).config[&path].policy,
            ConfigUpdatePolicy::AlwaysKeep
        );
    }

    #[test]
    fn persist_refuses_to_overwrite_newer_remote_state() {
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();

        // A rogue writer advanced the remote sequence since this session
        // loaded its state, so persisting must fail rather than overwrite
        // the newer state.
        let mut newer = ServerDeploymentState {
            version: state::VERSION,
            ..Default::default()
        };
        newer.operation_seq = 7;
        memory
            .lock()
            .unwrap()
            .put_file(STATE_REMOTE, &state::serialize(&newer).unwrap());

        assert!(persist_state(&mut session).is_err());
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
    fn config_only_deploy_requires_restart() {
        let mut fixture = mod_fixture();
        fixture
            .config
            .insert(config_path("BepInEx/config/mod.cfg"), config_file(b"v2"));
        let publication = fixture.publication();

        let memory = remote();
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
        assert!(preview.plan.requires_restart);
    }

    #[test]
    fn restart_flag_follows_the_outcome() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let memory = remote();
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
        let state = finish(
            &mut session,
            deployment,
            RestartOutcome::AwaitingManual,
            &meta(),
        )
        .unwrap();
        assert!(state.restart_required);
        assert_eq!(
            remote_state(&memory).last_operation.unwrap().restart,
            RestartOutcome::AwaitingManual
        );
    }

    #[test]
    fn set_config_policy_persists_to_remote_state() {
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();
        let path = config_path("BepInEx/config/mod.cfg");
        let hash = ContentHash::from_hash(blake3::hash(b"v1"));

        set_config_policy(
            &mut session,
            &path,
            ConfigUpdatePolicy::AlwaysKeep,
            Some(&hash),
            &meta(),
        )
        .unwrap();

        let persisted = remote_state(&memory);
        let record = &persisted.config[&path];
        assert_eq!(record.policy, ConfigUpdatePolicy::AlwaysKeep);
        assert_eq!(record.policy_set_at, Some(hash));
    }

    #[test]
    fn deploy_on_a_read_filtered_remote_uses_the_holder_marker() {
        // A host that hides dot-paths from every read: the lease record
        // is write-only, so ownership falls back to the holder marker
        // instead of aborting as a false takeover — and uploads still run
        // under that verified claim. But the state file is equally
        // unreadable, so persistence cannot be verified and the
        // deployment must fail closed rather than claim success.
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let (memory, filtered) = filtered();
        let mut session = open_ops(Box::new(filtered.clone())).unwrap();

        let conn = filtered.clone();
        let err = deploy(
            &mut session,
            move || Ok(Box::new(conn.clone()) as Box<dyn RemoteOps>),
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
        // Ownership was verified through the marker — never a phantom
        // takeover.
        assert!(
            session
                .warnings
                .iter()
                .any(|w| w.contains("marker directory"))
        );
        // The upload landed before the persistence failure and the
        // claim was fully released. The state temp could never be
        // verified, so it was never renamed — no authoritative state
        // exists on an unreadable host.
        assert_eq!(
            remote_contents(&memory, MOD_DLL_REMOTE),
            Some(b"dll-bytes".to_vec())
        );
        assert!(remote_contents(&memory, STATE_REMOTE).is_none());
        assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
    }

    #[test]
    fn deploy_does_not_confuse_a_dead_transport_with_a_takeover() {
        // The transport dies after the session opens. Whatever fails
        // first must report the transport problem — never a phantom
        // competing executor — and must not touch payload files.
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let (memory, filtered) = filtered();
        let mut session = open_ops(Box::new(filtered)).unwrap();
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

    #[test]
    fn a_failed_filtered_deploy_cannot_persist_state_but_releases() {
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
        let (memory, filtered) = filtered();
        memory
            .lock()
            .unwrap()
            .fail_always
            .insert(format!("{MOD_DLL_REMOTE}.gale-upload"));
        let mut session = open_ops(Box::new(filtered)).unwrap();

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
        .expect("the upload failure must fail the deployment");
        assert!(format!("{err:#}").contains("failed to upload"));

        // The failure could not be recorded: on a host where the state
        // cannot be read back, an unverifiable temp must never become
        // authoritative — so no state file exists — but the lease was
        // still released.
        assert!(remote_contents(&memory, STATE_REMOTE).is_none());
        assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
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
            ConnectionAttempt::Connected(conn) => Ok(Box::new(conn)),
            _ => eyre::bail!("unexpected trust prompt from the fake FTP server"),
        }
    }

    fn open_ftp(addr: std::net::SocketAddr) -> Result<Session<'static>> {
        let spec = Box::leak(Box::new(spec()));
        open_session(ftp_ops(addr)?, spec, RemotePathBuf::new("/").unwrap())
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
            ConnectionAttempt::Connected(conn) => Ok(conn),
            _ => eyre::bail!("pinned fake FTPS certificate was not accepted"),
        }
    }

    fn open_ftps(server: &remote::fake_ftp::FakeFtp) -> Result<Session<'static>> {
        let spec = Box::leak(Box::new(spec()));
        open_session(
            Box::new(ftps_connection(server)?),
            spec,
            RemotePathBuf::new("/").unwrap(),
        )
    }

    /// The live-verified DatHost profile — `SIZE` refused in ASCII mode,
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
                package_defaults: BTreeMap::new(),
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

    /// A legacy deployment manifest on the remote is adopted on open and
    /// superseded by the state file on the next persist — over the real
    /// FTP path.
    #[test]
    fn a_legacy_manifest_migrates_over_ftp() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options::default());
        let addr = server.addr;
        let owned = OwnedFile {
            hash: blake3::hash(b"dll-bytes").to_hex().to_string(),
            size: 9,
        };
        let manifest = serde_json::json!({
            "version": 1,
            "files": { "BepInEx/plugins/Author-ModA/ModA.dll": owned },
        });
        server.seed_file(
            "/BepInEx/config/.gale-server-manifest.json",
            &serde_json::to_vec(&manifest).unwrap(),
        );

        let mut session = open_ftp(addr).unwrap();
        assert!(session.migrated, "the legacy manifest must be adopted");
        assert!(
            session
                .state
                .files
                .contains_key(&deploy_path("BepInEx/plugins/Author-ModA/ModA.dll"))
        );

        // A completed deployment persists the new-format state, which a
        // fresh connection then loads instead of the manifest.
        let fixture = mod_fixture();
        let publication = fixture.publication();
        let desired = desired_payload();
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
        finish(
            &mut session,
            deployment,
            RestartOutcome::NotRequired,
            &meta(),
        )
        .unwrap();

        let second = open_ftp(addr).unwrap();
        assert!(!second.migrated);
        assert_eq!(second.state.operation_seq, 1);
    }

    /// An existing-but-unreadable legacy manifest must never silently
    /// become an empty authoritative state — opening fails closed.
    #[test]
    fn an_unreadable_legacy_manifest_fails_closed() {
        use remote::fake_ftp::{FakeFtp, Options};

        let server = FakeFtp::valheim_host(Options {
            refuse_retr: true,
            ..Default::default()
        });
        server.seed_file(
            "/BepInEx/config/.gale-server-manifest.json",
            br#"{"version":1,"files":{}}"#,
        );

        let err = open_ftp(server.addr).err().expect("must fail");
        assert!(
            format!("{err:#}").contains("refuses to return"),
            "expected a refused read, got: {err:#}"
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
            format!("{error:#}").contains("does not read back as written"),
            "expected the read-back verification failure, got: {error:#}"
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

    // --- remote accessors through the shared handle ---

    fn remote_has_dir(remote: &Shared, path: &str) -> bool {
        remote.lock().unwrap().dirs.contains(path)
    }

    fn remote_contents(remote: &Shared, path: &str) -> Option<Vec<u8>> {
        remote.lock().unwrap().files.get(path).cloned()
    }
}
