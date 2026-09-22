//! Cross-process deployment coordination.
//!
//! Two executors, a desktop in Local mode and a worker, can run on
//! separate machines, so a process-local mutex cannot prevent overlapping
//! deployments. The lease is a directory claimed atomically on the remote
//! server itself: `MKD`/mkdir either creates the directory or fails, which
//! gives both FTP and SFTP a genuine mutual-exclusion mechanism.
//!
//! The holder heartbeats a `lease.json` inside the directory through a
//! second connection. Ownership is explicit: heartbeat and release verify
//! the stored record still names this holder before touching it, so an old
//! owner can never delete or overwrite a newer executor's lease. A
//! `holder-<operation>` marker directory inside the claim is the fallback
//! proof on hosts that filter the record file from read commands — the
//! marker also keeps the claim non-empty, so a claim that cannot be
//! inspected cannot be silently removed either.
//!
//! Stale takeover is deliberately conservative. A lease whose heartbeat
//! expired is broken automatically only when it belongs to the *same*
//! owner (a restarted executor recovering its own crash); a foreign stale
//! lease requires an explicit `force` decision, because an expired
//! heartbeat does not prove the old executor stopped writing. FTP/SFTP
//! have no fencing mechanism that can revoke an in-flight write. This is
//! the strongest guarantee the transports provide; the documented recovery
//! path is to stop the old executor and retry with `force`.

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use chrono::{DateTime, Utc};
use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{
    paths::{RemotePath, RemotePathBuf},
    remote::RemoteOps,
    state::{self, ExecutorKind},
};

/// How long a heartbeat keeps the lease alive. Stale detection adds a skew
/// tolerance so imperfect clocks don't break leases early.
pub const LEASE_TTL: Duration = Duration::from_secs(300);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const STALE_SKEW: Duration = Duration::from_secs(90);
const CLAIM_RETRIES: usize = 2;
/// How long an empty lease directory is observed before it counts as a
/// crashed claim. A live holder writes its record immediately after mkdir,
/// so a husk persisting past this grace is a crash remnant, not a race.
const HUSK_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRecord {
    /// Who holds the lease: `local:<profile_id>` or a worker id.
    pub owner: String,
    pub executor: ExecutorKind,
    pub operation_id: String,
    pub acquired_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
    pub ttl_secs: u64,
}

/// Shared stop signal for the heartbeat thread. A `Condvar` rather than a
/// polled flag so `release` wakes the sleeper immediately instead of
/// blocking `join` for the remainder of the heartbeat interval.
type StopSignal = Arc<(Mutex<bool>, Condvar)>;

fn stop_signal() -> StopSignal {
    Arc::new((Mutex::new(false), Condvar::new()))
}

fn signal_stop(stop: &StopSignal) {
    let (lock, cv) = &**stop;
    *lock.lock().unwrap() = true;
    cv.notify_all();
}

/// The holder side of an acquired lease.
pub struct Lease {
    dir: RemotePathBuf,
    file: RemotePathBuf,
    /// A directory named `holder-<operation_id>` inside the claim. It is
    /// the ownership fallback on hosts where the record file cannot be
    /// read back — whether because the server filters dot-paths like
    /// `.gale-deploy.lock` from reads, or refuses `RETR`/`SIZE` on them
    /// while `MKD`, `STOR`, `CWD`, `DELE`, and `RMD` all still work. The
    /// marker is still verifiable (`is_dir`/`CWD`/`MLST`), can only exist
    /// inside the claim this executor made, and must be removed before
    /// the claim directory itself can be — so its continued presence
    /// proves the claim is still ours.
    marker: RemotePathBuf,
    pub record: LeaseRecord,
    stop: StopSignal,
    heartbeat: Option<JoinHandle<()>>,
    /// Set by the heartbeat when the stored lease was replaced by another
    /// owner, meaning this executor must stop mutating.
    lost: Arc<AtomicBool>,
}

/// The result of a lease ownership check.
///
/// `still_ours` used to collapse every one of these into the same
/// `false`, which reported a read-filtered or transiently unreadable
/// record as a takeover and aborted healthy deployments.
#[derive(Debug)]
pub enum Ownership {
    /// The claim verifiably still belongs to this holder: the stored
    /// record names this operation.
    Held,
    /// The record cannot be read back, but the holder marker proves the
    /// claim is intact — the host filters lease reads (e.g. FTP servers
    /// that refuse SIZE/RETR/LIST on dot-prefixed paths).
    HeldViaMarker,
    /// The claim verifiably no longer belongs to this holder: a foreign
    /// record was read (`holder` names its owner), or our marker was
    /// removed, which only happens when the claim is dismantled.
    Lost { holder: Option<String> },
    /// Neither the record nor the marker could be checked — a transport
    /// failure, not evidence of a takeover.
    Unverifiable(eyre::Report),
}

