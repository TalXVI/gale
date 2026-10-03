//! The worker's durable state, persisted as JSON next to its config.
//!
//! The journal survives restarts and is what makes the worker idempotent:
//! a refresh token rotation, a seen publication revision, and the last
//! operation record all live here rather than in memory. Writes go through
//! a temp file flushed to disk and renamed so a crash mid-write cannot
//! corrupt the previous state.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::profile::{
    export::ModRevision,
    server::{settings::RestartPolicy, state::OperationRecord},
};

pub const JOURNAL_FILE: &str = "gale-worker-state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkerJournal {
    /// A publication whose mod payload is not yet confirmed deployed on
    /// the server. Retained across restarts and retried with backoff
    /// until it lands. Config state is never owed: the server is
    /// authoritative for its config files after setup, so the marker
    /// only ever tracks the mod payload.
    pub pending: Option<PendingWork>,
    /// The newest publication revision whose mod payload is confirmed
    /// deployed on the server. A deployment that skips the mods phase,
    /// such as an explicit config push, never advances it.
    pub last_deployed_revision: Option<DateTime<Utc>>,
    /// The mod revision the remote deployment state last reported as
    /// applied, mirrored from `.gale-server-state.json` after each
    /// deployment and on refreshed status reads. It lets a new
    /// publication's mod revision be classified as owed or already
    /// deployed without opening a remote session.
    pub deployed_mods_revision: Option<ModRevision>,
    /// The current sync refresh token (rotated on each token grant).
    /// Seeded from `GALE_WORKER_REFRESH_TOKEN` on first run. When the
    /// sync service rejects it, this still records the rejected value so
    /// the same credential is never presented twice.
    pub refresh_token: Option<String>,
    /// Set when the sync service rejected `refresh_token` (HTTP 400/401).
    /// Only a new sign-in recovers, so no token requests are made until
    /// setup replaces the credential or a different seed is supplied.
    pub sync_reauthorization_required: bool,
    pub auto_deploy_mods: bool,
    pub restart_policy: RestartPolicy,
    /// Whether automation and restart policy were seeded from the config file.
    /// The config seeds once; afterwards `/v1/config` and this journal are
    /// the source of truth so runtime changes survive restarts.
    pub automation_seeded: bool,
    pub last_operation: Option<OperationRecord>,
    /// The last deployment failure, reported through the status endpoint.
    pub last_error: Option<String>,
    /// The current publication-poll failure, cleared by a successful poll
    /// or by setup replacing the sign-in that caused a latched rejection.
    pub poll_error: Option<String>,
}

/// A publication whose mod payload still needs to reach the server.
/// The marker records owed work. There is no partially completed
/// pending work because configs are never owed: the marker is either
/// outstanding or dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingWork {
    /// The publication `updated_at` whose mod payload is owed.
    pub revision: DateTime<Utc>,
    /// The mod revision owed to the server.
    pub owed_mods: ModRevision,
    /// Consecutive failed deployment attempts.
    #[serde(default)]
    pub attempts: u32,
    /// Earliest time the next attempt may start (bounded backoff).
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
}

impl PendingWork {
    /// Fresh work for a newly observed publication whose mod revision
    /// differs from the remote's last-reported one.
    pub fn new(revision: DateTime<Utc>, owed_mods: ModRevision) -> Self {
        Self {
            revision,
            owed_mods,
            attempts: 0,
            next_attempt_at: None,
        }
    }
}

impl WorkerJournal {
    pub fn observed_revision(&self) -> Option<DateTime<Utc>> {
        self.pending
            .as_ref()
            .map(|work| work.revision)
            .into_iter()
            .chain(self.last_deployed_revision)
            .max()
    }

