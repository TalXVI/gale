use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{
    paths::{DeployPathBuf, RemotePath},
    remote::RemoteOps,
    spec::DeploymentSpec,
};
use crate::profile::{
    export::{ConfigPath, ContentHash, ModRevision, R2Mod},
    sync::{AppliedFile, ConfigUpdatePolicy, PendingConfigReason},
};

pub const FILE_NAME: &str = ".gale-server-state.json";
pub const LEGACY_FILE_NAME: &str = ".gale-server-manifest.json";
pub const LEASE_DIR_NAME: &str = ".gale-deploy.lock";
pub const LEASE_FILE_NAME: &str = "lease.json";
pub const VERSION: u32 = 2;
/// Upper bound for the remote state file. Reads are refused past it so a
/// corrupt or hostile file cannot cause an unbounded download.
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HISTORY: usize = 10;

/// A Gale-owned remote payload file recorded by a completed deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedFile {
    pub hash: String,
    pub size: u64,
}

/// Which executor ran an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutorKind {
    Local,
    Worker,
}

/// How an operation was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationKind {
    Manual,
    Automatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationStatus {
    Succeeded,
    /// The operation ran to completion but part of the selected work did
    /// not land — e.g. a config write failed. Per-file records describe
    /// exactly what succeeded; the outstanding work stays retryable.
    Partial,
    /// The deployment did not finish cleanly; applied-file records still
    /// reflect exactly what succeeded.
    Failed,
}

/// What happened to the game server process after deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestartOutcome {
    /// No payload change, so no restart was needed.
    NotRequired,
    /// The configured policy leaves the restart to the user.
    AwaitingManual,
    /// Players were present or presence was unknown, so a WhenEmpty restart
    /// was deferred.
    AwaitingEmpty,
    /// A restart was issued and verified to the extent the provider allows.
    Restarted,
    /// A restart was issued but its result could not be verified.
    StartupUnverified,
    /// The restart attempt itself failed.
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationSummary {
    pub uploaded_files: usize,
    pub uploaded_bytes: u64,
    pub removed_files: usize,
    pub config_writes: usize,
    pub unchanged_files: usize,
}

/// A completed deployment attempt recorded on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub id: String,
    pub executor: ExecutorKind,
    pub kind: OperationKind,
    pub worker_id: Option<String>,
    /// Publication timestamp (`updatedAt`) this operation applied.
    pub publication_revision: Option<DateTime<Utc>>,
    /// The published mod revision the operation targeted.
    pub mods_revision: Option<ModRevision>,
    pub status: OperationStatus,
    pub summary: OperationSummary,
    pub restart: RestartOutcome,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

/// The authoritative record of what a dedicated server's managed content
/// looks like, stored on the server itself at [`FILE_NAME`].
///
/// Both executors (Local mode and workers) read and update this single
/// record, so switching modes never loses synchronization history. It is
/// deliberately not a single timestamp: mod and per-file config revisions
/// are tracked independently, as are pending/declined decisions, ownership
/// of deployed payload files, and recent operation results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerDeploymentState {
    pub version: u32,
    /// Bumped by every completed operation; part of the plan hash so a plan
    /// approved before another operation is detected as stale.
    #[serde(default)]
    pub operation_seq: u64,
    /// The published mod revision whose payload is fully deployed. Only
    /// advances once the whole payload phase succeeded.
    #[serde(default)]
    pub mods_revision: Option<ModRevision>,
    /// The published mod set `mods_revision` describes, for display and
    /// recovery information.
    #[serde(default)]
    pub deployed_mods: Vec<R2Mod>,
    /// Payload files Gale deployed and therefore owns, keyed by deploy path.
    /// Together with mirrored extras these are the only paths a deployment
    /// may remove.
    #[serde(default)]
    pub files: BTreeMap<DeployPathBuf, OwnedFile>,
    /// Per-config application records mirroring the client-side
    /// [`AppliedFile`] semantics: the applied published revision, what Gale
    /// last wrote (also used for package-default seeds), the declined
    /// revision, and the file's persistent update policy.
    #[serde(default)]
    pub config: BTreeMap<ConfigPath, AppliedFile>,
    /// Published configs awaiting a decision because applying them would
    /// overwrite a remote modification or recreate a remote deletion.
    #[serde(default)]
    pub pending: BTreeMap<ConfigPath, PendingConfigReason>,
    /// Whether deployed payload changes still need a server restart.
    #[serde(default)]
    pub restart_required: bool,
    #[serde(default)]
    pub last_operation: Option<OperationRecord>,
    #[serde(default)]
    pub history: Vec<OperationRecord>,
}

