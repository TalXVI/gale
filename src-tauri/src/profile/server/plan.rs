//! The deployment planner shared by Local and Worker execution. Given one
//! canonical publication, a remote snapshot, and the user's selection, it
//! produces the exact same plan no matter which executor computed it.
//! Producing the same plan is what keeps the two modes interchangeable.
//!
//! Nothing in this module does I/O, which is what makes the semantics
//! unit-testable and the plan hash deterministic.

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
    export::{ConfigPath, ContentHash, ModRevision},
    sync::{
        ConfigUpdatePolicy, FetchedPublication, PendingConfigReason, archive::ValidatedConfigFile,
    },
};

/// Which scope a deployment covers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SelectionRequest", into = "SelectionRequest")]
pub struct DeploySelection {
    /// Synchronize the published mod payload (mods phase).
    pub include_mods: bool,
    /// The config phase's decisions, or `None` to skip the phase. Empty
    /// decisions still evaluate published configs, so Mods+Configs with
    /// zero selected files stays valid. A published config without a
    /// decision stays as it is on the server.
    pub configs: Option<BTreeMap<ConfigPath, ConfigDecision>>,
}

/// What the caller decided for one published config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigDecision {
    /// Write the published revision.
    Apply,
    /// Write the published revision, recreating a remote file deleted
    /// after Gale deployed it. Recreating needs this explicit
    /// authorization.
    Restore,
    /// Decline the published revision.
    Decline,
}

impl DeploySelection {
    /// The decision for `path`, if the caller made one.
    fn decision(&self, path: &ConfigPath) -> Option<ConfigDecision> {
        self.configs.as_ref()?.get(path).copied()
    }

    /// Paths the selection writes when the plan allows it.
    pub fn written_configs(&self) -> impl Iterator<Item = &ConfigPath> {
        self.configs
            .iter()
            .flatten()
            .filter(|(_, decision)| **decision != ConfigDecision::Decline)
            .map(|(path, _)| path)
    }

    /// Rejects decisions for files the publication does not offer as
    /// server configs.
    fn validate(
        &self,
        published: &BTreeMap<ConfigPath, ValidatedConfigFile>,
        spec: &DeploymentSpec,
    ) -> Result<()> {
        for (path, decision) in self.configs.iter().flatten() {
            let role = match decision {
                ConfigDecision::Apply | ConfigDecision::Restore => "selected",
                ConfigDecision::Decline => "declined",
            };
            ensure!(
                published.contains_key(path) && spec.is_managed_config(path),
                "{role} config file is not a supported published server config: {path}"
            );
        }
        Ok(())
    }
}

/// The selection's wire shape, which the page and the worker API send.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectionRequest {
    include_mods: bool,
    include_configs: bool,
    /// Published config files to apply explicitly.
    #[serde(default)]
    apply_configs: Vec<ConfigPath>,
    /// Applied configs that may recreate a remotely deleted file.
    #[serde(default)]
    restore_configs: Vec<ConfigPath>,
    /// Published config revisions to decline.
    #[serde(default)]
    decline_configs: Vec<ConfigPath>,
}

impl TryFrom<SelectionRequest> for DeploySelection {
    type Error = String;

    fn try_from(request: SelectionRequest) -> std::result::Result<Self, Self::Error> {
        if !request.include_configs {
            // Silently dropping config decisions in a mods-only selection
            // would hide the caller's intent.
            if request.apply_configs.is_empty()
                && request.restore_configs.is_empty()
                && request.decline_configs.is_empty()
            {
                return Ok(Self {
                    include_mods: request.include_mods,
                    configs: None,
                });
            }
            return Err("config selections require the config phase".to_owned());
        }

        let mut configs = BTreeMap::new();
        for path in request.apply_configs {
            if configs.contains_key(&path) {
                return Err(format!("duplicate selected config path: {path}"));
            }
            configs.insert(path, ConfigDecision::Apply);
        }
        for path in request.restore_configs {
            match configs.get_mut(&path) {
                Some(decision) => *decision = ConfigDecision::Restore,
                None => return Err(format!("restore entry was not selected: {path}")),
            }
        }
        for path in request.decline_configs {
            if configs
                .get(&path)
                .is_some_and(|decision| *decision != ConfigDecision::Decline)
            {
                return Err(format!("config file is both applied and declined: {path}"));
            }
            configs.insert(path, ConfigDecision::Decline);
        }
        Ok(Self {
            include_mods: request.include_mods,
            configs: Some(configs),
        })
    }
}

