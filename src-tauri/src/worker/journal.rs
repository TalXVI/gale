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

use crate::profile::{
    export::ModRevision,
    server::{
        settings::RestartPolicy,
        state::{OperationKind, OperationRecord},
    },
};
use crate::worker::api::BusyOperation;

pub const JOURNAL_FILE: &str = "gale-worker-state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkerJournal {
    /// The newest publication `updated_at` the worker has observed.
    /// Observation only advances this marker; it never acknowledges
    /// deployment, so a failed or skipped revision stays recoverable.
    pub last_seen_revision: Option<DateTime<Utc>>,
    /// A publication whose mod payload is not yet confirmed deployed on
    /// the server. Retained across restarts and retried with backoff
    /// until it lands. Config state is never owed: the server is
    /// authoritative for its config files after setup, so the marker
    /// only ever tracks the mod payload.
    pub pending: Option<PendingWork>,
    /// The newest publication revision whose mod payload is confirmed
    /// deployed on the server. A deployment that skips the mods phase —
    /// an explicit config push — never advances it.
    pub last_deployed_revision: Option<DateTime<Utc>>,
    /// The mod revision the remote deployment state last reported as
    /// applied, mirrored from `.gale-server-state.json` after each
    /// deployment and on refreshed status reads. It lets a new
    /// publication's mod revision be classified as owed or already
    /// deployed without opening a remote session.
    #[serde(default)]
    pub deployed_mods_revision: Option<ModRevision>,
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
    /// The last deployment failure, reported through the status endpoint.
    /// Legacy journals also used this field for `poll failed: ...` errors.
    pub last_error: Option<String>,
    /// The current publication-poll failure, cleared by the next successful poll.
    pub poll_error: Option<String>,
    /// An operation that was in flight when the worker stopped. The remote
    /// lease is the real lock; this marker is diagnostic only.
    pub interrupted_operation: Option<BusyOperation>,
}

impl Default for WorkerJournal {
    fn default() -> Self {
        Self {
            last_seen_revision: None,
            pending: None,
            last_deployed_revision: None,
            deployed_mods_revision: None,
            refresh_token: None,
            auto_sync: false,
            auto_mods: false,
            restart_policy: RestartPolicy::default(),
            automation_seeded: false,
            last_operation: None,
            last_error: None,
            poll_error: None,
            interrupted_operation: None,
        }
    }
}

/// A publication whose mod payload still needs to reach the server.
/// The marker's existence is the debt — there is no partially completed
/// pending work because configs are never owed: the marker is either
/// outstanding or dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingWork {
    /// The publication `updated_at` whose mod payload is owed.
    pub revision: DateTime<Utc>,
    /// The mod revision owed to the server, recorded when the
    /// publication was observed. `None` means the revision is unknown —
    /// a journal written before it was tracked — so the work stays owed
    /// until a deployment or a fresh remote read settles the truth.
    #[serde(default)]
    pub owed_mods: Option<ModRevision>,
    /// Consecutive failed deployment attempts.
    #[serde(default)]
    pub attempts: u32,
    /// Earliest time the next attempt may start (bounded backoff).
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// The last deployment failure, for status reporting.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Legacy phase flag, read only during load-time migration. Journals
    /// from the phase-scoped design wrote `modsPending: false` when the
    /// marker held nothing but config debt; config debt no longer
    /// exists, so those markers are dropped on load. Never serialized.
    #[serde(default, skip_serializing)]
    mods_pending: Option<bool>,
}

impl PendingWork {
    /// Fresh work for a newly observed publication whose mod revision
    /// differs from the remote's last-reported one.
    pub fn new(revision: DateTime<Utc>, owed_mods: ModRevision) -> Self {
        Self {
            revision,
            owed_mods: Some(owed_mods),
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
            mods_pending: None,
        }
    }
}

impl WorkerJournal {
    /// Separates poll errors written by older workers into their own field.
    fn migrate_legacy_poll_error(&mut self) {
        let Some(message) = self
            .last_error
            .as_deref()
            .and_then(|error| error.strip_prefix("poll failed: "))
        else {
            return;
        };
        if self.poll_error.is_none() {
            self.poll_error = Some(format!(
                "Publication check failed: {}",
                message.split(": ").next().unwrap_or(message)
            ));
        }
        self.last_error = self
            .pending
            .as_ref()
            .and_then(|work| work.last_error.as_ref())
            .map(|error| format!("automatic deployment failed: {error}"));
    }