/// The deployment manifest written by the previous implementation. Read once
/// to adopt its ownership records, then superseded by [`FILE_NAME`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyManifest {
    version: u32,
    #[serde(default)]
    files: BTreeMap<DeployPathBuf, OwnedFile>,
}

/// State plus warnings produced while reading it (dropped out-of-scope
/// entries, adopted legacy manifests).
pub struct LoadedState {
    pub state: ServerDeploymentState,
    pub warnings: Vec<String>,
    /// Whether the on-disk state predates the current format and should be
    /// rewritten on the next persist.
    pub migrated: bool,
}

impl ServerDeploymentState {
    /// Validates the state's authority scope against the deployment spec.
    ///
    /// Payload entries outside [`DeploymentSpec::valid_owned_path`] and
    /// config entries outside the config dirs are dropped rather than
    /// trusted: a tampered state file can narrow Gale's own records but must
    /// never widen deletion authority to arbitrary remote paths.
    fn validate(mut self, spec: &DeploymentSpec, warnings: &mut Vec<String>) -> Result<Self> {
        ensure!(
            self.version == VERSION,
            "unsupported remote deployment state version {}",
            self.version
        );

        self.files.retain(|path, _| {
            let valid = spec.valid_owned_path(path);
            if !valid {
                warnings.push(format!(
                    "remote state entry '{path}' is outside Gale's managed scope and was ignored"
                ));
            }
            valid
        });

        let retain_config = |path: &ConfigPath| {
            DeployPathBuf::new(path.as_str())
                .map(|path| spec.is_config(path.as_path()))
                .unwrap_or(false)
        };

        self.config.retain(|path, _| {
            let valid = retain_config(path);
            if !valid {
                warnings.push(format!(
                    "remote state config entry '{path}' is outside the config directories and was ignored"
                ));
            }
            valid
        });
        self.pending.retain(|path, _| retain_config(path));

        Ok(self)
    }

    /// Mirrors [`crate::profile::sync::apply`]'s `record_applied`: the file
    /// now holds the published revision on the server.
    pub fn record_applied(&mut self, path: &ConfigPath, hash: &ContentHash) {
        let record = self.config.entry(path.clone()).or_default();
        record.applied = Some(hash.clone());
        record.written = Some(hash.clone());
        record.declined = None;
        record.policy_set_at = None;
        self.pending.remove(path);
    }

    /// Records that the published `hash` of `path` was declined. Declines
    /// are revision-keyed, so a newer publication requires a fresh decision.
    pub fn record_declined(&mut self, path: &ConfigPath, hash: &ContentHash) {
        self.pending.remove(path);
        let record = self.config.entry(path.clone()).or_default();
        if record.declined.as_ref() != Some(hash) {
            record.declined = Some(hash.clone());
            record.policy_set_at = None;
        }
    }

    /// Records that `path` awaits a decision for `reason`. A persistent
    /// policy set while pending is pinned at the conflicting revision so it
    /// governs future updates rather than the current conflict, matching
    /// `preserve_pending_policy_boundaries` on the client.
    pub fn record_pending(
        &mut self,
        path: &ConfigPath,
        hash: &ContentHash,
        reason: PendingConfigReason,
    ) {
        self.pending.insert(path.clone(), reason);
        let record = self.config.entry(path.clone()).or_default();
        record.declined = None;
        if record.policy != ConfigUpdatePolicy::Ask && record.policy_set_at.is_none() {
            record.policy_set_at = Some(hash.clone());
        }
    }

    /// Drops pending entries for files no longer published.
    pub fn retain_published(&mut self, published: &BTreeMap<ConfigPath, ContentHash>) {
        self.pending.retain(|path, _| published.contains_key(path));
    }

    pub fn record_operation(&mut self, record: OperationRecord) {
        self.operation_seq += 1;
        self.history.insert(0, record);
        self.history.truncate(MAX_HISTORY);
        self.last_operation = self.history.first().cloned();
    }
}