/// A lease acquisition that found another live executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseBusy {
    pub record: LeaseRecord,
    /// Whether the blocking lease's heartbeat has expired. The caller can
    /// offer `force` takeover only when it has.
    pub stale: bool,
}

impl std::fmt::Display for LeaseBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.stale {
            write!(
                f,
                "a stale deployment lease is held by {} (since {}); stop that executor or take it over",
                self.record.owner, self.record.acquired_at
            )
        } else {
            write!(
                f,
                "a deployment is already in progress (held by {} since {})",
                self.record.owner, self.record.acquired_at
            )
        }
    }
}

impl std::error::Error for LeaseBusy {}

impl Lease {
    /// Whether the stored lease still belongs to this holder. Read fresh
    /// from the remote, this is the cheap ownership check the engine runs
    /// before each mutation phase.
    pub fn check_ownership(&self, ops: &mut dyn RemoteOps) -> Ownership {
        check_ownership(
            ops,
            &self.dir,
            &self.file,
            &self.marker,
            &self.record.operation_id,
        )
    }

    /// Whether the heartbeat observed a different owner. If so, the
    /// executor should treat the deployment as compromised and stop
    /// mutating.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Stops the heartbeat and removes the lease, but only while the
    /// stored record still names this holder. If another executor took
    /// over, its lease stays untouched, because release must never delete
    /// a foreign lock. The whole operation is best-effort. A failure here
    /// only means the lease expires on its own.
    pub fn release(mut self, ops: &mut dyn RemoteOps) {
        signal_stop(&self.stop);
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }

        match self.check_ownership(ops) {
            Ownership::Held | Ownership::HeldViaMarker => {
                if let Err(err) = ops.delete_file(&self.file) {
                    warn!("failed to remove lease file: {err}");
                }
                if let Err(err) = ops.delete_dir(&self.marker) {
                    warn!("failed to remove lease holder marker: {err}");
                }
                if let Err(err) = ops.delete_dir(&self.dir) {
                    warn!("failed to remove lease directory: {err}");
                }
                info!(owner = %self.record.owner, "released deployment lease");
            }
            Ownership::Lost { .. } => {
                // Another owner holds the lease now, so theirs stays.
                // Our marker, keyed to this operation's id, is the only
                // piece that is still ours to remove — cleaning it keeps
                // the new holder's eventual rmdir from failing on it.
                ops.delete_dir(&self.marker).ok();
                warn!(
                    owner = %self.record.owner,
                    "lease ownership changed; leaving the new holder's lease in place"
                );
            }
            Ownership::Unverifiable(err) => {
                warn!("could not verify lease ownership for release: {err}; leaving it to expire");
            }
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        signal_stop(&self.stop);
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
    }
}

