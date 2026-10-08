use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use super::{execute::replace_remote, persist_state, snapshot::MAX_SNAPSHOT_READERS, *};
use crate::{
    game::mod_loader::{ModLoader, ModLoaderKind},
    profile::{
        export::{ConfigPath, ModRevision, ProfileManifest, R2Mod},
        server::{
            host::{self, HostStatus},
            lease::LeaseRecord,
            paths::{DeployPathBuf, RemotePathBuf},
            plan::{ConfigAction, ConfigDecision, FileSource, StagedFile, UploadKind},
            progress::SyncOperation,
            remote::{self, ConnectionAttempt, memory::MemoryRemote},
            settings::{RemoteAuthentication, RemoteProtocol, TransportSettings},
            state::{self, OwnedFile},
        },
        sync::{PendingConfigReason, archive::ValidatedConfigFile},
    },
    thunderstore::{Backend, PackageIdent},
};

mod deferred_restart;

const BASE: &str = "/srv";
const STATE_REMOTE: &str = "/srv/BepInEx/config/.gale-server-state.json";
const LEASE_DIR_REMOTE: &str = "/srv/BepInEx/config/.gale-deploy.lock";
const LEASE_FILE_REMOTE: &str = "/srv/BepInEx/config/.gale-deploy.lock/lease.json";
const MOD_DLL_REMOTE: &str = "/srv/BepInEx/plugins/Author-ModA/ModA.dll";

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

fn deploy_path(value: &str) -> DeployPathBuf {
    DeployPathBuf::new(value).unwrap()
}

fn config_path(value: &str) -> ConfigPath {
    ConfigPath::try_from(value.to_owned()).unwrap()
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
        hash: ContentHash::from_hash(blake3::hash(bytes)),
        bytes: bytes.to_vec(),
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
            mods_revision: ModRevision::from_hash(blake3::hash(b"rev-1")),
            manifest: ProfileManifest {
                name: String::new(),
                mods: self.mods.clone(),
                game: None,
                ignored_version_updates: Default::default(),
                ignored_package_updates: Default::default(),
                excluded_files: Default::default(),
                sync: None,
            },
            config: self.config.clone(),
        }
    }
}

fn mod_fixture() -> Fixture {
    Fixture {
        mods: vec![R2Mod {
            ident: PackageIdent::from(("Author", "ModA")),
            version: semver::Version::new(1, 0, 0).into(),
            enabled: true,
            source: Backend::Thunderstore,
        }],
        config: BTreeMap::new(),
    }
}

fn desired_payload() -> DesiredDeployment {
    let mut payload = BTreeMap::new();
    payload.insert(
        deploy_path("BepInEx/plugins/Author-ModA/ModA.dll"),
        staged(b"dll-bytes"),
    );
    payload
}

/// The multi-mod payload the FTPS reader-pool tests and the latency
/// benchmark share: `mod_dirs` directories under `BepInEx/plugins`,
/// each with Mod.dll, manifest.json and docs.txt (README.md would be
/// an excluded package-metadata file); the first eight also carry
/// en/de translations. 50 dirs make 166 desired payload files across
/// 58 payload subdirectories.
fn large_mods_payload(mod_dirs: usize) -> DesiredDeployment {
    let mut payload = BTreeMap::new();
    for index in 0..mod_dirs {
        let dir = format!("BepInEx/plugins/Author-Mod{index:02}");
        for file in ["Mod.dll", "manifest.json", "docs.txt"] {
            let path = format!("{dir}/{file}");
            payload.insert(deploy_path(&path), staged(path.as_bytes()));
        }
        if index < 8 {
            for file in ["translations/en.json", "translations/de.json"] {
                let path = format!("{dir}/{file}");
                payload.insert(deploy_path(&path), staged(path.as_bytes()));
            }
        }
    }
    payload
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

/// A remote with the server root present.
/// A remote handle the test can keep observing after the session boxes
/// a clone of it.
type Shared = Arc<Mutex<MemoryRemote>>;

fn remote() -> Shared {
    let mut remote = MemoryRemote::new();
    remote.dirs.insert(BASE.to_owned());
    Arc::new(Mutex::new(remote))
}

fn open(remote: Shared) -> Result<Session> {
    let spec = spec();
    open_session(Box::new(remote), &spec, RemotePathBuf::new(BASE).unwrap())
}

fn context() -> PlanContext {
    PlanContext {
        profile_id: "1".to_owned(),
        game: "valheim".to_owned(),
        target: "sftp://host:22/srv".to_owned(),
        restart_policy: RestartPolicy::Manual,
    }
}

/// The heartbeat would only connect after the 60s interval; deployments
/// in these tests finish first, so the factory is never exercised.
fn no_connect() -> Result<Box<dyn RemoteOps>> {
    eyre::bail!("heartbeat connection not expected in tests");
}

fn meta() -> OperationMeta {
    OperationMeta::local("1", OperationKind::Manual)
}

fn preview(
    session: &mut Session,
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    context: &PlanContext,
    meta: &OperationMeta,
) -> Result<Preview> {
    let mut progress = ProgressReporter::silent(SyncOperation::Preview, selection);
    preview_with_progress(
        session,
        publication,
        desired,
        selection,
        context,
        meta,
        &mut progress,
    )
}

#[allow(clippy::too_many_arguments)]
fn deploy(
    session: &mut Session,
    connect: impl FnMut() -> Result<Box<dyn RemoteOps>> + Send + 'static,
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
    selection: &DeploySelection,
    context: &PlanContext,
    meta: &OperationMeta,
    expected_plan_hash: Option<&str>,
    force: bool,
) -> Result<Deployment> {
    let mut progress = ProgressReporter::silent(SyncOperation::Deploy, selection);
    deploy_with_progress(
        session,
        connect,
        publication,
        desired,
        selection,
        context,
        meta,
        expected_plan_hash,
        force,
        &mut progress,
    )
}

fn remote_state(remote: &Shared) -> ServerDeploymentState {
    serde_json::from_slice(
        remote
            .lock()
            .unwrap()
            .contents(STATE_REMOTE)
            .expect("state file missing"),
    )
    .expect("remote state file is not valid deployment state")
}

fn plant_live_lease(remote: &mut MemoryRemote) {
    remote.dirs.insert(LEASE_DIR_REMOTE.to_owned());
    let record = LeaseRecord {
        owner: "worker:vps".to_owned(),
        executor: ExecutorKind::Worker,
        operation_id: "op-x".to_owned(),
        acquired_at: Utc::now(),
        heartbeat_at: Utc::now(),
        ttl_secs: lease::LEASE_TTL.as_secs(),
    };
    remote.put_file(LEASE_FILE_REMOTE, &serde_json::to_vec(&record).unwrap());
}

#[test]
fn missing_server_directory_is_rejected() {
    assert!(open(Arc::new(Mutex::new(MemoryRemote::new()))).is_err());
}

#[test]
fn ambiguous_rename_does_not_start_a_second_mutation_sequence() {
    let mut remote = MemoryRemote::new();
    remote.ftp_rename_semantics = true;
    let target = RemotePathBuf::new("/srv/BepInEx/config/mod.cfg").unwrap();
    let temporary = target.with_suffix(".gale-upload");
    remote.put_file(target.as_str(), b"original");
    // A missing temporary file can mean an earlier rename completed
    // but its reply was lost. The old target and any backup stay put.
    let error = replace_remote(&mut remote, &temporary, &target).unwrap_err();
    assert!(format!("{error:#}").contains("may need reconciliation"));
    assert!(format!("{error:#}").contains("was absent"));
    assert_eq!(
        remote.contents(target.as_str()),
        Some(b"original".as_slice())
    );
    assert!(
        remote
            .contents(target.with_suffix(".gale-backup").as_str())
            .is_none()
    );
}

#[test]
fn definite_ftp_rename_conflict_replaces_via_backup() {
    let mut remote = MemoryRemote::new();
    remote.ftp_rename_semantics = true;
    let target = RemotePathBuf::new("/srv/BepInEx/config/mod.cfg").unwrap();
    let temporary = target.with_suffix(".gale-upload");
    remote.put_file(target.as_str(), b"original");
    remote.put_file(temporary.as_str(), b"updated");

    replace_remote(&mut remote, &temporary, &target).unwrap();
    assert_eq!(
        remote.contents(target.as_str()),
        Some(b"updated".as_slice())
    );
    assert!(remote.contents(temporary.as_str()).is_none());
    assert!(
        remote
            .contents(target.with_suffix(".gale-backup").as_str())
            .is_none()
    );
}

#[test]
fn payload_deploy_uploads_and_commits_state() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();
    let memory = remote();
    let mut session = open(memory.clone()).unwrap();
    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();

    assert!(
        remote_state(&memory).restart_required,
        "restart must survive a crash before finalization"
    );
    assert_eq!(deployment.summary.uploaded_files, 1);

    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    // The payload landed, ownership and revision were recorded, the
    // state file was persisted remotely, and the lease was released.
    assert_eq!(
        remote_contents(&memory, MOD_DLL_REMOTE),
        Some(b"dll-bytes".to_vec())
    );
    assert_eq!(state.mods_revision, Some(publication.mods_revision.clone()));
    assert_eq!(
        state
            .files
            .get(&deploy_path("BepInEx/plugins/Author-ModA/ModA.dll")),
        Some(&OwnedFile {
            hash: ContentHash::from_hash(blake3::hash(b"dll-bytes")),
            size: 9,
        })
    );
    assert!(state.restart_required);
    let record = state.last_operation.unwrap();
    assert_eq!(record.status, OperationStatus::Succeeded);
    assert_eq!(record.executor, ExecutorKind::Local);

    let persisted = remote_state(&memory);
    assert_eq!(persisted.mods_revision, state.mods_revision);
    assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
}

