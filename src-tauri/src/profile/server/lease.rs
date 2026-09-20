//! Cross-process deployment coordination.
//!
//! Two executors — a desktop in Local mode and a worker — can run on
//! separate machines, so a process-local mutex cannot prevent overlapping
//! deployments. The lease is a directory claimed atomically on the remote
//! server itself: `MKD`/mkdir either creates the directory or fails, which
//! gives both FTP and SFTP a genuine mutual-exclusion primitive.
//!
//! The holder heartbeats a `lease.json` inside the directory through a
//! second connection. Ownership is explicit: heartbeat and release verify
//! the stored record still names this holder before touching it, so an old
//! owner can never delete or overwrite a newer executor's lease.
//!
//! Stale takeover is deliberately conservative. A lease whose heartbeat
//! expired is broken automatically only when it belongs to the *same*
//! owner (a restarted executor recovering its own crash); a foreign stale
//! lease requires an explicit `force` decision, because an expired
//! heartbeat does not prove the old executor stopped writing — FTP/SFTP
//! have no fencing primitive that can revoke an in-flight write. This is
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
    paths::RemotePathBuf,
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
    pub record: LeaseRecord,
    stop: StopSignal,
    heartbeat: Option<JoinHandle<()>>,
    /// Set by the heartbeat when the stored lease was replaced by another
    /// owner — this executor must stop mutating.
    lost: Arc<AtomicBool>,
}

/// A lease acquisition that found another live executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseBusy {
    pub record: LeaseRecord,
    /// Whether the blocking lease's heartbeat has expired — the caller can
    /// offer `force` takeover only when it is.
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
    /// from the remote — the cheap ownership check the engine runs before
    /// each mutation phase.
    pub fn still_ours(&self, ops: &mut dyn RemoteOps) -> bool {
        match read_lease(ops, &self.file) {
            Ok(Some(record)) => record.operation_id == self.record.operation_id,
            _ => false,
        }
    }

    /// Whether the heartbeat observed a different owner — the executor
    /// should treat the deployment as compromised and stop mutating.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Stops the heartbeat and removes the lease — but only while the
    /// stored record still names this holder. If another executor took
    /// over, its lease is left untouched: release must never delete a
    /// foreign lock. Best-effort: remaining failures only mean the lease
    /// expires on its own.
    pub fn release(mut self, ops: &mut dyn RemoteOps) {
        signal_stop(&self.stop);
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }

        match read_lease(ops, &self.file) {
            Ok(Some(record)) if record.operation_id == self.record.operation_id => {
                if let Err(err) = ops.delete_file(&self.file) {
                    warn!("failed to remove lease file: {err}");
                }
                if let Err(err) = ops.delete_dir(&self.dir) {
                    warn!("failed to remove lease directory: {err}");
                }
                info!(owner = %self.record.owner, "released deployment lease");
            }
            Ok(_) => {
                // Another owner holds the lease now — theirs stays.
                warn!(
                    owner = %self.record.owner,
                    "lease ownership changed; leaving the new holder's lease in place"
                );
            }
            Err(err) => {
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
/// executor recovering); a stale foreign lease requires `force` — the
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
                ops.write(&lease_file, &bytes)
                    .context("failed to write deployment lease")?;
                info!(owner, ?executor, "acquired deployment lease");
                return Ok(Lease {
                    dir: lease_dir.clone(),
                    file: lease_file,
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
                        // mid-operation and restarted — safe to recover.
                        info!(owner, "recovering own stale deployment lease");
                        break_lease(ops, lease_dir, &lease_file)?;
                    } else if force {
                        warn!(
                            owner = %record.owner,
                            "taking over a stale foreign deployment lease by request"
                        );
                        break_lease(ops, lease_dir, &lease_file)?;
                    } else {
                        return Err(LeaseBusy {
                            record,
                            stale: true,
                        }
                        .into());
                    }
                }
                // An empty husk or an unreadable record: a live holder
                // writes its file right after mkdir, so a husk that
                // persists past the grace window is a crash remnant.
                None if !husk_observed => {
                    husk_observed = true;
                    thread::sleep(HUSK_GRACE);
                }
                None => break_lease(ops, lease_dir, &lease_file)?,
            },
        }

        if attempt == CLAIM_RETRIES {
            bail!("deployment lease could not be acquired");
        }
    }

    unreachable!()
}

/// Starts a heartbeat that keeps the lease alive through a second
/// connection at [`HEARTBEAT_INTERVAL`]. Each beat first re-reads the
/// stored record: if another owner has taken over, the heartbeat stops and
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
    let file = lease.file.clone();
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
            // stored record is still ours before writing.
            match read_lease(conn.as_mut(), &file) {
                Ok(Some(current)) if current.operation_id == record.operation_id => {}
                Ok(_) => {
                    lost.store(true, Ordering::SeqCst);
                    warn!(
                        owner = %record.owner,
                        "deployment lease was lost to another executor; heartbeat stopped"
                    );
                    return;
                }
                Err(_) => {
                    // Cannot verify ownership — skip this beat rather than
                    // risk overwriting a foreign lease.
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

/// Reads the current lease record, if present and parseable.
pub fn read_lease(
    ops: &mut dyn RemoteOps,
    lease_file: &RemotePathBuf,
) -> Result<Option<LeaseRecord>> {
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

fn break_lease(
    ops: &mut dyn RemoteOps,
    lease_dir: &RemotePathBuf,
    lease_file: &RemotePathBuf,
) -> Result<()> {
    ops.delete_file(lease_file).ok();
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
        // A crashed executor restarting finds its own stale lease — the
        // same owner may safely break it without a force decision.
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

        // The foreign lease survives — the old owner must not delete it.
        let stored: LeaseRecord =
            serde_json::from_slice(remote.contents(LEASE_FILE).unwrap()).unwrap();
        assert_eq!(stored.owner, "worker:vps");
        assert!(remote.dirs.contains(LEASE_DIR));
    }

    #[test]
    fn still_ours_tracks_stored_ownership() {
        let mut remote = MemoryRemote::new();
        let lease = acquire(&mut remote).unwrap();

        assert!(lease.still_ours(&mut remote));

        plant_lease(&mut remote, Utc::now());
        assert!(!lease.still_ours(&mut remote));
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

        let conn = shared.clone();
        start_heartbeat_every(
            &mut lease,
            move || Ok(Box::new(conn.clone()) as Box<dyn RemoteOps>),
            Duration::from_millis(50),
        );

        thread::sleep(Duration::from_millis(160));
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