    /// Records the latest publication. The worker only owes the mod
    /// payload: when the publication's mod revision already matches the
    /// remote's last-reported one the publication settles immediately,
    /// regardless of any config differences. The server is
    /// authoritative for its config files.
    pub fn observe_publication(&mut self, revision: DateTime<Utc>, mods_revision: &ModRevision) {
        self.pending = (self.deployed_mods_revision.as_ref() != Some(mods_revision))
            .then(|| PendingWork::new(revision, mods_revision.clone()));
        if self.pending.is_none() {
            self.last_deployed_revision = Some(revision);
        }
    }

    /// Records a deployment against outstanding work. A deployment
    /// discharges the pending publication only when it actually ran the
    /// mods phase for a covering publication (`pending.revision <=
    /// publication`, since deploying a newer publication supersedes the older
    /// one's work), or when the remote state it read back already
    /// reports the owed mod revision. Another executor may have
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
                || self.deployed_mods_revision.as_ref() == Some(&work.owed_mods)
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
    /// already satisfies is discharged without a deployment. A manual
    /// deployment from another executor settles it here.
    ///
    pub fn observe_remote_mods(&mut self, remote_mods: &Option<ModRevision>) {
        self.deployed_mods_revision = remote_mods.clone();

        if let Some(work) = self.pending.as_ref()
            && remote_mods.as_ref() == Some(&work.owed_mods)
        {
            let revision = work.revision;
            self.pending = None;
            self.last_deployed_revision = Some(
                self.last_deployed_revision
                    .map_or(revision, |prev| prev.max(revision)),
            );
        }
    }

    /// Installs a freshly issued sync credential (same-binding setup).
    /// Clears the poll error for a latched credential rejection, including
    /// the generic token error retained by older workers. Other poll errors
    /// and all operational state are kept.
    pub fn replace_sync_credential(&mut self, refresh_token: String) {
        if self.sync_reauthorization_required
            && self
                .poll_error
                .as_deref()
                .and_then(|error| error.strip_prefix("Publication check failed: "))
                .is_some_and(|error| {
                    error == super::sync_client::SYNC_REAUTHORIZATION_REQUIRED
                        || error == "sync token request failed"
                })
        {
            self.poll_error = None;
        }
        self.refresh_token = Some(refresh_token);
        self.sync_reauthorization_required = false;
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
        let state: WorkerJournal = match std::fs::read(&path) {
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
    /// journal holds a rotated refresh token, so the temp file is made
    /// owner-only before the rename publishes it. A leftover temp file from
    /// a crash keeps its old mode on reopen, hence the explicit chmod.
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
            file.sync_all().context("failed to flush worker journal")?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .context("failed to restrict worker journal permissions")?;
        }

        std::fs::rename(&tmp, &self.path).context("failed to persist worker journal")
    }

    /// Records a completed operation.
    pub async fn record_operation(&self, record: OperationRecord) -> Result<()> {
        let mut state = self.state.lock().await;
        state.last_operation = Some(record);
        self.save(&state)
    }

    /// Records a publication-poll error for status reporting.
    pub async fn record_poll_error(&self, error: Option<String>) -> Result<()> {
        let mut state = self.state.lock().await;
        state.poll_error = error;
        self.save(&state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::server::state::{
        ExecutorKind, OperationKind, OperationStatus, OperationSummary, RestartOutcome,
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
    async fn record_operation_persists_the_latest_record() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        journal.record_operation(operation("op-1")).await.unwrap();

        let state = journal.state.lock().await;
        assert_eq!(state.last_operation.as_ref().unwrap().id, "op-1");
        drop(state);
        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(state.last_operation.as_ref().unwrap().id, "op-1");
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

    #[test]
    fn malformed_journal_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(JOURNAL_FILE), b"{ not json").unwrap();
        assert!(Journal::load(dir.path()).is_err());
    }

    /// A fresh sign-in clears the rejected credential's poll error;
    /// owed work, deployed revisions, automation, and history survive.
    #[tokio::test]
    async fn replace_sync_credential_preserves_operational_state() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path()).unwrap();
        let revision = Utc::now();
        let before = {
            let mut state = journal.state.lock().await;
            state.pending = Some(PendingWork::new(revision, mod_rev('b')));
            state.pending.as_mut().unwrap().attempts = 3;
            state.pending.as_mut().unwrap().next_attempt_at =
                Some(revision + chrono::Duration::minutes(5));
            state.last_deployed_revision = Some(revision - chrono::Duration::hours(1));
            state.deployed_mods_revision = Some(mod_rev('a'));
            state.refresh_token = Some("rejected-token".to_owned());
            state.sync_reauthorization_required = true;
            state.auto_deploy_mods = true;
            state.restart_policy = RestartPolicy::WhenEmpty;
            state.automation_seeded = true;
            state.last_operation = Some(operation("op-1"));
            state.last_error = Some("automatic deployment failed: upload failed".to_owned());
            state.poll_error = Some(format!(
                "Publication check failed: {}",
                crate::worker::sync_client::SYNC_REAUTHORIZATION_REQUIRED
            ));
            journal.save(&state).unwrap();
            state.clone()
        };