/// Attempts to claim the deployment lease.
///
/// A live foreign lease yields [`LeaseBusy`]. A stale lease is broken
/// automatically only when it belongs to the same owner (a crashed
/// executor recovering); a stale foreign lease requires `force`, the
/// documented recovery once the old executor is confirmed stopped. An
/// empty lease directory is a claim interrupted mid-write: it gets one
/// grace observation before being treated as a crash remnant.
pub fn acquire(
    ops: &mut dyn RemoteOps,
    lease_dir: &RemotePathBuf,
    owner: &str,
    executor: ExecutorKind,
    operation_id: &str,
    force: bool,
) -> Result<Lease> {
    let lease_file = lease_file(lease_dir);
    let mut husk_observed = false;

    for attempt in 0..=CLAIM_RETRIES {
        match ops.claim_dir(lease_dir)? {
            true => {
                let record = LeaseRecord {
                    owner: owner.to_owned(),
                    executor,
                    operation_id: operation_id.to_owned(),
                    acquired_at: Utc::now(),
                    heartbeat_at: Utc::now(),
                    ttl_secs: LEASE_TTL.as_secs(),
                };
                let bytes = serde_json::to_vec_pretty(&record)?;
                // The marker goes in first: a claim that dies before its
                // record write still looks claimed to other executors
                // instead of being mistaken for a husk. Both are removed
                // if the claim cannot be established so it self-heals.
                let marker = marker_path(lease_dir, operation_id);
                if let Err(err) = ops
                    .ensure_dir(&marker)
                    .and_then(|_| ops.write(&lease_file, &bytes))
                {
                    ops.delete_dir(&marker).ok();
                    ops.delete_dir(lease_dir).ok();
                    return Err(err).context("failed to establish the deployment lease");
                }
                info!(owner, ?executor, "acquired deployment lease");
                return Ok(Lease {
                    dir: lease_dir.clone(),
                    file: lease_file,
                    marker,
                    record,
                    stop: stop_signal(),
                    heartbeat: None,
                    lost: Arc::new(AtomicBool::new(false)),
                });
            }
            false => match read_lease(ops, &lease_file)? {
                Some(record) if !is_stale(&record) => {
                    return Err(LeaseBusy {
                        record,
                        stale: false,
                    }
                    .into());
                }
                Some(record) => {
                    if record.owner == owner {
                        // Our own stale lease: this executor crashed
                        // mid-operation and restarted, so it is safe to
                        // recover.
                        info!(owner, "recovering own stale deployment lease");
                        break_lease(ops, lease_dir, &lease_file, Some(&record))?;
                    } else if force {
                        warn!(
                            owner = %record.owner,
                            "taking over a stale foreign deployment lease by request"
                        );
                        break_lease(ops, lease_dir, &lease_file, Some(&record))?;
                    } else {
                        return Err(LeaseBusy {
                            record,
                            stale: true,
                        }
                        .into());
                    }
                }
                // An empty husk or an unreadable record means the claim
                // never finished — unless a holder marker is present.
                // Hosts that filter dot-paths from reads hide the record
                // of a *live* claim, but cannot hide the marker from a
                // directory listing; either way it must not be broken
                // without checking for one first.
                None => match find_markers(ops, lease_dir) {
                    Ok(markers) if !markers.is_empty() => {
                        // A marker proves a claim was established; whether
                        // its holder is still alive cannot be judged
                        // without the record. Force is only authorized for
                        // a confirmed stale lease, never an unknown one.
                        return Err(LeaseBusy {
                            record: marker_record(&markers[0]),
                            stale: false,
                        }
                        .into());
                    }
                    Ok(_) if !husk_observed => {
                        husk_observed = true;
                        thread::sleep(HUSK_GRACE);
                    }
                    Ok(_) => break_lease(ops, lease_dir, &lease_file, None)?,
                    Err(err) => {
                        return Err(err).context("failed to inspect the deployment lease");
                    }
                },
            },
        }

        if attempt == CLAIM_RETRIES {
            bail!("deployment lease could not be acquired");
        }
    }

    unreachable!()
}

/// Starts a heartbeat that keeps the lease alive through a second
/// connection at [`HEARTBEAT_INTERVAL`]. Each beat re-reads the stored
/// record first. If another owner has taken over, the heartbeat stops and
/// flags `lost` instead of overwriting the foreign lease. Write failures
/// only mean the lease can go stale, which is safe.
pub fn start_heartbeat(
    lease: &mut Lease,
    connect: impl FnMut() -> Result<Box<dyn RemoteOps>> + Send + 'static,
) {
    start_heartbeat_every(lease, connect, HEARTBEAT_INTERVAL);
}

fn start_heartbeat_every(
    lease: &mut Lease,
    mut connect: impl FnMut() -> Result<Box<dyn RemoteOps>> + Send + 'static,
    interval: Duration,
) {
    let stop = lease.stop.clone();
    let lost = lease.lost.clone();
    let dir = lease.dir.clone();
    let file = lease.file.clone();
    let marker = lease.marker.clone();
    let mut record = lease.record.clone();

    lease.heartbeat = Some(thread::spawn(move || {
        let mut connection: Option<Box<dyn RemoteOps>> = None;
        let (lock, cv) = &*stop;
        loop {
            let mut guard = lock.lock().unwrap();
            if *guard {
                break;
            }
            guard = cv.wait_timeout(guard, interval).unwrap().0;
            if *guard {
                break;
            }
            drop(guard);

            if connection.is_none() {
                connection = connect().ok();
            }
            let Some(conn) = connection.as_mut() else {
                continue;
            };

            // Never overwrite a lease that was taken over: verify the
            // claim is still ours before writing. On hosts that filter
            // the record file from reads, the holder marker is the proof.
            match check_ownership(conn.as_mut(), &dir, &file, &marker, &record.operation_id) {
                Ownership::Held | Ownership::HeldViaMarker => {}
                Ownership::Lost { .. } => {
                    lost.store(true, Ordering::SeqCst);
                    warn!(
                        owner = %record.owner,
                        "deployment lease was lost to another executor; heartbeat stopped"
                    );
                    return;
                }
                Ownership::Unverifiable(_) => {
                    // Cannot verify ownership, so skip this beat rather
                    // than risk overwriting a foreign lease.
                    continue;
                }
            }

            record.heartbeat_at = Utc::now();
            let bytes = match serde_json::to_vec(&record) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if conn.write(&file, &bytes).is_err() {
                connection = None;
            }
        }
    }));
}