#[test]
fn deploy_rejects_a_stale_plan_hash() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();
    let memory = remote();
    let mut session = open(memory.clone()).unwrap();

    let result = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        Some("bogus-hash"),
        false,
    );

    assert!(result.is_err());
    // Nothing was written: no upload, no state, no lease.
    assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
    assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
    assert!(remote_contents(&memory, STATE_REMOTE).is_none());
}

#[test]
fn live_lease_blocks_deploy_and_is_reported_by_preview() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();
    let memory = remote();
    plant_live_lease(&mut memory.lock().unwrap());
    let mut session = open(memory.clone()).unwrap();

    let preview = preview(
        &mut session,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert_eq!(preview.busy.unwrap().record.unwrap().owner, "worker:vps");

    let result = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    );
    match result {
        Err(err) => assert!(err.downcast_ref::<lease::LeaseBusy>().is_some()),
        Ok(_) => panic!("expected LeaseBusy"),
    }
    assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
}

#[test]
fn failed_upload_records_failure_and_releases_lease() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();
    let memory = remote();
    memory
        .lock()
        .unwrap()
        .fail_always
        .insert(format!("{MOD_DLL_REMOTE}.gale-upload"));
    let mut session = open(memory.clone()).unwrap();

    let result = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    );
    assert!(result.is_err());

    // The accurate state was still committed remotely: the operation is
    // recorded as failed, restart is flagged, and the failed file was
    // never claimed as owned.
    let persisted = remote_state(&memory);
    let record = persisted.last_operation.unwrap();
    assert_eq!(record.status, OperationStatus::Failed);
    assert!(persisted.restart_required);
    assert!(persisted.files.is_empty());
    assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
}

#[test]
fn a_failure_that_changed_no_files_does_not_flag_a_restart() {
    // An operation can fail before it touches the server, e.g. when
    // the lease cannot be verified. Its failure record must not ask
    // for a restart the server does not need.
    let memory = remote();
    let mut session = open(memory.clone()).unwrap();
    let lease = session.acquire_lease(&meta(), false).unwrap();
    let plan = DeploymentPlan {
        hash: String::new(),
        publication_revision: Utc::now(),
        mods_revision: ModRevision::from_hash(blake3::hash(b"rev")),
        mods_phase: true,
        configs_phase: false,
        uploads: Vec::new(),
        upload_bytes: 0,
        removals: Vec::new(),
        directory_removals: Vec::new(),
        unchanged_files: 0,
        config_entries: Vec::new(),
        unmanaged: Vec::new(),
    };

    let _ = abort(
        &mut session,
        lease,
        &meta(),
        &plan,
        eyre::eyre!("could not verify the deployment lease"),
        "deployment failed during the file phase",
    );

    let persisted = remote_state(&memory);
    assert_eq!(
        persisted.last_operation.unwrap().status,
        OperationStatus::Failed
    );
    assert!(!persisted.restart_required);
}

