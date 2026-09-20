//! The authoritative deployment planner shared by Local and Worker
//! execution. Given one canonical publication, a remote snapshot, and the
//! user's selection it produces the exact same plan regardless of which
//! executor computed it — plan equivalence is what keeps the two modes
//! interchangeable.
//!
//! Everything here is pure: no I/O happens in this module, which is what
//! makes the semantics unit-testable and the plan hash deterministic.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::{
    paths::{DeployPath, DeployPathBuf},
    spec::DeploymentSpec,
    state::ServerDeploymentState,
};
use crate::profile::{
    export::{ConfigPath, ContentHash, ModRevision, R2Mod},
    sync::{ConfigUpdatePolicy, PendingConfigReason, archive::ValidatedConfigFile},
};

/// Which scope a deployment covers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploySelection {
    /// Synchronize the published mod payload (mods phase).
    pub include_mods: bool,
    /// Evaluate published configs (config phase). Independent of the file
    /// lists so Mods+Configs with zero selected files stays valid.
    pub include_configs: bool,
    /// Published config files to apply explicitly.
    #[serde(default)]
    pub apply_configs: Vec<ConfigPath>,
    /// Selected configs whose remote file was deleted after Gale deployed
    /// it — recreating them needs this explicit authorization.
    #[serde(default)]
    pub restore_configs: Vec<ConfigPath>,
    /// Published config revisions to decline.
    #[serde(default)]
    pub decline_configs: Vec<ConfigPath>,
}

impl DeploySelection {
    fn validate(&self, published: &BTreeMap<ConfigPath, ValidatedConfigFile>) -> Result<()> {
        let mut seen = BTreeSet::new();
        for path in &self.apply_configs {
            ensure!(seen.insert(path), "duplicate selected config path: {path}");
            ensure!(
                published.contains_key(path),
                "selected config file is not published: {path}"
            );
        }
        for path in &self.restore_configs {
            ensure!(
                seen.contains(path),
                "restore entry was not selected: {path}"
            );
        }
        for path in &self.decline_configs {
            ensure!(
                !seen.contains(path),
                "config file is both applied and declined: {path}"
            );
            ensure!(
                published.contains_key(path),
                "declined config file is not published: {path}"
            );
        }
        Ok(())
    }
}

/// A canonical publication the server deploys — the same artifact friends'
/// clients consume. Local unpublished profile changes are never part of it.
pub struct Publication<'a> {
    /// Remote `updatedAt`, identifying this exact revision.
    pub revision: DateTime<Utc>,
    pub mods_revision: ModRevision,
    pub mods: &'a [R2Mod],
    /// Published config bytes keyed by canonical config path.
    pub config: &'a BTreeMap<ConfigPath, ValidatedConfigFile>,
}

impl<'a> Publication<'a> {
    /// Views a fetched canonical publication through the planner's lens —
    /// the single conversion both executors use, so "the publication" means
    /// the same artifact on both sides.
    pub fn from_fetched(publication: &'a crate::profile::sync::FetchedPublication) -> Self {
        Self {
            revision: publication.revision,
            mods_revision: publication.mods_revision.clone(),
            mods: &publication.manifest.mods,
            config: &publication.config,
        }
    }
}

/// A file the publication wants on the server.
#[derive(Debug, Clone)]
pub struct StagedFile {
    /// Where the bytes come from during upload.
    pub source: FileSource,
    /// blake3 hex of the content.
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub enum FileSource {
    /// A staged file on local disk (extracted package tree).
    Path(std::path::PathBuf),
    /// In-memory bytes (published configs).
    Bytes(std::sync::Arc<Vec<u8>>),
}

/// The published content offered to the planner, in deploy-path space.
#[derive(Debug, Default)]
pub struct DesiredDeployment {
    /// Mod payload files (outside config dirs).
    pub payload: BTreeMap<DeployPathBuf, StagedFile>,
    /// Package-bundled config files that may seed the server when absent.
    pub package_defaults: BTreeMap<DeployPathBuf, StagedFile>,
}

/// What the remote actually looks like, gathered fresh for every plan.
pub struct RemoteSnapshot {
    /// Validated remote deployment state (ownership + config records).
    pub state: ServerDeploymentState,
    /// Files and dirs inside the payload dirs, deploy-path → remote size.
    pub payload_files: BTreeMap<DeployPathBuf, u64>,
    pub payload_dirs: BTreeSet<DeployPathBuf>,
    /// Remote content hash for every config path a decision is needed on
    /// (published ∪ recorded ∪ seeded); `None` = absent remotely.
    pub config_remote: BTreeMap<ConfigPath, Option<ContentHash>>,
    pub host_managed: bool,
    pub layout: RemoteLayout,
}

/// How the remote server lays out the profile relative to the configured
/// server directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteLayout {
    /// The remote directory maps directly onto the profile root.
    Standard,
    /// The host is restricted to the mod loader's directory: it exposes
    /// `BepInEx/` contents at the remote root, so the mirror-root prefix is
    /// stripped when mapping deploy paths.
    MirrorRoot,
}

