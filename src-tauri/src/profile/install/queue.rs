use std::{
    collections::{HashSet, VecDeque},
    fs,
    future::Future,
    io::Cursor,
    iter,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

use eyre::{Context, Result, bail, eyre};
use itertools::Itertools;
use tauri::AppHandle;
use tokio::sync::{Notify, futures::Notified, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;
use zip::ZipArchive;

use crate::{
    logger,
    profile::{DependencyInstall, ModKey, Profile, server},
    state::ManagerExt,
    thunderstore::Thunderstore,
    util::error::IoResultExt,
};

use super::{
    CancelBehavior, HideReason, InstallError, InstallEvent, InstallOptions, InstallResult,
    InstallTask, ModInstall, check_cancel, emit,
};

pub struct InstallQueue {
    state: Mutex<State>,
    /// Notified when a batch is pushed to the queue.
    notify_push: Notify,
    /// Notified when all batches have been completed.
    notify_empty: Notify,
    /// Notified when any single batch has been completed.
    notify_batch_done: Notify,
    cancel: AtomicBool,
}

#[derive(Default)]
struct State {
    pending: VecDeque<InstallBatch>,
    /// The profile id and package uuids of the currently installing batch.
    processing: Option<(i64, Vec<Uuid>)>,
}

impl InstallQueue {
    pub fn new(app: AppHandle) -> Arc<Self> {
        let cancel = AtomicBool::new(false);

        let this = Arc::new(Self {
            state: Mutex::new(State::default()),
            notify_push: Notify::new(),
            notify_empty: Notify::new(),
            notify_batch_done: Notify::new(),
            cancel,
        });

        tauri::async_runtime::spawn(handle_queue(Arc::clone(&this), app));

        this
    }

    pub fn wait_for_empty(&'_ self) -> Notified<'_> {
        self.notify_empty.notified()
    }

    /// Returns a future that resolves when any batch finishes processing.
    ///
    /// Use [`Notified::enable`] to subscribe before checking queue state.
    pub fn wait_for_batch(&'_ self) -> Notified<'_> {
        self.notify_batch_done.notified()
    }

    pub fn cancel_all(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn lock(&'_ self) -> InstallQueueLock<'_> {
        InstallQueueLock {
            state: self.state.lock().unwrap(),
            queue: self,
        }
    }

    /// Queue mods to be installed. Returns a future that resolves when the batch is completed,
    /// errors out, or is cancelled.
    ///
    /// This checks for mods already in the queue and skips them.
    pub fn install(
        &self,
        mods: impl IntoIterator<Item = ModInstall>,
        profile_id: i64,
        options: InstallOptions,
        app: &AppHandle,
    ) -> impl Future<Output = InstallResult<()>> {
        self.lock().push_batch(mods, profile_id, options, app)
    }

    /// Queue mods to be installed, along with their dependencies. Returns a future that resolves
    /// when the batch is completed, errors out, or is cancelled.
    ///
    /// This checks for mods already in the queue *and* already installed ones and skips them.
    ///
    /// Errors out if a "root" mod is already installed and is not replacing that entry.
    pub fn install_with_deps(
        &self,
        mods: Vec<ModInstall>,
        profile_id: i64,
        options: InstallOptions,
        app: &AppHandle,
    ) -> Result<impl Future<Output = InstallResult<()>>> {
        // Resolve before taking the queue lock: the manager is always locked
        // before the queue, so taking it while holding the queue could
        // deadlock against a command that holds the manager.
        let mods = resolve_with_deps(mods, profile_id, app)?;
        Ok(self.lock().push_batch(mods, profile_id, options, app))
    }
}

/// A RAII guard for the inner state of the [`InstallQueue`].
pub struct InstallQueueLock<'a> {
    state: MutexGuard<'a, State>,
    queue: &'a InstallQueue,
}

impl InstallQueueLock<'_> {
    /// Whether any mods are currently being installed.
    pub fn is_processing(&self) -> bool {
        self.state.processing.is_some()
    }

    /// Whether any mods are queued for this installation on this profile.
    pub fn has_any_for_profile(&self, profile_id: i64) -> bool {
        self.state
            .processing
            .as_ref()
            .is_some_and(|(other_profile_id, _)| *other_profile_id == profile_id)
            || self
                .state
                .pending
                .iter()
                .any(|batch| batch.profile_id == profile_id)
    }

    pub fn has_mod(&self, uuid: Uuid, profile_id: i64) -> bool {
        self.state
            .processing
            .as_ref()
            .is_some_and(|(other_profile_id, other_uuids)| {
                *other_profile_id == profile_id && other_uuids.contains(&uuid)
            })
            || self.state.pending.iter().any(|batch| {
                batch.profile_id == profile_id
                    && batch
                        .mods
                        .iter()
                        .any(|install| install.uuid() == uuid || install.replaces() == Some(uuid))
            })
    }

    fn push_batch<I>(
        &mut self,
        mods: I,
        profile_id: i64,
        options: InstallOptions,
        app: &AppHandle,
    ) -> impl Future<Output = InstallResult<()>> + use<I>
    where
        I: IntoIterator<Item = ModInstall>,
    {
        let (tx, rx) = oneshot::channel();

        let mods = mods
            .into_iter()
            .filter(|install| !self.has_mod(install.uuid(), profile_id))
            .collect_vec();

        let mod_count = mods.len();
        let bytes = mods.iter().map(|install| install.file_size as i64).sum();

        let batch = InstallBatch {
            mods,
            options,
            profile_id,
            on_complete: tx,
        };

        if mod_count == 0 {
            info!(options = ?batch.options, "no mods to install, completing batch immediately");
            // complete the task immediately since there are no mods to install
            batch.complete(Ok(()), app);
        } else if let Err(err) = server::ensure_profile_unlocked(app, profile_id) {
            // fail before downloading anything; the install itself checks
            // again under the manager lock right before writing
            batch.complete(Err(InstallError::Error(err)), app);
        } else {
            info!(len = mod_count, options = ?batch.options, "pushing batch to install queue");
            self.state.pending.push_back(batch);
            self.queue.notify_push.notify_waiters();

            emit(
                InstallEvent::AddCount {
                    mods: mod_count,
                    bytes,
                },
                app,
            );
        }

        async move {
            match rx.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(err),
                Err(err) => Err(InstallError::Error(eyre!(err))),
            }
        }
    }

    fn pop_next(&mut self) -> Option<InstallBatch> {
        let next = self.state.pending.pop_front();
        self.state.processing = next.as_ref().map(|batch| {
            (
                batch.profile_id,
                batch
                    .mods
                    .iter()
                    .flat_map(|install| iter::once(install.uuid()).chain(install.replaces()))
                    .collect(),
            )
        });

        // notify after `processing` changes so a waiter can never see a completed batch as still running
        // also covers the final transition that clears `processing` when the queue drains
        self.queue.notify_batch_done.notify_waiters();

        next
    }
}

/// Adds the missing dependencies of `mods`, ordered so dependencies install
/// first.
fn resolve_with_deps(
    mods: Vec<ModInstall>,
    profile_id: i64,
    app: &AppHandle,
) -> Result<Vec<ModInstall>> {
    let manager = app.lock_manager();
    let thunderstore = app.lock_thunderstore();
    let (_, profile) = manager.profile_by_id(profile_id)?;

    resolve_profile_installs(mods, profile, &thunderstore)
}

fn resolve_profile_installs(
    mods: Vec<ModInstall>,
    profile: &Profile,
    thunderstore: &Thunderstore,
) -> Result<Vec<ModInstall>> {
    for install in mods.iter().filter(|install| install.replaces().is_none()) {
        if let Err(err) = profile.ensure_key_free(&install.key(), None) {
            return Err(with_switch_hint(err, install, profile, thunderstore));
        }
    }

    for (i, install) in mods.iter().enumerate() {
        if let Some(other) = mods[..i]
            .iter()
            .find(|other| other.key() == install.key() && other.uuid() != install.uuid())
        {
            bail!(
                "{} and {} are the same mod; install only one of them",
                other.ident,
                install.ident
            );
        }
    }

    let planned = mods
        .iter()
        .map(|install| install.id.borrow(thunderstore))
        .collect::<Result<Vec<_>>>()?;

    // Validation and installation use the same resolved dependency versions.
    let mut resolved = Vec::new();
    for install in &mods {
        let borrowed = install.id.borrow(thunderstore)?;
        let dependencies = profile
            .resolve_install_dependencies(borrowed.dependencies(), &planned, thunderstore)
            .context("failed to resolve dependencies")?;

        resolved.push(install.clone());
        for dependency in dependencies {
            resolved.push(dependency_install(dependency, &mods, profile)?);
        }
    }

    Ok(dedupe_by_key(resolved)
        .into_iter()
        .rev() // install dependencies first
        .collect_vec())
}

/// Points at the installed entry's switch action when `install` is the copy it offers.
fn with_switch_hint(
    error: eyre::Report,
    install: &ModInstall,
    profile: &Profile,
    thunderstore: &Thunderstore,
) -> eyre::Report {
    let Some((entry, _)) = profile
        .find_by_key(&install.key())
        .and_then(|(_, profile_mod)| profile_mod.as_thunderstore())
    else {
        return error;
    };

    match profile.source_alternative(entry, thunderstore) {
        Some(alternative)
            if alternative
                .reason
                .is_some_and(|reason| reason.is_prompted())
                && alternative.target.package.uuid == install.uuid()
                && alternative.target.version.uuid == install.mod_id().version_uuid =>
        {
            let site = alternative.target.package.backend.display_name();
            let version = alternative.target.version.version();
            eyre!(
                "{error}. To use {site} {version}, choose \"Switch to {site} {version}\" on the installed mod."
            )
        }
        _ => error,
    }
}

fn dependency_install(
    dependency: DependencyInstall<'_>,
    planned: &[ModInstall],
    profile: &Profile,
) -> Result<ModInstall> {
    Ok(match dependency {
        DependencyInstall::Add(dependency) => planned
            .iter()
            .find(|planned| planned.uuid() == dependency.package.uuid)
            .cloned()
            .unwrap_or_else(|| ModInstall::from(dependency)),
        DependencyInstall::Raise { installed, target } => {
            let index = profile.index_of(installed)?;
            let entry = &profile.mods[index];

            ModInstall::from(target)
                .replacing(installed)
                .with_index(index)
                .with_state(entry.enabled)
                .with_time(entry.install_time)
        }
    })
}

/// Keeps one install per mod name. Repeats of the same version keep the first
/// occurrence; different versions of one name keep the newest.
fn dedupe_by_key(installs: Vec<ModInstall>) -> Vec<ModInstall> {
    let mut unique: Vec<ModInstall> = Vec::new();
    for install in installs {
        let key = install.key();
        match unique.iter_mut().find(|kept| kept.key() == key) {
            None => unique.push(install),
            Some(kept) if kept.mod_id() != install.mod_id() && is_newer(&install, kept) => {
                *kept = install;
            }
            Some(_) => {}
        }
    }
    unique
}

fn is_newer(install: &ModInstall, kept: &ModInstall) -> bool {
    match (
        install.ident.version().parse::<semver::Version>(),
        kept.ident.version().parse::<semver::Version>(),
    ) {
        (Ok(install), Ok(kept)) => install > kept,
        _ => false,
    }
}

/// Makes room for `install` before any of its files are written: removes the
/// entry it replaces, and errors if another entry still holds its name.
fn prepare_slot(install: &ModInstall, profile: &mut Profile) -> Result<()> {
    profile.ensure_key_free(&install.key(), install.replaces())?;

    if let Some(replaced) = install.replaces()
        && profile.has_mod(replaced)
    {
        profile
            .force_remove_mod(replaced)
            .context("failed to remove existing version")?;
    }

    Ok(())
}

pub struct InstallBatch {
    mods: Vec<ModInstall>,
    options: InstallOptions,
    profile_id: i64,
    on_complete: oneshot::Sender<InstallResult<()>>,
}

impl InstallBatch {
    fn complete(self, result: InstallResult<()>, app: &AppHandle) {
        match self.on_complete.send(result) {
            Ok(()) => (),
            // The receiver was dropped (meaning the original caller doesn't care anymore), however
            // if we succeeded or was cancelled we don't care either ...
            Err(Ok(())) => (),
            Err(Err(InstallError::Cancelled)) => (),
            Err(Err(InstallError::Error(err))) => {
                // ... but on errors we want to log the error and notify the frontend.
                logger::log_webview_err("Failed to install batch", err, app);
            }
        }
    }
}

async fn handle_queue(queue: Arc<InstallQueue>, app: AppHandle) {
    // continously wait for new batches to be pushed and process them
    loop {
        queue.notify_push.notified().await;
        queue.cancel.store(false, Ordering::SeqCst);

        debug!("started processing batches");

        let mut reason = HideReason::Done;

        emit(InstallEvent::Show, &app);

        loop {
            let batch = queue.lock().pop_next();

            match batch {
                Some(batch) => {
                    reason = handle_batch(batch, &queue.cancel, &app).await;
                }
                None => break,
            }
        }

        debug!(?reason, "finished processing batches");

        emit(InstallEvent::Hide { reason }, &app);

        app.lock_manager()
            .cache_mods(&app.lock_thunderstore(), &app.lock_prefs())
            .ok();

        queue.notify_empty.notify_waiters();
    }
}

async fn handle_batch(batch: InstallBatch, cancel: &AtomicBool, app: &AppHandle) -> HideReason {
    let mut result = Ok(());
    let mut reason = HideReason::Done;

    for (i, install) in batch.mods.iter().enumerate() {
        result = handle_install(&batch, i, cancel, app).await;

        match result {
            Ok(()) => (),
            Err(InstallError::Cancelled) => {
                rollback_batch(&batch, app, i).unwrap_or_else(|err| {
                    warn!("failed to rollback cancelled installation: {}", err);
                });

                // cancel all pending bathes
                let mut handle = app.install_queue().lock();
                for batch in handle.state.pending.drain(..) {
                    batch.complete(Err(InstallError::Cancelled), app);
                }

                reason = HideReason::Cancelled;
                break;
            }
            Err(InstallError::Error(err)) => {
                rollback_batch(&batch, app, i)
                    .unwrap_or_else(|err| warn!(?err, "failed to rollback failed installation",));

                result = Err(InstallError::Error(
                    err.wrap_err(format!("failed to install {}", install.ident)),
                ));

                reason = HideReason::Error;
                break;
            }
        }
    }

    info!(len = batch.mods.len(), "completed batch");

    batch.complete(result, app);
    reason
}

fn rollback_batch(batch: &InstallBatch, app: &AppHandle, count: usize) -> Result<()> {
    match batch.options.cancel_behavior {
        CancelBehavior::Individual => Ok(()),
        CancelBehavior::Prevent => {
            warn!("tried to roll back batch with CancelBehavior::Prevent!");

            Ok(())
        }
        CancelBehavior::Batch => {
            info!(
                batch_len = batch.mods.len(),
                rollback_count = count,
                "rolling back batch"
            );

            let mut manager = app.lock_manager();
            let thunderstore = app.lock_thunderstore();

            let (_, profile) = manager.profile_by_id_mut(batch.profile_id)?;

            for uuid in rollback_removals(&batch.mods[..count], &thunderstore) {
                profile.force_remove_mod(uuid).unwrap_or_else(|err| {
                    warn!(?uuid, ?err, "failed to rollback installed mod");
                });
            }
            drop(thunderstore);

            profile.save(app, true)?;
            Ok(())
        }
    }
}

/// Which of `installed` a batch rollback removes. Installs that replace an old
/// entry are kept (the old files are already gone), and so is anything they
/// transitively depend on — removing those would strand the kept copy.
fn rollback_removals(installed: &[ModInstall], thunderstore: &Thunderstore) -> Vec<Uuid> {
    let mut retained: HashSet<ModKey> = installed
        .iter()
        .filter(|install| install.replaces().is_some())
        .map(ModInstall::key)
        .collect();
    let mut queue: Vec<ModKey> = retained.iter().cloned().collect();

    while let Some(key) = queue.pop() {
        let Some(install) = installed.iter().find(|install| install.key() == key) else {
            continue;
        };
        // an install that doesn't resolve in the catalog has no readable dependencies
        let Ok(borrowed) = install.mod_id().borrow(thunderstore) else {
            continue;
        };
        for dependency in &borrowed.version.dependencies {
            let dependency_key = ModKey::new(dependency.full_name());
            if retained.insert(dependency_key.clone()) {
                queue.push(dependency_key);
            }
        }
    }

    installed
        .iter()
        .filter(|install| install.replaces().is_none() && !retained.contains(&install.key()))
        .map(ModInstall::uuid)
        .collect()
}

async fn handle_install(
    batch: &InstallBatch,
    index: usize,
    cancel: &AtomicBool,
    app: &AppHandle,
) -> InstallResult<()> {
    match try_cache_install(batch, index, app)? {
        CacheStatus::Hit => Ok(()),
        CacheStatus::Miss => {
            let bytes = super::download::download(&batch.mods[index], cancel, &batch.options, app)
                .await
                .context("error while downloading")?;
            install_from_download(bytes, batch, index, cancel, app)
                .context("error while installing")?;

            Ok(())
        }
    }
}

enum CacheStatus {
    Hit,
    Miss,
}

fn try_cache_install(batch: &InstallBatch, index: usize, app: &AppHandle) -> Result<CacheStatus> {
    let install = &batch.mods[index];

    let cache_path = super::cache::path(
        &app.lock_prefs().cache_dir(),
        &install.ident,
        install.id.backend,
    );

    if !cache_path.exists() {
        return Ok(CacheStatus::Miss);
    }

    emit(
        InstallEvent::set_task(&install.ident, InstallTask::Install),
        app,
    );

    let mut manager = app.lock_manager();

    let (game, profile) = manager.profile_by_id_mut(batch.profile_id)?;
    server::ensure_profile_unlocked(app, batch.profile_id)?;

    prepare_slot(install, profile)?;

    debug!(
        ident = %install.ident,
        "cache hit, installing from cache"
    );

    let package_name = install.ident.full_name();
    let mut installer = game.mod_loader.installer_for(package_name);
    installer.install(&cache_path, package_name, profile)?;

    install.clone().insert_into(profile)?;

    profile.save(app, true)?;

    emit(
        InstallEvent::AddProgress {
            mods: 1,
            bytes: install.file_size as i64,
        },
        app,
    );

    Ok(CacheStatus::Hit)
}

fn install_from_download(
    data: Vec<u8>,
    batch: &InstallBatch,
    index: usize,
    cancel: &AtomicBool,
    app: &AppHandle,
) -> InstallResult<()> {
    let manager = app.lock_manager();

    let install = &batch.mods[index];

    let cache_path = super::cache::path(
        &app.lock_prefs().cache_dir(),
        &install.ident,
        install.id.backend,
    );
    let package_name = install.ident.full_name();

    let (game, _) = manager.profile_by_id(batch.profile_id)?;
    drop(manager);

    fs::create_dir_all(&cache_path).fs_context("creating mod cache dir", &cache_path)?;

    let mut installer = game.mod_loader.installer_for(package_name);

    emit(
        InstallEvent::set_task(&install.ident, InstallTask::Extract),
        app,
    );

    let archive = ZipArchive::new(Cursor::new(data)).context("failed to open archive")?;

    installer
        .extract(archive, package_name, cache_path.clone())
        .inspect_err(|_| {
            // the cached mod is probably in an invalid state, so remove it
            fs::remove_dir_all(&cache_path).unwrap_or_else(|err| {
                warn!(
                    "failed to clean up after failed extraction of {}: {:#}",
                    install.ident, err
                );
            });
        })
        .context("error while extracting")?;

    check_cancel(cancel, &batch.options)?;

    emit(
        InstallEvent::set_task(&install.ident, InstallTask::Install),
        app,
    );

    let mut manager = app.lock_manager();
    let (_, profile) = manager.profile_by_id_mut(batch.profile_id)?;
    server::ensure_profile_unlocked(app, batch.profile_id)?;

    prepare_slot(install, profile)?;

    debug!(
        ident = %install.ident,
        "installing mod"
    );

    installer.install(&cache_path, package_name, profile)?;
    install.clone().insert_into(profile)?;

    profile.save(app, true)?;

    emit(InstallEvent::AddProgress { mods: 1, bytes: 0 }, app);

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use futures_util::FutureExt;
    use serde_json::json;

    use crate::{
        config::ConfigCache,
        game,
        prefs::Prefs,
        profile::ModKey,
        thunderstore::{Backend, BorrowedMod, PackageListing},
        util::fs::JsonStyle,
    };

    use super::*;

    pub(crate) fn listing(
        name: &str,
        uuid: u128,
        version: &str,
        dependencies: &[&str],
    ) -> PackageListing {
        let patch = version.parse::<semver::Version>().unwrap().patch;
        serde_json::from_value(json!({
            "full_name": format!("Example-{name}"),
            "uuid4": Uuid::from_u128(uuid),
            "categories": [],
            "date_created": "2024-01-01T00:00:00Z",
            "date_updated": "2024-01-01T00:00:00Z",
            "has_nsfw_content": false,
            "is_deprecated": false,
            "is_pinned": false,
            "package_url": "https://example.com/",
            "rating_score": 0,
            "versions": [{
                "full_name": format!("Example-{name}-{version}"),
                "uuid4": Uuid::from_u128(uuid * 100 + u128::from(patch)),
                "date_created": "2024-01-01T00:00:00Z",
                "dependencies": dependencies,
                "description": "Test dependency graph",
                "downloads": 0,
                "file_size": 100,
                "is_active": true,
                "website_url": ""
            }]
        }))
        .unwrap()
    }

    /// A listing whose versions are the given `versions`, newest first.
    pub(crate) fn listing_versions(name: &str, uuid: u128, versions: &[&str]) -> PackageListing {
        let mut package = listing(name, uuid, versions[0], &[]);
        for version in &versions[1..] {
            package
                .versions
                .push(listing(name, uuid, version, &[]).versions.remove(0));
        }
        package
    }

    pub(crate) fn catalog(packages: &[PackageListing]) -> Thunderstore {
        catalogs(&[(Backend::Thunderstore, packages)])
    }

    pub(crate) fn catalogs(backends: &[(Backend, &[PackageListing])]) -> Thunderstore {
        let dir = tempfile::tempdir().unwrap();
        let prefs: Prefs = serde_json::from_value(json!({ "dataDir": dir.path() })).unwrap();
        let game = game::from_slug("valheim").unwrap();
        let folder = dir.path().join("valheim");
        fs::create_dir(&folder).unwrap();
        let mut state = Thunderstore::new();
        for (backend, packages) in backends {
            crate::util::fs::write_json(
                folder.join(format!("{backend}_cache.json")),
                *packages,
                JsonStyle::Compact,
            )
            .unwrap();
            state
                .backend_mut(*backend)
                .read_and_insert_cache(game, &prefs);
        }
        state
    }

    pub(crate) fn install(state: &Thunderstore, uuid: u128) -> ModInstall {
        install_from(state, Backend::Thunderstore, uuid)
    }

    pub(crate) fn install_from(state: &Thunderstore, backend: Backend, uuid: u128) -> ModInstall {
        ModInstall::new(BorrowedMod::latest(
            state.get_package(Uuid::from_u128(uuid), backend).unwrap(),
        ))
    }

    pub(crate) fn install_version(
        state: &Thunderstore,
        backend: Backend,
        package_uuid: u128,
        version_uuid: u128,
    ) -> ModInstall {
        ModInstall::new(
            state
                .get_mod(
                    Uuid::from_u128(package_uuid),
                    Uuid::from_u128(version_uuid),
                    backend,
                )
                .unwrap(),
        )
    }

    pub(crate) fn profile(mods: Vec<ModInstall>) -> Profile {
        profile_at(PathBuf::new(), mods)
    }

    pub(crate) fn profile_at(path: PathBuf, mods: Vec<ModInstall>) -> Profile {
        Profile {
            id: 1,
            name: "Dependency test".to_owned(),
            path,
            mods: mods.iter().map(ModInstall::profile_mod).collect(),
            game: game::from_slug("valheim").unwrap(),
            ignored_version_updates: Default::default(),
            ignored_package_updates: Default::default(),
            config_cache: ConfigCache::default(),
            linked_config: Default::default(),
            modpack: None,
            sync: None,
            custom_args: String::new(),
            server_settings: None,
            missing: false,
            excluded_export_files: Default::default(),
        }
    }

    #[test]
    fn installed_dependency_satisfies_a_withdrawn_older_requirement() {
        let state = catalog(&[
            listing("Compat", 1, "1.1.4", &["Example-Loader-1.0.0"]),
            listing("Loader", 2, "1.0.1", &[]),
        ]);
        let profile = profile(vec![install(&state, 2)]);

        let planned = resolve_profile_installs(vec![install(&state, 1)], &profile, &state).unwrap();

        assert_eq!(
            planned.iter().map(ModInstall::uuid).collect_vec(),
            [Uuid::from_u128(1)]
        );
    }

    #[test]
    fn missing_uninstalled_dependency_still_rejects_the_install() {
        let state = catalog(&[listing("Compat", 1, "1.1.4", &["Example-Missing-1.0.0"])]);

        let error = resolve_profile_installs(vec![install(&state, 1)], &profile(vec![]), &state)
            .unwrap_err();

        assert!(format!("{error:#}").contains("Example-Missing-1.0.0"));
    }

    #[test]
    fn installed_dependency_resolves_its_actual_transitive_requirements() {
        let state = catalog(&[
            listing("Compat", 1, "1.1.4", &["Example-Loader-1.0.0"]),
            listing("Loader", 2, "1.0.1", &["Example-Support-1.0.0"]),
            listing("Support", 3, "1.0.0", &[]),
        ]);
        let profile = profile(vec![install(&state, 2)]);
        let planned = resolve_profile_installs(vec![install(&state, 1)], &profile, &state).unwrap();
        assert_eq!(
            planned.iter().map(ModInstall::uuid).collect_vec(),
            [Uuid::from_u128(3), Uuid::from_u128(1)]
        );
    }

    #[test]
    fn an_unknown_installed_dependency_version_is_not_replaced_with_catalog_metadata() {
        let state = catalog(&[
            listing("Compat", 1, "1.1.4", &["Example-Loader-1.0.0"]),
            listing("Loader", 2, "1.0.0", &[]),
        ]);
        let old_state = catalog(&[listing("Loader", 2, "1.0.1", &[])]);
        let profile = profile(vec![install(&old_state, 2)]);
        let error =
            resolve_profile_installs(vec![install(&state, 1)], &profile, &state).unwrap_err();
        assert!(format!("{error:#}").contains("version with id"));
    }

    #[test]
    fn planned_dependency_version_keeps_its_state_and_resolves_its_own_dependencies() {
        let mut loader = listing("Loader", 2, "1.0.1", &["Example-Support-1.0.0"]);
        loader.versions.push(
            listing("Loader", 2, "1.0.0", &["Example-Retired-1.0.0"])
                .versions
                .remove(0),
        );
        let state = catalog(&[
            listing("Compat", 1, "1.1.4", &["Example-Loader-1.0.0"]),
            loader,
            listing("Support", 3, "1.0.0", &[]),
        ]);
        let loader_install = install(&state, 2).with_state(false).with_index(4);

        let planned = resolve_profile_installs(
            vec![install(&state, 1), loader_install],
            &profile(vec![]),
            &state,
        )
        .unwrap();

        assert_eq!(
            planned.iter().map(ModInstall::uuid).collect_vec(),
            [Uuid::from_u128(3), Uuid::from_u128(2), Uuid::from_u128(1)]
        );
        assert!(!planned[1].enabled());
        assert_eq!(planned[1].index, Some(4));
    }

    pub(crate) fn test_queue() -> InstallQueue {
        InstallQueue {
            state: Mutex::new(State::default()),
            notify_push: Notify::new(),
            notify_empty: Notify::new(),
            notify_batch_done: Notify::new(),
            cancel: AtomicBool::new(false),
        }
    }

    fn batch(profile_id: i64) -> (InstallBatch, oneshot::Receiver<InstallResult<()>>) {
        let (tx, rx) = oneshot::channel();
        (
            InstallBatch {
                mods: Vec::new(),
                options: InstallOptions::default(),
                profile_id,
                on_complete: tx,
            },
            rx,
        )
    }

    // Regression test for the final batch hang: `processing` only clears on the next `pop_next`,
    // so the notification has to fire on that transition and not just after `handle_batch` returns.
    #[test]
    fn pop_next_notifies_when_processing_clears() {
        let queue = test_queue();

        let (batch, _rx) = batch(7);
        queue.lock().state.pending.push_back(batch);

        let started = queue.lock().pop_next();
        assert!(started.is_some());
        assert!(queue.lock().has_any_for_profile(7));

        let notified = queue.wait_for_batch();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // the last pop clears `processing`; the waiter gets notified so it
        // doesn't see the completed batch as still running and wait forever
        assert!(queue.lock().pop_next().is_none());
        assert!(!queue.lock().has_any_for_profile(7));
        assert!(notified.as_mut().now_or_never().is_some());
    }

    #[test]
    fn pop_next_notifies_when_next_batch_starts() {
        let queue = test_queue();

        let (first, _rx1) = batch(7);
        let (second, _rx2) = batch(8);
        let mut lock = queue.lock();
        lock.state.pending.push_back(first);
        lock.state.pending.push_back(second);
        drop(lock);

        queue.lock().pop_next().unwrap();
        assert!(queue.lock().has_any_for_profile(7));

        let notified = queue.wait_for_batch();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // the transition to the next batch also notifies, so a waiter on the
        // finished batch doesn't stall until the new batch completes
        assert!(queue.lock().pop_next().is_some());
        assert!(!queue.lock().has_any_for_profile(7));
        assert!(queue.lock().has_any_for_profile(8));
        assert!(notified.as_mut().now_or_never().is_some());
    }

    /// `Example-Mod` 1.0.0 on Thunderstore and Hexium, under different package uuids.
    /// The copies are identical versions so the refusal carries no switch hint.
    fn same_name_catalog() -> Thunderstore {
        catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])]),
            (Backend::Hexium, &[listing("Mod", 2, "1.0.0", &[])]),
        ])
    }

    #[test]
    fn installing_the_other_sites_copy_of_an_installed_mod_is_refused() {
        let state = same_name_catalog();
        let profile = profile(vec![install(&state, 1)]);

        let error = resolve_profile_installs(
            vec![install_from(&state, Backend::Hexium, 2)],
            &profile,
            &state,
        )
        .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Example-Mod is already installed from Thunderstore 1.0.0"
        );
    }

    #[test]
    fn the_refusal_offers_the_switch_when_the_requested_copy_is_the_alternative() {
        // Hexium has a newer 1.0.1, so it is the prompted alternative target.
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])]),
            (
                Backend::Hexium,
                &[listing_versions("Mod", 2, &["1.0.1", "1.0.0"])],
            ),
        ]);
        let profile = profile(vec![install(&state, 1)]);

        // version 1.0.1 has uuid 201 and is the prompted alternative target
        let error = resolve_profile_installs(
            vec![install_version(&state, Backend::Hexium, 2, 201)],
            &profile,
            &state,
        )
        .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Example-Mod is already installed from Thunderstore 1.0.0. \
             To use Hexium 1.0.1, choose \"Switch to Hexium 1.0.1\" on the installed mod."
        );

        // requesting a version other than the alternative target keeps the plain refusal
        let error = resolve_profile_installs(
            vec![install_version(&state, Backend::Hexium, 2, 200)],
            &profile,
            &state,
        )
        .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Example-Mod is already installed from Thunderstore 1.0.0"
        );
    }

    #[test]
    fn the_refusal_matches_names_that_differ_only_in_case() {
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])]),
            (Backend::Hexium, &[listing("mod", 2, "1.0.0", &[])]),
        ]);
        let profile = profile(vec![install(&state, 1)]);

        let error = resolve_profile_installs(
            vec![install_from(&state, Backend::Hexium, 2)],
            &profile,
            &state,
        )
        .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Example-Mod is already installed from Thunderstore 1.0.0"
        );
    }

    #[test]
    fn a_pack_raises_an_older_installed_module_on_the_same_site() {
        let state = catalogs(&[(
            Backend::Thunderstore,
            &[
                listing("Other", 3, "1.0.0", &[]),
                listing("Pack", 1, "1.0.0", &["Example-Module-0.3.2"]),
                listing_versions("Module", 2, &["0.3.2", "0.3.1"]),
            ],
        )]);
        let installed_at = "2024-02-03T04:05:06Z".parse::<DateTime<Utc>>().unwrap();
        let mut profile = profile(vec![
            install(&state, 3),
            install_version(&state, Backend::Thunderstore, 2, 201),
        ]);
        profile.mods[1].enabled = false;
        profile.mods[1].install_time = installed_at;

        let planned = resolve_profile_installs(vec![install(&state, 1)], &profile, &state).unwrap();

        let module = &planned[0];
        assert_eq!(module.uuid(), Uuid::from_u128(2));
        assert_eq!(module.replaces(), Some(Uuid::from_u128(2)));
        assert_eq!(module.mod_id().version_uuid, Uuid::from_u128(202));
        assert_eq!(module.index, Some(1));
        assert!(!module.enabled());
        assert_eq!(module.install_time, Some(installed_at));
    }

    #[test]
    fn a_pack_replaces_an_older_hexium_copy_with_the_pinned_thunderstore_version() {
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[
                    listing("Pack", 1, "1.0.0", &["Example-Module-0.3.2"]),
                    listing("Module", 2, "0.3.2", &[]),
                ],
            ),
            (Backend::Hexium, &[listing("Module", 3, "0.3.1", &[])]),
        ]);
        let profile = profile(vec![install_from(&state, Backend::Hexium, 3)]);

        let planned = resolve_profile_installs(vec![install(&state, 1)], &profile, &state).unwrap();

        assert_eq!(
            planned
                .iter()
                .map(|install| (install.uuid(), install.replaces(), install.mod_id().backend))
                .collect_vec(),
            [
                (
                    Uuid::from_u128(2),
                    Some(Uuid::from_u128(3)),
                    Backend::Thunderstore
                ),
                (Uuid::from_u128(1), None, Backend::Thunderstore),
            ]
        );
    }

    #[test]
    fn a_raise_stays_on_the_installed_entrys_site_when_both_sites_have_the_pin() {
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[
                    listing("Pack", 1, "1.0.0", &["Example-Module-0.3.2"]),
                    listing("Module", 2, "0.3.2", &[]),
                ],
            ),
            (
                Backend::Hexium,
                &[listing_versions("Module", 3, &["0.3.2", "0.3.1"])],
            ),
        ]);
        let profile = profile(vec![install_version(&state, Backend::Hexium, 3, 301)]);

        let planned = resolve_profile_installs(vec![install(&state, 1)], &profile, &state).unwrap();

        assert_eq!(planned[0].replaces(), Some(Uuid::from_u128(3)));
        assert_eq!(planned[0].mod_id().backend, Backend::Hexium);
        assert_eq!(planned[0].mod_id().version_uuid, Uuid::from_u128(302));
    }

    #[test]
    fn an_installed_copy_from_the_other_site_at_the_pin_is_not_installed_again() {
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Module", 2, "0.3.2", &[])]),
            (
                Backend::Hexium,
                &[listing("Pack", 1, "1.0.0", &["Example-Module-0.3.2"])],
            ),
        ]);
        let profile = profile(vec![install(&state, 2)]);

        let planned = resolve_profile_installs(
            vec![install_from(&state, Backend::Hexium, 1)],
            &profile,
            &state,
        )
        .unwrap();

        assert_eq!(
            planned.iter().map(ModInstall::uuid).collect_vec(),
            [Uuid::from_u128(1)]
        );
    }

    #[test]
    fn dependants_pinning_one_module_on_different_sites_install_it_once_at_the_highest_version() {
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[
                    listing("DependantA", 1, "1.0.0", &["Example-Module-0.3.1"]),
                    listing("Module", 2, "0.3.1", &[]),
                ],
            ),
            (
                Backend::Hexium,
                &[
                    listing("DependantB", 4, "1.0.0", &["Example-Module-0.3.2"]),
                    listing("Module", 3, "0.3.2", &[]),
                ],
            ),
        ]);
        let profile = profile(vec![]);

        let planned = resolve_profile_installs(
            vec![install(&state, 1), install_from(&state, Backend::Hexium, 4)],
            &profile,
            &state,
        )
        .unwrap();

        let modules = planned
            .iter()
            .filter(|install| install.key() == ModKey::new("Example-Module"))
            .collect_vec();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].mod_id().backend, Backend::Hexium);
        assert_eq!(modules[0].mod_id().version_uuid, Uuid::from_u128(302));
    }

    #[test]
    fn a_pinned_version_missing_everywhere_is_an_error_naming_the_installed_copy() {
        let state = catalog(&[
            listing("Pack", 1, "1.0.0", &["Example-Module-0.3.2"]),
            listing("Module", 2, "0.3.1", &[]),
        ]);
        let profile = profile(vec![install(&state, 2)]);

        let error =
            resolve_profile_installs(vec![install(&state, 1)], &profile, &state).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("Example-Module-0.3.2"), "{message}");
        assert!(message.contains("0.3.1 from Thunderstore"), "{message}");
    }

    #[test]
    fn a_same_name_install_without_replacement_is_refused_before_files_change() {
        let dir = tempfile::tempdir().unwrap();
        let state = same_name_catalog();
        let mut profile = profile_at(dir.path().to_path_buf(), vec![install(&state, 1)]);
        let plugin = dir.path().join("BepInEx/plugins/Example-Mod/plugin.dll");
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(&plugin, b"data").unwrap();

        let error =
            prepare_slot(&install_from(&state, Backend::Hexium, 2), &mut profile).unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Example-Mod is already installed from Thunderstore 1.0.0"
        );
        assert_eq!(
            profile.mods.iter().map(|m| m.uuid()).collect_vec(),
            [Uuid::from_u128(1)]
        );
        assert_eq!(fs::read(&plugin).unwrap(), b"data");
    }

    #[test]
    fn a_replacing_install_removes_the_entry_it_replaces_first() {
        let dir = tempfile::tempdir().unwrap();
        let state = same_name_catalog();
        let mut profile = profile_at(dir.path().to_path_buf(), vec![install(&state, 1)]);
        let plugin = dir.path().join("BepInEx/plugins/Example-Mod/plugin.dll");
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(&plugin, b"data").unwrap();

        let replacement = install_from(&state, Backend::Hexium, 2).replacing(Uuid::from_u128(1));
        prepare_slot(&replacement, &mut profile).unwrap();

        assert!(profile.mods.is_empty());
        assert!(!plugin.exists());
    }

    #[test]
    fn a_second_entry_for_one_name_is_refused_by_insert_mod() {
        let state = same_name_catalog();
        let mut profile = profile(vec![install(&state, 1)]);

        let error = profile
            .insert_mod(None, install_from(&state, Backend::Hexium, 2).profile_mod())
            .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Example-Mod is already installed from Thunderstore 1.0.0"
        );
        assert_eq!(profile.mods.len(), 1);
    }

    #[test]
    fn rollback_keeps_new_prerequisites_of_a_kept_replacement() {
        let state = catalog(&[
            listing("Module", 1, "1.0.1", &["Example-Needed-1.0.0"]),
            listing("Needed", 2, "1.0.0", &[]),
            listing("Unrelated", 3, "1.0.0", &[]),
        ]);

        let installed = [
            install(&state, 2),
            install_version(&state, Backend::Thunderstore, 1, 101).replacing(Uuid::from_u128(1)),
            install(&state, 3),
        ];

        assert_eq!(rollback_removals(&installed, &state), [Uuid::from_u128(3)]);
    }

    #[test]
    fn rollback_follows_prerequisites_transitively() {
        let state = catalog(&[
            listing("Module", 1, "1.0.1", &["Example-Needed-1.0.0"]),
            listing("Needed", 2, "1.0.0", &["Example-Base-1.0.0"]),
            listing("Base", 3, "1.0.0", &[]),
        ]);

        let installed = [
            install_version(&state, Backend::Thunderstore, 1, 101).replacing(Uuid::from_u128(1)),
            install(&state, 2),
            install(&state, 3),
        ];

        assert!(rollback_removals(&installed, &state).is_empty());
    }

    #[test]
    fn rollback_without_replacements_removes_everything_installed() {
        let state = catalog(&[
            listing("One", 1, "1.0.0", &[]),
            listing("Two", 2, "1.0.0", &[]),
        ]);

        let installed = [install(&state, 1), install(&state, 2)];

        assert_eq!(
            rollback_removals(&installed, &state),
            [Uuid::from_u128(1), Uuid::from_u128(2)]
        );
    }
}