#[test]
fn failed_removal_keeps_ownership_for_retry() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    // Desired payload does not include the stale owned file.
    let desired = DesiredDeployment::new();

    let memory = remote();
    let stale = "BepInEx/plugins/Old/Old.dll";
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(&format!("{BASE}/{stale}"), b"old");
    }
    let mut state = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    state.files.insert(
        deploy_path(stale),
        OwnedFile {
            hash: ContentHash::from_hash(blake3::hash(b"old-hash")),
            size: 3,
        },
    );
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
        remote.fail_always.insert(format!("{BASE}/{stale}"));
    }

    let mut session = open(memory.clone()).unwrap();
    let result = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .err()
    .expect("failed removal must fail the deployment");
    assert!(format!("{result:#}").contains("failed to remove"));
    assert!(remote_contents(&memory, &format!("{BASE}/{stale}")).is_some());
    let state = remote_state(&memory);
    assert!(state.files.contains_key(&deploy_path(stale)));
    assert_eq!(state.mods_revision, None);
    assert_eq!(
        state.last_operation.unwrap().status,
        OperationStatus::Failed
    );
}

#[test]
fn selected_config_is_written_and_applied() {
    let mut fixture = mod_fixture();
    fixture
        .config
        .insert(config_path("BepInEx/config/mod.cfg"), config_file(b"v2"));
    let publication = fixture.publication();

    let memory = remote();
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file("/srv/BepInEx/config/mod.cfg", b"custom");
        remote.put_file("/srv/BepInEx/config/other.cfg", b"server-only");
    }
    let mut session = open(memory.clone()).unwrap();

    let mut sel = selection(false, true);
    decide(
        &mut sel,
        config_path("BepInEx/config/mod.cfg"),
        ConfigDecision::Apply,
    );

    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &sel,
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    // Selected config applied; the unselected server file is untouched.
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
        Some(b"v2".to_vec())
    );
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/other.cfg"),
        Some(b"server-only".to_vec())
    );
    assert_eq!(
        state.config[&config_path("BepInEx/config/mod.cfg")].applied,
        Some(ContentHash::from_hash(blake3::hash(b"v2")))
    );
    // A config write also requires a restart; NotRequired cannot clear it.
    assert!(state.restart_required);
}

#[test]
fn deleted_published_config_is_restored_only_by_explicit_selection() {
    let path = config_path("BepInEx/config/mod.cfg");
    let file = config_file(b"published");
    let mut fixture = mod_fixture();
    fixture.config.insert(path.clone(), file.clone());
    let publication = fixture.publication();
    let memory = remote();
    let mut previous = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    previous.record_applied(&path, &file.hash);
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(STATE_REMOTE, &state::serialize(&previous).unwrap());
        remote.put_file("/srv/BepInEx/config/server-only.cfg", b"server-only");
    }

    let mut session = open(memory.clone()).unwrap();
    let pending = preview(
        &mut session,
        &publication,
        &DesiredDeployment::default(),
        &selection(false, true),
        &context(),
        &meta(),
    )
    .unwrap();
    assert_eq!(
        pending.plan.config_entries[0].action,
        ConfigAction::Pending {
            reason: PendingConfigReason::DeletedLocally
        }
    );
    assert!(pending.plan.uploads.is_empty());

    let mut selected = selection(false, true);
    decide(&mut selected, path.clone(), ConfigDecision::Apply);
    let still_pending = preview(
        &mut session,
        &publication,
        &DesiredDeployment::default(),
        &selected,
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(still_pending.plan.uploads.is_empty());

    decide(&mut selected, path.clone(), ConfigDecision::Restore);
    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &selected,
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    finish(
        &mut session,
        deployment,
        RestartOutcome::AwaitingManual,
        &meta(),
    )
    .unwrap();
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
        Some(b"published".to_vec())
    );
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/server-only.cfg"),
        Some(b"server-only".to_vec())
    );
    assert_eq!(
        open(memory).unwrap().state.config[&path].applied,
        Some(file.hash)
    );
}

#[test]
fn publication_config_removal_leaves_remote_copy_untouched() {
    let path = config_path("BepInEx/config/server-only.cfg");
    let publication = mod_fixture().publication();
    let memory = remote();
    let mut previous = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    previous.record_applied(&path, &config_file(b"formerly-published").hash);
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(STATE_REMOTE, &state::serialize(&previous).unwrap());
        remote.put_file("/srv/BepInEx/config/server-only.cfg", b"server-customized");
    }

    let mut session = open(memory.clone()).unwrap();
    let selected = selection(false, true);
    let previewed = preview(
        &mut session,
        &publication,
        &DesiredDeployment::default(),
        &selected,
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(previewed.plan.config_entries.is_empty());
    assert!(previewed.plan.uploads.is_empty());
    assert!(previewed.plan.removals.is_empty());

    let deployed = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &selected,
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    finish(&mut session, deployed, RestartOutcome::NotRequired, &meta()).unwrap();
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/server-only.cfg"),
        Some(b"server-customized".to_vec())
    );
}

#[test]
fn out_of_scope_published_text_does_not_write_or_recreate_config_records() {
    let ordinary = config_path("BepInEx/config/mod.cfg");
    let translation = config_path("BepInEx/plugins/Mod/translations/en.json");
    let loader = config_path("doorstop_config.ini");
    let mut fixture = mod_fixture();
    for path in [&ordinary, &translation, &loader] {
        fixture
            .config
            .insert(path.clone(), config_file(b"publication"));
    }
    let publication = fixture.publication();
    let memory = remote();
    let mut previous = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    for path in [&translation, &loader] {
        previous.config.insert(path.clone(), Default::default());
    }
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(STATE_REMOTE, &state::serialize(&previous).unwrap());
        remote.put_file(
            "/srv/BepInEx/plugins/Mod/translations/en.json",
            b"server translation",
        );
        remote.put_file("/srv/doorstop_config.ini", b"host loader");
    }

    let mut session = open(memory.clone()).unwrap();
    assert_eq!(session.warnings.len(), 1);
    assert!(session.warnings[0].contains("ignored 2 out-of-scope"));
    let mut selected = selection(false, true);
    decide(&mut selected, ordinary.clone(), ConfigDecision::Apply);
    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &selected,
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    finish(
        &mut session,
        deployment,
        RestartOutcome::AwaitingManual,
        &meta(),
    )
    .unwrap();
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
        Some(b"publication".to_vec())
    );
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/plugins/Mod/translations/en.json"),
        Some(b"server translation".to_vec())
    );
    assert_eq!(
        remote_contents(&memory, "/srv/doorstop_config.ini"),
        Some(b"host loader".to_vec())
    );
    let reopened = open(memory).unwrap();
    assert!(reopened.warnings.is_empty());
    assert_eq!(reopened.state.config.len(), 1);
    assert!(reopened.state.config.contains_key(&ordinary));
}

