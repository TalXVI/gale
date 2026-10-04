//! Applies an approved plan's file operations to the remote.

use std::{
    collections::{BTreeMap, BTreeSet},
    thread,
    time::Duration,
};

use eyre::{Context, Result, ensure};
use ssh2::ErrorCode;
use suppaftp::{FtpError, Status};
use tracing::warn;

use super::{ensure_ownership, session::Session};
use crate::profile::{
    export::{ConfigPath, ContentHash},
    server::{
        lease::Lease,
        paths::{DeployPath, RemotePath, RemotePathBuf},
        plan::{ConfigAction, DeploymentPlan, DesiredDeployment, FileSource, UploadKind},
        progress::{ProgressReporter, SyncPhase},
        remote::RemoteOps,
        state::{OperationSummary, OwnedFile},
    },
    sync::FetchedPublication,
};

const TRANSFER_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Applies the plan's file operations and updates `session.state` to
/// describe exactly what succeeded. Lease ownership is verified between
/// phases: an executor that loses the lease stops before its next mutation
/// rather than interleaving writes with the new holder.
pub(super) fn execute(
    session: &mut Session,
    plan: &DeploymentPlan,
    desired: &DesiredDeployment,
    publication: &FetchedPublication,
    lease: &Lease,
    progress: &mut ProgressReporter,
) -> Result<(OperationSummary, Vec<String>, Vec<ConfigPath>)> {
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
        let deleted = session.ops.delete_file(&remote);
        if changed_or_unknown(session, deleted)
            .with_context(|| format!("failed to remove {path}"))?
        {
            summary.removed_files += 1;
            session.state.restart_required = true;
        }
        session.state.files.remove(path);
        removed += 1;
        progress.advance(removed, None);
    }

    for dir in &plan.directory_removals {
        progress.item(format!("{dir}/"));
        let remote = session.mapper.remote_path(dir);
        if let Err(error) = session.ops.delete_dir(&remote) {
            warnings.push(format!("could not remove {dir}/: {error}"));
        }
        removed += 1;
        progress.advance(removed, None);
    }

    // ---- Uploads: payload first, then published config writes.
    ensure_ownership(lease, session)?;
    let mut ensured = BTreeSet::new();
    let (payload_uploads, config_uploads): (Vec<_>, Vec<_>) = plan
        .uploads
        .iter()
        .partition(|upload| upload.kind == UploadKind::Payload);

    if !payload_uploads.is_empty() {
        progress.phase(SyncPhase::UploadingPayload);
        let bytes = payload_uploads.iter().map(|upload| upload.size).sum();
        progress.work(payload_uploads.len(), Some(bytes));
    }
    for (index, upload) in payload_uploads.into_iter().enumerate() {
        progress.item(upload.path.to_string());
        // A payload failure means the published mod set is not fully
        // deployed; remaining uploads are abandoned rather than
        // half-applying them silently.
        let staged = desired
            .get(&upload.path)
            .ok_or_else(|| eyre::eyre!("planned upload missing staged content: {}", upload.path))?;
        let uploaded = upload_file(session, &upload.path, &staged.source, &mut ensured);
        changed_or_unknown(session, uploaded)
            .wrap_err_with(|| format!("failed to upload {}", upload.path))?;
        session.state.files.insert(
            upload.path.clone(),
            OwnedFile {
                hash: staged.hash.clone(),
                size: staged.size,
            },
        );
        session.state.restart_required = true;
        summary.uploaded_files += 1;
        summary.uploaded_bytes += upload.size;
        progress.advance(index + 1, Some(summary.uploaded_bytes));
    }

    if !config_uploads.is_empty() {
        progress.phase(SyncPhase::WritingConfigs);
        progress.work(config_uploads.len(), None);
    }
    for (index, upload) in config_uploads.into_iter().enumerate() {
        progress.item(upload.path.to_string());
        let path = ConfigPath::try_from(upload.path.as_str().to_owned())
            .map_err(|_| eyre::eyre!("config path is not deployable: {}", upload.path))?;
        let file = publication
            .config
            .get(&path)
            .ok_or_else(|| eyre::eyre!("planned config write is not published: {path}"))?;
        match upload_file(
            session,
            &upload.path,
            &FileSource::Bytes(file.bytes.clone()),
            &mut ensured,
        ) {
            Ok(()) => {
                session.state.record_applied(&path, &file.hash);
                session.state.restart_required = true;
                summary.config_writes += 1;
            }
            Err(error) => {
                // A failed config write must not advance the file's record;
                // it stays pending/reviewable for the next attempt.
                warnings.push(format!("could not write config {path}: {error}"));
                failed_config_writes.push(path);
            }
        }
        progress.advance(index + 1, None);
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
                ConfigAction::Write | ConfigAction::Unapplied => {}
            }
        }
    }

    // ---- Advance the recorded revision: reaching this point means the
    // whole payload phase succeeded, since a failure returns early above.
    if plan.mods_phase {
        session.state.mods_revision = Some(plan.mods_revision.clone());
    }

    Ok((summary, warnings, failed_config_writes))
}

/// Passes through the result of a remote change. A failed change may still
/// have partly happened, so the server is assumed to need a restart.
fn changed_or_unknown<T>(session: &mut Session, result: Result<T>) -> Result<T> {
    if result.is_err() {
        session.state.restart_required = true;
    }
    result
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

    with_retry(session, |session| match source {
        FileSource::Path(local) => session.ops.upload(local, &temporary),
        FileSource::Bytes(bytes) => session.ops.write(&temporary, bytes),
    })?;
    replace_remote(session.ops.as_mut(), &temporary, &target)
        .with_context(|| format!("failed to upload remote file {path}"))
}

/// Retries `work` after reconnecting, for transient transport failures.
pub(super) fn with_retry<T>(
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
pub(super) fn replace_remote(
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