/// Reads the current lease record. Returns `None` when the file is
/// missing or its contents do not parse.
pub fn read_lease(ops: &mut dyn RemoteOps, lease_file: &RemotePath) -> Result<Option<LeaseRecord>> {
    let Some(bytes) = ops.read(lease_file, 64 * 1024)? else {
        return Ok(None);
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Whether a lease's heartbeat has expired beyond the skew tolerance.
fn is_stale(record: &LeaseRecord) -> bool {
    let age = Utc::now()
        .signed_duration_since(record.heartbeat_at)
        .to_std()
        .unwrap_or_default();
    age > record.ttl() + STALE_SKEW
}

impl LeaseRecord {
    fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs.max(LEASE_TTL.as_secs()))
    }
}

/// Verifies a claim against the remote: the stored record is the primary
/// proof, the holder marker the fallback when the record cannot be read.
fn check_ownership(
    ops: &mut dyn RemoteOps,
    lease_dir: &RemotePath,
    lease_file: &RemotePath,
    marker: &RemotePath,
    operation_id: &str,
) -> Ownership {
    let read_error = match read_lease(ops, lease_file) {
        Ok(Some(record)) => {
            return if record.operation_id == operation_id {
                Ownership::Held
            } else {
                Ownership::Lost {
                    holder: Some(record.owner),
                }
            };
        }
        Ok(None) => None,
        Err(err) => Some(err),
    };

    // The record could not be read — missing, malformed, torn mid-write,
    // or filtered by the host. The marker decides: it only exists inside
    // the claim this holder made, and a competing claim must remove it
    // before the lease directory can be reused. No other executor knows
    // its name (the record that carries it is exactly what is unreadable),
    // so a surviving marker means the claim is still ours.
    match ops.is_dir(marker) {
        Ok(true) => Ownership::HeldViaMarker,
        Ok(false) => match ops.is_dir(lease_dir) {
            // The claim directory stands but our marker is gone: the
            // claim was dismantled by another executor.
            Ok(true) => Ownership::Lost { holder: None },
            // Neither marker nor claim directory can be observed. That
            // either means the claim really is gone, or this host filters
            // even directory probes on the lease path — the checks cannot
            // tell those apart, so the honest answer is unverifiable.
            Ok(false) => Ownership::Unverifiable(read_error.unwrap_or_else(|| {
                eyre::eyre!("the lease directory at {lease_dir} is not observable on this host")
            })),
            Err(err) => Ownership::Unverifiable(err),
        },
        Err(err) => Ownership::Unverifiable(read_error.unwrap_or(err)),
    }
}

/// Marker directory name prefix; the operation id follows it.
const HOLDER_PREFIX: &str = "holder-";

/// `lease_dir/holder-<operation_id>` — the fallback ownership proof.
fn marker_path(lease_dir: &RemotePathBuf, operation_id: &str) -> RemotePathBuf {
    let name = super::paths::DeployPathBuf::new(format!("{HOLDER_PREFIX}{operation_id}"))
        .expect("operation ids are path-safe");
    lease_dir.join(&name)
}

/// Holder markers found inside an existing claim directory.
fn find_markers(ops: &mut dyn RemoteOps, lease_dir: &RemotePath) -> Result<Vec<RemotePathBuf>> {
    Ok(ops
        .list(lease_dir)?
        .into_iter()
        .filter(|entry| entry.is_directory && entry.name.starts_with(HOLDER_PREFIX))
        .filter_map(|entry| super::paths::DeployPathBuf::new(entry.name).ok())
        .map(|name| lease_dir.join(&name))
        .collect())
}

/// A placeholder record for a claim whose marker is visible but whose
/// record is unreadable, so the busy path can still report it honestly.
fn marker_record(marker: &RemotePath) -> LeaseRecord {
    LeaseRecord {
        owner: "another executor (its lease record is unreadable)".to_owned(),
        executor: ExecutorKind::Worker,
        operation_id: marker
            .file_name()
            .and_then(|name| name.strip_prefix(HOLDER_PREFIX))
            .unwrap_or_default()
            .to_owned(),
        acquired_at: Utc::now(),
        heartbeat_at: Utc::now(),
        ttl_secs: LEASE_TTL.as_secs(),
    }
}