        {
            let mut state = journal.state.lock().await;
            state.replace_sync_credential("fresh-token".to_owned());
            journal.save(&state).unwrap();
        }

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        assert_eq!(state.refresh_token.as_deref(), Some("fresh-token"));
        assert!(!state.sync_reauthorization_required);
        let mut expected = before;
        expected.refresh_token = Some("fresh-token".to_owned());
        expected.sync_reauthorization_required = false;
        expected.poll_error = None;
        assert_eq!(
            serde_json::to_value(&*state).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "only the credential and its rejection state changed"
        );
    }

    #[test]
    fn replace_sync_credential_clears_only_latched_authentication_poll_errors() {
        let rejection = format!(
            "Publication check failed: {}",
            crate::worker::sync_client::SYNC_REAUTHORIZATION_REQUIRED
        );
        for error in [
            rejection.as_str(),
            "Publication check failed: sync token request failed",
            "Publication check failed: failed to reach the sync service",
            "Publication check failed: canonical publication failed validation",
            "Publication check failed: failed to download the publication",
        ] {
            for latched in [false, true] {
                let mut state = WorkerJournal {
                    refresh_token: Some("rejected-token".to_owned()),
                    sync_reauthorization_required: latched,
                    poll_error: Some(error.to_owned()),
                    ..Default::default()
                };
                state.replace_sync_credential("fresh-token".to_owned());
                let authentication_error = error == rejection
                    || error == "Publication check failed: sync token request failed";
                assert_eq!(
                    state.poll_error.as_deref(),
                    if latched && authentication_error {
                        None
                    } else {
                        Some(error)
                    },
                    "latched={latched}, error={error}"
                );
            }
        }
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
        assert_eq!(state.observed_revision(), Some(revision));
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
        assert_eq!(work.owed_mods, mod_rev('b'));
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
        assert_eq!(pending.owed_mods, owed);
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
    async fn a_newer_publication_supersedes_without_acknowledging() {
        // Pending work for an older revision is discharged by a
        // deployment of a newer publication, but only when that
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

        // Deploying newer mods clears pending work for the older revision.
        state.acknowledge_deployment(newer, true, Some(mod_rev('c')));
        assert!(state.pending.is_none());
        assert_eq!(state.last_deployed_revision, Some(newer));
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
            state.pending = Some(PendingWork {
                revision,
                owed_mods: mod_rev('a'),
                attempts: 2,
                next_attempt_at: Some(revision + chrono::Duration::minutes(5)),
            });
            journal.save(&state).unwrap();
        }

        let journal = Journal::load(dir.path()).unwrap();
        let state = journal.state.lock().await;
        let pending = state.pending.as_ref().unwrap();
        assert_eq!(pending.revision, revision);
        assert_eq!(pending.owed_mods, mod_rev('a'));
        assert_eq!(pending.attempts, 2);
        assert_eq!(state.observed_revision(), Some(revision));
        assert_eq!(state.last_deployed_revision, None);
    }
}
