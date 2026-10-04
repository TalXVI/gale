//! The remote snapshot the planner runs against, read over a pool of
//! parallel read-only helper connections.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use eyre::{Context, Result, bail};
use tracing::{debug, info, warn};

use super::session::{Session, refresh_state};
use crate::profile::{
    export::{ConfigPath, ContentHash},
    server::{
        paths::{DeployPathBuf, RemotePath},
        plan::{DeploySelection, DesiredDeployment, RemoteSnapshot},
        progress::{ProgressReporter, SyncPhase},
        remote::{ReadOnly, RemoteOps, RemoteReader},
    },
    sync::FetchedPublication,
};

/// Config files are small; remote reads for decision-making are bounded.
const MAX_CONFIG_READ: u64 = 1024 * 1024;
/// Payload scanning and verification are round-trip bound (each LIST/RETR is
/// several control and data-channel round trips carrying little data), so
/// helper connections scale throughput almost linearly. Four keeps a Deploy
/// at six concurrent control connections (session + heartbeat + four
/// helpers), each still subject to proactive renewal.
pub(super) const MAX_SNAPSHOT_READERS: usize = 4;
/// A helper login costs roughly as many round trips as verifying one or two
/// files; only open one per this many desired payload files.
const FILES_PER_SNAPSHOT_READER: usize = 8;

/// Gathers everything the planner needs to know about the remote: a fresh
/// authoritative state read, the payload listing, the content hashes of
/// owned managed payloads, and the hashes of every config path a decision
/// may touch.
pub(super) fn take_snapshot(
    session: &mut Session,
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    progress: &mut ProgressReporter,
) -> Result<RemoteSnapshot> {
    progress.phase(SyncPhase::RefreshingState);
    refresh_state(session)?;
    info!(
        phase = "snapshot",
        owned_files = session.state.files.len(),
        desired_files = desired.len(),
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
        let requested = (desired.len() / FILES_PER_SNAPSHOT_READER).min(MAX_SNAPSHOT_READERS);
        let mut readers: Vec<Box<dyn RemoteReader + '_>> =
            open_snapshot_readers(session.ops.as_ref(), requested);
        if readers.is_empty() {
            readers.push(Box::new(ReadOnly(session.ops.as_mut())));
        }

        // List the payload tree level by level: every directory in a
        // level is listed in parallel, then its children form the next
        // level. The files/dirs maps are order-independent, so this is
        // equivalent to the old recursive walk while keeping each connection
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

        // Helpers are done before the config phase, and before any
        // mutation, so the authoritative connection is free again.
        drop(readers);
    }

    // Hash every config path a decision could touch: published files,
    // recorded ones, and explicitly selected paths. A mods-only operation
    // never reads config files. The server owns them.
    let mut config_remote = BTreeMap::new();
    if selection.configs.is_some() {
        let mut paths: BTreeSet<ConfigPath> = BTreeSet::new();
        paths.extend(
            publication
                .config
                .keys()
                .filter(|path| spec.is_managed_config(path))
                .cloned(),
        );
        paths.extend(session.state.config.keys().cloned());
        paths.extend(selection.written_configs().cloned());

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
/// skipped. The snapshot still runs over however many opened, and an
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

/// Runs `work` on every item. Each reader uses one scoped thread to claim
/// items in order. It runs inline when only the
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

/// Streams `remote` through a fresh hasher. Retries once after a transport
/// failure, as `read_snapshot_file` does for the read-only
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
