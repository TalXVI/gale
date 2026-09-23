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
    /// Work observed but not yet completed successfully. Retained across
    /// restarts and retried with backoff until it lands. Completion is
    /// phase-scoped: a config-only deployment discharges the config
    /// evaluation while the mod payload stays owed.
    pub pending: Option<PendingWork>,
    /// The newest publication revision whose mod payload *and* config
    /// evaluation are both fully applied to the server. A config-only
    /// deployment does not advance it while the publication's mods are
    /// still owed.
    pub last_deployed_revision: Option<DateTime<Utc>>,
    /// The mod revision the remote deployment state last reported as
    /// applied, mirrored from `.gale-server-state.json` after each
    /// deployment and on refreshed status reads. It lets a new
    /// publication's mod revision be classified as owed or already
    /// deployed without opening a remote session.
    #[serde(default)]
    pub deployed_mods_revision: Option<ModRevision>,
    /// Managed published configs last evaluated successfully, including
    /// evaluations that needed no write or left user decisions pending.
    #[serde(default)]
    pub evaluated_config_revision: Option<String>,
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
    /// The last poll/deploy error, reported through the status endpoint.
    pub last_error: Option<String>,
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
            evaluated_config_revision: None,
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

/// What a pending publication still owes the server, tracked per phase
/// so a config-only deployment never discharges mod work it never ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingWork {
    /// The publication `updated_at` waiting to be fully applied.
    pub revision: DateTime<Utc>,
    /// Whether the publication's mod payload is not yet recorded as
    /// deployed. Cleared only by a deployment that actually ran the mods
    /// phase for a covering publication, or by a fresh remote read
    /// showing the owed revision already in place.
    ///
    /// Journals written before phase tracking decode as owed: one
    /// deployment or refreshed status read settles the truth.
    #[serde(default = "owed")]
    pub mods_pending: bool,
    /// The mod revision `mods_pending` refers to, recorded when the
    /// publication was observed. `None` means the revision is unknown
    /// (a pre-phase-tracking journal) — the flag stays owed until a
    /// deployment settles it.
    #[serde(default)]
    pub owed_mods: Option<ModRevision>,
    /// Whether config evaluation is still owed for this revision. A
    /// configs-phase deployment clears it only when no selected write
    /// failed, so partial deployments stay retryable.
    ///
    /// Journals written before phase tracking decode as owed.
    #[serde(default = "owed")]
    pub configs_pending: bool,
    /// Consecutive failed deployment attempts.
    #[serde(default)]
    pub attempts: u32,
    /// Earliest time the next attempt may start (bounded backoff).
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// The last deployment failure, for status reporting.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Phase flags decode as owed when a pre-split journal lacks them.
fn owed() -> bool {
    true
}

impl PendingWork {
    /// Fresh work for a newly observed publication. `owed_mods` records
    /// the publication's mod revision when it is not the one the remote
    /// state last reported deployed. Config work depends on its own revision.
    pub fn new(
        revision: DateTime<Utc>,
        owed_mods: Option<ModRevision>,
        configs_pending: bool,
    ) -> Self {
        Self {
            revision,
            mods_pending: owed_mods.is_some(),
            owed_mods,
            configs_pending,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
        }
    }

    /// Nothing remains owed for this publication.
    pub fn resolved(&self) -> bool {
        !self.mods_pending && !self.configs_pending
    }
}

impl WorkerJournal {
    /// Records the latest publication without treating its timestamp as
    /// evidence that either deployment phase changed.
    pub fn observe_publication(
        &mut self,
        revision: DateTime<Utc>,
        mods_revision: &ModRevision,
        config_revision: &str,
    ) {
        let owed_mods = (self.deployed_mods_revision.as_ref() != Some(mods_revision))
            .then(|| mods_revision.clone());
        let configs_pending = self.evaluated_config_revision.as_deref() != Some(config_revision);
        self.last_seen_revision = Some(revision);
        self.pending = if owed_mods.is_none() && !configs_pending {
            None
        } else {
            Some(PendingWork::new(revision, owed_mods, configs_pending))
        };
        if self.pending.is_none() {
            self.last_deployed_revision = Some(revision);
        }
    }