/// What kind of content an upload carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UploadKind {
    /// Mod payload file.
    Payload,
    /// Package-bundled config seeded because no file exists remotely.
    ConfigSeed,
    /// Published config applied by selection or policy.
    Config,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanUpload {
    pub path: DeployPathBuf,
    pub size: u64,
    pub kind: UploadKind,
}

/// The decided fate of one published config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    tag = "action",
    rename_all_fields = "camelCase"
)]
pub enum ConfigAction {
    /// Remote already matches the published bytes; only records update.
    MarkApplied,
    /// Published bytes will be uploaded.
    Write,
    /// The published revision stays declined this revision.
    Decline,
    /// The remote file stays untouched (already applied, server-customized
    /// after apply, or AlwaysKeep policy).
    Keep,
    /// Awaits a decision — applying would clobber a remote change or
    /// recreate a remote deletion.
    Pending { reason: PendingConfigReason },
    /// Never deployed and not selected; simply stays absent.
    Unapplied,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanConfigEntry {
    pub path: ConfigPath,
    #[serde(flatten)]
    pub action: ConfigAction,
    pub policy: ConfigUpdatePolicy,
    /// Whether the user explicitly selected this file for application.
    pub selected: bool,
}

/// A config situation the deployment could not resolve on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanConflict {
    pub path: ConfigPath,
    pub reason: PendingConfigReason,
    /// `true` for package-bundled defaults, `false` for published configs.
    pub seed: bool,
}

/// The complete, approved description of one deployment. Executed exactly —
/// the plan hash binds preview approval to execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentPlan {
    /// Fingerprint of everything the plan will do plus the inputs it was
    /// computed from. A stale approval produces a different hash and is
    /// rejected instead of silently executing a different plan.
    pub hash: String,
    /// The publication this plan applies.
    pub publication_revision: DateTime<Utc>,
    pub mods_revision: ModRevision,
    /// The mod revision the server had before this plan.
    pub deployed_mods_revision: Option<ModRevision>,
    /// The `operationSeq` of the remote state this plan was computed from.
    pub state_seq: u64,
    pub layout: RemoteLayout,
    pub host_managed: bool,
    /// Whether the plan touches the mod payload.
    pub mods_phase: bool,
    /// Whether the plan evaluates published configs.
    pub configs_phase: bool,
    pub uploads: Vec<PlanUpload>,
    pub upload_bytes: u64,
    pub removals: Vec<DeployPathBuf>,
    pub directory_removals: Vec<DeployPathBuf>,
    pub unchanged_files: usize,
    /// Every published config file and its decided fate.
    pub config_entries: Vec<PlanConfigEntry>,
    /// Seed/published files the plan cannot write without a decision.
    pub conflicts: Vec<PlanConflict>,
    /// Payload changes require a server restart to take effect.
    pub requires_restart: bool,
    /// The published mod set, recorded as `deployed_mods` on success.
    pub deployed_mods: Vec<R2Mod>,
}

