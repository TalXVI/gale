//! An open remote session and the authoritative deployment state it
//! reads and persists.

use eyre::{Context, Result, bail, ensure};
use tracing::{info, warn};

use super::{
    OperationMeta,
    execute::{replace_remote, with_retry},
    layout::{RemoteMapper, detect_host_managed, detect_layout},
};
use crate::profile::server::{
    lease::{self, Lease, LeaseRecord},
    paths::RemotePathBuf,
    plan::RemoteLayout,
    remote::RemoteOps,
    spec::DeploymentSpec,
    state::{self, LoadedState, ServerDeploymentState},
};

/// An open remote session: connection, path mapping, layout, and the
/// authoritative deployment state.
pub struct Session {
    pub ops: Box<dyn RemoteOps>,
    pub(super) mapper: RemoteMapper,
    pub layout: RemoteLayout,
    pub host_managed: bool,
    pub state: ServerDeploymentState,
    /// The `operation_seq` the in-memory `state` was loaded with. Writing
    /// is refused when the remote sequence has moved past it, because a
    /// stale writer must never overwrite newer deployment state.
    pub(super) base_seq: u64,
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

impl Session {
    pub(super) fn acquire_lease(&mut self, meta: &OperationMeta, force: bool) -> Result<Lease> {
        lease::acquire(
            self.ops.as_mut(),
            &self.mapper.remote_path(&self.mapper.spec.lease_dir),
            &meta.owner,
            meta.executor,
            &meta.id,
            force,
        )
    }
}

/// Re-reads the authoritative deployment state from the remote. Called
/// under the deployment lease so planning, policy updates, and the
/// persistence sequence guard all work on fresh state rather than what
/// was loaded when the session opened.
pub(super) fn refresh_state(session: &mut Session) -> Result<()> {
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
/// on a fresh connection after replacement. A truncated or refused
/// transfer can never silently become the authoritative state.
pub(super) fn persist_state(session: &mut Session) -> Result<()> {
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
            Some(_) => {
                bail!(
                    "temporary remote deployment state at {temporary} does not read back as written"
                );
            }
            None => {
                bail!(
                    "temporary remote deployment state was written to {temporary} but cannot be read back"
                );
            }
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
    // operation sequence on the next session. Report that failure
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
        Some(_) => {
            bail!(
                "remote deployment state at {target} does not read back as written; \
             refusing to treat the deployment state as durable"
            );
        }
        None => {
            bail!(
                "remote deployment state was written to {target} but cannot be read back; \
             refusing to treat the deployment state as durable"
            );
        }
    }

    session.base_seq = session.state.operation_seq;
    Ok(())
}
