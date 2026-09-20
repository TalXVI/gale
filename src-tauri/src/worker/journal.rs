//! The worker's durable state, persisted as JSON next to its config.
//!
//! The journal survives restarts and is what makes the worker idempotent:
//! a refresh token rotation, a seen publication revision, and the last
//! operation record all live here rather than in memory. Writes go through
//! a temp file + rename so a crash mid-write cannot corrupt the previous
//! state.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::profile::server::{
    settings::RestartPolicy,
    state::{OperationKind, OperationRecord},
};
use crate::worker::api::BusyOperation;

pub const JOURNAL_FILE: &str = "gale-worker-state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkerJournal {
    /// The newest publication `updated_at` the worker has observed.
    pub last_seen_revision: Option<DateTime<Utc>>,
    /// The current sync refresh token (rotated on each token grant).
    /// Seeded from `GALE_WORKER_REFRESH_TOKEN` on first run.
    pub refresh_token: Option<String>,
    pub auto_sync: bool,
    pub auto_mods: bool,
    pub restart_policy: RestartPolicy,
    /// Whether the automation flags above were seeded from the config file.
    /// The config seeds once; afterwards `/v1/config` and this journal are
    /// the source of truth so runtime changes survive restarts.
    pub automation_seeded: bool,
    pub last_operation: Option<OperationRecord>,
    /// The last poll/deploy error, surfaced through the status endpoint.
    pub last_error: Option<String>,
    /// An operation that was in flight when the worker stopped. The remote
    /// lease is the real lock; this marker is diagnostic only.
    pub interrupted_operation: Option<BusyOperation>,
}

impl Default for WorkerJournal {
    fn default() -> Self {
        Self {
            last_seen_revision: None,
            refresh_token: None,
            auto_sync: false,
            auto_mods: false,
            restart_policy: RestartPolicy::default(),
            automation_seeded: false,
            last_operation: None,
            last_error: None,
            interrupted_operation: None,
        }
    }
}

/// Journal plus its persistence path, guarded so concurrent operations and
/// the poll loop serialize on it.
pub struct Journal {
    path: PathBuf,
    pub state: Mutex<WorkerJournal>,
}

impl Journal {
    pub fn load(state_dir: &std::path::Path) -> Result<Self> {
        let path = state_dir.join(JOURNAL_FILE);
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("worker journal is not valid JSON")?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => WorkerJournal::default(),
            Err(err) => return Err(err).context("failed to read worker journal"),
        };

        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Persists the current journal state through temp + rename.
    pub fn save(&self, state: &WorkerJournal) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).context("failed to write worker journal")?;
        std::fs::rename(&tmp, &self.path).context("failed to persist worker journal")
    }

    /// Records a completed operation.
    pub async fn record_operation(&self, record: OperationRecord) -> Result<()> {
        let mut state = self.state.lock().await;
        state.last_operation = Some(record);
        state.interrupted_operation = None;
        self.save(&state)
    }

    /// Records an error for status reporting.
    pub async fn record_error(&self, error: Option<String>) -> Result<()> {
        let mut state = self.state.lock().await;
        state.last_error = error;
        self.save(&state)
    }
}

/// What kind of work the poll loop wants to start, used to record the
/// in-flight marker before the operation runs.
pub fn busy_marker(id: String, kind: OperationKind) -> BusyOperation {
    BusyOperation {
        id,
        kind,
        started_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::server::state::{
        ExecutorKind, OperationStatus, OperationSummary, RestartOutcome,
    };

    fn operation(id: &str) -> OperationRecord {
        OperationRecord {
            id: id.to_owned(),
            executor: ExecutorKind::Worker,
            kind: OperationKind::Automatic,
            worker_id: Some("test".to_owned()),
            publication_revision: None,
            mods_revision: None,
            status: OperationStatus::Succeeded,
            summary: OperationSummary::default(),
            restart: RestartOutcome::NotRequired,
            error: None,
            started_at: Utc::now(),
            finished_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn missing_journal_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;

        assert!(!state.auto_sync);
        assert!(!state.automation_seeded);
        assert!(state.refresh_token.is_none());
        assert!(state.last_operation.is_none());
    }

    #[tokio::test]
    async fn journal_round_trips_durable_state() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = Journal::load(dir.path()).unwrap();
            let mut state = journal.state.lock().await;
            state.refresh_token = Some("rotated".to_owned());
            state.auto_sync = true;
            state.auto_mods = true;
            state.automation_seeded = true;
            state.last_seen_revision = Some(Utc::now());
            journal.save(&state).unwrap();
        }

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(state.refresh_token.as_deref(), Some("rotated"));
        assert!(state.auto_sync);
        assert!(state.auto_mods);
        assert!(state.automation_seeded);
        assert!(state.last_seen_revision.is_some());
    }

    #[tokio::test]
    async fn record_operation_clears_the_interrupted_marker() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        {
            let mut state = journal.state.lock().await;
            state.interrupted_operation =
                Some(busy_marker("op-1".to_owned(), OperationKind::Automatic));
            journal.save(&state).unwrap();
        }

        journal.record_operation(operation("op-1")).await.unwrap();

        let state = journal.state.lock().await;
        assert!(state.interrupted_operation.is_none());
        assert_eq!(state.last_operation.as_ref().unwrap().id, "op-1");
        drop(state);

        // The cleared marker survives reload — a restarted worker doesn't
        // report a phantom in-flight operation.
        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert!(state.interrupted_operation.is_none());
    }

    #[tokio::test]
    async fn record_error_persists() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        journal
            .record_error(Some("poll failed".to_owned()))
            .await
            .unwrap();

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(state.last_error.as_deref(), Some("poll failed"));
    }

    #[test]
    fn malformed_journal_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(JOURNAL_FILE), b"{ not json").unwrap();
        assert!(Journal::load(dir.path()).is_err());
    }
}