/// Computes the deployment plan. This is the only place sync semantics are
/// decided — Local and Worker modes call the same function, so identical
/// inputs always produce identical plans.
pub fn build_plan(
    publication: &Publication<'_>,
    desired: &DesiredDeployment,
    snapshot: &RemoteSnapshot,
    selection: &DeploySelection,
    spec: &DeploymentSpec,
) -> Result<DeploymentPlan> {
    selection.validate(publication.config)?;

    let state = &snapshot.state;
    let mut uploads: Vec<PlanUpload> = Vec::new();
    let mut removals: Vec<DeployPathBuf> = Vec::new();
    let mut directory_removals: Vec<DeployPathBuf> = Vec::new();
    let mut unchanged_files = 0usize;
    let mut conflicts: Vec<PlanConflict> = Vec::new();

    // ---- Mods phase: converge the payload dirs on the exact published set.
    if selection.include_mods {
        for (path, staged) in &desired.payload {
            if !spec.deploys(path, snapshot.host_managed) {
                continue;
            }

            let unchanged = state
                .files
                .get(path)
                .is_some_and(|owned| owned.hash == staged.hash && owned.size == staged.size)
                && remote_intact(path, staged, snapshot, spec);
            if unchanged {
                unchanged_files += 1;
            } else {
                uploads.push(PlanUpload {
                    path: path.clone(),
                    size: staged.size,
                    kind: UploadKind::Payload,
                });
            }
        }

        // Removal is bounded to Gale-owned paths: recorded files plus
        // mirrored extras that are no longer part of the publication.
        let desired_paths: BTreeSet<&DeployPath> =
            desired.payload.keys().map(DeployPathBuf::as_path).collect();
        for path in state.files.keys().chain(snapshot.payload_files.keys()) {
            if desired_paths.contains(path.as_path())
                || !spec.owns_for_removal(path, snapshot.host_managed)
            {
                continue;
            }
            removals.push(path.clone());
        }
        removals.sort();
        removals.dedup();

        // Payload dirs left with no surviving remote files after removals
        // can go, deepest first. The payload roots themselves are kept so a
        // restricted host's expected layout survives an empty deployment.
        directory_removals = snapshot
            .payload_dirs
            .iter()
            .filter(|dir| {
                spec.payload_dirs
                    .iter()
                    .any(|root| root.is_ancestor_of(dir))
            })
            .filter(|dir| {
                snapshot
                    .payload_files
                    .keys()
                    .all(|file| !dir.is_ancestor_of(file) || removals.contains(file))
            })
            .cloned()
            .collect();
        directory_removals.sort_by(|a, b| b.cmp(a));

        // ---- Package-default configs: seed only when absent, never
        // overwrite or delete server configuration.
        for (path, staged) in &desired.package_defaults {
            let config_path = ConfigPath::try_from(path.as_str())
                .map_err(|_| eyre::eyre!("package default has an unsafe path: {path}"))?;
            if publication.config.contains_key(&config_path) {
                continue; // published configs are governed by the config phase
            }

            let remote = snapshot.config_remote.get(&config_path).cloned().flatten();
            let record = state.config.get(&config_path);

            match remote {
                Some(remote_hash) if remote_hash.as_str() == staged.hash => {
                    unchanged_files += 1;
                }
                Some(remote_hash) => {
                    // Only overwrite content we know is unmodified ours.
                    if record.and_then(|r| r.written.as_ref()) == Some(&remote_hash) {
                        uploads.push(PlanUpload {
                            path: path.clone(),
                            size: staged.size,
                            kind: UploadKind::ConfigSeed,
                        });
                    }
                    // Otherwise the file was customized on the server: keep.
                }
                None => {
                    if record.and_then(|r| r.written.as_ref()).is_some()
                        && !selection.restore_configs.contains(&config_path)
                    {
                        // A file Gale seeded was deleted remotely: honor the
                        // deletion, but surface it.
                        conflicts.push(PlanConflict {
                            path: config_path,
                            reason: PendingConfigReason::DeletedLocally,
                            seed: true,
                        });
                    } else {
                        uploads.push(PlanUpload {
                            path: path.clone(),
                            size: staged.size,
                            kind: UploadKind::ConfigSeed,
                        });
                    }
                }
            }
        }
    }

    // ---- Config phase: selective application of published config files.
    let configs_phase = selection.include_configs;
    let mut config_entries = Vec::new();
    if configs_phase {
        for (path, file) in publication.config {
            let published = file.hash.clone();
            let remote = snapshot.config_remote.get(path).cloned().flatten();
            let record = state.config.get(path);
            let selected = selection.apply_configs.contains(path);
            let action = decide_config(
                &published,
                remote.as_ref(),
                record,
                selected,
                selection.restore_configs.contains(path),
                selection.decline_configs.contains(path),
            );

            if let ConfigAction::Write = action {
                uploads.push(PlanUpload {
                    path: DeployPathBuf::new(path.as_str())?,
                    size: file.bytes.len() as u64,
                    kind: UploadKind::Config,
                });
            } else if let ConfigAction::Pending { reason } = action {
                conflicts.push(PlanConflict {
                    path: path.clone(),
                    reason,
                    seed: false,
                });
            }

            config_entries.push(PlanConfigEntry {
                path: path.clone(),
                action,
                policy: record.map(|record| record.policy).unwrap_or_default(),
                selected,
            });
        }
    }

    let requires_restart = selection.include_mods
        && (uploads
            .iter()
            .any(|upload| upload.kind == UploadKind::Payload)
            || !removals.is_empty());

    let mut plan = DeploymentPlan {
        hash: String::new(),
        publication_revision: publication.revision,
        mods_revision: publication.mods_revision.clone(),
        deployed_mods_revision: state.mods_revision.clone(),
        state_seq: state.operation_seq,
        layout: snapshot.layout,
        host_managed: snapshot.host_managed,
        mods_phase: selection.include_mods,
        configs_phase,
        upload_bytes: uploads.iter().map(|upload| upload.size).sum(),
        uploads,
        removals,
        directory_removals,
        unchanged_files,
        config_entries,
        conflicts,
        requires_restart,
        deployed_mods: publication.mods.to_vec(),
    };
    plan.hash = plan_hash(&plan, selection);

    Ok(plan)
}