/// Reads and validates the remote deployment state.
///
/// A missing state file falls back to the legacy deployment manifest and
/// adopts its ownership records. An unparsable state file is an error —
/// deploying without knowing Gale's ownership could delete foreign files.
pub fn read_state(
    ops: &mut dyn RemoteOps,
    spec: &DeploymentSpec,
    state_remote_path: &RemotePath,
    legacy_remote_path: &RemotePath,
) -> Result<LoadedState> {
    let mut warnings = Vec::new();

    if let Some(bytes) = ops.read(state_remote_path, MAX_STATE_BYTES)? {
        let state: ServerDeploymentState =
            serde_json::from_slice(&bytes).context("remote Gale deployment state is invalid")?;
        let state = state.validate(spec, &mut warnings)?;
        return Ok(LoadedState {
            state,
            warnings,
            migrated: false,
        });
    }

    match ops.read(legacy_remote_path, MAX_STATE_BYTES)? {
        Some(bytes) => {
            let legacy: LegacyManifest = serde_json::from_slice(&bytes)
                .context("remote Gale deployment manifest is invalid")?;
            ensure!(
                legacy.version == 1,
                "unsupported remote Gale manifest version {}",
                legacy.version
            );

            warnings
                .push("adopted previous deployment manifest into the new state format".to_owned());

            let state = ServerDeploymentState {
                version: VERSION,
                files: legacy.files,
                ..Default::default()
            }
            .validate(spec, &mut warnings)?;

            Ok(LoadedState {
                state,
                warnings,
                migrated: true,
            })
        }
        None => Ok(LoadedState {
            state: ServerDeploymentState {
                version: VERSION,
                ..Default::default()
            },
            warnings,
            migrated: false,
        }),
    }
}