impl From<DeploySelection> for SelectionRequest {
    fn from(selection: DeploySelection) -> Self {
        let mut request = Self {
            include_mods: selection.include_mods,
            include_configs: selection.configs.is_some(),
            apply_configs: Vec::new(),
            restore_configs: Vec::new(),
            decline_configs: Vec::new(),
        };
        for (path, decision) in selection.configs.into_iter().flatten() {
            match decision {
                ConfigDecision::Apply => request.apply_configs.push(path),
                ConfigDecision::Restore => {
                    request.apply_configs.push(path.clone());
                    request.restore_configs.push(path);
                }
                ConfigDecision::Decline => request.decline_configs.push(path),
            }
        }
        request
    }
}

/// A file the publication wants on the server.
#[derive(Debug)]
pub struct StagedFile {
    /// Where the bytes come from during upload.
    pub source: FileSource,
    pub hash: ContentHash,
    pub size: u64,
}

#[derive(Debug)]
pub enum FileSource {
    /// A staged file on local disk (extracted package tree).
    Path(std::path::PathBuf),
    /// In-memory bytes (published configs).
    Bytes(Vec<u8>),
}

/// Staged mod payload files in deploy-path space. Package configs are excluded.
pub type DesiredDeployment = BTreeMap<DeployPathBuf, StagedFile>;

/// Identity and behavior options the plan hash binds. An approval is
/// specific to exactly one profile, server target, and restart policy.
/// Changing any of them after preview must not reuse the approval.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanContext {
    /// The profile the operation was issued for.
    pub profile_id: String,
    /// The game the profile belongs to.
    pub game: String,
    /// Normalized description of the remote target (protocol, host, path).
    pub target: String,
    /// The restart behavior the operation will apply.
    pub restart_policy: crate::profile::server::settings::RestartPolicy,
}

/// What the remote actually looks like, gathered fresh for every plan.
pub struct RemoteSnapshot {
    /// Validated remote deployment state (ownership + config records).
    pub state: ServerDeploymentState,
    /// Files and dirs inside the payload dirs, deploy-path → remote size.
    pub payload_files: BTreeMap<DeployPathBuf, u64>,
    pub payload_dirs: BTreeSet<DeployPathBuf>,
    /// Remote content hash for every *Gale-owned* payload file that is
    /// also desired by the publication and size-matched. This is the
    /// integrity check that catches remote modification of managed mods.
    /// Never populated for unowned files: hashing foreign content gains
    /// nothing.
    pub payload_hashes: BTreeMap<DeployPathBuf, ContentHash>,
    /// Remote content hash for every config path a decision is needed on
    /// (published ∪ recorded ∪ explicitly selected); `None` = absent
    /// remotely. It is empty when the selection skips the config phase. A
    /// mods-only operation never reads remote configs.
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
    /// Awaits a decision. Applying would clobber a remote change or
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
}

/// The complete, approved description of one deployment. It runs exactly
/// as written, because the plan hash binds the preview approval to the
/// execution.
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
    /// Remote payload files that are neither published nor recorded as
    /// Gale-owned, like manually installed mods or leftovers from other
    /// tools. Shown in the preview for visibility; never removed
    /// automatically.
    pub unmanaged: Vec<DeployPathBuf>,
}