/// Whether the remote copy of a payload file provably still matches. Inside
/// mirrored dirs the remote listing is authoritative — a missing remote file
/// or a size mismatch means re-upload. Outside them (loader payloads at the
/// root) remote drift is not observable and the state record is trusted.
fn remote_intact(
    path: &DeployPath,
    staged: &StagedFile,
    snapshot: &RemoteSnapshot,
    spec: &DeploymentSpec,
) -> bool {
    if !spec.is_mirrored(path) {
        return true;
    }

    snapshot.payload_files.get(path) == Some(&staged.size)
}

/// The server-side config decision, mirroring the client's `decide` plus the
/// server's explicit selection/restore gates. `remote` is the actual remote
/// content hash — never the state record — so remote customization is always
/// detected.
fn decide_config(
    published: &ContentHash,
    remote: Option<&ContentHash>,
    record: Option<&crate::profile::sync::AppliedFile>,
    selected: bool,
    restore: bool,
    declined: bool,
) -> ConfigAction {
    if remote == Some(published) {
        return ConfigAction::MarkApplied;
    }

    if declined {
        return ConfigAction::Decline;
    }

    if selected {
        // Recreating a file Gale deployed that was deleted remotely needs
        // the explicit restore authorization.
        if remote.is_none()
            && record.is_some_and(|r| r.applied.is_some() || r.written.is_some())
            && !restore
        {
            return ConfigAction::Pending {
                reason: PendingConfigReason::DeletedLocally,
            };
        }
        return ConfigAction::Write;
    }

    if record.and_then(|r| r.applied.as_ref()) == Some(published) {
        return ConfigAction::Keep;
    }

    if record.and_then(|r| r.declined.as_ref()) == Some(published) {
        return ConfigAction::Decline;
    }

    // A policy set at the currently advertised hash governs updates, not the
    // conflict it was created under.
    let policy = match record {
        Some(record) if record.policy_set_at.as_ref() != Some(published) => record.policy,
        _ => ConfigUpdatePolicy::Ask,
    };

    match (remote, policy) {
        (_, ConfigUpdatePolicy::AlwaysApply) => ConfigAction::Write,
        (_, ConfigUpdatePolicy::AlwaysKeep) => ConfigAction::Decline,
        (None, ConfigUpdatePolicy::Ask) => {
            if record.is_some_and(|r| r.applied.is_some() || r.written.is_some()) {
                ConfigAction::Pending {
                    reason: PendingConfigReason::DeletedLocally,
                }
            } else {
                ConfigAction::Unapplied
            }
        }
        (Some(remote_hash), ConfigUpdatePolicy::Ask) => {
            // Content we seeded ourselves and nobody touched may update
            // without asking.
            let written = record.and_then(|r| r.written.as_ref());
            if record.is_none_or(|r| r.applied.is_none() && r.declined.is_none())
                && written == Some(remote_hash)
            {
                ConfigAction::Write
            } else {
                ConfigAction::Pending {
                    reason: PendingConfigReason::ModifiedLocally,
                }
            }
        }
    }
}