    /// Records a deployment against outstanding work. Clearing is
    /// phase-scoped: a deployment only discharges the phases it actually
    /// ran, and only for publications it covers (`pending.revision <=
    /// publication` — a deployment of a newer publication supersedes the
    /// older one's remaining work). `configs_done` additionally requires
    /// that no selected config write failed.
    ///
    /// `remote_mods` is the remote state's post-deployment mod revision,
    /// mirrored for classifying future observations.
    ///
    /// `last_deployed_revision` advances only when the deployment leaves
    /// nothing of that publication outstanding — a config-only operation
    /// never marks a changed mod revision as deployed.
    pub fn acknowledge_deployment(
        &mut self,
        publication: DateTime<Utc>,
        mods_deployed: bool,
        configs_done: bool,
        remote_mods: Option<ModRevision>,
        config_revision: &str,
    ) {
        self.deployed_mods_revision = remote_mods;
        if configs_done {
            self.evaluated_config_revision = Some(config_revision.to_owned());
        }

        // `last_deployed_revision` only names a publication whose phases
        // are *both* confirmed applied: either this operation ran them,
        // or the phase it skipped was already settled for that revision.
        let mut applied = (mods_deployed && configs_done).then_some(publication);

        if let Some(work) = self.pending.as_mut()
            && work.revision <= publication
        {
            if mods_deployed {
                work.mods_pending = false;
                work.owed_mods = None;
            }
            if configs_done {
                work.configs_pending = false;
            }
            if work.resolved() {
                // The pending publication is fully applied — this
                // operation covered whichever phases were still owed.
                applied = Some(applied.map_or(work.revision, |rev| rev.max(work.revision)));
                self.pending = None;
            }
        }

        if let Some(revision) = applied {
            self.last_deployed_revision = Some(
                self.last_deployed_revision
                    .map_or(revision, |prev| prev.max(revision)),
            );
        }
    }

    /// Merges a freshly read remote state into the journal. The mod
    /// mirror tracks what the remote reports, and pending mod work it
    /// already satisfies is discharged without a deployment — a manual
    /// deployment from another executor settles it here.
    ///
    /// Owed work whose revision was never recorded (a pre-phase-tracking
    /// journal) stays owed: the remote having *a* revision deployed does
    /// not prove it is the publication's.
    pub fn observe_remote_mods(&mut self, remote_mods: &Option<ModRevision>) {
        self.deployed_mods_revision = remote_mods.clone();

        if let Some(work) = self.pending.as_mut()
            && work.mods_pending
            && work.owed_mods.is_some()
            && work.owed_mods.as_ref() == remote_mods.as_ref()
        {
            work.mods_pending = false;
            work.owed_mods = None;
            if work.resolved() {
                // Settling the last owed phase means the pending
                // publication is fully applied.
                let revision = work.revision;
                self.pending = None;
                self.last_deployed_revision = Some(
                    self.last_deployed_revision
                        .map_or(revision, |prev| prev.max(revision)),
                );
            }
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

    /// Records an error for status reporting.
    pub async fn record_error(&self, error: Option<String>) -> Result<()> {
        let mut state = self.state.lock().await;
        state.last_error = error;
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
            state.evaluated_config_revision = Some("managed-configs".to_owned());
            state.automation_seeded = true;
            state.last_seen_revision = Some(Utc::now());
            journal.save(&state).unwrap();
        }

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(state.refresh_token.as_deref(), Some("rotated"));
        assert!(state.auto_sync);
        assert!(state.auto_mods);
        assert_eq!(
            state.evaluated_config_revision.as_deref(),
            Some("managed-configs")
        );
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

    fn mod_rev(byte: char) -> ModRevision {
        ModRevision::try_from(byte.to_string().repeat(64)).unwrap()
    }

    #[test]
    fn publication_phases_follow_mod_and_managed_config_revisions() {
        let first = Utc::now();
        let mut state = WorkerJournal {
            deployed_mods_revision: Some(mod_rev('a')),
            evaluated_config_revision: Some("configs-a".into()),
            ..Default::default()
        };

        state.observe_publication(first, &mod_rev('b'), "configs-a");
        let work = state.pending.as_ref().unwrap();
        assert!(work.mods_pending);
        assert!(!work.configs_pending);
        state.acknowledge_deployment(first, true, false, Some(mod_rev('b')), "configs-a");
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(first));

        let second = first + chrono::Duration::seconds(1);
        state.observe_publication(second, &mod_rev('b'), "configs-b");
        let work = state.pending.as_ref().unwrap();
        assert!(!work.mods_pending);
        assert!(work.configs_pending);
        state.acknowledge_deployment(second, false, true, Some(mod_rev('b')), "configs-b");
        assert!(state.pending.is_none());

        let third = second + chrono::Duration::seconds(1);
        state.observe_publication(third, &mod_rev('c'), "configs-c");
        let work = state.pending.as_ref().unwrap();
        assert!(work.mods_pending);
        assert!(work.configs_pending);
        state.acknowledge_deployment(third, true, false, Some(mod_rev('c')), "configs-c");
        assert!(state.pending.as_ref().unwrap().configs_pending);
        state.acknowledge_deployment(third, false, true, Some(mod_rev('c')), "configs-c");
        assert!(state.pending.is_none());
    }

    #[test]
    fn first_run_and_migrated_journal_evaluate_configs_once() {
        let revision = Utc::now();
        let mut state = WorkerJournal::default();
        state.deployed_mods_revision = Some(mod_rev('a'));
        state.observe_publication(revision, &mod_rev('a'), "configs-a");
        assert!(state.pending.as_ref().unwrap().configs_pending);
        state.acknowledge_deployment(revision, false, true, Some(mod_rev('a')), "configs-a");
        assert_eq!(
            state.evaluated_config_revision.as_deref(),
            Some("configs-a")
        );

        let migrated: WorkerJournal = serde_json::from_value(serde_json::json!({
            "deployedModsRevision": mod_rev('a').as_str(),
            "lastSeenRevision": revision,
        }))
        .unwrap();
        assert!(migrated.evaluated_config_revision.is_none());
        let mut migrated = migrated;
        migrated.observe_publication(
            revision + chrono::Duration::seconds(1),
            &mod_rev('a'),
            "configs-a",
        );
        assert!(migrated.pending.as_ref().unwrap().configs_pending);
    }

    #[tokio::test]
    async fn acknowledge_deployed_clears_only_covered_pending_work() {
        let mut state = WorkerJournal::default();
        let older = Utc::now() - chrono::Duration::hours(2);
        let deployed = Utc::now() - chrono::Duration::hours(1);
        let newer = Utc::now();

        // A full deployment covering the pending revision clears it.
        state.pending = Some(PendingWork::new(older, Some(mod_rev('a')), true));
        state.acknowledge_deployment(deployed, true, true, Some(mod_rev('b')), "config-a");
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(deployed));

        // A newer pending publication survives, since the deployment
        // didn't reach it.
        state.pending = Some(PendingWork::new(newer, Some(mod_rev('c')), true));
        state.acknowledge_deployment(deployed, true, true, Some(mod_rev('b')), "config-a");
        assert_eq!(state.pending.as_ref().unwrap().revision, newer);

        // last_deployed_revision advances monotonically, never backwards.
        state.acknowledge_deployment(older, true, true, Some(mod_rev('a')), "config-a");
        assert_eq!(state.last_deployed_revision, Some(deployed));
    }