/// Computes the deployment plan. This is the only place sync semantics are
/// decided. Local and Worker modes call the same function, so identical
/// inputs always produce identical plans.
pub fn build_plan(
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
    snapshot: &RemoteSnapshot,
    selection: &DeploySelection,
    context: &PlanContext,
    spec: &DeploymentSpec,
) -> Result<DeploymentPlan> {
    selection
        .validate(&publication.config, spec)
        .map_err(InvalidSelection)?;

    let state = &snapshot.state;
    let mut uploads: Vec<PlanUpload> = Vec::new();
    let mut removals: Vec<DeployPathBuf> = Vec::new();
    let mut directory_removals: Vec<DeployPathBuf> = Vec::new();
    let mut unmanaged: Vec<DeployPathBuf> = Vec::new();
    let mut unchanged_files = 0usize;

    // ---- Mods phase: converge the payload dirs on the exact published set.
    if selection.include_mods {
        for (path, staged) in desired {
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

        // Removal authority comes from ownership records alone: only
        // files a completed deployment recorded as Gale-owned may be
        // deleted. Remote files that were never recorded, like manually
        // installed mods or other tools' output, are not ours to remove,
        // no matter how much they resemble published content. They are
        // reported as unmanaged instead, so the user sees what else lives
        // in the payload directories.
        removals = state
            .files
            .keys()
            .filter(|path| {
                !desired.contains_key(*path) && spec.deploys(path, snapshot.host_managed)
            })
            .cloned()
            .collect();

        unmanaged = snapshot
            .payload_files
            .keys()
            .filter(|path| {
                !desired.contains_key(*path)
                    && !state.files.contains_key(*path)
                    && spec.valid_owned_path(path)
            })
            .cloned()
            .collect();

        // Payload dirs left with no surviving remote files after removals
        // can go, deepest first. The payload roots themselves are kept so a
        // restricted host's expected layout survives an empty deployment.
        let removed: BTreeSet<&DeployPathBuf> = removals.iter().collect();
        let mut occupied = BTreeSet::new();
        for file in snapshot.payload_files.keys() {
            if removed.contains(file) {
                continue;
            }
            let mut parent = file.parent();
            while let Some(dir) = parent {
                parent = dir.parent();
                if !occupied.insert(dir) {
                    // Its ancestors were marked by an earlier file.
                    break;
                }
            }
        }
        directory_removals = snapshot
            .payload_dirs
            .iter()
            .rev()
            .filter(|dir| {
                spec.payload_dirs
                    .iter()
                    .any(|root| root.is_ancestor_of(dir))
            })
            .filter(|dir| !occupied.contains(*dir))
            .cloned()
            .collect();
    }

    // ---- Config phase: selective application of published config files.
    let configs_phase = selection.configs.is_some();
    let mut config_entries = Vec::new();
    if configs_phase {
        for (path, file) in &publication.config {
            if !spec.is_managed_config(path) {
                continue;
            }
            let published = file.hash.clone();
            let remote = snapshot.config_remote.get(path).cloned().flatten();
            let record = state.config.get(path);
            let action = decide_config(
                &published,
                remote.as_ref(),
                record,
                selection.decision(path),
            );

            if let ConfigAction::Write = action {
                uploads.push(PlanUpload {
                    path: DeployPathBuf::new(path.as_str())?,
                    size: file.bytes.len() as u64,
                    kind: UploadKind::Config,
                });
            }

            config_entries.push(PlanConfigEntry {
                path: path.clone(),
                action,
                policy: record.map(|record| record.policy).unwrap_or_default(),
            });
        }
    }

    let mut plan = DeploymentPlan {
        hash: String::new(),
        publication_revision: publication.revision,
        mods_revision: publication.mods_revision.clone(),
        mods_phase: selection.include_mods,
        configs_phase,
        upload_bytes: uploads.iter().map(|upload| upload.size).sum(),
        uploads,
        removals,
        directory_removals,
        unchanged_files,
        config_entries,
        unmanaged,
    };
    plan.hash = plan_hash(&plan, snapshot, selection, context);

    Ok(plan)
}

/// Whether the remote copy of a payload file still matches the staged
/// bytes. Inside mirrored dirs the engine hashes the remote content of
/// every owned, size-matched file, so a same-size remote edit is detected
/// and re-uploaded. A file that cannot be verified, for example a missing
/// hash, a size mismatch, or an unreadable file, counts as diverged and
/// is re-uploaded to be safe. Outside mirrored dirs, like loader payloads
/// at the root, remote drift is not observable and the state record is
/// trusted.
fn remote_intact(
    path: &DeployPath,
    staged: &StagedFile,
    snapshot: &RemoteSnapshot,
    spec: &DeploymentSpec,
) -> bool {
    if !spec.is_mirrored(path) {
        return true;
    }

    snapshot
        .payload_hashes
        .get(path)
        .is_some_and(|remote| *remote == staged.hash)
}

/// The server-side config decision. It mirrors the client's `decide` and
/// adds the server's explicit selection and restore gates. `remote` is
/// the actual remote content hash, never the state record, so remote
/// customization is always detected.
fn decide_config(
    published: &ContentHash,
    remote: Option<&ContentHash>,
    record: Option<&crate::profile::sync::AppliedFile>,
    decision: Option<ConfigDecision>,
) -> ConfigAction {
    const DELETED: ConfigAction = ConfigAction::Pending {
        reason: PendingConfigReason::DeletedLocally,
    };

    if remote == Some(published) {
        return ConfigAction::MarkApplied;
    }

    if decision == Some(ConfigDecision::Decline) {
        return ConfigAction::Decline;
    }

    // A recorded hash cannot prove a file still exists. Recreating a file
    // Gale deployed that was deleted remotely needs the explicit restore
    // authorization, even when the publication is unchanged or an
    // automatic update policy was previously selected.
    let deleted =
        remote.is_none() && record.is_some_and(|r| r.applied.is_some() || r.written.is_some());

    if let Some(decision) = decision {
        return if deleted && decision != ConfigDecision::Restore {
            DELETED
        } else {
            ConfigAction::Write
        };
    }

    if record.and_then(|r| r.declined.as_ref()) == Some(published) {
        return ConfigAction::Decline;
    }

    if deleted {
        return DELETED;
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
        (None, ConfigUpdatePolicy::Ask) => ConfigAction::Unapplied,
        (Some(remote_hash), ConfigUpdatePolicy::Ask) => {
            // Content Gale wrote and nobody modified on the server may
            // update without asking.
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

/// Fingerprint of the plan's actions and every input they were derived
/// from: publication identity, the approving context (profile, game,
/// target, restart policy), the authoritative state revision, and the
/// remote content preconditions (config hashes and owned-payload hashes).
/// A remote byte changing between preview and deploy produces a different
/// hash, even when the planned action stays `Write`, so a stale approval
/// gets rejected instead of silently running a different plan.
fn plan_hash(
    plan: &DeploymentPlan,
    snapshot: &RemoteSnapshot,
    selection: &DeploySelection,
    context: &PlanContext,
) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Signature<'a> {
        context: &'a PlanContext,
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
        /// Remote content preconditions the plan was computed against.
        config_remote: &'a BTreeMap<ConfigPath, Option<ContentHash>>,
        payload_hashes: &'a BTreeMap<DeployPathBuf, ContentHash>,
    }

    let signature = Signature {
        context,
        publication_revision: plan.publication_revision,
        mods_revision: &plan.mods_revision,
        state_seq: snapshot.state.operation_seq,
        layout: snapshot.layout,
        host_managed: snapshot.host_managed,
        selection,
        uploads: &plan.uploads,
        removals: &plan.removals,
        directory_removals: &plan.directory_removals,
        config_entries: &plan.config_entries,
        config_remote: &snapshot.config_remote,
        payload_hashes: &snapshot.payload_hashes,
    };

    let bytes = serde_json::to_vec(&signature).expect("plan signature serializes");
    blake3::hash(&bytes).to_hex().to_string()
}

/// An approved plan no longer matches the one computed now: the server
/// changed in between, so the caller must preview again.
#[derive(Debug, thiserror::Error)]
#[error("the server state changed since the preview was approved; please preview again")]
pub struct StalePlan;

/// A selection the publication cannot satisfy. The request is wrong, not
/// the server.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidSelection(eyre::Report);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        game::mod_loader::{ModLoader, ModLoaderKind},
        profile::{
            export::{ProfileManifest, R2Mod},
            sync::AppliedFile,
        },
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
            source: FileSource::Bytes(bytes.to_vec()),
            hash: ContentHash::from_hash(blake3::hash(bytes)),
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

    struct Fixture {
        mods: Vec<R2Mod>,
        config: BTreeMap<ConfigPath, ValidatedConfigFile>,
    }

    impl Fixture {
        fn publication(&self) -> FetchedPublication {
            FetchedPublication {
                revision: DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                    .unwrap()
                    .to_utc(),
                mods_revision: mod_revision("rev-1"),
                manifest: ProfileManifest {
                    name: String::new(),
                    mods: self.mods.clone(),
                    game: None,
                    ignored_version_updates: Vec::new(),
                    ignored_package_updates: Vec::new(),
                    sync: None,
                },
                config: self.config.clone(),
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
            configs: configs.then(BTreeMap::new),
        }
    }

    /// Records `decision` for `path` in a selection's config phase.
    fn decide(selection: &mut DeploySelection, path: ConfigPath, decision: ConfigDecision) {
        selection
            .configs
            .as_mut()
            .expect("the config phase is selected")
            .insert(path, decision);
    }

    fn context() -> PlanContext {
        PlanContext {
            profile_id: "1".to_owned(),
            game: "valheim".to_owned(),
            target: "sftp://host:22/srv".to_owned(),
            restart_policy: crate::profile::server::settings::RestartPolicy::Manual,
        }
    }

    fn empty_snapshot() -> RemoteSnapshot {
        RemoteSnapshot {
            state: ServerDeploymentState::default(),
            payload_files: BTreeMap::new(),
            payload_dirs: BTreeSet::new(),
            payload_hashes: BTreeMap::new(),
            config_remote: BTreeMap::new(),
            host_managed: false,
            layout: RemoteLayout::Standard,
        }
    }

    fn payload(bytes: &[u8]) -> DesiredDeployment {
        [(deploy("BepInEx/plugins/ModA/ModA.dll"), staged(bytes))]
            .into_iter()
            .collect()
    }

    /// The error a selection sent in the wire shape is rejected with.
    fn wire_error(value: serde_json::Value) -> String {
        serde_json::from_value::<DeploySelection>(value)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn mods_only_selection_rejects_config_decisions() {
        // A mods-only selection carrying config decisions is incoherent:
        // silently ignoring them would hide caller intent, so validation
        // fails loudly instead.
        let error = wire_error(serde_json::json!({
            "includeMods": true,
            "includeConfigs": false,
            "applyConfigs": ["BepInEx/config/mod.cfg"],
        }));
        assert!(
            error.contains("config selections require the config phase"),
            "{error}"
        );
    }

    /// Every decision survives the wire shape the page and the worker API
    /// carry, so a worker sees exactly the selection the desktop sent.
    #[test]
    fn every_config_decision_round_trips_through_the_wire_shape() {
        let mut sel = selection(true, true);
        decide(
            &mut sel,
            config_path("BepInEx/config/a.cfg"),
            ConfigDecision::Apply,
        );
        decide(
            &mut sel,
            config_path("BepInEx/config/b.cfg"),
            ConfigDecision::Restore,
        );
        decide(
            &mut sel,
            config_path("BepInEx/config/c.cfg"),
            ConfigDecision::Decline,
        );

        let wire = serde_json::to_value(&sel).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "includeMods": true,
                "includeConfigs": true,
                "applyConfigs": ["BepInEx/config/a.cfg", "BepInEx/config/b.cfg"],
                "restoreConfigs": ["BepInEx/config/b.cfg"],
                "declineConfigs": ["BepInEx/config/c.cfg"],
            })
        );
        assert_eq!(
            serde_json::from_value::<DeploySelection>(wire).unwrap(),
            sel
        );

        let mods_only: DeploySelection = serde_json::from_value(
            serde_json::json!({ "includeMods": true, "includeConfigs": false }),
        )
        .unwrap();
        assert_eq!(mods_only, selection(true, false));
    }

    #[test]
    fn configs_only_selection_never_touches_payload() {
        let fixture = fixture();
        let mut snapshot = empty_snapshot();
        // A stale payload file the mods phase would remove. The configs
        // phase must leave the whole payload alone.
        snapshot.state.files.insert(
            deploy("BepInEx/plugins/Old/Old.dll"),
            crate::profile::server::state::OwnedFile {
                hash: hash_of(b"x"),
                size: 1,
            },
        );

        let plan = build_plan(
            &fixture.publication(),
            &payload(b"dll"),
            &snapshot,
            &selection(false, true),
            &context(),
            &spec(),
        )
        .unwrap();

        assert!(plan.uploads.is_empty());
        assert!(plan.removals.is_empty());
        assert!(!plan.mods_phase);
        assert!(plan.uploads.is_empty() && plan.removals.is_empty());
    }

    #[test]
    fn selection_rejects_unpublished_apply() {
        let fixture = fixture();
        let mut sel = selection(false, true);
        decide(
            &mut sel,
            config_path("BepInEx/config/ghost.cfg"),
            ConfigDecision::Apply,
        );

        assert!(
            build_plan(
                &fixture.publication(),
                &DesiredDeployment::default(),
                &empty_snapshot(),
                &sel,
                &context(),
                &spec(),
            )
            .is_err()
        );
    }

    #[test]
    fn selection_rejects_apply_and_decline_together() {
        let error = wire_error(serde_json::json!({
            "includeMods": false,
            "includeConfigs": true,
            "applyConfigs": ["BepInEx/config/mod.cfg"],
            "declineConfigs": ["BepInEx/config/mod.cfg"],
        }));
        assert!(error.contains("both applied and declined"), "{error}");
    }

    #[test]
    fn selection_rejects_restore_without_apply() {
        let error = wire_error(serde_json::json!({
            "includeMods": false,
            "includeConfigs": true,
            "restoreConfigs": ["BepInEx/config/mod.cfg"],
        }));
        assert!(error.contains("restore entry was not selected"), "{error}");
    }

    #[test]
    fn unverifiable_owned_file_is_reuploaded_conservatively() {
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
        // Size matches but the remote read produced no hash. When content
        // cannot be verified, the plan re-uploads the canonical bytes.
        snapshot
            .payload_files
            .insert(deploy("BepInEx/plugins/ModA/ModA.dll"), staged_file.size);

        let plan = build_plan(
            &fixture.publication(),
            &payload(bytes),
            &snapshot,
            &selection(true, false),
            &context(),
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
            &context(),
            &spec(),
        )
        .unwrap();

        assert_eq!(plan.config_entries[0].path, path);
        assert_eq!(
            plan.config_entries[0].action,
            ConfigAction::Pending {
                reason: PendingConfigReason::ModifiedLocally
            }
        );

        // Applying the selection turns the same snapshot into a write.
        let mut apply = selection(false, true);
        decide(&mut apply, path.clone(), ConfigDecision::Apply);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &apply,
            &context(),
            &spec(),
        )
        .unwrap();
        assert_eq!(plan.config_entries[0].action, ConfigAction::Write);
        assert!(plan.uploads.iter().any(|u| u.kind == UploadKind::Config));

        // Declining keeps the remote file untouched.
        let mut decline = selection(false, true);
        decide(&mut decline, path, ConfigDecision::Decline);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &decline,
            &context(),
            &spec(),
        )
        .unwrap();
        assert!(plan.uploads.is_empty());
        assert_eq!(plan.config_entries[0].action, ConfigAction::Decline);
    }

    #[test]
    fn removed_publication_config_is_absent_from_preview_and_cannot_be_selected() {
        let fixture = fixture();
        let path = config_path("BepInEx/config/server-only.cfg");
        let mut snapshot = empty_snapshot();
        snapshot
            .config_remote
            .insert(path.clone(), Some(hash_of(b"server")));
        snapshot.state.config.insert(
            path.clone(),
            AppliedFile {
                applied: Some(hash_of(b"formerly-published")),
                ..Default::default()
            },
        );

        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &selection(false, true),
            &context(),
            &spec(),
        )
        .unwrap();
        assert!(plan.config_entries.is_empty());
        assert!(plan.uploads.is_empty());
        assert!(plan.removals.is_empty());

        let mut select_old_config = selection(false, true);
        decide(&mut select_old_config, path, ConfigDecision::Apply);
        assert!(
            build_plan(
                &fixture.publication(),
                &DesiredDeployment::default(),
                &snapshot,
                &select_old_config,
                &context(),
                &spec(),
            )
            .is_err()
        );
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

        // Unselected: the remote deletion must remain visible, even though
        // the recorded applied hash still matches the publication.
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &selection(false, true),
            &context(),
            &spec(),
        )
        .unwrap();
        assert_eq!(plan.config_entries.len(), 1);
        assert_eq!(
            plan.config_entries[0].action,
            ConfigAction::Pending {
                reason: PendingConfigReason::DeletedLocally
            }
        );
        assert!(plan.uploads.is_empty());

        // Selected without restore still stays pending.
        let mut apply_only = selection(false, true);
        decide(&mut apply_only, path.clone(), ConfigDecision::Apply);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &apply_only,
            &context(),
            &spec(),
        )
        .unwrap();
        assert_eq!(
            plan.config_entries[0].action,
            ConfigAction::Pending {
                reason: PendingConfigReason::DeletedLocally
            }
        );

        // Apply + restore authorization finally writes.
        let mut restore = apply_only;
        decide(&mut restore, path, ConfigDecision::Restore);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &snapshot,
            &restore,
            &context(),
            &spec(),
        )
        .unwrap();
        assert_eq!(plan.config_entries[0].action, ConfigAction::Write);
        assert!(plan.uploads.iter().any(|u| u.kind == UploadKind::Config));
    }

    #[test]
    fn actual_remote_config_presence_precedes_recorded_applied_hash_and_policy() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        let published = config_file(b"published");
        fixture.config.insert(path.clone(), published.clone());
        let mut snapshot = empty_snapshot();

        let action = |snapshot: &RemoteSnapshot| {
            build_plan(
                &fixture.publication(),
                &DesiredDeployment::default(),
                snapshot,
                &selection(false, true),
                &context(),
                &spec(),
            )
            .unwrap()
            .config_entries[0]
                .action
                .clone()
        };

        assert_eq!(action(&snapshot), ConfigAction::Unapplied);
        snapshot
            .config_remote
            .insert(path.clone(), Some(published.hash.clone()));
        assert_eq!(action(&snapshot), ConfigAction::MarkApplied);

        snapshot.state.config.insert(
            path.clone(),
            AppliedFile {
                applied: Some(published.hash.clone()),
                written: Some(published.hash.clone()),
                policy: ConfigUpdatePolicy::AlwaysApply,
                ..Default::default()
            },
        );
        snapshot.config_remote.insert(path.clone(), None);
        assert_eq!(
            action(&snapshot),
            ConfigAction::Pending {
                reason: PendingConfigReason::DeletedLocally
            }
        );
        snapshot
            .config_remote
            .insert(path, Some(ContentHash::from_hash(blake3::hash(b"custom"))));
        assert_eq!(action(&snapshot), ConfigAction::Write);
    }

    #[test]
    fn published_text_outside_server_config_scope_cannot_be_selected() {
        let mut fixture = fixture();
        let ordinary = config_path("BepInEx/config/mod.cfg");
        let translation = config_path("BepInEx/plugins/Mod/translations/en.json");
        let loader = config_path("doorstop_config.ini");
        for path in [&ordinary, &translation, &loader] {
            fixture
                .config
                .insert(path.clone(), config_file(b"published"));
        }
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &empty_snapshot(),
            &selection(false, true),
            &context(),
            &spec(),
        )
        .unwrap();
        assert_eq!(plan.config_entries.len(), 1);
        assert_eq!(plan.config_entries[0].path, ordinary);

        for path in [translation, loader] {
            let mut selected = selection(false, true);
            decide(&mut selected, path, ConfigDecision::Apply);
            assert!(
                build_plan(
                    &fixture.publication(),
                    &DesiredDeployment::default(),
                    &empty_snapshot(),
                    &selected,
                    &context(),
                    &spec(),
                )
                .is_err()
            );
        }
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
            &context(),
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

        let first = build_plan(
            &fixture.publication(),
            &desired,
            &snapshot,
            &sel,
            &context(),
            &spec(),
        )
        .unwrap();
        let second = build_plan(
            &fixture.publication(),
            &desired,
            &snapshot,
            &sel,
            &context(),
            &spec(),
        )
        .unwrap();
        assert_eq!(first.hash, second.hash);
        assert!(!first.hash.is_empty());

        // A different selection yields a different approval fingerprint.
        let other = build_plan(
            &fixture.publication(),
            &desired,
            &snapshot,
            &selection(true, false),
            &context(),
            &spec(),
        )
        .unwrap();
        assert_ne!(first.hash, other.hash);

        // So does a newer remote state sequence. A plan approved before
        // another operation cannot be replayed.
        let mut newer = empty_snapshot();
        newer.state.operation_seq = snapshot.state.operation_seq + 1;
        let third = build_plan(
            &fixture.publication(),
            &desired,
            &newer,
            &sel,
            &context(),
            &spec(),
        )
        .unwrap();
        assert_ne!(first.hash, third.hash);

        // A different restart policy or target identity also binds the
        // approval. Changing behavior-relevant options after preview must
        // not reuse it.
        let mut context = context();
        context.restart_policy = crate::profile::server::settings::RestartPolicy::Immediate;
        let restarted = build_plan(
            &fixture.publication(),
            &desired,
            &snapshot,
            &sel,
            &context,
            &spec(),
        )
        .unwrap();
        assert_ne!(first.hash, restarted.hash);

        context.restart_policy = crate::profile::server::settings::RestartPolicy::Manual;
        context.profile_id = "2".to_owned();
        let other_profile = build_plan(
            &fixture.publication(),
            &desired,
            &snapshot,
            &sel,
            &context,
            &spec(),
        )
        .unwrap();
        assert_ne!(first.hash, other_profile.hash);
    }

    #[test]
    fn only_directories_emptied_by_removals_are_removed() {
        let owned = |path: &str| {
            (
                deploy(path),
                crate::profile::server::state::OwnedFile {
                    hash: ContentHash::from_hash(blake3::hash(path.as_bytes())),
                    size: 1,
                },
            )
        };
        let mut snapshot = empty_snapshot();
        snapshot.state.files = [
            owned("BepInEx/plugins/Old/Old.dll"),
            owned("BepInEx/plugins/Old/sub/Data.bin"),
            owned("BepInEx/plugins/Mixed/Owned.dll"),
        ]
        .into_iter()
        .collect();
        snapshot.payload_files = snapshot
            .state
            .files
            .keys()
            .cloned()
            .chain([deploy("BepInEx/plugins/Mixed/Manual.dll")])
            .map(|path| (path, 1))
            .collect();
        snapshot.payload_dirs = [
            "BepInEx/plugins",
            "BepInEx/plugins/Empty",
            "BepInEx/plugins/Mixed",
            "BepInEx/plugins/Old",
            "BepInEx/plugins/Old/sub",
        ]
        .into_iter()
        .map(deploy)
        .collect();

        let plan = build_plan(
            &fixture().publication(),
            &payload(b"dll"),
            &snapshot,
            &selection(true, false),
            &context(),
            &spec(),
        )
        .unwrap();

        // Deepest first. `Mixed` still holds a manually installed mod, and
        // the payload root itself always stays.
        assert_eq!(
            plan.directory_removals,
            vec![
                deploy("BepInEx/plugins/Old/sub"),
                deploy("BepInEx/plugins/Old"),
                deploy("BepInEx/plugins/Empty"),
            ]
        );
    }

    #[test]
    fn applied_configs_and_payload_are_uploaded_but_declined_configs_are_not() {
        let mut fixture = fixture();
        let path = config_path("BepInEx/config/mod.cfg");
        fixture
            .config
            .insert(path.clone(), config_file(b"published"));

        let mut apply = selection(false, true);
        decide(&mut apply, path.clone(), ConfigDecision::Apply);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &empty_snapshot(),
            &apply,
            &context(),
            &spec(),
        )
        .unwrap();
        assert!(!plan.uploads.is_empty());

        let mut decline = selection(false, true);
        decide(&mut decline, path, ConfigDecision::Decline);
        let plan = build_plan(
            &fixture.publication(),
            &DesiredDeployment::default(),
            &empty_snapshot(),
            &decline,
            &context(),
            &spec(),
        )
        .unwrap();
        assert!(plan.uploads.is_empty() && plan.removals.is_empty());

        let plan = build_plan(
            &fixture.publication(),
            &payload(b"dll"),
            &empty_snapshot(),
            &selection(true, false),
            &context(),
            &spec(),
        )
        .unwrap();
        assert!(!plan.uploads.is_empty());
    }
}