/// Fingerprint of the plan's actions and the inputs they were derived from.
/// Compared between preview and execution to reject stale approvals.
fn plan_hash(plan: &DeploymentPlan, selection: &DeploySelection) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Signature<'a> {
        publication_revision: DateTime<Utc>,
        mods_revision: &'a ModRevision,
        state_seq: u64,
        layout: RemoteLayout,
        host_managed: bool,
        selection: &'a DeploySelection,
        uploads: &'a [PlanUpload],
        removals: &'a [DeployPathBuf],
        directory_removals: &'a [DeployPathBuf],
        config_entries: &'a [PlanConfigEntry],
    }

    let signature = Signature {
        publication_revision: plan.publication_revision,
        mods_revision: &plan.mods_revision,
        state_seq: plan.state_seq,
        layout: plan.layout,
        host_managed: plan.host_managed,
        selection,
        uploads: &plan.uploads,
        removals: &plan.removals,
        directory_removals: &plan.directory_removals,
        config_entries: &plan.config_entries,
    };

    let bytes = serde_json::to_vec(&signature).expect("plan signature serializes");
    blake3::hash(&bytes).to_hex().to_string()
}

/// Error raised when an approved plan no longer matches the computed one.
pub fn stale_plan_error() -> eyre::Report {
    eyre::eyre!("the server state changed since the preview was approved; please preview again")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        game::mod_loader::{ModLoader, ModLoaderKind},
        profile::{export::R2Mod, sync::AppliedFile},
        thunderstore::{Backend, PackageIdent},
    };

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

    fn deploy(value: &str) -> DeployPathBuf {
        DeployPathBuf::new(value).unwrap()
    }

    fn config_path(value: &str) -> ConfigPath {
        ConfigPath::try_from(value.to_owned()).unwrap()
    }

    fn hash_of(bytes: &[u8]) -> ContentHash {
        ContentHash::from_hash(blake3::hash(bytes))
    }

    fn staged(bytes: &[u8]) -> StagedFile {
        StagedFile {
            source: FileSource::Bytes(Arc::new(bytes.to_vec())),
            hash: blake3::hash(bytes).to_hex().to_string(),
            size: bytes.len() as u64,
        }
    }

    fn config_file(bytes: &[u8]) -> ValidatedConfigFile {
        ValidatedConfigFile {
            hash: hash_of(bytes),
            bytes: bytes.to_vec(),
        }
    }

    fn mod_revision(tag: &str) -> ModRevision {
        ModRevision::from_hash(blake3::hash(tag.as_bytes()))
    }

    fn r2_mod(name: &str) -> R2Mod {
        R2Mod {
            ident: PackageIdent::from(("Author", name)),
            version: semver::Version::new(1, 0, 0).into(),
            enabled: true,
            source: Backend::Thunderstore,
        }
    }

    /// Owns the borrowed halves of a `Publication` so tests can build one
    /// per case without lifetime gymnastics.
    struct Fixture {
        mods: Vec<R2Mod>,
        config: BTreeMap<ConfigPath, ValidatedConfigFile>,
    }

    impl Fixture {
        fn publication(&self) -> Publication<'_> {
            Publication {
                revision: DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                    .unwrap()
                    .to_utc(),
                mods_revision: mod_revision("rev-1"),
                mods: &self.mods,
                config: &self.config,
            }
        }
    }

    fn fixture() -> Fixture {
        Fixture {
            mods: vec![r2_mod("ModA")],
            config: BTreeMap::new(),
        }
    }

    fn selection(mods: bool, configs: bool) -> DeploySelection {
        DeploySelection {
            include_mods: mods,
            include_configs: configs,
            apply_configs: Vec::new(),
            restore_configs: Vec::new(),
            decline_configs: Vec::new(),
        }
    }

    fn empty_snapshot() -> RemoteSnapshot {
        RemoteSnapshot {
            state: ServerDeploymentState::default(),
            payload_files: BTreeMap::new(),
            payload_dirs: BTreeSet::new(),
            config_remote: BTreeMap::new(),
            host_managed: false,
            layout: RemoteLayout::Standard,
        }
    }

    fn payload(bytes: &[u8]) -> DesiredDeployment {
        DesiredDeployment {
            payload: [(deploy("BepInEx/plugins/ModA/ModA.dll"), staged(bytes))]
                .into_iter()
                .collect(),
            package_defaults: BTreeMap::new(),
        }
    }

    #[test]
    fn mods_only_deploys_payload_and_skips_configs() {
        let fixture = fixture();
        let plan = build_plan(
            &fixture.publication(),
            &payload(b"dll-bytes"),
            &empty_snapshot(),
            &selection(true, false),
            &spec(),
        )
        .unwrap();

        assert_eq!(plan.uploads.len(), 1);
        assert_eq!(plan.uploads[0].kind, UploadKind::Payload);
        assert!(plan.mods_phase);
        assert!(!plan.configs_phase);
        assert!(plan.requires_restart);
    }

    #[test]
    fn configs_only_selection_never_touches_payload() {
        let fixture = fixture();
        let mut snapshot = empty_snapshot();
        // A stale payload file the mods phase would remove — the configs
        // phase must leave the whole payload alone.
        snapshot.state.files.insert(
            deploy("BepInEx/plugins/Old/Old.dll"),
            crate::profile::server::state::OwnedFile {
                hash: "x".into(),
                size: 1,
            },
        );

        let plan = build_plan(
            &fixture.publication(),
            &payload(b"dll"),
            &snapshot,
            &selection(false, true),
            &spec(),
        )
        .unwrap();

        assert!(plan.uploads.is_empty());
        assert!(plan.removals.is_empty());
        assert!(!plan.mods_phase);
        assert!(!plan.requires_restart);
    }

    #[test]
    fn mods_and_configs_with_empty_lists_is_valid() {
        let fixture = fixture();
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &empty_snapshot(),
            &selection(true, true),
            &spec(),
        )
        .unwrap();

        assert!(plan.mods_phase && plan.configs_phase);
    }

    #[test]
    fn selection_rejects_unpublished_apply() {
        let fixture = fixture();
        let mut sel = selection(false, true);
        sel.apply_configs
            .push(config_path("BepInEx/config/ghost.cfg"));

        assert!(
            build_plan(
                &fixture.publication(),
                &DesiredDeployment::default(),
                &empty_snapshot(),
                &sel,
                &spec(),
            )
            .is_err()
        );
    }

    #[test]
    fn selection_rejects_apply_and_decline_together() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        fixture.config.insert(path.clone(), config_file(b"x"));

        let mut sel = selection(false, true);
        sel.apply_configs.push(path.clone());
        sel.decline_configs.push(path);

        assert!(
            build_plan(
                &fixture.publication(),
                &DesiredDeployment::default(),
                &empty_snapshot(),
                &sel,
                &spec(),
            )
            .is_err()
        );
    }

    #[test]
    fn selection_rejects_restore_without_apply() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        fixture.config.insert(path.clone(), config_file(b"x"));

        let mut sel = selection(false, true);
        sel.restore_configs.push(path);

        assert!(
            build_plan(
                &fixture.publication(),
                &DesiredDeployment::default(),
                &empty_snapshot(),
                &sel,
                &spec(),
            )
            .is_err()
        );
    }

    #[test]
    fn only_owned_and_mirrored_files_are_removed() {
        let fixture = fixture();
        let mut snapshot = empty_snapshot();
        // Gale owns this recorded file, absent from the new publication.
        snapshot.state.files.insert(
            deploy("BepInEx/plugins/Old/Old.dll"),
            crate::profile::server::state::OwnedFile {
                hash: "x".into(),
                size: 1,
            },
        );
        // A stray file inside a mirrored dir is removed (the dir is
        // authoritative); a file outside the managed scope is never touched.
        snapshot
            .payload_files
            .insert(deploy("BepInEx/plugins/stray.dll"), 5);
        snapshot.payload_files.insert(deploy("saves/world.db"), 99);

        let plan = build_plan(
            &fixture.publication(),
            &payload(b"dll"),
            &snapshot,
            &selection(true, false),
            &spec(),
        )
        .unwrap();

        assert_eq!(
            plan.removals,
            vec![
                deploy("BepInEx/plugins/Old/Old.dll"),
                deploy("BepInEx/plugins/stray.dll"),
            ]
        );
    }

    #[test]
    fn unchanged_owned_files_are_not_reuploaded() {
        let fixture = fixture();
        let bytes = b"dll-bytes";
        let staged_file = staged(bytes);
        let mut snapshot = empty_snapshot();
        snapshot.state.files.insert(
            deploy("BepInEx/plugins/ModA/ModA.dll"),
            crate::profile::server::state::OwnedFile {
                hash: staged_file.hash.clone(),
                size: staged_file.size,
            },
        );
        snapshot
            .payload_files
            .insert(deploy("BepInEx/plugins/ModA/ModA.dll"), staged_file.size);

        let plan = build_plan(
            &fixture.publication(),
            &payload(bytes),
            &snapshot,
            &selection(true, false),
            &spec(),
        )
        .unwrap();

        assert!(plan.uploads.is_empty());
        assert_eq!(plan.unchanged_files, 1);
        assert!(!plan.requires_restart);
    }

    #[test]
    fn remote_drift_on_owned_file_reuploads() {
        let fixture = fixture();
        let bytes = b"dll-bytes";
        let staged_file = staged(bytes);
        let mut snapshot = empty_snapshot();
        snapshot.state.files.insert(
            deploy("BepInEx/plugins/ModA/ModA.dll"),
            crate::profile::server::state::OwnedFile {
                hash: staged_file.hash.clone(),
                size: staged_file.size,
            },
        );
        // Remote file exists but its size drifted — upload again.
        snapshot
            .payload_files
            .insert(deploy("BepInEx/plugins/ModA/ModA.dll"), 999);

        let plan = build_plan(
            &fixture.publication(),
            &payload(bytes),
            &snapshot,
            &selection(true, false),
            &spec(),
        )
        .unwrap();

        assert_eq!(plan.uploads.len(), 1);
    }

    #[test]
    fn remote_modified_published_config_is_pending_until_decided() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        let published = config_file(b"published");
        fixture.config.insert(path.clone(), published.clone());

        let mut snapshot = empty_snapshot();
        snapshot
            .config_remote
            .insert(path.clone(), Some(hash_of(b"server-customized")));

        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &selection(false, true),
            &spec(),
        )
        .unwrap();

        assert_eq!(
            plan.conflicts,
            vec![PlanConflict {
                path: path.clone(),
                reason: PendingConfigReason::ModifiedLocally,
                seed: false,
            }]
        );

        // Applying the selection turns the same snapshot into a write.
        let mut apply = selection(false, true);
        apply.apply_configs.push(path.clone());
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &apply,
            &spec(),
        )
        .unwrap();
        assert!(plan.conflicts.is_empty());
        assert!(plan.uploads.iter().any(|u| u.kind == UploadKind::Config));

        // Declining keeps the remote file untouched.
        let mut decline = selection(false, true);
        decline.decline_configs.push(path);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &decline,
            &spec(),
        )
        .unwrap();
        assert!(plan.uploads.is_empty() && plan.conflicts.is_empty());
        assert_eq!(plan.config_entries[0].action, ConfigAction::Decline);
    }

    #[test]
    fn remote_deleted_applied_config_needs_restore_authorization() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        let published = config_file(b"published");
        fixture.config.insert(path.clone(), published.clone());

        let mut snapshot = empty_snapshot();
        snapshot.config_remote.insert(path.clone(), None);
        // Gale previously applied this file; the server deleted it.
        snapshot.state.config.insert(
            path.clone(),
            AppliedFile {
                applied: Some(published.hash.clone()),
                written: Some(published.hash.clone()),
                ..Default::default()
            },
        );

        // Unselected: the remote deletion of a file applied at the
        // published revision is honored — the file stays absent (Keep),
        // never silently recreated.
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &selection(false, true),
            &spec(),
        )
        .unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.config_entries[0].action, ConfigAction::Keep);

        // Selected without restore still stays pending.
        let mut apply_only = selection(false, true);
        apply_only.apply_configs.push(path.clone());
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &apply_only,
            &spec(),
        )
        .unwrap();
        assert_eq!(
            plan.conflicts[0].reason,
            PendingConfigReason::DeletedLocally
        );

        // Apply + restore authorization finally writes.
        let mut restore = apply_only;
        restore.restore_configs.push(path);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &restore,
            &spec(),
        )
        .unwrap();
        assert!(plan.conflicts.is_empty());
        assert!(plan.uploads.iter().any(|u| u.kind == UploadKind::Config));
    }

    #[test]
    fn package_defaults_seed_absent_configs_only() {
        let fixture = fixture();
        let mut desired = payload(b"dll");
        desired.package_defaults.insert(
            deploy("BepInEx/config/seeded.cfg"),
            staged(b"default-content"),
        );

        // Absent remotely: the seed is uploaded.
        let plan = build_plan(
            &fixture.publication(),
            &desired,
            &empty_snapshot(),
            &selection(true, false),
            &spec(),
        )
        .unwrap();
        assert!(
            plan.uploads
                .iter()
                .any(|u| u.kind == UploadKind::ConfigSeed)
        );

        // Present remotely with different content: nothing is uploaded —
        // the server's customization is never clobbered by a default.
        let mut snapshot = empty_snapshot();
        snapshot.config_remote.insert(
            config_path("BepInEx/config/seeded.cfg"),
            Some(hash_of(b"server-customized")),
        );
        let plan = build_plan(
            &fixture.publication(),
            &desired,
            &snapshot,
            &selection(true, false),
            &spec(),
        )
        .unwrap();
        assert!(
            !plan
                .uploads
                .iter()
                .any(|u| u.kind == UploadKind::ConfigSeed)
        );
    }

    #[test]
    fn persistent_policy_governs_updates() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        fixture
            .config
            .insert(path.clone(), config_file(b"published-v2"));

        let mut snapshot = empty_snapshot();
        snapshot
            .config_remote
            .insert(path.clone(), Some(hash_of(b"server-customized")));
        // An AlwaysApply policy set before this revision applies updates
        // without asking.
        snapshot.state.config.insert(
            path.clone(),
            AppliedFile {
                applied: Some(hash_of(b"published-v1")),
                written: Some(hash_of(b"published-v1")),
                policy: ConfigUpdatePolicy::AlwaysApply,
                policy_set_at: None,
                ..Default::default()
            },
        );

        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &selection(false, true),
            &spec(),
        )
        .unwrap();
        assert_eq!(plan.config_entries[0].action, ConfigAction::Write);
    }

    #[test]
    fn plan_hash_is_deterministic_and_selection_sensitive() {
        let fixture = fixture();
        let desired = payload(b"dll");
        let snapshot = empty_snapshot();
        let sel = selection(true, true);

        let first = build_plan(&fixture.publication(), &desired, &snapshot, &sel, &spec()).unwrap();
        let second =
            build_plan(&fixture.publication(), &desired, &snapshot, &sel, &spec()).unwrap();
        assert_eq!(first.hash, second.hash);
        assert!(!first.hash.is_empty());

        // A different selection yields a different approval fingerprint.
        let other = build_plan(
            &fixture.publication(),
            &desired,
            &snapshot,
            &selection(true, false),
            &spec(),
        )
        .unwrap();
        assert_ne!(first.hash, other.hash);

        // So does a newer remote state sequence — a plan approved before
        // another operation cannot be replayed.
        let mut newer = empty_snapshot();
        newer.state.operation_seq = snapshot.state.operation_seq + 1;
        let third = build_plan(&fixture.publication(), &desired, &newer, &sel, &spec()).unwrap();
        assert_ne!(first.hash, third.hash);
    }

    #[test]
    fn restart_is_required_only_for_payload_changes() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        fixture
            .config
            .insert(path.clone(), config_file(b"published"));

        // Config-only write: no restart needed.
        let mut apply = selection(false, true);
        apply.apply_configs.push(path);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &empty_snapshot(),
            &apply,
            &spec(),
        )
        .unwrap();
        assert!(!plan.requires_restart);

        // Payload upload: restart required.
        let plan = build_plan(
            &fixture.publication(),
            &payload(b"dll"),
            &empty_snapshot(),
            &selection(true, false),
            &spec(),
        )
        .unwrap();
        assert!(plan.requires_restart);
    }
}