    #[tokio::test]
    async fn a_config_only_deploy_leaves_the_mods_owed() {
        // The defect: `autoSync` on, `autoMods` off — the config pass
        // completes but the publication's mod payload never deployed.
        let mut state = WorkerJournal::default();
        let revision = Utc::now();
        let owed = mod_rev('a');
        state.pending = Some(PendingWork::new(revision, Some(owed.clone()), true));

        state.acknowledge_deployment(revision, false, true, Some(mod_rev('0')), "config-a");

        let pending = state.pending.as_ref().expect("mod work remains owed");
        assert!(pending.mods_pending);
        assert_eq!(pending.owed_mods.as_ref(), Some(&owed));
        assert!(!pending.configs_pending);
        assert_eq!(
            state.last_deployed_revision, None,
            "a config-only pass must not mark the publication deployed"
        );

        // The subsequent mods-only deployment discharges what remained.
        state.acknowledge_deployment(revision, true, false, Some(owed.clone()), "config-a");
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(revision));
        assert_eq!(state.deployed_mods_revision.as_ref(), Some(&owed));
    }

    #[tokio::test]
    async fn partial_config_work_keeps_the_revision_owed() {
        // Config writes that failed mean the evaluation did not land —
        // the phase stays owed so the retry re-runs it.
        let mut state = WorkerJournal::default();
        let revision = Utc::now();
        state.pending = Some(PendingWork::new(revision, None, true));

        state.acknowledge_deployment(revision, false, false, None, "config-a");

        let pending = state.pending.as_ref().expect("config work remains owed");
        assert!(!pending.mods_pending);
        assert!(pending.configs_pending);
        assert_eq!(state.last_deployed_revision, None);
    }

    #[tokio::test]
    async fn observe_remote_mods_settles_already_deployed_work() {
        // A manual deployment by another executor shows up in the remote
        // state; a refreshed status read must discharge matching owed
        // mods without the worker deploying anything.
        let mut state = WorkerJournal::default();
        let revision = Utc::now();
        let owed = mod_rev('a');
        state.pending = Some(PendingWork::new(revision, Some(owed.clone()), true));

        state.observe_remote_mods(&Some(mod_rev('b')));
        assert!(
            state.pending.as_ref().unwrap().mods_pending,
            "a different revision does not settle the owed mods"
        );

        state.observe_remote_mods(&Some(owed));
        let pending = state.pending.as_ref().expect("configs remain owed");
        assert!(!pending.mods_pending);
        assert!(pending.configs_pending);
    }

    #[tokio::test]
    async fn an_unknown_owed_revision_stays_owed() {
        // A pending marker written before phase tracking has no recorded
        // revision: the remote having *a* deployment does not prove it
        // is this publication's, so it stays owed until a deployment
        // settles it.
        let mut state = WorkerJournal::default();
        let revision = Utc::now();
        state.pending = Some(PendingWork {
            revision,
            mods_pending: true,
            owed_mods: None,
            configs_pending: false,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
        });

        state.observe_remote_mods(&Some(mod_rev('a')));
        assert!(state.pending.as_ref().unwrap().mods_pending);
    }

    #[tokio::test]
    async fn a_newer_publication_supersedes_without_acknowledging() {
        // Pending work for an older revision is discharged by a
        // deployment of a newer publication — but only for the phases
        // that deployment actually ran.
        let mut state = WorkerJournal::default();
        let older = Utc::now() - chrono::Duration::hours(1);
        let newer = Utc::now();
        state.pending = Some(PendingWork::new(older, Some(mod_rev('a')), true));

        // A config-only deploy of the newer publication discharges the
        // old config debt but not its mods.
        state.acknowledge_deployment(newer, false, true, Some(mod_rev('b')), "config-a");
        let pending = state.pending.as_ref().expect("mods stay owed");
        assert!(pending.mods_pending);
        assert!(!pending.configs_pending);
        assert_eq!(state.last_deployed_revision, None);

        // A full deploy of the newer publication clears everything it
        // covered — the older revision's leftover work is obsolete.
        state.acknowledge_deployment(newer, true, true, Some(mod_rev('c')), "config-a");
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(newer));
    }

    #[tokio::test]
    async fn pending_phases_survive_a_restart() {
        // The durable contract: outstanding mod work persists across a
        // worker restart without re-owing the completed config pass.
        let dir = tempfile::tempdir().unwrap();
        let revision = Utc::now();
        let owed = mod_rev('a');
        {
            let journal = Journal::load(dir.path()).unwrap();
            let mut state = journal.state.lock().await;
            state.last_seen_revision = Some(revision);
            state.pending = Some(PendingWork::new(revision, Some(owed.clone()), true));
            state.acknowledge_deployment(revision, false, true, Some(mod_rev('0')), "config-a");
            journal.save(&state).unwrap();
        }

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        let pending = state.pending.as_ref().expect("mod work survives a restart");
        assert!(pending.mods_pending);
        assert_eq!(pending.owed_mods.as_ref(), Some(&owed));
        assert!(!pending.configs_pending);
        assert_eq!(state.last_deployed_revision, None);
    }

    #[test]
    fn a_legacy_pending_marker_decodes_as_fully_owed() {
        // Journals written before phase tracking have no `modsPending`/
        // `configsPending` — they must decode as owed, not complete.
        let json = serde_json::json!({
            "revision": "2024-01-01T00:00:00Z",
            "attempts": 1,
            "nextAttemptAt": null,
            "lastError": null
        });
        let work: PendingWork = serde_json::from_value(json).unwrap();
        assert!(work.mods_pending);
        assert!(work.configs_pending);
        assert!(work.owed_mods.is_none());
        assert!(!work.resolved());
    }

    #[tokio::test]
    async fn pending_work_survives_a_restart() {
        // The core unattended-sync guarantee: a revision observed but not
        // yet deployed is still pending after the journal reloads.
        let dir = tempfile::tempdir().unwrap();
        let revision = Utc::now();
        {
            let journal = Journal::load(dir.path()).unwrap();
            let mut state = journal.state.lock().await;
            state.last_seen_revision = Some(revision);
            state.pending = Some(PendingWork {
                revision,
                mods_pending: true,
                owed_mods: Some(mod_rev('a')),
                configs_pending: true,
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
        assert_eq!(pending.attempts, 2);
        assert_eq!(pending.last_error.as_deref(), Some("upload failed"));
        assert_eq!(state.last_seen_revision, Some(revision));
        assert_eq!(state.last_deployed_revision, None);
    }
}