/// Serializes the state for persistence. Kept separate from writing so the
/// engine controls the remote write path (atomic tmp+rename).
pub fn serialize(state: &ServerDeploymentState) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(state).context("failed to serialize deployment state")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        game::mod_loader::{ModLoader, ModLoaderKind},
        profile::server::{paths::RemotePathBuf, remote::memory::MemoryRemote},
    };

    const STATE_PATH: &str = "/srv/.gale-server-state.json";
    const LEGACY_PATH: &str = "/srv/.gale-server-manifest.json";

    fn spec() -> DeploymentSpec {
        DeploymentSpec::for_loader(&ModLoader {
            package_name: None,
            file_target: None,
            kind: ModLoaderKind::BepInEx {
                extra_subdirs: Vec::new(),
            },
        })
        .unwrap()
    }

    fn remote_path(value: &str) -> RemotePathBuf {
        RemotePathBuf::new(value).unwrap()
    }

    fn read(remote: &mut MemoryRemote) -> Result<LoadedState> {
        read_state(
            remote,
            &spec(),
            &remote_path(STATE_PATH),
            &remote_path(LEGACY_PATH),
        )
    }

    fn deploy(value: &str) -> DeployPathBuf {
        DeployPathBuf::new(value).unwrap()
    }

    fn config_path(value: &str) -> ConfigPath {
        ConfigPath::try_from(value.to_owned()).unwrap()
    }

    fn hash_of(tag: &str) -> ContentHash {
        ContentHash::from_hash(blake3::hash(tag.as_bytes()))
    }

    #[test]
    fn absent_state_yields_defaults() {
        let mut remote = MemoryRemote::new();
        let loaded = read(&mut remote).unwrap();

        assert!(!loaded.migrated);
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.state.version, VERSION);
        assert!(loaded.state.files.is_empty());
        assert!(loaded.state.config.is_empty());
    }

    #[test]
    fn malformed_state_is_an_error() {
        let mut remote = MemoryRemote::new();
        remote.put_file(STATE_PATH, b"{ not json");
        assert!(read(&mut remote).is_err());
    }

    #[test]
    fn oversized_state_is_refused() {
        let mut remote = MemoryRemote::new();
        // Just over the 4 MiB bound; the reader must refuse rather than truncate.
        remote.put_file(STATE_PATH, &vec![b' '; MAX_STATE_BYTES as usize + 1]);
        assert!(read(&mut remote).is_err());
    }

    #[test]
    fn wrong_version_state_is_an_error() {
        let mut remote = MemoryRemote::new();
        remote.put_file(STATE_PATH, br#"{"version": 3}"#);
        assert!(read(&mut remote).is_err());
    }

    #[test]
    fn legacy_manifest_is_adopted_once() {
        let mut remote = MemoryRemote::new();
        remote.put_file(
            LEGACY_PATH,
            br#"{
                "version": 1,
                "files": {
                    "BepInEx/plugins/ModA.dll": { "hash": "abc", "size": 12 }
                }
            }"#,
        );

        let loaded = read(&mut remote).unwrap();
        assert!(loaded.migrated);
        assert_eq!(
            loaded.state.files.get(&deploy("BepInEx/plugins/ModA.dll")),
            Some(&OwnedFile {
                hash: "abc".to_owned(),
                size: 12,
            })
        );
        assert!(loaded.warnings.iter().any(|w| w.contains("manifest")));

        // The new state file takes precedence once it exists.
        let state = ServerDeploymentState {
            version: VERSION,
            ..Default::default()
        };
        remote.put_file(STATE_PATH, &serialize(&state).unwrap());
        let loaded = read(&mut remote).unwrap();
        assert!(!loaded.migrated);
        assert!(loaded.state.files.is_empty());
    }

    #[test]
    fn unsupported_legacy_version_is_an_error() {
        let mut remote = MemoryRemote::new();
        remote.put_file(LEGACY_PATH, br#"{"version": 9, "files": {}}"#);
        assert!(read(&mut remote).is_err());
    }

    #[test]
    fn out_of_scope_entries_are_dropped_with_warnings() {
        let mut state = ServerDeploymentState {
            version: VERSION,
            ..Default::default()
        };
        // A config-dir path can never be an owned payload file.
        state.files.insert(
            deploy("BepInEx/config/evil.cfg"),
            OwnedFile {
                hash: "x".to_owned(),
                size: 1,
            },
        );
        // A payload path can never be a config record.
        state.config.insert(
            config_path("BepInEx/plugins/ModA.dll"),
            AppliedFile::default(),
        );
        state.pending.insert(
            config_path("BepInEx/plugins/ModA.dll"),
            PendingConfigReason::ModifiedLocally,
        );

        let mut remote = MemoryRemote::new();
        remote.put_file(STATE_PATH, &serialize(&state).unwrap());
        let loaded = read(&mut remote).unwrap();

        assert!(loaded.state.files.is_empty());
        assert!(loaded.state.config.is_empty());
        assert!(loaded.state.pending.is_empty());
        assert!(loaded.warnings.len() >= 2);
    }

    #[test]
    fn applied_then_declined_at_same_revision_records_decline() {
        let mut state = ServerDeploymentState::default();
        let path = config_path("BepInEx/config/mod.cfg");
        let hash = hash_of("v1");

        state.record_applied(&path, &hash);
        assert_eq!(state.config[&path].applied, Some(hash.clone()));

        state.record_declined(&path, &hash);
        assert_eq!(state.config[&path].declined, Some(hash.clone()));
        assert!(state.pending.is_empty());
    }

    #[test]
    fn record_pending_pins_policy_at_conflicting_revision() {
        let mut state = ServerDeploymentState::default();
        let path = config_path("BepInEx/config/mod.cfg");
        let v1 = hash_of("v1");
        let v2 = hash_of("v2");

        // Establish a persistent policy while a conflict is pending: the
        // policy is pinned at v1 so it governs later updates, not this one.
        let record = state.config.entry(path.clone()).or_default();
        record.policy = ConfigUpdatePolicy::AlwaysApply;
        state.record_pending(&path, &v1, PendingConfigReason::ModifiedLocally);

        let record = &state.config[&path];
        assert_eq!(record.policy_set_at, Some(v1.clone()));
        assert_eq!(state.pending[&path], PendingConfigReason::ModifiedLocally);

        // A new conflicting revision doesn't move the pin.
        state.record_pending(&path, &v2, PendingConfigReason::ModifiedLocally);
        assert_eq!(state.config[&path].policy_set_at, Some(v1));

        // Resolving clears pending and the pin.
        state.record_applied(&path, &v2);
        assert!(state.pending.is_empty());
        assert_eq!(state.config[&path].policy_set_at, None);
        assert_eq!(state.config[&path].declined, None);
    }

    #[test]
    fn operation_history_is_bounded_and_sequenced() {
        let mut state = ServerDeploymentState::default();
        let record = |id: &str| OperationRecord {
            id: id.to_owned(),
            executor: ExecutorKind::Local,
            kind: OperationKind::Manual,
            worker_id: None,
            publication_revision: None,
            mods_revision: None,
            status: OperationStatus::Succeeded,
            summary: OperationSummary::default(),
            restart: RestartOutcome::NotRequired,
            error: None,
            started_at: Utc::now(),
            finished_at: Utc::now(),
        };

        for i in 0..(MAX_HISTORY + 3) {
            state.record_operation(record(&format!("op-{i}")));
        }

        assert_eq!(state.history.len(), MAX_HISTORY);
        assert_eq!(state.last_operation.as_ref().unwrap().id, "op-12");
        assert_eq!(state.operation_seq, MAX_HISTORY as u64 + 3);
    }
}