#[test]
fn modified_unselected_config_stays_pending_and_untouched() {
    let mut fixture = mod_fixture();
    fixture
        .config
        .insert(config_path("BepInEx/config/mod.cfg"), config_file(b"v2"));
    let publication = fixture.publication();

    let memory = remote();
    memory
        .lock()
        .unwrap()
        .put_file("/srv/BepInEx/config/mod.cfg", b"custom");
    let mut session = open(memory.clone()).unwrap();

    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &selection(false, true),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
        Some(b"custom".to_vec())
    );
    // The conflict is recorded for the next preview: nothing applied,
    // nothing declined, the file is still undecided.
    let record = &state.config[&config_path("BepInEx/config/mod.cfg")];
    assert!(record.applied.is_none() && record.written.is_none());
    assert!(record.declined.is_none());
}

#[test]
fn declined_config_is_recorded_without_a_write_or_restart() {
    let path = config_path("BepInEx/config/mod.cfg");
    let mut fixture = mod_fixture();
    fixture.config.insert(path.clone(), config_file(b"v2"));
    let publication = fixture.publication();

    let memory = remote();
    memory
        .lock()
        .unwrap()
        .put_file("/srv/BepInEx/config/mod.cfg", b"custom");
    let mut session = open(memory.clone()).unwrap();

    let mut sel = selection(false, true);
    decide(&mut sel, path.clone(), ConfigDecision::Decline);
    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &sel,
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
        Some(b"custom".to_vec())
    );
    assert_eq!(
        state.config[&path].declined,
        Some(ContentHash::from_hash(blake3::hash(b"v2")))
    );
    assert!(!state.restart_required);
    assert!(!remote_state(&memory).restart_required);
}

#[test]
fn mods_only_never_reads_writes_or_records_configs() {
    // The hard boundary: includeMods without includeConfigs performs
    // literally zero config interaction. No reconciliation reads. No
    // writes, no entries, no conflicts, no config state mutation.
    let mut fixture = mod_fixture();
    fixture.config.insert(
        config_path("BepInEx/config/published.cfg"),
        config_file(b"v2"),
    );
    let publication = fixture.publication();

    let memory = remote();
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file("/srv/BepInEx/config/server.cfg", b"server-owned");
        remote.put_file("/srv/BepInEx/config/published.cfg", b"old");
        // Any config reconciliation read must fail the operation.
        remote
            .fail_read_always
            .insert("/srv/BepInEx/config/server.cfg".to_owned());
        remote
            .fail_read_always
            .insert("/srv/BepInEx/config/published.cfg".to_owned());
    }
    let mut session = open(memory.clone()).unwrap();
    let preview = preview(
        &mut session,
        &publication,
        &desired_payload(),
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(preview.plan.config_entries.is_empty());
    assert!(
        preview
            .plan
            .uploads
            .iter()
            .all(|upload| upload.kind == UploadKind::Payload)
    );

    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired_payload(),
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    // Nothing read, nothing written, nothing recorded.
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/server.cfg"),
        Some(b"server-owned".to_vec())
    );
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/published.cfg"),
        Some(b"old".to_vec())
    );
    assert!(state.config.is_empty());
}

/// Deploys `desired` onto the memory remote and finishes with an
/// unchanged remote. Reader-pool tests share this setup.
fn deploy_to_memory(
    memory: &Shared,
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
) {
    let mut session = open(memory.clone()).unwrap();
    let deployment = deploy(
        &mut session,
        no_connect,
        publication,
        desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
}

/// The pooled snapshot must match the serial plan with bounded helpers.
#[test]
fn memory_readers_match_the_serial_snapshot() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = large_mods_payload(8);
    let memory = remote();
    deploy_to_memory(&memory, &publication, &desired);

    let serial = {
        let mut session = open(memory.clone()).unwrap();
        preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap()
    };
    assert!(serial.plan.uploads.is_empty());

    memory.lock().unwrap().allow_readers = true;
    memory.lock().unwrap().readers_opened = 0;
    let mut session = open(memory.clone()).unwrap();
    let pooled = preview(
        &mut session,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert_eq!(pooled.plan.hash, serial.plan.hash);
    assert_eq!(pooled.plan.uploads, serial.plan.uploads);
    let opened = memory.lock().unwrap().readers_opened;
    assert!(
        (1..=MAX_SNAPSHOT_READERS).contains(&opened),
        "expected bounded helper readers, opened {opened}"
    );
}

/// When helper connections cannot be opened the snapshot runs over
/// the authoritative connection and produces the serial plan.
#[test]
fn memory_reader_connect_failure_falls_back_to_serial() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = large_mods_payload(8);
    let memory = remote();
    deploy_to_memory(&memory, &publication, &desired);

    let serial = {
        let mut session = open(memory.clone()).unwrap();
        preview(
            &mut session,
            &publication,
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap()
    };

    {
        let mut remote = memory.lock().unwrap();
        remote.allow_readers = true;
        remote.fail_reader_connect = true;
    }
    let mut session = open(memory.clone()).unwrap();
    let fallback = preview(
        &mut session,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert_eq!(fallback.plan.hash, serial.plan.hash);
    assert_eq!(memory.lock().unwrap().readers_opened, 0);
}

/// The zero-config boundary holds with the pool active: a mods-only
/// preview opens reader helpers but performs no config reads. Any
/// config read fails the operation.
#[test]
fn mods_only_preview_with_readers_never_reads_configs() {
    let mut fixture = mod_fixture();
    fixture.config.insert(
        config_path("BepInEx/config/published.cfg"),
        config_file(b"v2"),
    );
    let publication = fixture.publication();
    let desired = large_mods_payload(8);
    let memory = remote();
    deploy_to_memory(&memory, &publication, &desired);

    {
        let mut remote = memory.lock().unwrap();
        remote.allow_readers = true;
        remote.put_file(
            &format!("{BASE}/BepInEx/config/server.cfg"),
            b"server-owned",
        );
        remote
            .fail_read_always
            .insert(format!("{BASE}/BepInEx/config/server.cfg"));
        remote
            .fail_read_always
            .insert(format!("{BASE}/BepInEx/config/published.cfg"));
    }
    let mut session = open(memory.clone()).unwrap();
    let previewed = preview(
        &mut session,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(
        memory.lock().unwrap().readers_opened > 0,
        "the reader pool must be active for this test to cover it"
    );
    assert!(previewed.plan.config_entries.is_empty());
    assert!(previewed.plan.uploads.is_empty());
}

#[test]
fn config_preview_reads_the_remote_configs_it_evaluates() {
    // The same boundary from the other side: an explicit config
    // preview does reconcile remote config content, so a refused
    // read fails the operation rather than silently skipping it.
    let mut fixture = mod_fixture();
    fixture.config.insert(
        config_path("BepInEx/config/published.cfg"),
        config_file(b"v2"),
    );
    let publication = fixture.publication();

    let memory = remote();
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file("/srv/BepInEx/config/published.cfg", b"old");
        remote
            .fail_read_always
            .insert("/srv/BepInEx/config/published.cfg".to_owned());
    }
    let mut session = open(memory.clone()).unwrap();
    assert!(
        preview(
            &mut session,
            &publication,
            &desired_payload(),
            &selection(false, true),
            &context(),
            &meta(),
        )
        .is_err(),
        "an explicit config preview must read the configs it evaluates"
    );
}

#[test]
fn removal_is_bounded_to_owned_files_and_strays_survive() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();

    let memory = remote();
    let stale = "BepInEx/plugins/Old/Old.dll";
    {
        let mut remote = memory.lock().unwrap();
        // A manually installed, never-owned plugin inside the payload
        // dir: removal authority comes from records, not directory
        // membership, so it must survive and be surfaced as unmanaged.
        remote.put_file("/srv/BepInEx/plugins/ServerOnly.dll", b"stray");
        remote.put_file(&format!("{BASE}/{stale}"), b"old");
        // Outside the managed scope nothing is ever touched: config
        // files and unrelated server content survive every deploy.
        remote.put_file("/srv/BepInEx/config/user.cfg", b"user");
        remote.put_file("/srv/saves/world.db", b"save");
    }
    let mut state = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    state.files.insert(
        deploy_path(stale),
        OwnedFile {
            hash: ContentHash::from_hash(blake3::hash(b"old")),
            size: 3,
        },
    );
    memory
        .lock()
        .unwrap()
        .put_file(STATE_REMOTE, &state::serialize(&state).unwrap());

    let mut session = open(memory.clone()).unwrap();
    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    // The never-owned server-only plugin is reported as unmanaged.
    let unmanaged = deployment.plan.unmanaged.clone();

    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    // The obsolete Gale-owned plugin was removed; the never-owned
    // server-only plugin survived.
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/plugins/ServerOnly.dll"),
        Some(b"stray".to_vec())
    );
    assert_eq!(
        unmanaged,
        vec![deploy_path("BepInEx/plugins/ServerOnly.dll")]
    );
    assert!(remote_contents(&memory, &format!("{BASE}/{stale}")).is_none());
    assert!(!state.files.contains_key(&deploy_path(stale)));
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/user.cfg"),
        Some(b"user".to_vec())
    );
    assert_eq!(
        remote_contents(&memory, "/srv/saves/world.db"),
        Some(b"save".to_vec())
    );
}