    /// Discharges markers written by the phase-scoped design that owe
    /// nothing but config evaluation (`modsPending: false`). Config debt
    /// no longer exists, so those markers would otherwise sit forever.
    /// Their publications' mod payloads were already deployed, which is
    /// all `last_deployed_revision` tracks now.
    fn migrate_config_debt(&mut self) {
        let Some(work) = self.pending.take() else {
            return;
        };
        if work.mods_pending == Some(false) {
            self.last_deployed_revision = Some(
                self.last_deployed_revision
                    .map_or(work.revision, |prev| prev.max(work.revision)),
            );
        } else {
            self.pending = Some(work);
        }
    }

    /// Records the latest publication. The worker only owes the mod
    /// payload: when the publication's mod revision already matches the
    /// remote's last-reported one the publication settles immediately,
    /// regardless of any config differences — the server is
    /// authoritative for its config files.
    pub fn observe_publication(&mut self, revision: DateTime<Utc>, mods_revision: &ModRevision) {
        self.last_seen_revision = Some(revision);
        self.pending = (self.deployed_mods_revision.as_ref() != Some(mods_revision))
            .then(|| PendingWork::new(revision, mods_revision.clone()));
        if self.pending.is_none() {
            self.last_deployed_revision = Some(revision);
        }
    }

    /// Records a deployment against outstanding work. A deployment
    /// discharges the pending publication only when it actually ran the
    /// mods phase for a covering publication (`pending.revision <=
    /// publication` — deploying a newer publication supersedes the older
    /// one's work), or when the remote state it read back already
    /// reports the owed mod revision — another executor may have
    /// deployed it.
    ///
    /// `remote_mods` is the remote state's post-deployment mod revision,
    /// mirrored for classifying future observations.
    ///
    /// `last_deployed_revision` names the newest publication whose mod
    /// payload is confirmed on the server: a config-only operation never
    /// advances it.
    pub fn acknowledge_deployment(
        &mut self,
        publication: DateTime<Utc>,
        mods_deployed: bool,
        remote_mods: Option<ModRevision>,
    ) {
        self.deployed_mods_revision = remote_mods;

        let settled = self.pending.as_ref().is_some_and(|work| {
            (mods_deployed && work.revision <= publication)
                || (work.owed_mods.is_some() && work.owed_mods == self.deployed_mods_revision)
        });
        let resolved_revision = settled
            .then(|| self.pending.take())
            .flatten()
            .map(|work| work.revision);

        // The deployment itself confirms `publication`'s mod payload;
        // a settled marker may cover a newer publication than it.
        if let Some(revision) = mods_deployed
            .then_some(publication)
            .into_iter()
            .chain(resolved_revision)
            .max()
        {
            self.last_deployed_revision = Some(
                self.last_deployed_revision
                    .map_or(revision, |prev| prev.max(revision)),
            );
        }
    }

    /// Merges a freshly read remote state into the journal. The mod
    /// mirror tracks what the remote reports, and pending work it
    /// already satisfies is discharged without a deployment — a manual
    /// deployment from another executor settles it here.
    ///
    /// Owed work whose revision was never recorded (a journal written
    /// before `owed_mods` was tracked) stays owed: the remote having *a*
    /// revision deployed does not prove it is the publication's.
    pub fn observe_remote_mods(&mut self, remote_mods: &Option<ModRevision>) {
        self.deployed_mods_revision = remote_mods.clone();

        if let Some(work) = self.pending.as_ref()
            && work.owed_mods.is_some()
            && work.owed_mods == *remote_mods
        {
            let revision = work.revision;
            self.pending = None;
            self.last_deployed_revision = Some(
                self.last_deployed_revision
                    .map_or(revision, |prev| prev.max(revision)),
            );
        }
    }
}

/// The journal state plus the file path it persists to. The mutex makes
/// the API handlers and the poll loop take turns on the state.
pub struct Journal {
    path: PathBuf,
    pub state: Mutex<WorkerJournal>,
}

impl Journal {
    pub fn load(state_dir: &std::path::Path) -> Result<Self> {
        let path = state_dir.join(JOURNAL_FILE);
        let mut state: WorkerJournal = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("worker journal is not valid JSON")?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => WorkerJournal::default(),
            Err(err) => return Err(err).context("failed to read worker journal"),
        };
        state.migrate_config_debt();
        state.migrate_legacy_poll_error();

        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Persists the current journal state through temp file + rename. The
    /// journal holds a rotated refresh token, so the file gets owner-only
    /// permissions, re-applied after the rename so atomic replacement
    /// cannot widen them.
    pub fn save(&self, state: &WorkerJournal) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        let tmp = self.path.with_extension("json.tmp");

