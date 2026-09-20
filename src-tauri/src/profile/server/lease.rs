//! Cross-process deployment coordination.
//!
//! Two executors — a desktop in Local mode and a worker — can run on
//! separate machines, so a process-local mutex cannot prevent overlapping
//! deployments. The lease is a directory claimed atomically on the remote
//! server itself: `MKD`/mkdir either creates the directory or fails, which
//! gives both FTP and SFTP a genuine mutual-exclusion primitive.
//!
//! The holder heartbeats a `lease.json` inside the directory through a
//! second connection. A lease whose heartbeat has expired is stale and may
//! be broken by the next claimant, so a crashed executor cannot wedge the
//! server forever.

use std::{
    sync::{Arc, Condvar, Mutex},
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
}

/// A lease acquisition that found another live executor.
#[derive(Debug)]
pub struct LeaseBusy(pub LeaseRecord);

impl std::fmt::Display for LeaseBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a deployment is already in progress (held by {} since {})",
            self.0.owner, self.0.acquired_at
        )
    }
}

impl std::error::Error for LeaseBusy {}

impl Lease {
    /// Stops the heartbeat and removes the lease. Best-effort: release
    /// failures only mean the lease expires on its own.
    pub fn release(mut self, ops: &mut dyn RemoteOps) {
        signal_stop(&self.stop);
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        if let Err(err) = ops.delete_file(&self.file) {
            warn!("failed to remove lease file: {err}");
        }
        if let Err(err) = ops.delete_dir(&self.dir) {
            warn!("failed to remove lease directory: {err}");
        }
        info!(owner = %self.record.owner, "released deployment lease");
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

/// Attempts to claim the deployment lease. A live lease held by another
/// executor yields [`LeaseBusy`]; a stale lease is broken and retried.
pub fn acquire(
    ops: &mut dyn RemoteOps,
    lease_dir: &RemotePathBuf,
    owner: &str,
    executor: ExecutorKind,
    operation_id: &str,
) -> Result<Lease> {
    let lease_file = lease_file(lease_dir);

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
                });
            }
            false => {
                match read_lease(ops, &lease_file)? {
                    Some(record) if !is_stale(&record) => {
                        return Err(LeaseBusy(record).into());
                    }
                    Some(_) => {
                        info!("breaking stale deployment lease");
                        break_lease(ops, lease_dir, &lease_file)?;
                    }
                    // An empty husk or an unreadable lease file: treat as
                    // stale and clean up.
                    None => break_lease(ops, lease_dir, &lease_file)?,
                }
            }
        }

        if attempt == CLAIM_RETRIES {
            bail!("deployment lease could not be acquired after clearing stale leases");
        }
    }

    unreachable!()
}

/// Starts a heartbeat that keeps the lease alive through a second
/// connection. The heartbeat rewrites the record with a fresh timestamp at
/// [`HEARTBEAT_INTERVAL`]; failures only mean the lease can go stale, which
/// is safe.
pub fn start_heartbeat(
    lease: &mut Lease,
    mut connect: impl FnMut() -> Result<Box<dyn RemoteOps>> + Send + 'static,
) {
    let stop = lease.stop.clone();
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
            guard = cv.wait_timeout(guard, HEARTBEAT_INTERVAL).unwrap().0;
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
        super::acquire(remote, &dir(), "local:1", ExecutorKind::Local, "op-1")
    }

    fn plant_lease(remote: &mut MemoryRemote, heartbeat_at: DateTime<Utc>) {
        remote.dirs.insert(LEASE_DIR.to_owned());
        let record = LeaseRecord {
            owner: "other".to_owned(),
            executor: ExecutorKind::Worker,
            operation_id: "op-old".to_owned(),
            acquired_at: heartbeat_at,
            heartbeat_at,
            ttl_secs: LEASE_TTL.as_secs(),
        };
        remote.put_file(LEASE_FILE, &serde_json::to_vec(&record).unwrap());
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
        assert_eq!(busy.0.owner, "other");
    }

    #[test]
    fn stale_lease_is_broken_and_replaced() {
        let mut remote = MemoryRemote::new();
        let expired = Utc::now()
            - ChronoDuration::from_std(LEASE_TTL + STALE_SKEW + Duration::from_secs(60)).unwrap();
        plant_lease(&mut remote, expired);

        let lease = acquire(&mut remote).unwrap();
        assert_eq!(lease.record.owner, "local:1");
        let stored: LeaseRecord =
            serde_json::from_slice(remote.contents(LEASE_FILE).unwrap()).unwrap();
        assert_eq!(stored.owner, "local:1");
    }

    #[test]
    fn empty_lease_husk_is_cleared() {
        let mut remote = MemoryRemote::new();
        remote.dirs.insert(LEASE_DIR.to_owned());

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