#[test]
fn same_size_remote_edit_of_owned_payload_is_repaired() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();
    let memory = remote();
    let mut session = open(memory.clone()).unwrap();

    // First deployment lands ModA.
    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    // The remote file is then edited to different bytes of the same
    // length. A size comparison alone cannot see the change.
    memory
        .lock()
        .unwrap()
        .put_file(MOD_DLL_REMOTE, b"edited-dd");

    let mut session = open(memory.clone()).unwrap();
    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();

    // Content hashing detected the divergence and re-uploaded.
    assert_eq!(deployment.summary.uploaded_files, 1);
    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
    assert_eq!(
        remote_contents(&memory, MOD_DLL_REMOTE),
        Some(b"dll-bytes".to_vec())
    );
    assert_eq!(state.mods_revision, Some(publication.mods_revision.clone()));
}

#[test]
fn deploy_rejects_approval_when_remote_config_changed() {
    let mut fixture = mod_fixture();
    fixture
        .config
        .insert(config_path("BepInEx/config/mod.cfg"), config_file(b"v2"));
    let publication = fixture.publication();

    let memory = remote();
    memory
        .lock()
        .unwrap()
        .put_file("/srv/BepInEx/config/mod.cfg", b"server-v1");
    let mut session = open(memory.clone()).unwrap();

    let mut sel = selection(false, true);
    decide(
        &mut sel,
        config_path("BepInEx/config/mod.cfg"),
        ConfigDecision::Apply,
    );

    let preview = preview(
        &mut session,
        &publication,
        &DesiredDeployment::default(),
        &sel,
        &context(),
        &meta(),
    )
    .unwrap();
    let approved = preview.plan.hash;

    // The remote file changes after the approval was given. The planned action is still Write,
    // but the approved content no longer matches what is actually there.
    memory
        .lock()
        .unwrap()
        .put_file("/srv/BepInEx/config/mod.cfg", b"server-v2-edited");

    let result = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &sel,
        &context(),
        &meta(),
        Some(&approved),
        false,
    );
    match result {
        Err(err) => assert!(err.to_string().contains("preview")),
        Ok(_) => panic!("a stale approval must be rejected"),
    }
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/mod.cfg"),
        Some(b"server-v2-edited".to_vec())
    );
}

#[test]
fn deploy_rejects_a_changed_restart_policy() {
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();
    let memory = remote();
    let mut session = open(memory.clone()).unwrap();

    let preview = preview(
        &mut session,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();

    // The approval was given for Manual restarts; deploying with
    // Immediate must not reuse it.
    let mut changed = context();
    changed.restart_policy = RestartPolicy::Immediate;
    let result = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &changed,
        &meta(),
        Some(&preview.plan.hash),
        false,
    );
    match result {
        Err(err) => assert!(err.to_string().contains("preview")),
        Ok(_) => panic!("a restart-policy change must invalidate the approval"),
    }
    assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
}