        {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).truncate(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&tmp)
                .context("failed to create worker journal")?;
            use std::io::Write;
            file.write_all(&bytes)
                .context("failed to write worker journal")?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }

        std::fs::rename(&tmp, &self.path).context("failed to persist worker journal")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
                .context("failed to restrict worker journal permissions")?;
        }
        Ok(())
    }

    /// Records a completed operation.
    pub async fn record_operation(&self, record: OperationRecord) -> Result<()> {
        let mut state = self.state.lock().await;
        state.last_operation = Some(record);
        state.interrupted_operation = None;
        self.save(&state)
    }

    /// Records a publication-poll error for status reporting.
    pub async fn record_poll_error(&self, error: Option<String>) -> Result<()> {
        let mut state = self.state.lock().await;
        state.poll_error = error;
        self.save(&state)
    }
}

/// The in-flight marker recorded before an operation starts. `kind` says
/// whether the poll loop or an API request started it.
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
            external_restart_acknowledged: false,
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
        assert!(state.pending.is_none());
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

        // The cleared marker survives reload, so a restarted worker
        // doesn't report a phantom in-flight operation.
        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert!(state.interrupted_operation.is_none());
    }

    #[tokio::test]
    async fn poll_and_deployment_errors_survive_restart_independently() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        {
            let mut state = journal.state.lock().await;
            state.last_error = Some("automatic deployment failed: upload failed".to_owned());
            journal.save(&state).unwrap();
        }
        journal
            .record_poll_error(Some(
                "Publication check failed: sync token request failed".to_owned(),
            ))
            .await
            .unwrap();

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(
            state.poll_error.as_deref(),
            Some("Publication check failed: sync token request failed")
        );
        assert_eq!(
            state.last_error.as_deref(),
            Some("automatic deployment failed: upload failed")
        );
        drop(state);
        journal.record_poll_error(None).await.unwrap();

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert!(state.poll_error.is_none());
        assert!(state.last_error.is_some());
    }

    #[tokio::test]
    async fn legacy_poll_errors_migrate_without_erasing_pending_deployment_failures() {
        let dir = tempfile::tempdir().unwrap();
        let mut old = serde_json::to_value(WorkerJournal::default()).unwrap();
        old.as_object_mut().unwrap().remove("pollError");
        old["lastError"] = serde_json::json!(
            "poll failed: sync token request failed: HTTP status server error (500 Internal Server Error) for url (https://example.test/api/auth/token)"
        );
        let mut work = PendingWork::new(Utc::now(), mod_rev('a'));
        work.last_error = Some("upload failed".to_owned());
        old["pending"] = serde_json::to_value(work).unwrap();
        std::fs::write(
            dir.path().join(JOURNAL_FILE),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(
            state.poll_error.as_deref(),
            Some("Publication check failed: sync token request failed")
        );
        assert_eq!(
            state.last_error.as_deref(),
            Some("automatic deployment failed: upload failed")
        );
        assert_eq!(
            state.pending.as_ref().unwrap().last_error.as_deref(),
            Some("upload failed")
        );
        drop(state);
        journal.record_poll_error(None).await.unwrap();

        let reloaded = Journal::load(dir.path()).unwrap();
        let state = reloaded.state.lock().await;
        assert!(
            state.poll_error.is_none(),
            "a recovered legacy poll stays recovered"
        );
        assert!(state.last_error.is_some());
    }

    #[tokio::test]
    async fn legacy_deployment_error_stays_a_deployment_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(JOURNAL_FILE),
            br#"{"lastError":"automatic deployment failed: upload failed"}"#,
        )
        .unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(
            state.last_error.as_deref(),
            Some("automatic deployment failed: upload failed")
        );
        assert!(state.poll_error.is_none());
    }

    #[test]
    fn malformed_journal_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(JOURNAL_FILE), b"{ not json").unwrap();
        assert!(Journal::load(dir.path()).is_err());
    }

    fn mod_rev(byte: char) -> ModRevision {
        ModRevision::try_from(byte.to_string().repeat(64)).unwrap()
    }

    #[test]
    fn same_mod_revision_settles_the_publication_regardless_of_configs() {
        // The publication carries its config fingerprint only as
        // metadata: the worker owes nothing when the server already
        // reports the publication's mod revision.
        let revision = Utc::now();
        let mut state = WorkerJournal {
            deployed_mods_revision: Some(mod_rev('a')),
            ..Default::default()
        };

        state.observe_publication(revision, &mod_rev('a'));

        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(revision));
        assert_eq!(state.last_seen_revision, Some(revision));
    }

    #[test]
    fn a_changed_mod_revision_creates_pending_work() {
        let revision = Utc::now();
        let mut state = WorkerJournal {
            deployed_mods_revision: Some(mod_rev('a')),
            ..Default::default()
        };

        state.observe_publication(revision, &mod_rev('b'));

        let work = state.pending.as_ref().unwrap();
        assert_eq!(work.revision, revision);
        assert_eq!(work.owed_mods.as_ref(), Some(&mod_rev('b')));
        assert_eq!(state.last_deployed_revision, None);
    }

    #[test]
    fn acknowledge_deployed_clears_only_covered_pending_work() {
        let mut state = WorkerJournal::default();
        let older = Utc::now() - chrono::Duration::hours(2);
        let deployed = Utc::now() - chrono::Duration::hours(1);
        let newer = Utc::now();

        // A mods deployment covering the pending revision clears it.
        state.pending = Some(PendingWork::new(older, mod_rev('a')));
        state.acknowledge_deployment(deployed, true, Some(mod_rev('b')));
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(deployed));

        // A newer pending publication survives, since the deployment
        // didn't reach it.
        state.pending = Some(PendingWork::new(newer, mod_rev('c')));
        state.acknowledge_deployment(deployed, true, Some(mod_rev('b')));
        assert_eq!(state.pending.as_ref().unwrap().revision, newer);

        // last_deployed_revision advances monotonically, never backwards.
        state.acknowledge_deployment(older, true, Some(mod_rev('a')));
        assert_eq!(state.last_deployed_revision, Some(deployed));
    }

    #[tokio::test]
    async fn a_config_only_deploy_never_discharges_owed_mods() {
        // An explicit config push runs no mods phase, so it must not
        // acknowledge a publication whose payload is still owed.
        let mut state = WorkerJournal::default();
        let revision = Utc::now();
        let owed = mod_rev('a');
        state.pending = Some(PendingWork::new(revision, owed.clone()));

        state.acknowledge_deployment(revision, false, Some(mod_rev('0')));

        let pending = state.pending.as_ref().expect("mod work remains owed");
        assert_eq!(pending.owed_mods.as_ref(), Some(&owed));
        assert_eq!(
            state.last_deployed_revision, None,
            "a config-only operation must not mark the publication deployed"
        );

        // The subsequent mods-only deployment discharges what remained.
        state.acknowledge_deployment(revision, true, Some(owed.clone()));
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(revision));
        assert_eq!(state.deployed_mods_revision.as_ref(), Some(&owed));
    }

    #[tokio::test]
    async fn observe_remote_mods_settles_already_deployed_work() {
        // A manual deployment by another executor shows up in the remote
        // state; a refreshed status read must discharge matching owed
        // mods without the worker deploying anything.
        let mut state = WorkerJournal::default();
        let revision = Utc::now();
        let owed = mod_rev('a');
        state.pending = Some(PendingWork::new(revision, owed.clone()));

        state.observe_remote_mods(&Some(mod_rev('b')));
        assert!(
            state.pending.is_some(),
            "a different revision does not settle the owed mods"
        );

        state.observe_remote_mods(&Some(owed));
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(revision));
    }

    #[tokio::test]
    async fn an_unknown_owed_revision_stays_owed() {
        // A pending marker written before `owed_mods` was tracked has no
        // recorded revision: the remote having *a* deployment does not
        // prove it is this publication's, so it stays owed until a
        // deployment settles it.
        let mut state = WorkerJournal::default();
        let revision = Utc::now();
        state.pending = Some(PendingWork {
            revision,
            mods_pending: None,
            owed_mods: None,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
        });

        state.observe_remote_mods(&Some(mod_rev('a')));
        assert!(state.pending.is_some());
    }

    #[tokio::test]
    async fn a_newer_publication_supersedes_without_acknowledging() {
        // Pending work for an older revision is discharged by a
        // deployment of a newer publication — but only when that
        // deployment actually ran the mods phase.
        let mut state = WorkerJournal::default();
        let older = Utc::now() - chrono::Duration::hours(1);
        let newer = Utc::now();
        state.pending = Some(PendingWork::new(older, mod_rev('a')));

        // A config-only push of the newer publication leaves the old
        // mod work owed.
        state.acknowledge_deployment(newer, false, Some(mod_rev('b')));
        let pending = state.pending.as_ref().expect("mods stay owed");
        assert_eq!(pending.revision, older);
        assert_eq!(state.last_deployed_revision, None);

        // A mods deploy of the newer publication clears the older
        // revision's leftover work — it is obsolete.
        state.acknowledge_deployment(newer, true, Some(mod_rev('c')));
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(newer));
    }

    #[tokio::test]
    async fn a_config_only_pending_marker_migrates_to_deployed() {
        // Journals from the phase-scoped design may hold markers that
        // owed only config evaluation (`modsPending: false`). Config
        // debt no longer exists: the marker is dropped on load and the
        // publication counts as deployed, since its mod payload was.
        let dir = tempfile::tempdir().unwrap();
        let revision = Utc::now();
        std::fs::write(
            dir.path().join(JOURNAL_FILE),
            serde_json::to_vec(&serde_json::json!({
                "lastSeenRevision": revision,
                "deployedModsRevision": mod_rev('a').as_str(),
                "evaluatedConfigRevision": "configs-a",
                "lastConfigSyncAt": "2024-01-01T00:00:00Z",
                "pending": {
                    "revision": revision,
                    "modsPending": false,
                    "configsPending": true,
                    "owedMods": mod_rev('a').as_str(),
                    "attempts": 0,
                },
            }))
            .unwrap(),
        )
        .unwrap();

        let journal = Journal::load(dir.path()).unwrap();
        {
            let mut state = journal.state.lock().await;
            assert!(
                state.pending.is_none(),
                "legacy config debt must not survive as owed work"
            );
            assert_eq!(state.last_deployed_revision, Some(revision));

            // Config divergence after migration still creates nothing.
            let later = revision + chrono::Duration::seconds(1);
            state.observe_publication(later, &mod_rev('a'));
            assert!(state.pending.is_none());
            assert_eq!(state.last_deployed_revision, Some(later));
            journal.save(&state).unwrap();
        }

        // The dropped marker and legacy config fields stay dropped.
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(JOURNAL_FILE)).unwrap()).unwrap();
        assert!(saved["pending"].is_null());
        assert!(saved.get("evaluatedConfigRevision").is_none());
        assert!(saved.get("lastConfigSyncAt").is_none());
    }

    #[tokio::test]
    async fn a_pending_marker_with_owed_mods_migrates_to_owed_work() {
        // Legacy markers with `modsPending: true` (or no flag at all,
        // written before phase tracking) still owe their mod payload.
        let dir = tempfile::tempdir().unwrap();
        let revision = Utc::now();
        std::fs::write(
            dir.path().join(JOURNAL_FILE),
            serde_json::to_vec(&serde_json::json!({
                "pending": {
                    "revision": revision,
                    "modsPending": true,
                    "configsPending": true,
                    "owedMods": mod_rev('a').as_str(),
                    "attempts": 1,
                },
            }))
            .unwrap(),
        )
        .unwrap();

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        let pending = state.pending.as_ref().expect("owed mods survive migration");
        assert_eq!(pending.owed_mods.as_ref(), Some(&mod_rev('a')));
        assert_eq!(pending.attempts, 1);
    }

    #[test]
    fn a_legacy_pending_marker_decodes_as_owed() {
        // Journals written before `owed_mods` was tracked carry no
        // revision — the marker still means the publication's mod
        // payload is owed.
        let json = serde_json::json!({
            "revision": "2024-01-01T00:00:00Z",
            "attempts": 1,
        });
        let work: PendingWork = serde_json::from_value(json).unwrap();
        assert!(work.mods_pending.is_none());
        assert!(work.owed_mods.is_none());
    }

    #[tokio::test]
    async fn pending_work_survives_a_restart() {
        // The core unattended-sync guarantee: a publication observed but
        // not yet deployed is still owed after the journal reloads.
        let dir = tempfile::tempdir().unwrap();
        let revision = Utc::now();
        {
            let journal = Journal::load(dir.path()).unwrap();
            let mut state = journal.state.lock().await;
            state.last_seen_revision = Some(revision);
            state.pending = Some(PendingWork {
                revision,
                mods_pending: None,
                owed_mods: Some(mod_rev('a')),
                attempts: 2,
                next_attempt_at: Some(revision + chrono::Duration::minutes(5)),
                last_error: Some("upload failed".to_owned()),
            });
            journal.save(&state).unwrap();
        }

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        let pending = state.pending.as_ref().unwrap();
        assert_eq!(pending.revision, revision);
        assert_eq!(pending.owed_mods.as_ref(), Some(&mod_rev('a')));
        assert_eq!(pending.attempts, 2);
        assert_eq!(pending.last_error.as_deref(), Some("upload failed"));
        assert_eq!(state.last_seen_revision, Some(revision));
        assert_eq!(state.last_deployed_revision, None);
    }
}