fn break_lease(
    ops: &mut dyn RemoteOps,
    lease_dir: &RemotePathBuf,
    lease_file: &RemotePathBuf,
    record: Option<&LeaseRecord>,
) -> Result<()> {
    ops.delete_file(lease_file).ok();
    // Holder markers keep the claim directory non-empty, so they must be
    // removed before the directory itself can be. A readable record gives
    // the marker's exact name even where listings are filtered; otherwise
    // fall back to listing. If neither works, the final rmdir fails on
    // the non-empty directory rather than removing a live claim.
    let mut markers = find_markers(ops, lease_dir).unwrap_or_default();
    if let Some(record) = record {
        let derived = marker_path(lease_dir, &record.operation_id);
        if !markers.contains(&derived) {
            markers.push(derived);
        }
    }
    for marker in markers {
        ops.delete_dir(&marker).ok();
    }
    ops.delete_dir(lease_dir)
        .context("failed to clear stale deployment lease")
}

fn lease_file(lease_dir: &RemotePathBuf) -> RemotePathBuf {
    lease_dir.join(
        super::paths::DeployPathBuf::new(state::LEASE_FILE_NAME)
            .unwrap()
            .as_path(),
    )
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;

    use super::*;
    use crate::profile::server::remote::memory::MemoryRemote;

    const LEASE_DIR: &str = "/srv/.gale-deploy.lock";
    const LEASE_FILE: &str = "/srv/.gale-deploy.lock/lease.json";

    fn dir() -> RemotePathBuf {
        RemotePathBuf::new(LEASE_DIR).unwrap()
    }

    fn acquire(remote: &mut MemoryRemote) -> Result<Lease> {
        acquire_as(remote, "local:1", "op-1", false)
    }

    fn acquire_as(remote: &mut MemoryRemote, owner: &str, op: &str, force: bool) -> Result<Lease> {
        super::acquire(remote, &dir(), owner, ExecutorKind::Local, op, force)
    }

    fn plant_lease(remote: &mut MemoryRemote, heartbeat_at: DateTime<Utc>) {
        plant_lease_as(remote, "other", "op-old", heartbeat_at);
    }

    fn plant_lease_as(
        remote: &mut MemoryRemote,
        owner: &str,
        operation_id: &str,
        heartbeat_at: DateTime<Utc>,
    ) {
        remote.dirs.insert(LEASE_DIR.to_owned());
        let record = LeaseRecord {
            owner: owner.to_owned(),
            executor: ExecutorKind::Worker,
            operation_id: operation_id.to_owned(),
            acquired_at: heartbeat_at,
            heartbeat_at,
            ttl_secs: LEASE_TTL.as_secs(),
        };
        remote.put_file(LEASE_FILE, &serde_json::to_vec(&record).unwrap());
    }

    fn expired() -> DateTime<Utc> {
        Utc::now()
            - ChronoDuration::from_std(LEASE_TTL + STALE_SKEW + Duration::from_secs(60)).unwrap()
    }

    #[test]
    fn acquire_writes_lease_record_and_release_cleans_up() {
        let mut remote = MemoryRemote::new();

        let lease = acquire(&mut remote).unwrap();
        assert_eq!(lease.record.owner, "local:1");
        assert_eq!(lease.record.operation_id, "op-1");
        assert_eq!(lease.record.ttl_secs, LEASE_TTL.as_secs());
        assert!(remote.dirs.contains(LEASE_DIR));
        let stored: LeaseRecord =
            serde_json::from_slice(remote.contents(LEASE_FILE).unwrap()).unwrap();
        assert_eq!(stored.owner, "local:1");

        lease.release(&mut remote);
        assert!(!remote.dirs.contains(LEASE_DIR));
        assert!(remote.contents(LEASE_FILE).is_none());

        // Releasing fully frees the lease: re-acquisition succeeds.
        acquire(&mut remote).unwrap();
    }

    #[test]
    fn live_lease_blocks_another_claimant() {
        let mut remote = MemoryRemote::new();
        plant_lease(&mut remote, Utc::now());

        let err = match acquire(&mut remote) {
            Err(err) => err,
            Ok(_) => panic!("expected LeaseBusy"),
        };
        let busy = err.downcast_ref::<LeaseBusy>().expect("expected LeaseBusy");
        assert_eq!(busy.record.owner, "other");
        assert!(!busy.stale);
    }

    #[test]
    fn foreign_stale_lease_blocks_without_force() {
        let mut remote = MemoryRemote::new();
        plant_lease(&mut remote, expired());

        let err = match acquire(&mut remote) {
            Err(err) => err,
            Ok(_) => panic!("expected LeaseBusy"),
        };
        let busy = err.downcast_ref::<LeaseBusy>().expect("expected LeaseBusy");
        assert_eq!(busy.record.owner, "other");
        assert!(busy.stale);

        // The foreign lease is untouched: nothing was deleted.
        assert!(remote.contents(LEASE_FILE).is_some());
        assert!(remote.dirs.contains(LEASE_DIR));
    }

    #[test]
    fn force_takes_over_a_stale_foreign_lease() {
        let mut remote = MemoryRemote::new();
        plant_lease(&mut remote, expired());

        let lease = acquire_as(&mut remote, "local:1", "op-1", true).unwrap();
        assert_eq!(lease.record.owner, "local:1");
        let stored: LeaseRecord =
            serde_json::from_slice(remote.contents(LEASE_FILE).unwrap()).unwrap();
        assert_eq!(stored.owner, "local:1");
    }

    #[test]
    fn own_stale_lease_recovers_automatically() {
        let mut remote = MemoryRemote::new();
        // A crashed executor restarting finds its own stale lease, and
        // the same owner may safely break it without a force decision.
        plant_lease_as(&mut remote, "local:1", "op-crashed", expired());

        let lease = acquire(&mut remote).unwrap();
        assert_eq!(lease.record.owner, "local:1");
        let stored: LeaseRecord =
            serde_json::from_slice(remote.contents(LEASE_FILE).unwrap()).unwrap();
        assert_eq!(stored.operation_id, "op-1");
    }

    #[test]
    fn force_does_not_take_over_a_live_lease() {
        let mut remote = MemoryRemote::new();
        plant_lease(&mut remote, Utc::now());

        // force only unlocks stale leases; a live foreign lease always wins.
        let result = acquire_as(&mut remote, "local:1", "op-1", true);
        assert!(result.is_err());
        assert!(remote.contents(LEASE_FILE).is_some());
    }

    #[test]
    fn release_leaves_a_replaced_lease_alone() {
        let mut remote = MemoryRemote::new();
        let lease = acquire(&mut remote).unwrap();

        // Another executor took over between acquire and release.
        let foreign = LeaseRecord {
            owner: "worker:vps".to_owned(),
            executor: ExecutorKind::Worker,
            operation_id: "op-foreign".to_owned(),
            acquired_at: Utc::now(),
            heartbeat_at: Utc::now(),
            ttl_secs: LEASE_TTL.as_secs(),
        };
        remote.put_file(LEASE_FILE, &serde_json::to_vec(&foreign).unwrap());

        lease.release(&mut remote);

        // The foreign lease survives; the old owner must not delete it.
        let stored: LeaseRecord =
            serde_json::from_slice(remote.contents(LEASE_FILE).unwrap()).unwrap();
        assert_eq!(stored.owner, "worker:vps");
        assert!(remote.dirs.contains(LEASE_DIR));
    }

    #[test]
    fn check_ownership_tracks_stored_ownership() {
        let mut remote = MemoryRemote::new();
        let lease = acquire(&mut remote).unwrap();

        assert!(matches!(
            lease.check_ownership(&mut remote),
            Ownership::Held
        ));

        plant_lease(&mut remote, Utc::now());
        assert!(matches!(
            lease.check_ownership(&mut remote),
            Ownership::Lost {
                holder: Some(ref owner)
            } if owner == "other"
        ));
    }

    /// A shared remote behind the dot-path read filter, mirroring hosts
    /// whose FTP answers SIZE/RETR/LIST on `.gale-deploy.lock` contents
    /// with 550 while MKD/STOR/CWD/DELE/RMD all work.
    fn filtered_remote() -> (
        Arc<std::sync::Mutex<MemoryRemote>>,
        crate::profile::server::remote::memory::FilteredReads,
    ) {
        let inner = Arc::new(std::sync::Mutex::new(MemoryRemote::new()));
        let filtered = crate::profile::server::remote::memory::FilteredReads::new(inner.clone());
        (inner, filtered)
    }

    #[test]
    fn ownership_survives_an_unreadable_record() {
        let (inner, mut filtered) = filtered_remote();
        let lease = super::acquire(
            &mut filtered,
            &dir(),
            "local:1",
            ExecutorKind::Local,
            "op-1",
            false,
        )
        .unwrap();

        // The record file exists on the remote but reads as missing.
        assert!(inner.lock().unwrap().contents(LEASE_FILE).is_some());
        assert!(matches!(
            lease.check_ownership(&mut filtered),
            Ownership::HeldViaMarker
        ));

        // Release still verifies ownership through the marker and cleans
        // the whole claim: record file, marker, and directory.
        lease.release(&mut filtered);
        let remote = inner.lock().unwrap();
        assert!(remote.contents(LEASE_FILE).is_none());
        assert!(!remote.dirs.contains(LEASE_DIR));
        assert!(!remote.dirs.iter().any(|d| d.starts_with(LEASE_DIR)));
    }

    #[test]
    fn ownership_is_lost_when_the_marker_is_removed() {
        let (inner, mut filtered) = filtered_remote();
        let lease = super::acquire(
            &mut filtered,
            &dir(),
            "local:1",
            ExecutorKind::Local,
            "op-1",
            false,
        )
        .unwrap();

        // A competing executor dismantles our claim and establishes its
        // own: the claim directory still stands but our marker is gone
        // and a foreign marker took its place.
        {
            let mut remote = inner.lock().unwrap();
            remote.dirs.remove(&format!("{LEASE_DIR}/holder-op-1"));
            remote.dirs.insert(format!("{LEASE_DIR}/holder-op-foreign"));
        }
        assert!(matches!(
            lease.check_ownership(&mut filtered),
            Ownership::Lost { holder: None }
        ));

        // If the claim directory itself is also gone, the checks cannot
        // tell "dismantled" from "this host filters even directory
        // probes" — that degrades to Unverifiable, not a takeover claim.
        inner.lock().unwrap().dirs.remove(LEASE_DIR);
        assert!(matches!(
            lease.check_ownership(&mut filtered),
            Ownership::Unverifiable(_)
        ));
    }

    #[test]
    fn ownership_is_unverifiable_when_the_transport_fails() {
        let (inner, mut filtered) = filtered_remote();
        let lease = super::acquire(
            &mut filtered,
            &dir(),
            "local:1",
            ExecutorKind::Local,
            "op-1",
            false,
        )
        .unwrap();

        inner.lock().unwrap().connection_dead = true;
        assert!(matches!(
            lease.check_ownership(&mut filtered),
            Ownership::Unverifiable(_)
        ));
    }

    #[test]
    fn a_claim_with_a_marker_is_busy_even_when_its_record_is_unreadable() {
        let (inner, mut filtered) = filtered_remote();
        // A live claim whose record cannot be read: lock dir plus holder
        // marker, no readable lease.json.
        {
            let mut remote = inner.lock().unwrap();
            remote.dirs.insert(LEASE_DIR.to_owned());
            remote.dirs.insert(format!("{LEASE_DIR}/holder-op-foreign"));
        }
        // Listings still work on this variant so the marker is visible.
        filtered.filter_lists = false;

        for force in [false, true] {
            let err = super::acquire(
                &mut filtered,
                &dir(),
                "local:1",
                ExecutorKind::Local,
                "op-1",
                force,
            )
            .err()
            .expect("expected LeaseBusy");
            let busy = err.downcast_ref::<LeaseBusy>().expect("expected LeaseBusy");
            assert!(!busy.stale, "an unreadable heartbeat does not prove expiry");
        }

        // And on a host whose listings are filtered too, the same claim
        // must not be silently broken: the marker keeps the directory
        // non-empty, so rmdir fails and the claim survives.
        let (inner, mut filtered) = filtered_remote();
        {
            let mut remote = inner.lock().unwrap();
            remote.dirs.insert(LEASE_DIR.to_owned());
            remote.dirs.insert(format!("{LEASE_DIR}/holder-op-foreign"));
        }
        assert!(
            super::acquire(
                &mut filtered,
                &dir(),
                "local:1",
                ExecutorKind::Local,
                "op-1",
                true,
            )
            .is_err()
        );
        let remote = inner.lock().unwrap();
        assert!(remote.dirs.contains(LEASE_DIR));
        assert!(
            remote
                .dirs
                .contains(&format!("{LEASE_DIR}/holder-op-foreign"))
        );
    }

    #[test]
    fn break_clears_holder_markers() {
        let mut remote = MemoryRemote::new();
        // A stale claim carrying a marker: crashed before release.
        plant_lease(&mut remote, expired());
        remote.dirs.insert(format!("{LEASE_DIR}/holder-op-old"));

        let lease = acquire_as(&mut remote, "local:1", "op-1", true).unwrap();
        assert_eq!(lease.record.operation_id, "op-1");
        assert!(remote.dirs.contains(&format!("{LEASE_DIR}/holder-op-1")));
        assert!(!remote.dirs.contains(&format!("{LEASE_DIR}/holder-op-old")));
    }

    #[test]
    fn heartbeat_stops_when_ownership_is_lost() {
        let mut remote = MemoryRemote::new();
        let mut lease = acquire(&mut remote).unwrap();

        // The heartbeat writes through the same MemoryRemote.
        let shared = Arc::new(std::sync::Mutex::new(MemoryRemote::new()));
        shared.lock().unwrap().dirs.insert(LEASE_DIR.to_owned());
        shared
            .lock()
            .unwrap()
            .put_file(LEASE_FILE, remote.contents(LEASE_FILE).unwrap());

        let conn = shared.clone();
        start_heartbeat_every(
            &mut lease,
            move || Ok(Box::new(conn.clone()) as Box<dyn RemoteOps>),
            Duration::from_millis(50),
        );

        // A foreign executor replaces the lease file.
        let foreign = LeaseRecord {
            owner: "worker:vps".to_owned(),
            executor: ExecutorKind::Worker,
            operation_id: "op-foreign".to_owned(),
            acquired_at: Utc::now(),
            heartbeat_at: Utc::now(),
            ttl_secs: LEASE_TTL.as_secs(),
        };
        shared
            .lock()
            .unwrap()
            .put_file(LEASE_FILE, &serde_json::to_vec(&foreign).unwrap());

        // Give the heartbeat a few beats to notice.
        for _ in 0..40 {
            if lease.is_lost() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(lease.is_lost());

        // The foreign record is never overwritten by our heartbeat.
        thread::sleep(Duration::from_millis(120));
        let stored: LeaseRecord =
            serde_json::from_slice(shared.lock().unwrap().contents(LEASE_FILE).unwrap()).unwrap();
        assert_eq!(stored.owner, "worker:vps");

        lease.release(&mut remote);
    }

    #[test]
    fn heartbeat_beats_through_the_marker_on_a_filtered_remote() {
        let (inner, mut filtered) = filtered_remote();
        let mut lease = super::acquire(
            &mut filtered,
            &dir(),
            "local:1",
            ExecutorKind::Local,
            "op-1",
            false,
        )
        .unwrap();
        let before = lease.record.heartbeat_at;

        // The heartbeat's second connection sees the same filtered remote.
        let (events, written) = std::sync::mpsc::channel();
        inner.lock().unwrap().write_events = Some(events);
        let conn = filtered.clone();
        start_heartbeat_every(
            &mut lease,
            move || Ok(Box::new(conn.clone()) as Box<dyn RemoteOps>),
            Duration::from_millis(50),
        );

        written
            .recv_timeout(Duration::from_secs(5))
            .expect("heartbeat did not write");
        assert!(!lease.is_lost());
        let after: LeaseRecord =
            serde_json::from_slice(inner.lock().unwrap().contents(LEASE_FILE).unwrap()).unwrap();
        assert!(after.heartbeat_at > before);
        assert_eq!(after.operation_id, "op-1");

        lease.release(&mut filtered);
    }

    #[test]
    fn heartbeat_refreshes_timestamp_while_held() {
        let mut remote = MemoryRemote::new();
        let mut lease = acquire(&mut remote).unwrap();

        let shared = Arc::new(std::sync::Mutex::new(MemoryRemote::new()));
        shared.lock().unwrap().dirs.insert(LEASE_DIR.to_owned());
        shared
            .lock()
            .unwrap()
            .put_file(LEASE_FILE, remote.contents(LEASE_FILE).unwrap());
        let before: LeaseRecord =
            serde_json::from_slice(shared.lock().unwrap().contents(LEASE_FILE).unwrap()).unwrap();

        let (events, written) = std::sync::mpsc::channel();
        shared.lock().unwrap().write_events = Some(events);
        let conn = shared.clone();
        start_heartbeat_every(
            &mut lease,
            move || Ok(Box::new(conn.clone()) as Box<dyn RemoteOps>),
            Duration::from_millis(50),
        );

        written
            .recv_timeout(Duration::from_secs(5))
            .expect("heartbeat did not write");
        let after: LeaseRecord =
            serde_json::from_slice(shared.lock().unwrap().contents(LEASE_FILE).unwrap()).unwrap();
        assert!(after.heartbeat_at > before.heartbeat_at);
        assert!(!lease.is_lost());

        lease.release(&mut remote);
    }

    #[test]
    fn persistent_lease_husk_is_cleared_after_grace() {
        let mut remote = MemoryRemote::new();
        remote.dirs.insert(LEASE_DIR.to_owned());

        // An empty husk observed past the grace window is a crash remnant.
        acquire(&mut remote).unwrap();
    }

    #[test]
    fn malformed_lease_file_is_treated_as_stale() {
        let mut remote = MemoryRemote::new();
        remote.dirs.insert(LEASE_DIR.to_owned());
        remote.put_file(LEASE_FILE, b"{not json");

        acquire(&mut remote).unwrap();
    }
}