#[test]
fn set_config_policy_is_serialized_under_the_lease() {
    let memory = remote();
    let mut session = open(memory.clone()).unwrap();
    let path = config_path("BepInEx/config/mod.cfg");
    let hash = ContentHash::from_hash(blake3::hash(b"v1"));

    // While another executor holds the lease, the policy write is busy.
    plant_live_lease(&mut memory.lock().unwrap());
    let blocked = set_config_policy(
        &mut session,
        &path,
        ConfigUpdatePolicy::AlwaysKeep,
        Some(&hash),
        &meta(),
    );
    match blocked {
        Err(err) => assert!(err.downcast_ref::<lease::LeaseBusy>().is_some()),
        Ok(()) => panic!("a policy update must not bypass the deployment lease"),
    }

    // Once the lease frees up, the policy write goes through. The
    // record file is deleted first because a directory can't be
    // removed while it still holds the file.
    memory
        .lock()
        .unwrap()
        .delete_file(&RemotePathBuf::new(LEASE_FILE_REMOTE).unwrap())
        .unwrap();
    memory
        .lock()
        .unwrap()
        .delete_dir(&RemotePathBuf::new(LEASE_DIR_REMOTE).unwrap())
        .unwrap();
    set_config_policy(
        &mut session,
        &path,
        ConfigUpdatePolicy::AlwaysKeep,
        Some(&hash),
        &meta(),
    )
    .unwrap();
    let record = remote_state(&memory).config[&path].clone();
    assert_eq!(record.policy, ConfigUpdatePolicy::AlwaysKeep);
    assert_eq!(record.policy_set_at, Some(hash));
}

#[test]
fn failed_config_write_marks_the_operation_partial() {
    let mut fixture = mod_fixture();
    fixture
        .config
        .insert(config_path("BepInEx/config/one.cfg"), config_file(b"one"));
    fixture
        .config
        .insert(config_path("BepInEx/config/two.cfg"), config_file(b"two"));
    let publication = fixture.publication();

    let memory = remote();
    memory
        .lock()
        .unwrap()
        .fail_always
        .insert("/srv/BepInEx/config/two.cfg.gale-upload".to_owned());
    let mut session = open(memory.clone()).unwrap();

    let mut sel = selection(false, true);
    decide(
        &mut sel,
        config_path("BepInEx/config/one.cfg"),
        ConfigDecision::Apply,
    );
    decide(
        &mut sel,
        config_path("BepInEx/config/two.cfg"),
        ConfigDecision::Apply,
    );

    let deployment = deploy(
        &mut session,
        no_connect,
        &publication,
        &DesiredDeployment::default(),
        &sel,
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(
        deployment.failed_config_writes,
        vec![config_path("BepInEx/config/two.cfg")]
    );

    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();

    // One config was written and recorded as applied. The other stays
    // retryable, and the operation is Partial, not Succeeded.
    let record = state.last_operation.unwrap();
    assert_eq!(record.status, OperationStatus::Partial);
    assert_eq!(
        state.config[&config_path("BepInEx/config/one.cfg")].applied,
        Some(ContentHash::from_hash(blake3::hash(b"one")))
    );
    assert_eq!(
        remote_contents(&memory, "/srv/BepInEx/config/one.cfg"),
        Some(b"one".to_vec())
    );
    assert!(
        state
            .config
            .get(&config_path("BepInEx/config/two.cfg"))
            .is_none_or(|record| record.applied.is_none())
    );
}

#[test]
fn external_restart_requires_explicit_acknowledgment_under_the_lease() {
    let memory = remote();
    let state = ServerDeploymentState {
        version: state::VERSION,
        restart_required: true,
        ..Default::default()
    };
    memory
        .lock()
        .unwrap()
        .put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
    let mut session = open(memory.clone()).unwrap();
    assert!(session.state.restart_required);

    let acknowledged = acknowledge_external_restart(&mut session, &meta()).unwrap();
    assert!(!acknowledged.restart_required);
    assert_eq!(acknowledged.operation_seq, 1);
    let record = acknowledged.last_operation.unwrap();
    assert_eq!(record.restart, RestartOutcome::Restarted);
    assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
    assert!(!open(memory.clone()).unwrap().state.restart_required);
    assert!(acknowledge_external_restart(&mut session, &meta()).is_err());

    // A later no-change deployment cannot resurrect the old reminder.
    let fixture = mod_fixture();
    let deployment = deploy(
        &mut session,
        no_connect,
        &fixture.publication(),
        &DesiredDeployment::default(),
        &selection(false, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    let state = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
    assert!(!state.restart_required);
}

#[test]
fn only_a_confirmed_gale_restart_clears_an_outstanding_restart() {
    for (outcome, required) in [
        (RestartOutcome::Restarted, false),
        (RestartOutcome::StartupUnverified, true),
        (RestartOutcome::Failed, true),
        (RestartOutcome::NotRequired, true),
        (RestartOutcome::AwaitingManual, true),
    ] {
        let fixture = mod_fixture();
        let memory = remote();
        let mut session = open(memory.clone()).unwrap();
        let deployment = deploy(
            &mut session,
            no_connect,
            &fixture.publication(),
            &desired_payload(),
            &selection(true, false),
            &context(),
            &meta(),
            None,
            false,
        )
        .unwrap();
        let state = finish(&mut session, deployment, outcome, &meta()).unwrap();
        assert_eq!(state.restart_required, required, "{outcome:?}");
        let persisted = open(memory).unwrap().state;
        assert_eq!(
            persisted.restart_required, required,
            "persisted {outcome:?}"
        );
        assert_eq!(persisted.last_operation.unwrap().restart, outcome);
    }
}

#[test]
fn deploy_does_not_confuse_a_dead_transport_with_a_takeover() {
    // The transport dies after the session opens. Whatever fails
    // first must report the transport problem, not a phantom
    // competing executor. It must leave payload files untouched.
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();
    let memory = remote();
    let mut session = open(memory.clone()).unwrap();
    memory.lock().unwrap().connection_dead = true;

    let err = deploy(
        &mut session,
        no_connect,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .err()
    .expect("a dead transport must fail the deployment");

    let chain = format!("{err:#}");
    assert!(
        !chain.contains("taken over"),
        "a transport failure must not blame another executor: {chain}"
    );
    assert!(remote_contents(&memory, MOD_DLL_REMOTE).is_none());
}

mod ftp;

#[test]
fn preview_reports_multifile_hashing_and_config_reads_after_a_reconnect() {
    use crate::profile::server::progress::{ProgressStatus, SyncPhase};

    let memory = remote();
    let mut fixture = mod_fixture();
    let mut desired = DesiredDeployment::default();
    let mut state = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    for index in 0..4 {
        let path = deploy_path(&format!("BepInEx/plugins/Author-ModA/file-{index:02}.dll"));
        let bytes = format!("payload-{index:02}");
        desired.insert(path.clone(), staged(bytes.as_bytes()));
        state.files.insert(
            path.clone(),
            OwnedFile {
                hash: ContentHash::from_hash(blake3::hash(bytes.as_bytes())),
                size: bytes.len() as u64,
            },
        );
        memory
            .lock()
            .unwrap()
            .put_file(&format!("{BASE}/{path}"), bytes.as_bytes());
    }
    for index in 0..2 {
        let path = config_path(&format!("BepInEx/config/file-{index:02}.cfg"));
        let bytes = format!("config-{index:02}");
        fixture
            .config
            .insert(path.clone(), config_file(bytes.as_bytes()));
        memory
            .lock()
            .unwrap()
            .put_file(&format!("{BASE}/{path}"), bytes.as_bytes());
    }
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
        remote
            .fail_read_once
            .insert(format!("{BASE}/BepInEx/plugins/Author-ModA/file-01.dll"));
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let mut progress = ProgressReporter::new(
        "preview-run".to_owned(),
        SyncOperation::Preview,
        &selection(true, true),
        move |snapshot| sink.lock().unwrap().push(snapshot),
    );
    let mut session = open(memory).unwrap();
    let result = preview_with_progress(
        &mut session,
        &fixture.publication(),
        &desired,
        &selection(true, true),
        &context(),
        &meta(),
        &mut progress,
    );
    assert!(result.is_ok(), "{result:?}");
    progress.succeeded();

    let events = events.lock().unwrap();
    let first = |phase| {
        events
            .iter()
            .position(|event| event.phase == phase)
            .unwrap()
    };
    assert!(first(SyncPhase::VerifyingPayload) < first(SyncPhase::CheckingConfigs));
    for (phase, total) in [
        (SyncPhase::VerifyingPayload, 4),
        (SyncPhase::CheckingConfigs, 2),
    ] {
        let updates: Vec<_> = events
            .iter()
            .filter(|event| event.phase == phase && event.total == Some(total))
            .collect();
        assert!(
            updates
                .windows(2)
                .all(|pair| pair[0].completed <= pair[1].completed)
        );
        let mut completions: Vec<_> = updates.iter().map(|event| event.completed).collect();
        completions.dedup();
        assert_eq!(completions, (0..=total).collect::<Vec<_>>());
    }
    assert_eq!(events.last().unwrap().status, ProgressStatus::Succeeded);
    assert_eq!(
        events.last().unwrap().completed_phases,
        events.last().unwrap().total_phases
    );
}

#[test]
fn failed_preview_keeps_the_hashing_phase_and_last_path() {
    use crate::profile::server::progress::{ProgressStatus, SyncPhase};

    let memory = remote();
    let path = deploy_path("BepInEx/plugins/Author-ModA/broken.dll");
    let bytes = b"owned payload";
    let mut desired = DesiredDeployment::default();
    desired.insert(path.clone(), staged(bytes));
    let mut state = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    state.files.insert(
        path.clone(),
        OwnedFile {
            hash: ContentHash::from_hash(blake3::hash(bytes)),
            size: bytes.len() as u64,
        },
    );
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(&format!("{BASE}/{path}"), bytes);
        remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
        remote.fail_read_always.insert(format!("{BASE}/{path}"));
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let mut progress = ProgressReporter::new(
        "failed-run".to_owned(),
        SyncOperation::Preview,
        &selection(true, false),
        move |snapshot| sink.lock().unwrap().push(snapshot),
    );
    let mut session = open(memory).unwrap();
    assert!(
        preview_with_progress(
            &mut session,
            &mod_fixture().publication(),
            &desired,
            &selection(true, false),
            &context(),
            &meta(),
            &mut progress,
        )
        .is_err()
    );
    progress.failed();
    let events = events.lock().unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.status, ProgressStatus::Failed);
    assert_eq!(last.phase, SyncPhase::VerifyingPayload);
    assert_eq!(last.item.as_deref(), Some(path.as_str()));
    assert_eq!(last.completed, 0);
    assert_eq!(last.total, Some(1));
}

#[tokio::test]
async fn deploy_reports_revalidation_mutations_bytes_and_completion() {
    use crate::profile::server::progress::{ProgressStatus, SyncPhase};

    let memory = remote();
    let old = deploy_path("BepInEx/plugins/Old/old.dll");
    let mut state = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    state.files.insert(
        old.clone(),
        OwnedFile {
            hash: ContentHash::from_hash(blake3::hash(b"old")),
            size: 3,
        },
    );
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(&format!("{BASE}/{old}"), b"old");
        remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
    }
    let mut desired = DesiredDeployment::default();
    for index in 0..4 {
        let path = deploy_path(&format!("BepInEx/plugins/New/file-{index}.dll"));
        desired.insert(path, staged(&vec![index as u8; (index + 1) * 1024]));
    }
    let config = config_path("BepInEx/config/published.cfg");
    let mut fixture = mod_fixture();
    fixture
        .config
        .insert(config.clone(), config_file(b"published"));
    let mut selected = selection(true, true);
    decide(&mut selected, config, ConfigDecision::Apply);
    let mut preview_session = open(memory.clone()).unwrap();
    let approved = preview(
        &mut preview_session,
        &fixture.publication(),
        &desired,
        &selected,
        &context(),
        &meta(),
    )
    .unwrap();
    let mut session = open(memory.clone()).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let mut progress = ProgressReporter::new(
        "deploy-run".to_owned(),
        SyncOperation::Deploy,
        &selected,
        move |snapshot| sink.lock().unwrap().push(snapshot),
    );
    let operation = meta();
    let deployment = deploy_with_progress(
        &mut session,
        no_connect,
        &fixture.publication(),
        &desired,
        &selected,
        &context(),
        &operation,
        Some(&approved.plan.hash),
        false,
        &mut progress,
    )
    .unwrap();
    let host = host::from_settings(&Default::default(), None);
    complete_with_progress(
        session,
        deployment,
        host.as_ref(),
        RestartPolicy::Manual,
        operation,
        &mut progress,
    )
    .await
    .unwrap();

    let events = events.lock().unwrap();
    let position = |phase| {
        events
            .iter()
            .position(|event| event.phase == phase)
            .unwrap()
    };
    assert!(position(SyncPhase::VerifyingPayload) < position(SyncPhase::RemovingFiles));
    assert!(position(SyncPhase::CheckingConfigs) < position(SyncPhase::RemovingFiles));
    assert!(position(SyncPhase::RemovingFiles) < position(SyncPhase::UploadingPayload));
    assert!(position(SyncPhase::UploadingPayload) < position(SyncPhase::PersistingState));
    assert!(position(SyncPhase::WritingConfigs) < position(SyncPhase::PersistingState));
    assert!(position(SyncPhase::PersistingState) < position(SyncPhase::ApplyingRestart));
    let completed = |phase| {
        events
            .iter()
            .rev()
            .find(|event| event.phase == phase)
            .unwrap()
    };
    let removal = completed(SyncPhase::RemovingFiles);
    assert_eq!(removal.completed, removal.total.unwrap());
    assert!(removal.completed >= 1);
    let upload = completed(SyncPhase::UploadingPayload);
    assert_eq!(upload.completed, 4);
    assert_eq!(upload.completed_bytes, Some(10 * 1024));
    assert_eq!(upload.total_bytes, Some(10 * 1024));
    assert_eq!(completed(SyncPhase::WritingConfigs).completed, 1);
    assert_eq!(events.last().unwrap().status, ProgressStatus::Succeeded);
}

#[tokio::test(start_paused = true)]
async fn restart_verification_requires_evidence_and_stops_promptly() {
    use std::collections::VecDeque;

    use futures_util::future::BoxFuture;

    struct RestartHost {
        statuses: Mutex<VecDeque<Result<HostStatus>>>,
        last: HostStatus,
        probes: Mutex<usize>,
        fail_restart: bool,
    }

    impl HostControl for RestartHost {
        fn can_restart(&self) -> bool {
            true
        }

        fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if self.fail_restart {
                    bail!("restart rejected");
                }
                Ok(())
            })
        }

        fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
            Box::pin(async move {
                *self.probes.lock().unwrap() += 1;
                self.statuses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok(self.last.clone()))
            })
        }

        fn name(&self) -> &'static str {
            "test host"
        }
    }

    let status = |running, booting| HostStatus {
        running: Some(running),
        booting,
        players: None,
    };
    let before = status(true, Some(false));
    for (name, statuses, last, fail_restart, expected, probes) in [
        (
            "dathost reboot",
            vec![
                Ok(before.clone()),
                Ok(status(true, Some(true))),
                Ok(status(true, Some(false))),
            ],
            before.clone(),
            false,
            RestartOutcome::Restarted,
            3,
        ),
        (
            "generic stop/start",
            vec![
                Ok(status(true, None)),
                Ok(status(false, None)),
                Ok(status(true, None)),
            ],
            status(true, None),
            false,
            RestartOutcome::Restarted,
            3,
        ),
        (
            "on alone",
            vec![Ok(before.clone())],
            before.clone(),
            false,
            RestartOutcome::StartupUnverified,
            13,
        ),
        (
            "unknown booting",
            vec![Ok(status(true, None)), Ok(status(true, Some(true)))],
            status(true, None),
            false,
            RestartOutcome::StartupUnverified,
            13,
        ),
        (
            "status failure",
            vec![Ok(before.clone()), Err(eyre::eyre!("status failed"))],
            before.clone(),
            false,
            RestartOutcome::StartupUnverified,
            2,
        ),
        (
            "stuck booting",
            vec![Ok(before.clone())],
            status(true, Some(true)),
            false,
            RestartOutcome::StartupUnverified,
            13,
        ),
        (
            "restart failure",
            vec![Ok(before.clone())],
            before.clone(),
            true,
            RestartOutcome::Failed,
            1,
        ),
    ] {
        let host = RestartHost {
            statuses: Mutex::new(VecDeque::from(statuses)),
            last,
            probes: Mutex::new(0),
            fail_restart,
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut progress = ProgressReporter::new(
            "restart-run".to_owned(),
            SyncOperation::Deploy,
            &selection(true, true),
            move |snapshot| sink.lock().unwrap().push(snapshot),
        );
        progress.phase(SyncPhase::ApplyingRestart);
        let result =
            apply_restart_policy_reporting(&host, RestartPolicy::Immediate, true, &mut progress)
                .await
                .outcome;
        assert_eq!(result, expected, "{name}");
        assert_eq!(*host.probes.lock().unwrap(), probes, "{name}");
        if name == "dathost reboot" {
            assert!(events.lock().unwrap().iter().any(|event| {
                event.item.as_deref()
                    == Some("Waiting for server to finish booting (check 2 of 12)")
            }));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_restart_reports_its_reason_in_the_deployment_warnings() {
    use futures_util::future::BoxFuture;

    struct FailingHost {
        restart_error: Option<&'static str>,
        status_error: &'static str,
    }

    impl HostControl for FailingHost {
        fn can_restart(&self) -> bool {
            true
        }

        fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                match self.restart_error {
                    Some(error) => {
                        bail!(error);
                    }
                    None => Ok(()),
                }
            })
        }

        fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
            Box::pin(async move {
                bail!(self.status_error);
            })
        }

        fn name(&self) -> &'static str {
            "test host"
        }
    }

    for (host, expected, reason) in [
        (
            FailingHost {
                restart_error: Some("provider rejected the restart"),
                status_error: "status unavailable",
            },
            RestartOutcome::Failed,
            "provider rejected the restart",
        ),
        (
            FailingHost {
                restart_error: None,
                status_error: "status endpoint timed out",
            },
            RestartOutcome::StartupUnverified,
            "status endpoint timed out",
        ),
    ] {
        let memory = remote();
        let mut desired = DesiredDeployment::default();
        desired.insert(deploy_path("BepInEx/plugins/New/new.dll"), staged(b"new"));
        let fixture = mod_fixture();
        let selected = selection(true, false);
        let approved = preview(
            &mut open(memory.clone()).unwrap(),
            &fixture.publication(),
            &desired,
            &selected,
            &context(),
            &meta(),
        )
        .unwrap();
        let mut session = open(memory).unwrap();
        let mut progress = ProgressReporter::silent(SyncOperation::Deploy, &selected);
        let operation = meta();
        let deployment = deploy_with_progress(
            &mut session,
            no_connect,
            &fixture.publication(),
            &desired,
            &selected,
            &context(),
            &operation,
            Some(&approved.plan.hash),
            false,
            &mut progress,
        )
        .unwrap();

        let result = complete_with_progress(
            session,
            deployment,
            &host,
            RestartPolicy::Immediate,
            operation,
            &mut progress,
        )
        .await
        .unwrap();

        assert_eq!(result.restart, expected);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains(reason)),
            "{reason} missing from {:?}",
            result.warnings
        );
    }
}

// --- remote accessors through the shared handle ---

fn remote_has_dir(remote: &Shared, path: &str) -> bool {
    remote.lock().unwrap().dirs.contains(path)
}

fn remote_contents(remote: &Shared, path: &str) -> Option<Vec<u8>> {
    remote.lock().unwrap().files.get(path).cloned()
}
